//! Accounts and sign-in.
//!
//! - Passwords are hashed with Argon2id (on a blocking thread, it's deliberately slow).
//! - Sign-in returns a random bearer token. Only its SHA-256 is stored in `sessions`.
//! - Sessions last 30 days and slide forward each time they're used.
//! - Each account owns one workspace. The very first account adopts the workspace that
//!   single-business mode used (`WORKSPACE_ID`), so existing data carries over.
//! - Repeated wrong passwords for one email are refused for a while (`LoginLimiter`).

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use argon2::{
    password_hash::{
        rand_core::OsRng, rand_core::RngCore, PasswordHash, PasswordHasher, PasswordVerifier,
        SaltString,
    },
    Argon2,
};
use axum::{extract::State, http::HeaderMap, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::routes::AppState;

const SESSION_DAYS: i32 = 30;
const MIN_PASSWORD: usize = 8;

#[derive(Debug, Clone, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub workspace_id: String,
}

/* ---------------- Tokens & passwords ---------------- */

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn hash_password(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
    })
    .await
    .map_err(|_| ApiError::bad("Couldn't secure the password."))?
    .map_err(|_| ApiError::bad("Couldn't secure the password."))
}

/// Checks a password. With no hash (unknown email) it checks against a throwaway hash, so a
/// wrong email takes as long as a wrong password and response times don't reveal accounts.
async fn verify_password(password: String, hash: Option<String>) -> bool {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    tokio::task::spawn_blocking(move || {
        let real = hash.is_some();
        let hash = hash.unwrap_or_else(|| {
            DUMMY
                .get_or_init(|| {
                    Argon2::default()
                        .hash_password(random_hex(16).as_bytes(), &SaltString::generate(&mut OsRng))
                        .map(|h| h.to_string())
                        .unwrap_or_default()
                })
                .clone()
        });
        let matches = PasswordHash::new(&hash)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false);
        matches && real
    })
    .await
    .unwrap_or(false)
}

/// The bearer token from `Authorization: Bearer <token>`.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The signed-in user for a token, sliding the session forward. `None` = unknown or expired.
pub async fn user_for_token(db: &PgPool, token: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as::<_, User>(
        "WITH s AS (
           UPDATE sessions SET last_seen_at = now(), expires_at = now() + make_interval(days => $2)
           WHERE token_hash = $1 AND expires_at > now()
           RETURNING user_id)
         SELECT u.id, u.email, u.name, u.workspace_id FROM users u JOIN s ON s.user_id = u.id",
    )
    .bind(hash_token(token))
    .bind(SESSION_DAYS)
    .fetch_optional(db)
    .await
}

async fn new_session(c: &mut PgConnection, user_id: &str) -> sqlx::Result<String> {
    let token = random_hex(32);
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(hash_token(&token))
    .bind(user_id)
    .bind(SESSION_DAYS)
    .execute(c)
    .await?;
    Ok(token)
}

/* ---------------- Brute-force protection ---------------- */

/// Counts failed sign-ins per email. After `max` failures inside `window`, sign-in for
/// that email is refused until the window passes. In memory: resets on restart.
pub struct LoginLimiter {
    max: u32,
    window: Duration,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(8, Duration::from_secs(15 * 60))
    }
}

impl LoginLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            failures: Mutex::new(HashMap::new()),
        }
    }
    fn blocked(&self, email: &str) -> bool {
        let mut m = self.failures.lock().expect("limiter lock");
        match m.get(email) {
            Some((_, since)) if since.elapsed() > self.window => {
                m.remove(email);
                false
            }
            Some((n, _)) => *n >= self.max,
            None => false,
        }
    }
    fn fail(&self, email: &str) {
        let mut m = self.failures.lock().expect("limiter lock");
        if m.len() > 10_000 {
            let window = self.window;
            m.retain(|_, (_, since)| since.elapsed() <= window);
        }
        let e = m.entry(email.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
    }
    fn clear(&self, email: &str) {
        self.failures.lock().expect("limiter lock").remove(email);
    }
}

/* ---------------- Handlers ---------------- */

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignupBody {
    email: String,
    password: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    business_name: String,
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
pub struct PasswordBody {
    current: String,
    next: String,
}

fn clean_email(e: &str) -> ApiResult<String> {
    let e = e.trim().to_lowercase();
    let ok = e.len() <= 254
        && e.split_once('@').is_some_and(|(a, b)| {
            !a.is_empty() && b.contains('.') && !b.starts_with('.') && !b.ends_with('.')
        })
        && !e.contains(char::is_whitespace);
    if ok {
        Ok(e)
    } else {
        Err(ApiError::bad("Enter a valid email address."))
    }
}

fn check_password(p: &str) -> ApiResult<()> {
    if p.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad(format!(
            "Use a password of at least {MIN_PASSWORD} characters."
        )));
    }
    if p.len() > 256 {
        return Err(ApiError::bad("That password is too long."));
    }
    Ok(())
}

fn session_json(token: String, user: &User) -> Json<Value> {
    Json(json!({ "token": token, "user": user }))
}

fn auth_off() -> ApiError {
    ApiError::NotFound("Accounts are turned off on this server (AUTH=off).".into())
}

/// `POST /api/auth/signup`
pub async fn signup(
    State(st): State<AppState>,
    Json(b): Json<SignupBody>,
) -> ApiResult<Json<Value>> {
    if !st.auth_required {
        return Err(auth_off());
    }
    let email = clean_email(&b.email)?;
    check_password(&b.password)?;
    let name = b.name.trim().chars().take(120).collect::<String>();
    let hash = hash_password(b.password).await?;

    let mut tx = st.db.begin().await?;
    // One sign-up at a time, so two "first" accounts can't both adopt the legacy workspace.
    sqlx::query("SELECT pg_advisory_xact_lock(727274)")
        .execute(&mut *tx)
        .await?;
    let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
    if taken.is_some() {
        return Err(ApiError::conflict(
            "An account with that email already exists. Sign in instead.",
        ));
    }
    let (any_user,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM users)")
        .fetch_one(&mut *tx)
        .await?;
    let id = random_hex(12);
    let ws = if any_user {
        format!("w-{id}")
    } else {
        st.workspace_id.clone()
    };
    let mut w = db::workspace(&mut tx, &ws).await?;
    // Fill in the business details the person gave, without overwriting what's there.
    let mut changed = false;
    for (field, value) in [
        (&mut w.owner_name, &name),
        (&mut w.email, &email),
        (&mut w.business_name, &b.business_name),
    ] {
        if field.trim().is_empty() && !value.trim().is_empty() {
            *field = value.trim().to_string();
            changed = true;
        }
    }
    if changed {
        db::save_workspace(&mut tx, &ws, &w).await?;
    }
    sqlx::query("INSERT INTO users (id, email, name, password_hash, workspace_id) VALUES ($1, $2, $3, $4, $5)")
        .bind(&id)
        .bind(&email)
        .bind(&name)
        .bind(&hash)
        .bind(&ws)
        .execute(&mut *tx)
        .await?;
    let token = new_session(&mut tx, &id).await?;
    db::log(
        &mut tx,
        &ws,
        &name,
        "create",
        "account",
        &id,
        &format!("Account created for {email}"),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(workspace = %ws, adopted = !any_user, "account created");
    Ok(session_json(
        token,
        &User {
            id,
            email,
            name,
            workspace_id: ws,
        },
    ))
}

/// `POST /api/auth/login`
pub async fn login(State(st): State<AppState>, Json(b): Json<LoginBody>) -> ApiResult<Json<Value>> {
    if !st.auth_required {
        return Err(auth_off());
    }
    let email = b.email.trim().to_lowercase();
    if st.limiter.blocked(&email) {
        return Err(ApiError::TooMany(
            "Too many wrong passwords. Wait 15 minutes and try again.".into(),
        ));
    }
    let row: Option<(String, String, String, String)> =
        sqlx::query_as("SELECT id, name, workspace_id, password_hash FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&st.db)
            .await?;
    let ok = verify_password(b.password, row.as_ref().map(|r| r.3.clone())).await;
    let Some((id, name, ws, _)) = row.filter(|_| ok) else {
        st.limiter.fail(&email);
        return Err(ApiError::Unauthorized("Wrong email or password.".into()));
    };
    st.limiter.clear(&email);
    let mut c = st.db.acquire().await?;
    let token = new_session(&mut c, &id).await?;
    Ok(session_json(
        token,
        &User {
            id,
            email,
            name,
            workspace_id: ws,
        },
    ))
}

/// `POST /api/auth/logout`: ends this session only.
pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    if let Some(t) = bearer(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(hash_token(t))
            .execute(&st.db)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/auth/me`: `{ authRequired, user }`. 401 when accounts are on and the token is bad.
pub async fn me(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    if !st.auth_required {
        return Ok(Json(json!({ "authRequired": false, "user": null })));
    }
    let user = current(&st, &headers).await?;
    Ok(Json(json!({ "authRequired": true, "user": user })))
}

/// `POST /api/auth/password`: change password; signs out every other session.
pub async fn change_password(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
    if !st.auth_required {
        return Err(auth_off());
    }
    let user = current(&st, &headers).await?;
    check_password(&b.next)?;
    let (hash,): (String,) = sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
        .bind(&user.id)
        .fetch_one(&st.db)
        .await?;
    if !verify_password(b.current, Some(hash)).await {
        return Err(ApiError::bad("Your current password isn't right."));
    }
    let new_hash = hash_password(b.next).await?;
    let mut tx = st.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(&user.id)
        .bind(&new_hash)
        .execute(&mut *tx)
        .await?;
    let keep = bearer(&headers).map(hash_token).unwrap_or_default();
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
        .bind(&user.id)
        .bind(keep)
        .execute(&mut *tx)
        .await?;
    db::log(
        &mut tx,
        &user.workspace_id,
        &user.name,
        "update",
        "account",
        &user.id,
        "Password changed",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The signed-in user, or 401.
pub async fn current(st: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let token =
        bearer(headers).ok_or_else(|| ApiError::Unauthorized("Sign in to continue.".into()))?;
    user_for_token(&st.db, token)
        .await?
        .ok_or_else(|| ApiError::Unauthorized("Your session has ended. Sign in again.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_blocks_after_max_failures() {
        let l = LoginLimiter::new(2, Duration::from_secs(60));
        assert!(!l.blocked("a@b.co"));
        l.fail("a@b.co");
        assert!(!l.blocked("a@b.co"));
        l.fail("a@b.co");
        assert!(l.blocked("a@b.co"));
        assert!(!l.blocked("other@b.co"));
        l.clear("a@b.co");
        assert!(!l.blocked("a@b.co"));
    }

    #[test]
    fn limiter_forgets_after_the_window() {
        let l = LoginLimiter::new(1, Duration::from_millis(0));
        l.fail("a@b.co");
        std::thread::sleep(Duration::from_millis(2));
        assert!(!l.blocked("a@b.co"));
    }

    #[test]
    fn emails_are_cleaned_and_checked() {
        assert_eq!(
            clean_email("  Alex@Example.COM ").unwrap(),
            "alex@example.com"
        );
        for bad in [
            "",
            "alex",
            "@example.com",
            "alex@",
            "alex@example",
            "a b@example.com",
            "alex@.com",
        ] {
            assert!(clean_email(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let a = random_hex(32);
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_hex(32));
        assert_eq!(hash_token("x").len(), 64);
        assert_ne!(hash_token("x"), "x");
    }

    #[test]
    fn bearer_header_is_parsed() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }
}
