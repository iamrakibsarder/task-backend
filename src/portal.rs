//! Client portal: a private link per client to see their invoices and download the PDFs,
//! without an account.
//!
//! Managed by the business (signed in, admin or owner to change it):
//! - `GET    /api/clients/{id}/portal`: `{ enabled, url, lastViewedAt }`
//! - `POST   /api/clients/{id}/portal`: `{ regenerate? }` turns the link on (a new link with
//!   `regenerate: true`, which stops the old one)
//! - `DELETE /api/clients/{id}/portal`: turns it off
//!
//! Public, by token:
//! - `GET /api/portal/{token}`: the business, the client and their invoices (sent, paid and
//!   overdue; never drafts, void or sample invoices)
//! - `GET /api/portal/{token}/invoices/{id}/pdf`
//!
//! Tokens are 32 random bytes, so they can't be guessed; a wrong or switched-off link is a 404.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;

use crate::auth::random_hex;
use crate::db::{self, Entity};
use crate::error::{ApiError, ApiResult};
use crate::invoices::{file_name, sender_of};
use crate::models::{Client, Invoice, Workspace};
use crate::pdf::{self, invoice_pdf};
use crate::routes::{AppState, Ws};
use crate::team::require;

/// The portal address for `token`.
pub fn portal_url(app_url: &str, token: &str) -> String {
    format!("{}/portal/{token}", app_url.trim_end_matches('/'))
}

/// The client's portal link when it's on, for invoice emails.
pub async fn active_url(st: &AppState, ws: &str, client_id: &str) -> sqlx::Result<Option<String>> {
    let token: Option<String> = sqlx::query_scalar(
        "SELECT token FROM client_portals WHERE workspace_id = $1 AND client_id = $2 AND enabled",
    )
    .bind(ws)
    .bind(client_id)
    .fetch_optional(&st.db)
    .await?;
    Ok(token.map(|t| portal_url(&st.app_url, &t)))
}

/* ---------------- Managing the link ---------------- */

async fn client_exists(c: &mut PgConnection, ws: &str, id: &str) -> ApiResult<Client> {
    db::fetch::<Client>(c, ws, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("No client with id {id}.")))
}

async fn status(
    st: &AppState,
    c: &mut PgConnection,
    ws: &str,
    client_id: &str,
) -> ApiResult<Value> {
    let row: Option<(String, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT token, enabled, last_viewed_at FROM client_portals WHERE workspace_id = $1 AND client_id = $2",
    )
    .bind(ws)
    .bind(client_id)
    .fetch_optional(c)
    .await?;
    Ok(match row {
        Some((token, true, seen)) => json!({
            "enabled": true,
            "url": portal_url(&st.app_url, &token),
            "lastViewedAt": seen,
        }),
        Some((_, false, seen)) => json!({ "enabled": false, "url": null, "lastViewedAt": seen }),
        None => json!({ "enabled": false, "url": null, "lastViewedAt": null }),
    })
}

/// `GET /api/clients/{id}/portal`
pub async fn get_link(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut c = st.db.acquire().await?;
    client_exists(&mut c, &ws.id, &id).await?;
    Ok(Json(status(&st, &mut c, &ws.id, &id).await?))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinkBody {
    #[serde(default)]
    regenerate: bool,
}

/// `POST /api/clients/{id}/portal`: `{ regenerate? }`
pub async fn enable(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
    body: Option<Json<LinkBody>>,
) -> ApiResult<Json<Value>> {
    require(&ws, "admin")?;
    let regenerate = body.map(|b| b.0.regenerate).unwrap_or(false);
    let mut tx = st.db.begin().await?;
    let client = client_exists(&mut tx, &ws.id, &id).await?;
    let token = random_hex(32);
    // New link: a fresh token. Turning an old one back on keeps its address.
    sqlx::query(
        "INSERT INTO client_portals (workspace_id, client_id, token) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, client_id) DO UPDATE SET enabled = true,
           token = CASE WHEN $4 THEN EXCLUDED.token ELSE client_portals.token END,
           created_at = CASE WHEN $4 THEN now() ELSE client_portals.created_at END",
    )
    .bind(&ws.id)
    .bind(&id)
    .bind(&token)
    .bind(regenerate)
    .execute(&mut *tx)
    .await?;
    let summary = if regenerate {
        format!(
            "Client portal for {}: new link (old one stopped)",
            client.name
        )
    } else {
        format!("Client portal for {} turned on", client.name)
    };
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "update",
        Client::NAME,
        &id,
        &summary,
    )
    .await?;
    let out = status(&st, &mut tx, &ws.id, &id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `DELETE /api/clients/{id}/portal`
pub async fn disable(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require(&ws, "admin")?;
    let mut tx = st.db.begin().await?;
    let client = client_exists(&mut tx, &ws.id, &id).await?;
    sqlx::query(
        "UPDATE client_portals SET enabled = false WHERE workspace_id = $1 AND client_id = $2",
    )
    .bind(&ws.id)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "update",
        Client::NAME,
        &id,
        &format!("Client portal for {} turned off", client.name),
    )
    .await?;
    let out = status(&st, &mut tx, &ws.id, &id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/* ---------------- The public page ---------------- */

const NOT_FOUND: &str = "This link doesn’t work any more. Ask for a new one.";

async fn open(st: &AppState, token: &str) -> ApiResult<(String, String)> {
    if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::NotFound(NOT_FOUND.into()));
    }
    sqlx::query_as(
        "UPDATE client_portals SET last_viewed_at = now()
         WHERE token = $1 AND enabled RETURNING workspace_id, client_id",
    )
    .bind(token)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| ApiError::NotFound(NOT_FOUND.into()))
}

/// Invoices a client may see: sent or paid, not drafts, void or sample ones.
async fn visible_invoices(
    c: &mut PgConnection,
    ws: &str,
    client_id: &str,
) -> sqlx::Result<Vec<Invoice>> {
    sqlx::query_as::<_, Invoice>(
        "SELECT * FROM invoices WHERE workspace_id = $1 AND client_id = $2
           AND status IN ('sent', 'paid') AND NOT sample
         ORDER BY issue_date DESC, seq DESC",
    )
    .bind(ws)
    .bind(client_id)
    .fetch_all(c)
    .await
}

/// `sent` past its due date with money owed is shown as `overdue`.
pub fn shown_status(inv: &Invoice, balance: f64, today: NaiveDate) -> &'static str {
    match inv.status.as_str() {
        "paid" => "paid",
        _ if balance <= 0.005 => "paid",
        _ if inv.due_date < today => "overdue",
        _ => "open",
    }
}

fn invoice_json(inv: &Invoice, today: NaiveDate) -> Value {
    let t = pdf::totals(inv);
    let balance = if inv.status == "paid" { 0.0 } else { t.balance };
    json!({
        "id": inv.id,
        "number": inv.number,
        "period": inv.period,
        "issueDate": inv.issue_date,
        "dueDate": inv.due_date,
        "paidAt": inv.paid_at,
        "currency": inv.currency,
        "total": t.total,
        "paid": ((t.total - balance) * 100.0).round() / 100.0,
        "balance": balance,
        "hours": crate::invoices::billed_minutes(inv),
        "status": shown_status(inv, balance, today),
    })
}

/// `GET /api/portal/{token}`
pub async fn page(State(st): State<AppState>, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    let (ws, client_id) = open(&st, &token).await?;
    let mut c = st.db.acquire().await?;
    let w: Workspace = db::workspace(&mut c, &ws).await?;
    let client = client_exists(&mut c, &ws, &client_id).await?;
    let invoices = visible_invoices(&mut c, &ws, &client_id).await?;
    let today = Utc::now().date_naive();
    let list: Vec<Value> = invoices.iter().map(|i| invoice_json(i, today)).collect();
    // What's owed, per currency.
    let mut owed: Vec<(String, f64, f64)> = vec![];
    for v in &list {
        let cur = v["currency"].as_str().unwrap_or("").to_string();
        let bal = v["balance"].as_f64().unwrap_or(0.0);
        let late = if v["status"] == "overdue" { bal } else { 0.0 };
        match owed.iter_mut().find(|(c, ..)| *c == cur) {
            Some(o) => {
                o.1 += bal;
                o.2 += late;
            }
            None => owed.push((cur, bal, late)),
        }
    }
    let from = sender_of(&w);
    Ok(Json(json!({
        "business": {
            "name": from.business_name,
            "ownerName": from.owner_name,
            "email": from.email,
            "address": from.address,
            "paymentInfo": from.payment_info,
        },
        "client": { "name": client.name, "contact": client.contact, "logo": client.logo, "color": client.color },
        "owed": owed.into_iter().map(|(currency, balance, overdue)| json!({
            "currency": currency,
            "balance": (balance * 100.0).round() / 100.0,
            "overdue": (overdue * 100.0).round() / 100.0,
        })).collect::<Vec<_>>(),
        "invoices": list,
    })))
}

/// `GET /api/portal/{token}/invoices/{id}/pdf`
pub async fn pdf(
    State(st): State<AppState>,
    Path((token, id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (ws, client_id) = open(&st, &token).await?;
    let mut c = st.db.acquire().await?;
    let inv = visible_invoices(&mut c, &ws, &client_id)
        .await?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| ApiError::NotFound("No such invoice.".into()))?;
    let w = db::workspace(&mut c, &ws).await?;
    let from = inv
        .sender
        .as_ref()
        .map(|s| s.0.clone())
        .unwrap_or_else(|| sender_of(&w));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", file_name(&inv)),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        invoice_pdf(&inv, &from),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn urls() {
        assert_eq!(
            super::portal_url("https://app.test/", "abc"),
            "https://app.test/portal/abc"
        );
    }
}
