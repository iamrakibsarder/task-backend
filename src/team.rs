//! Team workspaces: members, roles and invitations.
//!
//! Roles, from most to least:
//! - `owner`: everything, including changing roles. A workspace always keeps one owner.
//! - `admin`: invite and remove members (not owners), change workspace settings, import.
//! - `member`: work with clients, projects, tasks, invoices and expenses.
//!
//! An invite is a link with a random token (only its hash is stored), valid for 7 days,
//! for one email address. It's emailed when mail is set up, and always shown to whoever
//! sent it, so they can share it another way.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection};

use crate::auth::{self, auth_off, clean_email, hash_token, random_hex};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::mail::{send_later, Email};
use crate::routes::{AppState, Ws};

const INVITE_DAYS: i32 = 7;

pub fn rank(role: &str) -> u8 {
    match role {
        "owner" => 3,
        "admin" => 2,
        "member" => 1,
        _ => 0,
    }
}

/// 403 unless the person's role is at least `min`.
pub fn require(ws: &Ws, min: &str) -> ApiResult<()> {
    if rank(&ws.role) >= rank(min) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(match min {
            "owner" => "Only the workspace owner can do that.".into(),
            _ => "Only owners and admins can do that.".into(),
        }))
    }
}

fn accounts_on(st: &AppState) -> ApiResult<()> {
    if st.auth_required {
        Ok(())
    } else {
        Err(auth_off())
    }
}

/* ---------------- Invites (shared with sign-up) ---------------- */

#[derive(Debug, Clone, FromRow)]
pub struct OpenInvite {
    pub id: String,
    pub workspace_id: String,
    pub email: String,
    pub role: String,
}

/// The invite for a link token, if it's still open. Errors explain why not.
pub async fn open_invite(c: &mut PgConnection, token: &str) -> ApiResult<OpenInvite> {
    let row: Option<(String, String, String, String, bool, bool)> = sqlx::query_as(
        "SELECT id, workspace_id, email, role, accepted_at IS NOT NULL, expires_at <= now()
         FROM invites WHERE token_hash = $1",
    )
    .bind(hash_token(token.trim()))
    .fetch_optional(c)
    .await?;
    match row {
        None => Err(ApiError::NotFound(
            "This invite link isn't valid. Ask for a new one.".into(),
        )),
        Some((.., true, _)) => Err(ApiError::conflict("This invite was already used.")),
        Some((.., true)) => Err(ApiError::conflict(
            "This invite has expired. Ask for a new one.",
        )),
        Some((id, workspace_id, email, role, ..)) => Ok(OpenInvite {
            id,
            workspace_id,
            email,
            role,
        }),
    }
}

pub async fn mark_accepted(c: &mut PgConnection, id: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE invites SET accepted_at = now() WHERE id = $1")
        .bind(id)
        .execute(c)
        .await?;
    Ok(())
}

/* ---------------- Workspaces ---------------- */

#[derive(Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRow {
    id: String,
    name: String,
    role: String,
    members: i64,
    current: bool,
}

/// `GET /api/workspaces`: every workspace the person belongs to.
pub async fn list_workspaces(
    State(st): State<AppState>,
    ws: Ws,
) -> ApiResult<Json<Vec<WorkspaceRow>>> {
    accounts_on(&st)?;
    let rows = sqlx::query_as::<_, WorkspaceRow>(
        "SELECT w.id, COALESCE(NULLIF(w.business_name, ''), 'Untitled workspace') AS name, m.role,
                (SELECT count(*) FROM memberships x WHERE x.workspace_id = w.id) AS members,
                w.id = $2 AS current
         FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
         WHERE m.user_id = $1 ORDER BY name",
    )
    .bind(&ws.user_id)
    .bind(&ws.id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct NewWorkspace {
    name: String,
}

/// `POST /api/workspaces`: a new workspace you own; you switch to it.
pub async fn create_workspace(
    State(st): State<AppState>,
    ws: Ws,
    Json(b): Json<NewWorkspace>,
) -> ApiResult<Json<Value>> {
    accounts_on(&st)?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("Give the workspace a name."));
    }
    let mut tx = st.db.begin().await?;
    let (name, email): (String, String) =
        sqlx::query_as("SELECT name, email FROM users WHERE id = $1")
            .bind(&ws.user_id)
            .fetch_one(&mut *tx)
            .await?;
    let id = auth::create_workspace(&mut tx, &ws.user_id, &name, &email, &b.name).await?;
    sqlx::query("UPDATE users SET workspace_id = $2 WHERE id = $1")
        .bind(&ws.user_id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    db::log(
        &mut tx,
        &id,
        &ws.actor,
        "create",
        "workspace",
        &id,
        &format!("Workspace {} created", b.name.trim()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Switch {
    workspace_id: String,
}

/// `POST /api/workspaces/switch`
pub async fn switch_workspace(
    State(st): State<AppState>,
    ws: Ws,
    Json(b): Json<Switch>,
) -> ApiResult<StatusCode> {
    accounts_on(&st)?;
    let done = sqlx::query(
        "UPDATE users SET workspace_id = $2 WHERE id = $1
           AND EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND workspace_id = $2)",
    )
    .bind(&ws.user_id)
    .bind(&b.workspace_id)
    .execute(&st.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound(
            "You're not a member of that workspace.".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/* ---------------- Members & invites ---------------- */

#[derive(Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct MemberRow {
    user_id: String,
    name: String,
    email: String,
    role: String,
    joined_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct InviteRow {
    id: String,
    email: String,
    role: String,
    invited_by: String,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    expired: bool,
}

/// `GET /api/team`: members, plus open invites for owners and admins.
pub async fn team(State(st): State<AppState>, ws: Ws) -> ApiResult<Json<Value>> {
    accounts_on(&st)?;
    let members = sqlx::query_as::<_, MemberRow>(
        "SELECT u.id AS user_id, u.name, u.email, m.role, m.created_at AS joined_at
         FROM memberships m JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1
         ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 ELSE 2 END, lower(u.name), u.email",
    )
    .bind(&ws.id)
    .fetch_all(&st.db)
    .await?;
    let invites = if rank(&ws.role) >= rank("admin") {
        sqlx::query_as::<_, InviteRow>(
            "SELECT i.id, i.email, i.role, COALESCE(NULLIF(u.name, ''), u.email, '') AS invited_by,
                    i.created_at, i.expires_at, i.expires_at <= now() AS expired
             FROM invites i LEFT JOIN users u ON u.id = i.invited_by
             WHERE i.workspace_id = $1 AND i.accepted_at IS NULL ORDER BY i.created_at DESC",
        )
        .bind(&ws.id)
        .fetch_all(&st.db)
        .await?
    } else {
        Vec::new()
    };
    Ok(Json(
        json!({ "members": members, "invites": invites, "role": ws.role }),
    ))
}

#[derive(Deserialize)]
pub struct InviteBody {
    email: String,
    #[serde(default = "member_role")]
    role: String,
}

fn member_role() -> String {
    "member".into()
}

fn check_role(role: &str, allowed: &[&str]) -> ApiResult<()> {
    if allowed.contains(&role) {
        Ok(())
    } else {
        Err(ApiError::bad(format!(
            "role must be one of: {}.",
            allowed.join(", ")
        )))
    }
}

/// `POST /api/team/invites`: `{ email, role }` → `{ invite, link, emailed }`.
pub async fn create_invite(
    State(st): State<AppState>,
    ws: Ws,
    Json(b): Json<InviteBody>,
) -> ApiResult<Json<Value>> {
    accounts_on(&st)?;
    require(&ws, "admin")?;
    let email = clean_email(&b.email)?;
    check_role(&b.role, &["admin", "member"])?;
    let mut tx = st.db.begin().await?;
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id
                        WHERE m.workspace_id = $1 AND u.email = $2)",
    )
    .bind(&ws.id)
    .bind(&email)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        return Err(ApiError::conflict(format!(
            "{email} is already in this workspace."
        )));
    }
    // Inviting again replaces the old link.
    sqlx::query(
        "DELETE FROM invites WHERE workspace_id = $1 AND email = $2 AND accepted_at IS NULL",
    )
    .bind(&ws.id)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    let id = random_hex(12);
    let token = random_hex(32);
    sqlx::query(
        "INSERT INTO invites (id, workspace_id, email, role, token_hash, invited_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7))",
    )
    .bind(&id)
    .bind(&ws.id)
    .bind(&email)
    .bind(&b.role)
    .bind(hash_token(&token))
    .bind(&ws.user_id)
    .bind(INVITE_DAYS)
    .execute(&mut *tx)
    .await?;
    let workspace_name: String = sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(business_name, ''), 'a TaskX workspace') FROM workspaces WHERE id = $1",
    )
    .bind(&ws.id)
    .fetch_one(&mut *tx)
    .await?;
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "create",
        "invite",
        &id,
        &format!("Invited {email} as {}", b.role),
    )
    .await?;
    tx.commit().await?;

    let link = format!("{}/invite/{token}", st.app_url);
    send_later(
        &st.mailer,
        Email {
            to: email.clone(),
            subject: format!("{} invited you to {workspace_name} on TaskX", ws.actor),
            body: format!(
                "Hi,\n\n{} invited you to join {workspace_name} on TaskX as {} {}.\n\nAccept the invitation (the link works for {INVITE_DAYS} days):\n\n{link}\n",
                ws.actor,
                if b.role == "admin" { "an" } else { "a" },
                b.role
            ),
        },
    );
    Ok(Json(json!({
        "invite": { "id": id, "email": email, "role": b.role },
        "link": link,
        "emailed": st.mailer.delivers(),
    })))
}

/// `DELETE /api/team/invites/{id}`
pub async fn revoke_invite(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    accounts_on(&st)?;
    require(&ws, "admin")?;
    let mut tx = st.db.begin().await?;
    let email: Option<String> = sqlx::query_scalar(
        "DELETE FROM invites WHERE id = $1 AND workspace_id = $2 AND accepted_at IS NULL RETURNING email",
    )
    .bind(&id)
    .bind(&ws.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(email) = email {
        db::log(
            &mut tx,
            &ws.id,
            &ws.actor,
            "delete",
            "invite",
            &id,
            &format!("Invite for {email} cancelled"),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/invites/{token}`: what an invite link is for, so the app can show it before
/// sign-in. No sign-in needed: the token itself is the secret.
pub async fn invite_info(
    State(st): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<Value>> {
    accounts_on(&st)?;
    let mut c = st.db.acquire().await?;
    let inv = open_invite(&mut c, &token).await?;
    let (workspace_name, invited_by): (String, String) = sqlx::query_as(
        "SELECT COALESCE(NULLIF(w.business_name, ''), 'Untitled workspace'),
                COALESCE(NULLIF(u.name, ''), u.email, '')
         FROM invites i JOIN workspaces w ON w.id = i.workspace_id LEFT JOIN users u ON u.id = i.invited_by
         WHERE i.id = $1",
    )
    .bind(&inv.id)
    .fetch_one(&mut *c)
    .await?;
    let has_account: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE email = $1)")
            .bind(&inv.email)
            .fetch_one(&mut *c)
            .await?;
    Ok(Json(json!({
        "workspaceName": workspace_name,
        "invitedBy": invited_by,
        "email": inv.email,
        "role": inv.role,
        "hasAccount": has_account,
    })))
}

/// `POST /api/invites/{token}/accept`: the signed-in person joins and switches to it.
pub async fn accept_invite(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> ApiResult<StatusCode> {
    accounts_on(&st)?;
    let me = auth::current(&st, &headers).await?;
    let mut tx = st.db.begin().await?;
    let inv = open_invite(&mut tx, &token).await?;
    if inv.email != me.email {
        return Err(ApiError::Forbidden(format!(
            "This invite is for {}. You're signed in as {}.",
            inv.email, me.email
        )));
    }
    sqlx::query(
        "INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, user_id) DO NOTHING",
    )
    .bind(&inv.workspace_id)
    .bind(&me.id)
    .bind(&inv.role)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE users SET workspace_id = $2 WHERE id = $1")
        .bind(&me.id)
        .bind(&inv.workspace_id)
        .execute(&mut *tx)
        .await?;
    mark_accepted(&mut tx, &inv.id).await?;
    db::log(
        &mut tx,
        &inv.workspace_id,
        &me.display_name(),
        "create",
        "member",
        &me.id,
        &format!("{} joined as {}", me.email, inv.role),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RoleBody {
    role: String,
}

async fn owner_count(c: &mut PgConnection, ws: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE workspace_id = $1 AND role = 'owner'",
    )
    .bind(ws)
    .fetch_one(c)
    .await
}

async fn member_info(c: &mut PgConnection, ws: &str, user_id: &str) -> ApiResult<(String, String)> {
    sqlx::query_as(
        "SELECT m.role, u.email FROM memberships m JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1 AND m.user_id = $2 FOR UPDATE OF m",
    )
    .bind(ws)
    .bind(user_id)
    .fetch_optional(c)
    .await?
    .ok_or_else(|| ApiError::NotFound("That person isn't in this workspace.".into()))
}

/// `PATCH /api/team/members/{userId}`: `{ role }`. Owners only.
pub async fn set_role(
    State(st): State<AppState>,
    ws: Ws,
    Path(user_id): Path<String>,
    Json(b): Json<RoleBody>,
) -> ApiResult<StatusCode> {
    accounts_on(&st)?;
    require(&ws, "owner")?;
    check_role(&b.role, &["owner", "admin", "member"])?;
    let mut tx = st.db.begin().await?;
    // Serialize role changes in this workspace so the last owner can't be demoted twice.
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(&ws.id)
        .execute(&mut *tx)
        .await?;
    let (old, email) = member_info(&mut tx, &ws.id, &user_id).await?;
    if old == "owner" && b.role != "owner" && owner_count(&mut tx, &ws.id).await? <= 1 {
        return Err(ApiError::conflict(
            "A workspace needs an owner. Make someone else owner first.",
        ));
    }
    sqlx::query("UPDATE memberships SET role = $3 WHERE workspace_id = $1 AND user_id = $2")
        .bind(&ws.id)
        .bind(&user_id)
        .bind(&b.role)
        .execute(&mut *tx)
        .await?;
    if old != b.role {
        db::log(
            &mut tx,
            &ws.id,
            &ws.actor,
            "update",
            "member",
            &user_id,
            &format!("{email}: {old} → {}", b.role),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/team/members/{userId}`: remove someone, or leave (your own id).
/// Admins can remove members and admins; only owners can remove owners.
pub async fn remove_member(
    State(st): State<AppState>,
    ws: Ws,
    Path(user_id): Path<String>,
) -> ApiResult<StatusCode> {
    accounts_on(&st)?;
    let leaving = user_id == ws.user_id;
    if !leaving {
        require(&ws, "admin")?;
    }
    let mut tx = st.db.begin().await?;
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(&ws.id)
        .execute(&mut *tx)
        .await?;
    let (role, email) = member_info(&mut tx, &ws.id, &user_id).await?;
    if role == "owner" && !leaving && ws.role != "owner" {
        return Err(ApiError::Forbidden(
            "Only an owner can remove another owner.".into(),
        ));
    }
    if role == "owner" && owner_count(&mut tx, &ws.id).await? <= 1 {
        return Err(ApiError::conflict(if leaving {
            "You're the only owner. Make someone else owner before you leave."
        } else {
            "A workspace needs an owner."
        }));
    }
    sqlx::query("DELETE FROM memberships WHERE workspace_id = $1 AND user_id = $2")
        .bind(&ws.id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    // Their sessions keep working; their next request lands in another workspace
    // (`auth::member` moves them).
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "delete",
        "member",
        &user_id,
        &if leaving {
            format!("{email} left")
        } else {
            format!("{email} removed")
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_rank_in_order() {
        assert!(rank("owner") > rank("admin"));
        assert!(rank("admin") > rank("member"));
        assert_eq!(rank("stranger"), 0);
    }
}
