//! Invoice PDFs and sending invoices by email from the server.
//!
//! - `GET  /api/invoices/{id}/pdf`: the invoice as a PDF file.
//! - `POST /api/invoices/{id}/send`: emails it to the client with the PDF attached, then
//!   marks it sent (a draft becomes `sent`; `sentAt`, the sender details and the client's
//!   email are saved on the invoice, the same as "Mark as sent" in the app).
//!
//! The email goes out from `MAIL_FROM` under the business's name, with replies going to the
//! business's email (or the person sending), so the client answers the right people.

use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::Value;
use sqlx::types::Json as SqlJson;

use crate::auth::clean_email;
use crate::db::{self, Entity};
use crate::error::{ApiError, ApiResult};
use crate::mail::{Attachment, Email};
use crate::models::{Invoice, SenderSnapshot, Workspace};
use crate::pdf::{self, invoice_pdf};
use crate::routes::{AppState, Ws};

const SEND_TIMEOUT: Duration = Duration::from_secs(30);

fn sender_of(w: &Workspace) -> SenderSnapshot {
    SenderSnapshot {
        business_name: w.business_name.clone(),
        owner_name: w.owner_name.clone(),
        email: w.email.clone(),
        address: w.address.clone(),
        payment_info: w.payment_info.clone(),
    }
}

/// `INV-0007 Acme Cafe.pdf`, with only characters every mail client and OS accept.
pub fn file_name(inv: &Invoice) -> String {
    let raw = format!("{} {}", inv.number, inv.client.name);
    let clean: String = raw
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() || " -_.".contains(c) => c.to_string(),
            'à'..='å' => "a".into(),
            'À'..='Å' => "A".into(),
            'è'..='ë' => "e".into(),
            'È'..='Ë' => "E".into(),
            'ì'..='ï' => "i".into(),
            'Ì'..='Ï' => "I".into(),
            'ò'..='ö' | 'ø' => "o".into(),
            'Ò'..='Ö' | 'Ø' => "O".into(),
            'ù'..='ü' => "u".into(),
            'Ù'..='Ü' => "U".into(),
            'ç' => "c".into(),
            'Ç' => "C".into(),
            'ñ' => "n".into(),
            'Ñ' => "N".into(),
            'ý' | 'ÿ' => "y".into(),
            'ß' => "ss".into(),
            'æ' => "ae".into(),
            'Æ' => "AE".into(),
            '&' => "and".into(),
            _ => " ".into(),
        })
        .collect();
    let squashed = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}.pdf",
        if squashed.is_empty() {
            "invoice"
        } else {
            &squashed
        }
    )
}

async fn load(st: &AppState, ws: &Ws, id: &str) -> ApiResult<(Invoice, Workspace)> {
    let mut c = st.db.acquire().await?;
    let inv: Invoice = db::fetch(&mut c, &ws.id, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("No invoice with id {id}.")))?;
    let w = db::workspace(&mut c, &ws.id).await?;
    Ok((inv, w))
}

/// `GET /api/invoices/{id}/pdf`
pub async fn pdf(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (inv, w) = load(&st, &ws, &id).await?;
    // Sent invoices keep the details they were sent with; drafts show today's settings.
    let from = inv
        .sender
        .as_ref()
        .map(|s| s.0.clone())
        .unwrap_or_else(|| sender_of(&w));
    let bytes = invoice_pdf(&inv, &from);
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", file_name(&inv)),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendBody {
    to: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    /// Copy the person sending.
    #[serde(default)]
    copy_me: bool,
    /// The sender's local date; defaults to today (UTC).
    #[serde(default)]
    sent_at: Option<NaiveDate>,
}

fn default_message(inv: &Invoice, from: &SenderSnapshot) -> (String, String) {
    let t = pdf::totals(inv);
    let settled = t.credited > 0.0 || t.paid > 0.0;
    let due = pdf::money(if settled { t.balance } else { t.total }, &inv.currency);
    let business = if from.business_name.is_empty() {
        "us"
    } else {
        &from.business_name
    };
    let subject = format!(
        "Invoice {} from {business}: {due} due {}",
        inv.number,
        inv.due_date.format("%-d %b %Y")
    );
    let mut body = format!(
        "Hi {},\n\nHere is invoice {}. The {} is {due}, due by {}.\n\n",
        if inv.client.contact.is_empty() {
            &inv.client.name
        } else {
            &inv.client.contact
        },
        inv.number,
        if settled { "balance" } else { "total" },
        inv.due_date.format("%-d %b %Y"),
    );
    if !from.payment_info.is_empty() {
        body.push_str(&format!("How to pay:\n{}\n\n", from.payment_info));
    }
    body.push_str(
        "The invoice is attached as a PDF. Let me know if you have any questions.\n\nThank you,\n",
    );
    body.push_str(if from.owner_name.is_empty() {
        business
    } else {
        &from.owner_name
    });
    (subject, body)
}

/// `POST /api/invoices/{id}/send`: `{ to, subject?, body?, copyMe?, sentAt? }` → the saved invoice.
pub async fn send(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
    Json(b): Json<SendBody>,
) -> ApiResult<Json<Value>> {
    if !st.mailer.delivers() {
        return Err(ApiError::conflict(
            "Email isn't set up on this server (SMTP_URL). Use “Open in mail” instead.",
        ));
    }
    let to = clean_email(&b.to)?;
    let (inv, w) = load(&st, &ws, &id).await?;
    if inv.status == "void" {
        return Err(ApiError::conflict(
            "This invoice is void. Create a new one to send.",
        ));
    }
    let from = sender_of(&w);
    let (default_subject, default_body) = default_message(&inv, &from);
    let subject = b
        .subject
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_subject);
    if subject.contains(['\r', '\n']) || subject.chars().count() > 250 {
        return Err(ApiError::bad(
            "Keep the subject to one line under 250 characters.",
        ));
    }
    let body = b
        .body
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default_body);
    if body.len() > 50_000 {
        return Err(ApiError::bad("The message is too long."));
    }

    // Who's sending: for the copy, and for replies if the business has no email.
    let me: Option<String> = if ws.user_id.is_empty() {
        None
    } else {
        sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(&ws.user_id)
            .fetch_optional(&st.db)
            .await?
    };
    let reply_to = clean_email(&w.email).ok().or_else(|| me.clone());
    let cc = if b.copy_me {
        me.clone().filter(|m| *m != to).into_iter().collect()
    } else {
        vec![]
    };

    // The invoice as it will be once sent, so the PDF shows the details it's sent with.
    let mut next = inv.clone();
    if next.status == "draft" {
        next.status = "sent".into();
    }
    next.sent_at = Some(b.sent_at.unwrap_or_else(|| Utc::now().date_naive()));
    next.sender = Some(SqlJson(from.clone()));
    next.client.0.email = to.clone();
    next.validate()?;

    let email = Email {
        to: to.clone(),
        subject,
        body,
        cc,
        reply_to,
        from_name: (!from.business_name.trim().is_empty())
            .then(|| from.business_name.trim().replace(['"', '<', '>'], "")),
        attachments: vec![Attachment {
            filename: file_name(&next),
            content_type: "application/pdf".into(),
            bytes: invoice_pdf(&next, &from),
        }],
    };
    match tokio::time::timeout(SEND_TIMEOUT, st.mailer.send(email)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(invoice = %inv.number, error = %e, "sending invoice failed");
            return Err(ApiError::Upstream(
                "The mail server didn't accept the email. Check the address, or try again in a minute.".into(),
            ));
        }
        Err(_) => {
            return Err(ApiError::Upstream(
                "The mail server took too long. Try again in a minute.".into(),
            ))
        }
    }

    // Sent: record it. Lock the row so a payment recorded meanwhile isn't overwritten.
    let mut tx = st.db.begin().await?;
    let cur: Invoice = db::fetch_for_update(&mut tx, &ws.id, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("No invoice with id {id}.")))?;
    let mut saved = cur.clone();
    saved.status = if cur.status == "draft" {
        "sent".into()
    } else {
        cur.status.clone()
    };
    saved.sent_at = next.sent_at;
    saved.sender = next.sender.clone();
    saved.client.0.email = to.clone();
    Invoice::check_write(Some(&cur), &saved, &w)?;
    saved.upsert(&mut tx, &ws.id).await?;
    let mut summary = format!("{} emailed to {to}", saved.number);
    if cur.status == "draft" {
        summary.push_str(" (now sent)");
    }
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "update",
        Invoice::NAME,
        &id,
        &summary,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        serde_json::to_value(&saved).map_err(|e| ApiError::bad(e.to_string()))?,
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_are_safe() {
        let name = |n: &str, c: &str| {
            let mut s = crate::pdf::tests::sample(1);
            s.number = n.into();
            s.client.0.name = c.into();
            super::file_name(&s)
        };
        assert_eq!(
            name("INV-0007", "Acme Café / \"Ltd\""),
            "INV-0007 Acme Cafe Ltd.pdf"
        );
        assert_eq!(name("", "///"), "invoice.pdf");
    }
}
