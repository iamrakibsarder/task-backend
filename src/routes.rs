use std::sync::Arc;

use axum::{
    extract::{FromRef, FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::auth::{self, LoginLimiter};
use crate::db::{self, Entity};
use crate::error::{ApiError, ApiResult};
use crate::models::*;
use crate::team;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// With accounts off: the one workspace every request uses.
    /// With accounts on: the workspace the very first account adopts.
    pub workspace_id: String,
    /// Accounts on: every `/api` request needs `Authorization: Bearer <token>`.
    pub auth_required: bool,
    pub limiter: Arc<LoginLimiter>,
    pub mailer: Arc<dyn crate::mail::Mailer>,
    /// The web app's address, for links in emails.
    pub app_url: String,
    /// Where timer screenshots go (disk now, Google Drive later).
    pub screenshots: Arc<dyn crate::screenshots::ScreenshotStore>,
    /// Lets an outside scheduler run the reminder job. Empty = that endpoint is off.
    pub cron_secret: String,
}

impl FromRef<AppState> for PgPool {
    fn from_ref(s: &AppState) -> Self {
        s.db.clone()
    }
}

/// The workspace a request acts on, and who is acting (for the activity log).
/// With accounts on this comes from the bearer token; a missing or expired token is a 401.
pub struct Ws {
    pub id: String,
    pub actor: String,
    /// Empty with accounts off.
    pub user_id: String,
    /// `owner`, `admin` or `member` (always `owner` with accounts off).
    pub role: String,
}

impl<S: Send + Sync> FromRequestParts<S> for Ws
where
    AppState: FromRef<S>,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let st = AppState::from_ref(state);
        if !st.auth_required {
            return Ok(Ws {
                id: st.workspace_id,
                actor: String::new(),
                user_id: String::new(),
                role: "owner".into(),
            });
        }
        let m = auth::current(&st, &parts.headers).await?;
        Ok(Ws {
            actor: m.display_name(),
            id: m.workspace_id,
            user_id: m.id,
            role: m.role,
        })
    }
}

pub fn api() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api/auth/signup", post(auth::signup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .route("/api/auth/password", post(auth::change_password))
        .route("/api/auth/forgot", post(auth::forgot))
        .route("/api/auth/reset", post(auth::reset))
        .route(
            "/api/workspaces",
            get(team::list_workspaces).post(team::create_workspace),
        )
        .route("/api/workspaces/switch", post(team::switch_workspace))
        .route("/api/team", get(team::team))
        .route("/api/team/invites", post(team::create_invite))
        .route(
            "/api/team/invites/{id}",
            axum::routing::delete(team::revoke_invite),
        )
        .route(
            "/api/team/members/{user_id}",
            axum::routing::patch(team::set_role).delete(team::remove_member),
        )
        .route("/api/invites/{token}", get(team::invite_info))
        .route("/api/invites/{token}/accept", post(team::accept_invite))
        .route("/api/invoices/{id}/pdf", get(crate::invoices::pdf))
        .route("/api/invoices/{id}/send", post(crate::invoices::send))
        .route("/api/invoices/{id}/reminders", get(crate::reminders::list))
        .route("/api/invoices/{id}/remind", post(crate::reminders::remind))
        .route("/api/cron/reminders", post(crate::reminders::cron))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/import", post(import))
        .route("/api/workspace", get(get_workspace).put(put_workspace))
        .route("/api/activity", get(activity))
        .route("/api/screenshots", post(crate::screenshots::upload))
        .route("/api/{collection}", get(list_items))
        .route(
            "/api/{collection}/{id}",
            get(get_item)
                .put(put_item)
                .patch(patch_item)
                .delete(delete_item),
        )
}

async fn health(State(db): State<PgPool>) -> ApiResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&db).await?;
    Ok(Json(
        json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
    ))
}

async fn bootstrap(State(db): State<PgPool>, ws: Ws) -> ApiResult<Json<DataState>> {
    let mut c = db.acquire().await?;
    Ok(Json(db::load_all(&mut c, &ws.id).await?))
}

/// Replaces the whole workspace (restore a backup, or move browser data to the server).
async fn import(State(db): State<PgPool>, ws: Ws, body: Json<Value>) -> ApiResult<Json<DataState>> {
    team::require(&ws, "admin")?;
    let data: DataState = parse(body.0)?;
    data.workspace.validate()?;
    data.clients.iter().try_for_each(Client::validate)?;
    data.projects.iter().try_for_each(Project::validate)?;
    data.tasks.iter().try_for_each(Task::validate)?;
    data.invoices.iter().try_for_each(Invoice::validate)?;
    data.expenses.iter().try_for_each(Expense::validate)?;
    let mut tx = db.begin().await?;
    db::workspace(&mut tx, &ws.id).await?;
    db::replace_all(&mut tx, &ws.id, &data).await?;
    let out = db::load_all(&mut tx, &ws.id).await?;
    let summary = format!(
        "Imported {} clients, {} projects, {} tasks, {} invoices",
        out.clients.len(),
        out.projects.len(),
        out.tasks.len(),
        out.invoices.len()
    );
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "import",
        "workspace",
        "",
        &summary,
    )
    .await?;
    tx.commit().await?;
    tracing::info!(workspace = %ws.id, tasks = out.tasks.len(), "imported workspace");
    Ok(Json(out))
}

async fn get_workspace(State(db): State<PgPool>, ws: Ws) -> ApiResult<Json<Workspace>> {
    let mut c = db.acquire().await?;
    Ok(Json(db::workspace(&mut c, &ws.id).await?))
}

async fn put_workspace(
    State(db): State<PgPool>,
    ws: Ws,
    body: Json<Value>,
) -> ApiResult<Json<Workspace>> {
    team::require(&ws, "admin")?;
    let w: Workspace = parse(body.0)?;
    w.validate()?;
    let mut tx = db.begin().await?;
    let prev = db::workspace(&mut tx, &ws.id).await?;
    db::save_workspace(&mut tx, &ws.id, &w).await?;
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "settings",
        "workspace",
        "",
        &db::describe_settings(&prev, &w),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(w))
}

#[derive(Deserialize)]
pub struct ActivityQuery {
    #[serde(default)]
    limit: Option<i64>,
    /// Only entries older than this id (for "load more").
    #[serde(default)]
    before: Option<i64>,
}

/// `GET /api/activity?limit=&before=`: newest first.
async fn activity(
    State(db): State<PgPool>,
    ws: Ws,
    Query(q): Query<ActivityQuery>,
) -> ApiResult<Json<Vec<db::ActivityEntry>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let mut c = db.acquire().await?;
    Ok(Json(db::activity(&mut c, &ws.id, limit, q.before).await?))
}

/* ---------------- Collections ---------------- */

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> ApiResult<T> {
    serde_json::from_value(v).map_err(|e| ApiError::bad(format!("Invalid data: {e}")))
}

/// Routes `/api/{collection}` to the right entity type.
macro_rules! dispatch {
    ($col:expr, $f:ident ( $($arg:expr),* )) => {
        match $col.as_str() {
            "clients" => $f::<Client>($($arg),*).await,
            "projects" => $f::<Project>($($arg),*).await,
            "tasks" => $f::<Task>($($arg),*).await,
            "invoices" => $f::<Invoice>($($arg),*).await,
            "expenses" => $f::<Expense>($($arg),*).await,
            other => Err(ApiError::NotFound(format!("Unknown collection '{other}'."))),
        }
    };
}

async fn list_items(
    State(db): State<PgPool>,
    ws: Ws,
    Path(col): Path<String>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, list_t(&db, &ws.id)).map(Json)
}
async fn get_item(
    State(db): State<PgPool>,
    ws: Ws,
    Path((col, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, get_t(&db, &ws.id, &id)).map(Json)
}
async fn put_item(
    State(db): State<PgPool>,
    ws: Ws,
    Path((col, id)): Path<(String, String)>,
    body: Json<Value>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, put_t(&db, &ws, &id, body.0)).map(Json)
}
async fn patch_item(
    State(db): State<PgPool>,
    ws: Ws,
    Path((col, id)): Path<(String, String)>,
    body: Json<Value>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, patch_t(&db, &ws, &id, body.0)).map(Json)
}
async fn delete_item(
    State(db): State<PgPool>,
    ws: Ws,
    Path((col, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    dispatch!(col, delete_t(&db, &ws, &id)).map(|_| StatusCode::NO_CONTENT)
}

fn to_json<T: Entity>(x: &T) -> ApiResult<Value> {
    serde_json::to_value(x).map_err(|e| ApiError::bad(e.to_string()))
}

async fn list_t<T: Entity>(db: &PgPool, ws: &str) -> ApiResult<Value> {
    let mut c = db.acquire().await?;
    db::workspace(&mut c, ws).await?;
    let items: Vec<T> = db::list(&mut c, ws).await?;
    Ok(serde_json::to_value(items).unwrap_or(Value::Null))
}

async fn get_t<T: Entity>(db: &PgPool, ws: &str, id: &str) -> ApiResult<Value> {
    let mut c = db.acquire().await?;
    match db::fetch::<T>(&mut c, ws, id).await? {
        Some(x) => to_json(&x),
        None => Err(ApiError::NotFound(format!("No {} with id {id}.", T::NAME))),
    }
}

/// Create or replace. The body is the full entity; its `id` must match the URL.
async fn put_t<T: Entity>(db: &PgPool, ws: &Ws, id: &str, body: Value) -> ApiResult<Value> {
    let item: T = parse(body)?;
    if item.id() != id {
        return Err(ApiError::bad("The id in the body doesn't match the URL."));
    }
    save(db, ws, item).await
}

/// Shallow merge, like `{ ...current, ...fields }` in the app.
async fn patch_t<T: Entity>(db: &PgPool, ws: &Ws, id: &str, body: Value) -> ApiResult<Value> {
    let Value::Object(fields) = body else {
        return Err(ApiError::bad("PATCH body must be a JSON object."));
    };
    let mut c = db.acquire().await?;
    let cur: T = db::fetch(&mut c, &ws.id, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("No {} with id {id}.", T::NAME)))?;
    drop(c);
    let mut v = to_json(&cur)?;
    if let Value::Object(obj) = &mut v {
        for (k, val) in fields {
            if k != "id" {
                obj.insert(k, val);
            }
        }
    }
    save(db, ws, parse::<T>(v)?).await
}

async fn save<T: Entity>(db: &PgPool, ws: &Ws, item: T) -> ApiResult<Value> {
    item.validate()?;
    let mut tx = db.begin().await?;
    let w = db::workspace(&mut tx, &ws.id).await?;
    let prev: Option<T> = db::fetch_for_update(&mut tx, &ws.id, item.id()).await?;
    T::check_write(prev.as_ref(), &item, &w)?;
    item.upsert(&mut tx, &ws.id).await?;
    let saved: T = db::fetch(&mut tx, &ws.id, item.id())
        .await?
        .ok_or_else(|| ApiError::NotFound("Saved record vanished.".into()))?;
    let action = if prev.is_some() { "update" } else { "create" };
    let summary = T::describe(prev.as_ref(), &saved);
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        action,
        T::NAME,
        saved.id(),
        &summary,
    )
    .await?;
    tx.commit().await?;
    to_json(&saved)
}

/// Deleting something that isn't there is a success (the app may retry).
async fn delete_t<T: Entity>(db: &PgPool, ws: &Ws, id: &str) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    let w = db::workspace(&mut tx, &ws.id).await?;
    let Some(prev) = db::fetch_for_update::<T>(&mut tx, &ws.id, id).await? else {
        return Ok(());
    };
    T::check_delete(&prev, &w, &mut tx, &ws.id).await?;
    db::delete::<T>(&mut tx, &ws.id, id).await?;
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "delete",
        T::NAME,
        id,
        &T::describe(None, &prev),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
