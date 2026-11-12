use axum::{
    extract::{FromRef, FromRequestParts, Path, State},
    http::{request::Parts, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::db::{self, Entity};
use crate::error::{ApiError, ApiResult};
use crate::models::*;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// Single-business mode: every request uses this workspace.
    /// When sign-in lands, `Ws` below reads it from the user's token instead.
    pub workspace_id: String,
    /// Where timer screenshots go (disk now, Google Drive later).
    pub screenshots: std::sync::Arc<dyn crate::screenshots::ScreenshotStore>,
}

impl FromRef<AppState> for PgPool {
    fn from_ref(s: &AppState) -> Self {
        s.db.clone()
    }
}

/// The workspace a request acts on. The one place to change when auth is added.
pub struct Ws(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Ws
where
    AppState: FromRef<S>,
{
    type Rejection = ApiError;
    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Ws(AppState::from_ref(state).workspace_id))
    }
}

pub fn api() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/import", axum::routing::post(import))
        .route("/api/workspace", get(get_workspace).put(put_workspace))
        .route(
            "/api/screenshots",
            axum::routing::post(crate::screenshots::upload),
        )
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
    Ok(Json(json!({ "ok": true })))
}

async fn bootstrap(State(db): State<PgPool>, Ws(ws): Ws) -> ApiResult<Json<DataState>> {
    let mut c = db.acquire().await?;
    Ok(Json(db::load_all(&mut c, &ws).await?))
}

/// Replaces the whole workspace (restore a backup, or move browser data to the server).
async fn import(
    State(db): State<PgPool>,
    Ws(ws): Ws,
    body: Json<Value>,
) -> ApiResult<Json<DataState>> {
    let data: DataState = parse(body.0)?;
    data.workspace.validate()?;
    data.clients.iter().try_for_each(Client::validate)?;
    data.projects.iter().try_for_each(Project::validate)?;
    data.tasks.iter().try_for_each(Task::validate)?;
    data.invoices.iter().try_for_each(Invoice::validate)?;
    data.expenses.iter().try_for_each(Expense::validate)?;
    let mut tx = db.begin().await?;
    db::workspace(&mut tx, &ws).await?;
    db::replace_all(&mut tx, &ws, &data).await?;
    let out = db::load_all(&mut tx, &ws).await?;
    tx.commit().await?;
    tracing::info!(workspace = %ws, tasks = out.tasks.len(), "imported workspace");
    Ok(Json(out))
}

async fn get_workspace(State(db): State<PgPool>, Ws(ws): Ws) -> ApiResult<Json<Workspace>> {
    let mut c = db.acquire().await?;
    Ok(Json(db::workspace(&mut c, &ws).await?))
}

async fn put_workspace(
    State(db): State<PgPool>,
    Ws(ws): Ws,
    body: Json<Value>,
) -> ApiResult<Json<Workspace>> {
    let w: Workspace = parse(body.0)?;
    w.validate()?;
    let mut c = db.acquire().await?;
    db::save_workspace(&mut c, &ws, &w).await?;
    Ok(Json(w))
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
    Ws(ws): Ws,
    Path(col): Path<String>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, list_t(&db, &ws)).map(Json)
}
async fn get_item(
    State(db): State<PgPool>,
    Ws(ws): Ws,
    Path((col, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, get_t(&db, &ws, &id)).map(Json)
}
async fn put_item(
    State(db): State<PgPool>,
    Ws(ws): Ws,
    Path((col, id)): Path<(String, String)>,
    body: Json<Value>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, put_t(&db, &ws, &id, body.0)).map(Json)
}
async fn patch_item(
    State(db): State<PgPool>,
    Ws(ws): Ws,
    Path((col, id)): Path<(String, String)>,
    body: Json<Value>,
) -> ApiResult<Json<Value>> {
    dispatch!(col, patch_t(&db, &ws, &id, body.0)).map(Json)
}
async fn delete_item(
    State(db): State<PgPool>,
    Ws(ws): Ws,
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
async fn put_t<T: Entity>(db: &PgPool, ws: &str, id: &str, body: Value) -> ApiResult<Value> {
    let item: T = parse(body)?;
    if item.id() != id {
        return Err(ApiError::bad("The id in the body doesn't match the URL."));
    }
    save(db, ws, item).await
}

/// Shallow merge, like `{ ...current, ...fields }` in the app.
async fn patch_t<T: Entity>(db: &PgPool, ws: &str, id: &str, body: Value) -> ApiResult<Value> {
    let Value::Object(fields) = body else {
        return Err(ApiError::bad("PATCH body must be a JSON object."));
    };
    let mut c = db.acquire().await?;
    let cur: T = db::fetch(&mut c, ws, id)
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

async fn save<T: Entity>(db: &PgPool, ws: &str, item: T) -> ApiResult<Value> {
    item.validate()?;
    let mut tx = db.begin().await?;
    let w = db::workspace(&mut tx, ws).await?;
    let prev: Option<T> = db::fetch_for_update(&mut tx, ws, item.id()).await?;
    T::check_write(prev.as_ref(), &item, &w)?;
    item.upsert(&mut tx, ws).await?;
    let saved: T = db::fetch(&mut tx, ws, item.id())
        .await?
        .ok_or_else(|| ApiError::NotFound("Saved record vanished.".into()))?;
    tx.commit().await?;
    to_json(&saved)
}

/// Deleting something that isn't there is a success (the app may retry).
async fn delete_t<T: Entity>(db: &PgPool, ws: &str, id: &str) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    let w = db::workspace(&mut tx, ws).await?;
    let Some(prev) = db::fetch_for_update::<T>(&mut tx, ws, id).await? else {
        return Ok(());
    };
    T::check_delete(&prev, &w, &mut tx, ws).await?;
    db::delete::<T>(&mut tx, ws, id).await?;
    tx.commit().await?;
    Ok(())
}
