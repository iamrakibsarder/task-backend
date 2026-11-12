//! Payment reminders for unpaid invoices.
//!
//! With reminders on (Settings), a sent invoice with money still owed gets an email on each
//! reminder day: by default 3 days before the due date, on the day, then 7 and 14 days late.
//! Clients can be opted out one by one. Each email has the invoice PDF attached again.
//!
//! - The job runs every 30 minutes while the server is up, and on
//!   `POST /api/cron/reminders` (with `Authorization: Bearer $CRON_SECRET`) so an outside
//!   scheduler can wake a sleeping server once a day.
//! - Each scheduled reminder goes out once: a row in `invoice_reminders` is claimed before
//!   sending (unique per invoice and day) and removed again if the mail server refuses.
//! - Only the latest reminder that's due is sent, and only within 3 days of its date, so
//!   turning reminders on (or a server that slept for a week) never sends a burst.
//! - `GET  /api/invoices/{id}/reminders`: what was sent, what's next, and a ready-made draft.
//! - `POST /api/invoices/{id}/remind`: send one now, by hand.

use std::time::Duration as StdDuration;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::clean_email;
use crate::db::{self, Entity};
use crate::error::{ApiError, ApiResult};
use crate::invoices::{
    billed_minutes, hours_text, load, mail_invoice, message_or, my_email, sender_of, Outgoing,
};
use crate::models::{Invoice, SenderSnapshot, Workspace};
use crate::pdf;
use crate::routes::{AppState, Ws};

/// A scheduled reminder more than this many days past its date is skipped.
const GRACE_DAYS: i64 = 3;
/// How often the job runs while the server is awake.
const EVERY: StdDuration = StdDuration::from_secs(30 * 60);
/// Postgres advisory lock id, so two servers don't run the job at the same moment.
const JOB_LOCK: i64 = 0x7a5c_0005;

/// The scheduled reminder (days from the due date) to send today, if any.
pub fn due_stage(due: NaiveDate, today: NaiveDate, days: &[i32], sent: &[i32]) -> Option<i32> {
    let latest = days
        .iter()
        .copied()
        .filter(|d| due + Duration::days(i64::from(*d)) <= today)
        .max()?;
    if sent.iter().any(|s| *s >= latest) {
        return None;
    }
    let late_by = (today - (due + Duration::days(i64::from(latest)))).num_days();
    (late_by <= GRACE_DAYS).then_some(latest)
}

/// When the next scheduled reminder goes out: today if one is due and not sent yet.
pub fn next_date(
    due: NaiveDate,
    today: NaiveDate,
    days: &[i32],
    sent: &[i32],
) -> Option<NaiveDate> {
    if due_stage(due, today, days, sent).is_some() {
        return Some(today);
    }
    let last_sent = sent.iter().copied().max();
    days.iter()
        .copied()
        .filter(|d| last_sent.is_none_or(|s| *d > s))
        .map(|d| due + Duration::days(i64::from(d)))
        .filter(|at| *at > today)
        .min()
}

fn plural(n: i64, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// Subject and body for a reminder sent on `today`.
pub fn reminder_message(
    inv: &Invoice,
    from: &SenderSnapshot,
    today: NaiveDate,
) -> (String, String) {
    let t = pdf::totals(inv);
    let owed = pdf::money(t.balance, &inv.currency);
    let due = inv.due_date.format("%-d %b %Y");
    let late = (today - inv.due_date).num_days();
    let business = if from.business_name.is_empty() {
        "us"
    } else {
        &from.business_name
    };
    let subject = match late {
        ..0 => format!(
            "Reminder: invoice {} from {business} is due {due}",
            inv.number
        ),
        0 => format!("Invoice {} from {business} is due today", inv.number),
        _ => format!(
            "Overdue: invoice {} from {business} ({} late)",
            inv.number,
            plural(late, "day")
        ),
    };
    let name = if inv.client.contact.is_empty() {
        &inv.client.name
    } else {
        &inv.client.contact
    };
    let mut body = format!("Hi {name},\n\n");
    body.push_str(&match late {
        ..0 => format!(
            "A friendly reminder that invoice {} for {owed} is due on {due}, in {}.",
            inv.number,
            plural(-late, "day")
        ),
        0 => format!(
            "A friendly reminder that invoice {} for {owed} is due today.",
            inv.number
        ),
        _ => format!(
            "Invoice {} for {owed} was due on {due} and is now {} overdue. If you've already paid, \
             thank you, and please ignore this email.",
            inv.number,
            plural(late, "day")
        ),
    });
    body.push_str("\n\n");
    let min = billed_minutes(inv);
    if min > 0 {
        body.push_str(&format!("Total hours: {}\n", hours_text(min)));
    }
    body.push_str(&format!("Amount due: {owed}\n\n"));
    body.push_str("The invoice is attached again (PDF) for your reference.\n\n");
    if !from.payment_info.is_empty() {
        body.push_str(&format!("How to pay:\n{}\n\n", from.payment_info));
    }
    body.push_str("Let me know if you have any questions.\n\nThank you,\n");
    body.push_str(if from.owner_name.is_empty() {
        business
    } else {
        &from.owner_name
    });
    (subject, body)
}

/// Owes money and can be reminded: sent (not draft, paid or void) with a balance left.
fn owes(inv: &Invoice) -> bool {
    inv.status == "sent" && !inv.sample && pdf::totals(inv).balance > 0.005
}

/// The details on the invoice as sent, or today's settings for older invoices.
fn sender_for(inv: &Invoice, w: &Workspace) -> SenderSnapshot {
    inv.sender
        .as_ref()
        .map(|s| s.0.clone())
        .unwrap_or_else(|| sender_of(w))
}

/* ---------------- The job ---------------- */

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct RunReport {
    pub sent: usize,
    pub failed: usize,
}

/// Sends the reminders due on `today`. Does nothing when email isn't set up, or when
/// another server is already running the job.
pub async fn run(st: &AppState, today: NaiveDate) -> sqlx::Result<RunReport> {
    let mut report = RunReport::default();
    if !st.mailer.delivers() {
        return Ok(report);
    }
    let mut lock = st.db.acquire().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(JOB_LOCK)
        .fetch_one(&mut *lock)
        .await?;
    if !got {
        return Ok(report);
    }
    let result = run_locked(st, today, &mut report).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(JOB_LOCK)
        .execute(&mut *lock)
        .await;
    result.map(|_| report)
}

async fn run_locked(st: &AppState, today: NaiveDate, report: &mut RunReport) -> sqlx::Result<()> {
    let candidates: Vec<(String, String, Vec<i32>, Vec<i32>, String)> = sqlx::query_as(
        "SELECT i.workspace_id, i.id, w.reminder_days,
                COALESCE(array_agg(r.stage) FILTER (WHERE r.stage IS NOT NULL), '{}'),
                c.email
         FROM invoices i
         JOIN workspaces w ON w.id = i.workspace_id
         JOIN clients c ON c.workspace_id = i.workspace_id AND c.id = i.client_id
         LEFT JOIN invoice_reminders r ON r.workspace_id = i.workspace_id AND r.invoice_id = i.id
         WHERE w.reminders_on AND c.reminders AND i.status = 'sent' AND NOT i.sample
           AND i.due_date BETWEEN $1::date - 120 AND $1::date + 31
         GROUP BY i.workspace_id, i.id, w.reminder_days, c.email",
    )
    .bind(today)
    .fetch_all(&st.db)
    .await?;

    for (ws, id, days, sent, client_email) in candidates {
        let mut c = st.db.acquire().await?;
        let Some(inv) = db::fetch::<Invoice>(&mut c, &ws, &id).await? else {
            continue;
        };
        let Some(stage) = due_stage(inv.due_date, today, &days, &sent) else {
            continue;
        };
        if !owes(&inv) {
            continue;
        }
        let Ok(to) = clean_email(&inv.client.email).or_else(|_| clean_email(&client_email)) else {
            continue;
        };
        // Claim it first, so it can't go out twice.
        let claimed: Option<i64> = sqlx::query_scalar(
            "INSERT INTO invoice_reminders (workspace_id, invoice_id, stage, sent_to, actor)
             VALUES ($1, $2, $3, $4, 'TaskX')
             ON CONFLICT (workspace_id, invoice_id, stage) WHERE stage IS NOT NULL DO NOTHING
             RETURNING id",
        )
        .bind(&ws)
        .bind(&id)
        .bind(stage)
        .bind(&to)
        .fetch_optional(&mut *c)
        .await?;
        let Some(claim) = claimed else { continue };

        let w = db::workspace(&mut c, &ws).await?;
        let from = sender_for(&inv, &w);
        let (subject, body) = reminder_message(&inv, &from, today);
        let out = Outgoing {
            to: to.clone(),
            subject,
            body,
            cc: vec![],
            me: None,
        };
        match mail_invoice(st, &w, &inv, &from, out).await {
            Ok(()) => {
                report.sent += 1;
                let when = when_text(stage);
                db::log(
                    &mut c,
                    &ws,
                    "TaskX",
                    "update",
                    Invoice::NAME,
                    &id,
                    &format!("{} reminder emailed to {to} ({when})", inv.number),
                )
                .await?;
                tracing::info!(invoice = %inv.number, workspace = %ws, stage, "reminder sent");
            }
            Err(e) => {
                report.failed += 1;
                tracing::warn!(invoice = %inv.number, workspace = %ws, error = %e, "reminder not sent; will retry");
                sqlx::query("DELETE FROM invoice_reminders WHERE id = $1")
                    .bind(claim)
                    .execute(&mut *c)
                    .await?;
            }
        }
    }
    Ok(())
}

fn when_text(stage: i32) -> String {
    match stage {
        ..0 => format!("{} before the due date", plural(i64::from(-stage), "day")),
        0 => "due today".into(),
        _ => format!("{} overdue", plural(i64::from(stage), "day")),
    }
}

/// Runs the job every 30 minutes, starting a minute after the server starts.
pub fn spawn_scheduler(st: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(StdDuration::from_secs(60)).await;
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            match run(&st, Utc::now().date_naive()).await {
                Ok(r) if r.sent + r.failed > 0 => {
                    tracing::info!(sent = r.sent, failed = r.failed, "payment reminders")
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "payment reminder job failed"),
            }
        }
    });
}

/// `POST /api/cron/reminders` with `Authorization: Bearer $CRON_SECRET`.
pub async fn cron(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<RunReport>> {
    let given = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if st.cron_secret.is_empty() {
        return Err(ApiError::NotFound("Not found.".into()));
    }
    if !same(given.as_bytes(), st.cron_secret.as_bytes()) {
        return Err(ApiError::Unauthorized("Wrong cron secret.".into()));
    }
    Ok(Json(run(&st, Utc::now().date_naive()).await?))
}

/// Compares without leaking where the first difference is.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/* ---------------- Per invoice ---------------- */

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct SentReminder {
    /// Days from the due date it was scheduled for; `null` = sent by hand.
    stage: Option<i32>,
    sent_to: String,
    sent_at: DateTime<Utc>,
    actor: String,
}

async fn overview(st: &AppState, ws: &Ws, inv: &Invoice, w: &Workspace) -> ApiResult<Value> {
    let today = Utc::now().date_naive();
    let items: Vec<SentReminder> = sqlx::query_as(
        "SELECT stage, sent_to, sent_at, actor FROM invoice_reminders
         WHERE workspace_id = $1 AND invoice_id = $2 ORDER BY sent_at DESC",
    )
    .bind(&ws.id)
    .bind(&inv.id)
    .fetch_all(&st.db)
    .await?;
    let client_on: bool =
        sqlx::query_scalar("SELECT reminders FROM clients WHERE workspace_id = $1 AND id = $2")
            .bind(&ws.id)
            .bind(&inv.client_id)
            .fetch_optional(&st.db)
            .await?
            .unwrap_or(true);
    let sent: Vec<i32> = items.iter().filter_map(|r| r.stage).collect();
    let automatic = w.reminders_on && client_on && owes(inv) && st.mailer.delivers();
    let next = automatic
        .then(|| next_date(inv.due_date, today, &w.reminder_days, &sent))
        .flatten();
    let (subject, body) = reminder_message(inv, &sender_for(inv, w), today);
    Ok(json!({
        "items": items,
        "owes": owes(inv),
        "workspaceOn": w.reminders_on,
        "clientOn": client_on,
        "mailEnabled": st.mailer.delivers(),
        "next": next,
        "draft": { "to": inv.client.email, "subject": subject, "body": body },
    }))
}

/// `GET /api/invoices/{id}/reminders`
pub async fn list(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (inv, w) = load(&st, &ws, &id).await?;
    Ok(Json(overview(&st, &ws, &inv, &w).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemindBody {
    to: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    copy_me: bool,
}

/// `POST /api/invoices/{id}/remind`: `{ to, subject?, body?, copyMe? }` → same as `GET …/reminders`.
pub async fn remind(
    State(st): State<AppState>,
    ws: Ws,
    Path(id): Path<String>,
    Json(b): Json<RemindBody>,
) -> ApiResult<Json<Value>> {
    if !st.mailer.delivers() {
        return Err(ApiError::conflict(
            "Email isn’t set up on this server (SMTP_USER and SMTP_PASSWORD).",
        ));
    }
    let to = clean_email(&b.to)?;
    let (inv, w) = load(&st, &ws, &id).await?;
    if !owes(&inv) {
        return Err(ApiError::conflict(match inv.status.as_str() {
            "draft" => "Send the invoice first.",
            "void" => "This invoice is void.",
            _ => "Nothing is owed on this invoice.",
        }));
    }
    let from = sender_for(&inv, &w);
    let today = Utc::now().date_naive();
    let (subject, body) = message_or(b.subject, b.body, reminder_message(&inv, &from, today))?;
    let me = my_email(&st, &ws).await?;
    let cc = if b.copy_me {
        me.clone().filter(|m| *m != to).into_iter().collect()
    } else {
        vec![]
    };
    let out = Outgoing {
        to: to.clone(),
        subject,
        body,
        cc,
        me,
    };
    mail_invoice(&st, &w, &inv, &from, out).await?;

    let mut tx = st.db.begin().await?;
    sqlx::query(
        "INSERT INTO invoice_reminders (workspace_id, invoice_id, stage, sent_to, actor)
         VALUES ($1, $2, NULL, $3, $4)",
    )
    .bind(&ws.id)
    .bind(&id)
    .bind(&to)
    .bind(&ws.actor)
    .execute(&mut *tx)
    .await?;
    db::log(
        &mut tx,
        &ws.id,
        &ws.actor,
        "update",
        Invoice::NAME,
        &id,
        &format!("{} reminder emailed to {to}", inv.number),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(overview(&st, &ws, &inv, &w).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }
    const DAYS: &[i32] = &[-3, 0, 7, 14];

    #[test]
    fn picks_the_latest_reminder_due_within_grace() {
        let due = d("2026-11-20");
        assert_eq!(due_stage(due, d("2026-11-16"), DAYS, &[]), None);
        assert_eq!(due_stage(due, d("2026-11-17"), DAYS, &[]), Some(-3));
        assert_eq!(due_stage(due, d("2026-11-18"), DAYS, &[-3]), None);
        assert_eq!(due_stage(due, d("2026-11-20"), DAYS, &[-3]), Some(0));
        assert_eq!(due_stage(due, d("2026-11-27"), DAYS, &[-3, 0]), Some(7));
        // Server slept from the due date until day 9: only the 7-day one goes out.
        assert_eq!(due_stage(due, d("2026-11-29"), DAYS, &[-3]), Some(7));
        // Reminders turned on for an invoice 40 days late: nothing is sent.
        assert_eq!(due_stage(due, d("2026-12-30"), DAYS, &[]), None);
        // Something later already went out (days were changed): nothing.
        assert_eq!(due_stage(due, d("2026-11-27"), DAYS, &[14]), None);
    }

    #[test]
    fn next_reminder_date() {
        let due = d("2026-11-20");
        assert_eq!(
            next_date(due, d("2026-11-10"), DAYS, &[]),
            Some(d("2026-11-17"))
        );
        assert_eq!(
            next_date(due, d("2026-11-17"), DAYS, &[-3]),
            Some(d("2026-11-20"))
        );
        assert_eq!(next_date(due, d("2026-12-10"), DAYS, &[14]), None);
        // Due today but the job hasn't run yet.
        assert_eq!(
            next_date(due, d("2026-11-17"), DAYS, &[]),
            Some(d("2026-11-17"))
        );
    }

    #[test]
    fn messages_match_the_timing() {
        let mut inv = crate::pdf::tests::sample(2);
        inv.due_date = d("2026-11-20");
        let from = SenderSnapshot {
            business_name: "Studio".into(),
            payment_info: "IBAN 123".into(),
            ..Default::default()
        };
        let (s, b) = reminder_message(&inv, &from, d("2026-11-17"));
        assert!(s.starts_with("Reminder: invoice"), "{s}");
        assert!(b.contains("in 3 days"), "{b}");
        let (s, b) = reminder_message(&inv, &from, d("2026-11-20"));
        assert!(s.ends_with("is due today"), "{s}");
        assert!(b.contains("How to pay:\nIBAN 123"));
        let (s, b) = reminder_message(&inv, &from, d("2026-11-27"));
        assert!(s.contains("(7 days late)"), "{s}");
        assert!(
            b.contains("7 days overdue") && b.contains("attached again"),
            "{b}"
        );
    }

    #[test]
    fn secrets_compare() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
