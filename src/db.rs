//! Storage. One `Entity` impl per table: how to read it, write it, and which
//! business rules guard a write or delete.

use std::future::Future;

use serde::{de::DeserializeOwned, Serialize};
use sqlx::{postgres::PgRow, FromRow, PgConnection};

use crate::error::{ApiError, ApiResult};
use crate::models::*;

pub trait Entity:
    Serialize + DeserializeOwned + for<'r> FromRow<'r, PgRow> + Send + Sync + Unpin + 'static
{
    const TABLE: &'static str;
    /// Singular, for messages.
    const NAME: &'static str;
    const ORDER_BY: &'static str;

    fn id(&self) -> &str;
    fn validate(&self) -> ApiResult<()>;
    fn upsert<'a>(
        &'a self,
        c: &'a mut PgConnection,
        ws: &'a str,
    ) -> impl Future<Output = sqlx::Result<()>> + Send + 'a;

    /// One line for the activity log. `prev = None` for a create (or, on delete, the old record as `next`).
    fn describe(prev: Option<&Self>, next: &Self) -> String;

    /// Business rules for a create (`prev = None`) or update.
    fn check_write(_prev: Option<&Self>, _next: &Self, _w: &Workspace) -> ApiResult<()> {
        Ok(())
    }
    fn check_delete<'a>(
        _prev: &'a Self,
        _w: &'a Workspace,
        _c: &'a mut PgConnection,
        _ws: &'a str,
    ) -> impl Future<Output = ApiResult<()>> + Send + 'a {
        async { Ok(()) }
    }
}

pub async fn fetch<T: Entity>(c: &mut PgConnection, ws: &str, id: &str) -> sqlx::Result<Option<T>> {
    sqlx::query_as::<_, T>(&format!(
        "SELECT * FROM {} WHERE workspace_id = $1 AND id = $2",
        T::TABLE
    ))
    .bind(ws)
    .bind(id)
    .fetch_optional(c)
    .await
}

/// Same as `fetch` but locks the row until the transaction ends, so two edits can't interleave.
pub async fn fetch_for_update<T: Entity>(
    c: &mut PgConnection,
    ws: &str,
    id: &str,
) -> sqlx::Result<Option<T>> {
    sqlx::query_as::<_, T>(&format!(
        "SELECT * FROM {} WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        T::TABLE
    ))
    .bind(ws)
    .bind(id)
    .fetch_optional(c)
    .await
}

pub async fn list<T: Entity>(c: &mut PgConnection, ws: &str) -> sqlx::Result<Vec<T>> {
    sqlx::query_as::<_, T>(&format!(
        "SELECT * FROM {} WHERE workspace_id = $1 ORDER BY {}",
        T::TABLE,
        T::ORDER_BY
    ))
    .bind(ws)
    .fetch_all(c)
    .await
}

pub async fn delete<T: Entity>(c: &mut PgConnection, ws: &str, id: &str) -> sqlx::Result<()> {
    sqlx::query(&format!(
        "DELETE FROM {} WHERE workspace_id = $1 AND id = $2",
        T::TABLE
    ))
    .bind(ws)
    .bind(id)
    .execute(c)
    .await?;
    Ok(())
}

/* ---------------- Activity log ---------------- */

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub actor: String,
    pub action: String,
    pub entity: String,
    pub entity_id: String,
    pub summary: String,
}

pub async fn log(
    c: &mut PgConnection,
    ws: &str,
    actor: &str,
    action: &str,
    entity: &str,
    entity_id: &str,
    summary: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO activity (workspace_id, actor, action, entity, entity_id, summary) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(ws)
    .bind(actor)
    .bind(action)
    .bind(entity)
    .bind(entity_id)
    .bind(clip(summary, 300))
    .execute(c)
    .await?;
    Ok(())
}

pub async fn activity(
    c: &mut PgConnection,
    ws: &str,
    limit: i64,
    before: Option<i64>,
) -> sqlx::Result<Vec<ActivityEntry>> {
    sqlx::query_as::<_, ActivityEntry>(
        "SELECT id, at, actor, action, entity, entity_id, summary FROM activity
         WHERE workspace_id = $1 AND ($2::BIGINT IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(ws)
    .bind(before)
    .bind(limit)
    .fetch_all(c)
    .await
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// "Status review → billable" when the status moved, else the fallback.
fn status_change(prev: Option<&str>, next: &str, fallback: String) -> String {
    match prev {
        Some(p) if p != next => format!("{fallback} · {p} → {next}"),
        _ => fallback,
    }
}

pub fn describe_settings(prev: &Workspace, next: &Workspace) -> String {
    let mut changed = Vec::new();
    if prev.locked_through != next.locked_through {
        changed.push(if next.locked_through.is_empty() {
            "unlocked all months".to_string()
        } else {
            format!("locked through {}", next.locked_through)
        });
    }
    if prev.default_rate != next.default_rate {
        changed.push(format!("default rate {}", next.default_rate));
    }
    if prev.tax_rate != next.tax_rate {
        changed.push(format!("tax {}%", next.tax_rate));
    }
    if prev.currency != next.currency {
        changed.push(format!("currency {}", next.currency));
    }
    if prev.business_name != next.business_name {
        changed.push("business name".into());
    }
    if changed.is_empty() {
        "Settings updated".into()
    } else {
        format!("Settings: {}", changed.join(", "))
    }
}

/* ---------------- Workspace ---------------- */

const WORKSPACE_COLS: &str = "business_name, owner_name, email, address, currency, default_rate, invoice_prefix, terms_days, tax_rate, \
     tax_label, payment_info, invoice_note, auto_approve, team, members, rounding, locked_through, screenshot_minutes";

/// Loads the workspace, creating it with defaults on first use.
pub async fn workspace(c: &mut PgConnection, ws: &str) -> sqlx::Result<Workspace> {
    sqlx::query("INSERT INTO workspaces (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
        .bind(ws)
        .execute(&mut *c)
        .await?;
    sqlx::query_as::<_, Workspace>(&format!(
        "SELECT {WORKSPACE_COLS} FROM workspaces WHERE id = $1"
    ))
    .bind(ws)
    .fetch_one(c)
    .await
}

pub async fn save_workspace(c: &mut PgConnection, ws: &str, w: &Workspace) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workspaces (id, business_name, owner_name, email, address, currency, default_rate, invoice_prefix, terms_days, \
           tax_rate, tax_label, payment_info, invoice_note, auto_approve, team, members, rounding, locked_through, screenshot_minutes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
         ON CONFLICT (id) DO UPDATE SET business_name = EXCLUDED.business_name, owner_name = EXCLUDED.owner_name,
           email = EXCLUDED.email, address = EXCLUDED.address, currency = EXCLUDED.currency, default_rate = EXCLUDED.default_rate,
           invoice_prefix = EXCLUDED.invoice_prefix, terms_days = EXCLUDED.terms_days, tax_rate = EXCLUDED.tax_rate,
           tax_label = EXCLUDED.tax_label, payment_info = EXCLUDED.payment_info, invoice_note = EXCLUDED.invoice_note,
           auto_approve = EXCLUDED.auto_approve, team = EXCLUDED.team, members = EXCLUDED.members, rounding = EXCLUDED.rounding,
           locked_through = EXCLUDED.locked_through, screenshot_minutes = EXCLUDED.screenshot_minutes, updated_at = now()",
    )
    .bind(ws)
    .bind(&w.business_name)
    .bind(&w.owner_name)
    .bind(&w.email)
    .bind(&w.address)
    .bind(&w.currency)
    .bind(w.default_rate)
    .bind(&w.invoice_prefix)
    .bind(w.terms_days)
    .bind(w.tax_rate)
    .bind(&w.tax_label)
    .bind(&w.payment_info)
    .bind(&w.invoice_note)
    .bind(w.auto_approve)
    .bind(w.team)
    .bind(&w.members)
    .bind(&w.rounding)
    .bind(&w.locked_through)
    .bind(w.screenshot_minutes)
    .execute(c)
    .await?;
    Ok(())
}

/* ---------------- Clients ---------------- */

impl Entity for Client {
    const TABLE: &'static str = "clients";
    const NAME: &'static str = "client";
    const ORDER_BY: &'static str = "created_ms, name";
    fn id(&self) -> &str {
        &self.id
    }
    fn validate(&self) -> ApiResult<()> {
        Client::validate(self)
    }
    fn describe(_prev: Option<&Self>, next: &Self) -> String {
        format!("Client {}", next.name)
    }
    async fn upsert<'a>(&'a self, c: &'a mut PgConnection, ws: &'a str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO clients (workspace_id, id, name, color, logo, contact, email, address, billing, rate, retainer_fee, included_hours, cap, active, created_ms, sample)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
             ON CONFLICT (workspace_id, id) DO UPDATE SET name = EXCLUDED.name, color = EXCLUDED.color, logo = EXCLUDED.logo, contact = EXCLUDED.contact,
               email = EXCLUDED.email, address = EXCLUDED.address, billing = EXCLUDED.billing, rate = EXCLUDED.rate,
               retainer_fee = EXCLUDED.retainer_fee, included_hours = EXCLUDED.included_hours, cap = EXCLUDED.cap,
               active = EXCLUDED.active, sample = EXCLUDED.sample, updated_at = now()",
        )
        .bind(ws)
        .bind(&self.id)
        .bind(self.name.trim())
        .bind(&self.color)
        .bind(&self.logo)
        .bind(&self.contact)
        .bind(&self.email)
        .bind(&self.address)
        .bind(&self.billing)
        .bind(self.rate)
        .bind(self.retainer_fee)
        .bind(self.included_hours)
        .bind(self.cap)
        .bind(self.active)
        .bind(self.created_at)
        .bind(self.sample)
        .execute(c)
        .await?;
        Ok(())
    }
    async fn check_delete<'a>(
        prev: &'a Self,
        _w: &'a Workspace,
        c: &'a mut PgConnection,
        ws: &'a str,
    ) -> ApiResult<()> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM tasks t JOIN projects p ON p.workspace_id = t.workspace_id AND p.id = t.project_id
             WHERE t.workspace_id = $1 AND p.client_id = $2",
        )
        .bind(ws)
        .bind(&prev.id)
        .fetch_one(c)
        .await?;
        if n > 0 {
            return Err(ApiError::conflict(
                "This client has logged time. Make it inactive instead.",
            ));
        }
        Ok(())
    }
}

/* ---------------- Projects ---------------- */

impl Entity for Project {
    const TABLE: &'static str = "projects";
    const NAME: &'static str = "project";
    const ORDER_BY: &'static str = "name";
    fn id(&self) -> &str {
        &self.id
    }
    fn validate(&self) -> ApiResult<()> {
        Project::validate(self)
    }
    fn describe(prev: Option<&Self>, next: &Self) -> String {
        match prev {
            Some(p) if !p.archived && next.archived => format!("Project {} archived", next.name),
            _ => format!("Project {}", next.name),
        }
    }
    async fn upsert<'a>(&'a self, c: &'a mut PgConnection, ws: &'a str) -> sqlx::Result<()> {
        let client = if self.internal {
            None
        } else {
            self.client_id.as_deref()
        };
        sqlx::query(
            "INSERT INTO projects (workspace_id, id, client_id, name, rate, internal, archived, budget_hours, sample)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT (workspace_id, id) DO UPDATE SET client_id = EXCLUDED.client_id, name = EXCLUDED.name, rate = EXCLUDED.rate,
               internal = EXCLUDED.internal, archived = EXCLUDED.archived, budget_hours = EXCLUDED.budget_hours,
               sample = EXCLUDED.sample, updated_at = now()",
        )
        .bind(ws)
        .bind(&self.id)
        .bind(client)
        .bind(self.name.trim())
        .bind(self.rate)
        .bind(self.internal)
        .bind(self.archived)
        .bind(self.budget_hours)
        .bind(self.sample)
        .execute(c)
        .await?;
        Ok(())
    }
    async fn check_delete<'a>(
        prev: &'a Self,
        _w: &'a Workspace,
        c: &'a mut PgConnection,
        ws: &'a str,
    ) -> ApiResult<()> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM tasks WHERE workspace_id = $1 AND project_id = $2",
        )
        .bind(ws)
        .bind(&prev.id)
        .fetch_one(c)
        .await?;
        if n > 0 {
            return Err(ApiError::conflict(
                "This project has logged time. Archive it instead.",
            ));
        }
        Ok(())
    }
}

/* ---------------- Tasks ---------------- */

fn locked_msg(w: &Workspace) -> ApiError {
    ApiError::conflict(format!(
        "{} and earlier are locked. Unlock in Settings to change them.",
        w.locked_through
    ))
}

impl Entity for Task {
    const TABLE: &'static str = "tasks";
    const NAME: &'static str = "task";
    const ORDER_BY: &'static str = "date DESC, created_ms DESC";
    fn id(&self) -> &str {
        &self.id
    }
    fn validate(&self) -> ApiResult<()> {
        Task::validate(self)
    }
    fn describe(prev: Option<&Self>, next: &Self) -> String {
        let hours = f64::from(next.minutes) / 60.0;
        status_change(
            prev.map(|p| p.status.as_str()),
            &next.status,
            format!("{} ({hours:.2}h, {})", clip(&next.memo, 80), next.date),
        )
    }
    async fn upsert<'a>(&'a self, c: &'a mut PgConnection, ws: &'a str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO tasks (workspace_id, id, date, project_id, memo, link, minutes, status, by_name, invoice_id, waive_reason, merged_from, created_ms, sample)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
             ON CONFLICT (workspace_id, id) DO UPDATE SET date = EXCLUDED.date, project_id = EXCLUDED.project_id, memo = EXCLUDED.memo,
               link = EXCLUDED.link, minutes = EXCLUDED.minutes, status = EXCLUDED.status, by_name = EXCLUDED.by_name,
               invoice_id = EXCLUDED.invoice_id, waive_reason = EXCLUDED.waive_reason, merged_from = EXCLUDED.merged_from,
               sample = EXCLUDED.sample, updated_at = now()",
        )
        .bind(ws)
        .bind(&self.id)
        .bind(self.date)
        .bind(&self.project_id)
        .bind(self.memo.trim())
        .bind(self.link.trim())
        .bind(self.minutes)
        .bind(&self.status)
        .bind(&self.by)
        .bind(&self.invoice_id)
        .bind(&self.waive_reason)
        .bind(&self.merged_from)
        .bind(self.created_at)
        .bind(self.sample)
        .execute(c)
        .await?;
        Ok(())
    }
    /// Locked months and invoiced tasks keep their content. Status changes (approve, waive,
    /// invoice, void) stay allowed, matching the app.
    fn check_write(prev: Option<&Self>, next: &Self, w: &Workspace) -> ApiResult<()> {
        let content_changed = prev.is_none_or(|p| !p.same_content(next));
        if !content_changed {
            return Ok(());
        }
        if w.is_locked(next.date) || prev.is_some_and(|p| w.is_locked(p.date)) {
            return Err(locked_msg(w));
        }
        if prev.is_some_and(|p| p.status == "invoiced") && next.status == "invoiced" {
            return Err(ApiError::conflict(
                "Invoiced tasks can't be edited. Void the invoice first.",
            ));
        }
        Ok(())
    }
    async fn check_delete<'a>(
        prev: &'a Self,
        w: &'a Workspace,
        _c: &'a mut PgConnection,
        _ws: &'a str,
    ) -> ApiResult<()> {
        if prev.status == "invoiced" && !prev.sample {
            return Err(ApiError::conflict(
                "Invoiced tasks can't be deleted. Void the invoice first.",
            ));
        }
        if w.is_locked(prev.date) && !prev.sample {
            return Err(locked_msg(w));
        }
        Ok(())
    }
}

/* ---------------- Invoices ---------------- */

impl Entity for Invoice {
    const TABLE: &'static str = "invoices";
    const NAME: &'static str = "invoice";
    const ORDER_BY: &'static str = "seq DESC";
    fn id(&self) -> &str {
        &self.id
    }
    fn validate(&self) -> ApiResult<()> {
        Invoice::validate(self)
    }
    fn describe(prev: Option<&Self>, next: &Self) -> String {
        let paid_before = prev.map_or(0, |p| p.payments.len());
        if next.payments.len() > paid_before {
            if let Some(pay) = next.payments.last() {
                return format!(
                    "Invoice {} · payment {:.2} {}",
                    next.number, pay.amount, next.currency
                );
            }
        }
        status_change(
            prev.map(|p| p.status.as_str()),
            &next.status,
            format!("Invoice {} for {}", next.number, next.client.name),
        )
    }
    async fn upsert<'a>(&'a self, c: &'a mut PgConnection, ws: &'a str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO invoices (workspace_id, id, seq, number, client_id, client, sender, period, issue_date, due_date, currency, lines,
               discount, tax_rate, tax_label, notes, status, sent_at, paid_at, paid_note, credits, created_ms, sample, payments)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)
             ON CONFLICT (workspace_id, id) DO UPDATE SET seq = EXCLUDED.seq, number = EXCLUDED.number, client_id = EXCLUDED.client_id,
               client = EXCLUDED.client, sender = EXCLUDED.sender, period = EXCLUDED.period, issue_date = EXCLUDED.issue_date,
               due_date = EXCLUDED.due_date, currency = EXCLUDED.currency, lines = EXCLUDED.lines, discount = EXCLUDED.discount,
               tax_rate = EXCLUDED.tax_rate, tax_label = EXCLUDED.tax_label, notes = EXCLUDED.notes, status = EXCLUDED.status,
               sent_at = EXCLUDED.sent_at, paid_at = EXCLUDED.paid_at, paid_note = EXCLUDED.paid_note, credits = EXCLUDED.credits,
               sample = EXCLUDED.sample, payments = EXCLUDED.payments, updated_at = now()",
        )
        .bind(ws)
        .bind(&self.id)
        .bind(self.seq)
        .bind(&self.number)
        .bind(&self.client_id)
        .bind(&self.client)
        .bind(&self.sender)
        .bind(&self.period)
        .bind(self.issue_date)
        .bind(self.due_date)
        .bind(&self.currency)
        .bind(&self.lines)
        .bind(self.discount)
        .bind(self.tax_rate)
        .bind(&self.tax_label)
        .bind(&self.notes)
        .bind(&self.status)
        .bind(self.sent_at)
        .bind(self.paid_at)
        .bind(&self.paid_note)
        .bind(&self.credits)
        .bind(self.created_at)
        .bind(self.sample)
        .bind(&self.payments)
        .execute(c)
        .await?;
        Ok(())
    }
    /// Once sent, an invoice's money is fixed: only status, payment and credit notes change.
    fn check_write(prev: Option<&Self>, next: &Self, _w: &Workspace) -> ApiResult<()> {
        let Some(p) = prev else { return Ok(()) };
        if p.status == "void" && next.status != "void" {
            return Err(ApiError::conflict(
                "A void invoice can't be reopened. Create a new one.",
            ));
        }
        if p.status != "draft" {
            let same_money = serde_json::to_value(&p.lines.0).ok()
                == serde_json::to_value(&next.lines.0).ok()
                && p.discount == next.discount
                && p.tax_rate == next.tax_rate
                && p.number == next.number
                && p.client_id == next.client_id;
            if !same_money {
                return Err(ApiError::conflict(
                    "Sent invoices can't be edited. Issue a credit note or void it.",
                ));
            }
        }
        Ok(())
    }
    async fn check_delete<'a>(
        prev: &'a Self,
        _w: &'a Workspace,
        c: &'a mut PgConnection,
        ws: &'a str,
    ) -> ApiResult<()> {
        if prev.status != "draft" && !prev.sample {
            return Err(ApiError::conflict(
                "Only drafts can be deleted. Void the invoice instead.",
            ));
        }
        let (n,): (i64,) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM tasks WHERE workspace_id = $1 AND invoice_id = $2)
                  + (SELECT count(*) FROM expenses WHERE workspace_id = $1 AND invoice_id = $2)",
        )
        .bind(ws)
        .bind(&prev.id)
        .fetch_one(c)
        .await?;
        if n > 0 {
            return Err(ApiError::conflict(
                "Release this invoice's tasks and expenses before deleting it.",
            ));
        }
        Ok(())
    }
}

/* ---------------- Expenses ---------------- */

impl Entity for Expense {
    const TABLE: &'static str = "expenses";
    const NAME: &'static str = "expense";
    const ORDER_BY: &'static str = "date DESC";
    fn id(&self) -> &str {
        &self.id
    }
    fn validate(&self) -> ApiResult<()> {
        Expense::validate(self)
    }
    fn describe(_prev: Option<&Self>, next: &Self) -> String {
        format!("Expense {} ({})", clip(&next.desc, 80), next.amount)
    }
    async fn upsert<'a>(&'a self, c: &'a mut PgConnection, ws: &'a str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO expenses (workspace_id, id, date, client_id, description, amount, markup, billable, status, invoice_id, sample)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
             ON CONFLICT (workspace_id, id) DO UPDATE SET date = EXCLUDED.date, client_id = EXCLUDED.client_id,
               description = EXCLUDED.description, amount = EXCLUDED.amount, markup = EXCLUDED.markup, billable = EXCLUDED.billable,
               status = EXCLUDED.status, invoice_id = EXCLUDED.invoice_id, sample = EXCLUDED.sample, updated_at = now()",
        )
        .bind(ws)
        .bind(&self.id)
        .bind(self.date)
        .bind(&self.client_id)
        .bind(self.desc.trim())
        .bind(self.amount)
        .bind(self.markup)
        .bind(self.billable)
        .bind(&self.status)
        .bind(&self.invoice_id)
        .bind(self.sample)
        .execute(c)
        .await?;
        Ok(())
    }
}

/* ---------------- Whole workspace ---------------- */

pub async fn load_all(c: &mut PgConnection, ws: &str) -> sqlx::Result<DataState> {
    Ok(DataState {
        workspace: workspace(&mut *c, ws).await?,
        clients: list(&mut *c, ws).await?,
        projects: list(&mut *c, ws).await?,
        tasks: list(&mut *c, ws).await?,
        invoices: list(&mut *c, ws).await?,
        expenses: list(&mut *c, ws).await?,
    })
}

/// Replaces everything in the workspace with `data`. Call inside a transaction.
pub async fn replace_all(c: &mut PgConnection, ws: &str, data: &DataState) -> sqlx::Result<()> {
    save_workspace(&mut *c, ws, &data.workspace).await?;
    for table in ["tasks", "expenses", "invoices", "projects", "clients"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE workspace_id = $1"))
            .bind(ws)
            .execute(&mut *c)
            .await?;
    }
    for x in &data.clients {
        x.upsert(&mut *c, ws).await?;
    }
    for x in &data.projects {
        x.upsert(&mut *c, ws).await?;
    }
    for x in &data.invoices {
        x.upsert(&mut *c, ws).await?;
    }
    for x in &data.tasks {
        x.upsert(&mut *c, ws).await?;
    }
    for x in &data.expenses {
        x.upsert(&mut *c, ws).await?;
    }
    Ok(())
}
