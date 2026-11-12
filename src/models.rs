//! API shapes. Field names and JSON form match `taskx-frontend/src/types/domain.ts` exactly
//! (camelCase), so the frontend can send and receive them without mapping.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{types::Json, FromRow};

use crate::error::{ApiError, ApiResult};

fn empty<T>() -> Json<Vec<T>> {
    Json(Vec::new())
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn one_of(field: &str, value: &str, allowed: &[&str]) -> ApiResult<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(ApiError::bad(format!(
            "{field} must be one of: {}.",
            allowed.join(", ")
        )))
    }
}
fn required(field: &str, value: &str) -> ApiResult<()> {
    if value.trim().is_empty() {
        Err(ApiError::bad(format!("{field} is required.")))
    } else {
        Ok(())
    }
}
fn non_negative(field: &str, v: Decimal) -> ApiResult<()> {
    if v.is_sign_negative() && !v.is_zero() {
        Err(ApiError::bad(format!("{field} can't be negative.")))
    } else {
        Ok(())
    }
}
pub fn is_month(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && s[..4].parse::<u16>().is_ok()
        && matches!(s[5..].parse::<u8>(), Ok(1..=12))
}
fn currency(v: &str) -> ApiResult<()> {
    if v.len() == 3 && v.chars().all(|c| c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(ApiError::bad("currency must be a 3-letter code like USD."))
    }
}

/* ---------------- Workspace ---------------- */

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub business_name: String,
    pub owner_name: String,
    pub email: String,
    pub address: String,
    pub currency: String,
    pub default_rate: Decimal,
    pub invoice_prefix: String,
    pub terms_days: i32,
    pub tax_rate: Decimal,
    pub tax_label: String,
    pub payment_info: String,
    pub invoice_note: String,
    pub auto_approve: bool,
    pub team: bool,
    pub members: Vec<String>,
    pub rounding: String,
    /// `""` = nothing locked, else `YYYY-MM`.
    pub locked_through: String,
    /// Minutes between timer screenshots. 0 = off.
    #[serde(default = "five")]
    pub screenshot_minutes: i32,
}

fn five() -> i32 {
    5
}

impl Workspace {
    pub fn validate(&self) -> ApiResult<()> {
        currency(&self.currency)?;
        non_negative("defaultRate", self.default_rate)?;
        non_negative("taxRate", self.tax_rate)?;
        if self.tax_rate > Decimal::from(100) {
            return Err(ApiError::bad("taxRate must be 100 or less."));
        }
        if !(0..=365).contains(&self.terms_days) {
            return Err(ApiError::bad("termsDays must be between 0 and 365."));
        }
        one_of("rounding", &self.rounding, &["nearest", "up", "down"])?;
        if !self.locked_through.is_empty() && !is_month(&self.locked_through) {
            return Err(ApiError::bad("lockedThrough must be empty or YYYY-MM."));
        }
        if !(0..=240).contains(&self.screenshot_minutes) {
            return Err(ApiError::bad(
                "screenshotMinutes must be between 0 and 240.",
            ));
        }
        Ok(())
    }

    /// True when `date` falls in a locked month.
    pub fn is_locked(&self, date: NaiveDate) -> bool {
        !self.locked_through.is_empty() && date.format("%Y-%m").to_string() <= self.locked_through
    }
}

/* ---------------- Clients & projects ---------------- */

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub id: String,
    pub name: String,
    pub color: String,
    /// Data URL (`data:image/...`) or https link. Empty = initial on `color`.
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub contact: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub address: String,
    pub billing: String,
    pub rate: Option<Decimal>,
    #[serde(default)]
    pub retainer_fee: Decimal,
    #[serde(default)]
    pub included_hours: Decimal,
    #[serde(default)]
    pub cap: Decimal,
    pub active: bool,
    #[serde(default)]
    #[sqlx(rename = "created_ms")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sample: bool,
}

impl Client {
    pub fn validate(&self) -> ApiResult<()> {
        required("Client name", &self.name)?;
        one_of("billing", &self.billing, &["hourly", "retainer"])?;
        if !self.logo.is_empty() {
            if !(self.logo.starts_with("data:image/") || self.logo.starts_with("https://")) {
                return Err(ApiError::bad(
                    "logo must be an image data URL or an https link.",
                ));
            }
            if self.logo.len() > 400_000 {
                return Err(ApiError::bad(
                    "That logo is too large. Use an image under 300 KB.",
                ));
            }
        }
        if let Some(r) = self.rate {
            non_negative("rate", r)?;
        }
        non_negative("retainerFee", self.retainer_fee)?;
        non_negative("includedHours", self.included_hours)?;
        non_negative("cap", self.cap)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub client_id: Option<String>,
    pub name: String,
    pub rate: Option<Decimal>,
    pub internal: bool,
    pub archived: bool,
    pub budget_hours: Option<Decimal>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sample: bool,
}

impl Project {
    pub fn validate(&self) -> ApiResult<()> {
        required("Project name", &self.name)?;
        if !self.internal && self.client_id.as_deref().unwrap_or("").is_empty() {
            return Err(ApiError::bad("A client project needs a clientId."));
        }
        if let Some(r) = self.rate {
            non_negative("rate", r)?;
        }
        if let Some(b) = self.budget_hours {
            non_negative("budgetHours", b)?;
        }
        Ok(())
    }
}

/* ---------------- Tasks ---------------- */

pub const TASK_STATUSES: &[&str] = &["review", "billable", "invoiced", "waived", "internal"];

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub date: NaiveDate,
    pub project_id: String,
    pub memo: String,
    #[serde(default)]
    pub link: String,
    /// Always a positive multiple of 15.
    pub minutes: i32,
    pub status: String,
    #[serde(default)]
    #[sqlx(rename = "by_name")]
    pub by: String,
    pub invoice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waive_reason: Option<String>,
    /// Original entries when this task was made by merging (kept as given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_from: Option<Json<Vec<Value>>>,
    #[serde(default)]
    #[sqlx(rename = "created_ms")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sample: bool,
}

impl Task {
    pub fn validate(&self) -> ApiResult<()> {
        required("What you did (memo)", &self.memo)?;
        required("projectId", &self.project_id)?;
        if self.minutes <= 0 || self.minutes % 15 != 0 {
            return Err(ApiError::bad("minutes must be a positive multiple of 15."));
        }
        if self.minutes > 24 * 60 {
            return Err(ApiError::bad(
                "A single task can't be longer than 24 hours.",
            ));
        }
        one_of("status", &self.status, TASK_STATUSES)?;
        let invoiced = self.status == "invoiced";
        if invoiced != self.invoice_id.is_some() {
            return Err(ApiError::bad(
                "Only invoiced tasks have an invoiceId, and every invoiced task needs one.",
            ));
        }
        Ok(())
    }

    /// The parts of a task that are "content" (blocked in locked months and once invoiced),
    /// as opposed to status changes, which stay allowed.
    pub fn same_content(&self, o: &Task) -> bool {
        self.date == o.date
            && self.project_id == o.project_id
            && self.memo == o.memo
            && self.link == o.link
            && self.minutes == o.minutes
            && self.by == o.by
    }
}

/* ---------------- Invoices ---------------- */

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PartySnapshot {
    pub name: String,
    pub contact: String,
    pub email: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SenderSnapshot {
    pub business_name: String,
    pub owner_name: String,
    pub email: String,
    pub address: String,
    pub payment_info: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceLine {
    pub kind: String,
    pub desc: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub minutes: i64,
    #[serde(default)]
    pub rate: f64,
    pub amount: f64,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expense_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditNote {
    pub id: String,
    pub number: String,
    pub date: NaiveDate,
    pub amount: f64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Invoice {
    pub id: String,
    pub seq: i32,
    pub number: String,
    pub client_id: String,
    pub client: Json<PartySnapshot>,
    #[serde(rename = "from", default, skip_serializing_if = "Option::is_none")]
    #[sqlx(rename = "sender")]
    pub sender: Option<Json<SenderSnapshot>>,
    pub period: String,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub currency: String,
    pub lines: Json<Vec<InvoiceLine>>,
    #[serde(default)]
    pub discount: Decimal,
    #[serde(default)]
    pub tax_rate: Decimal,
    #[serde(default)]
    pub tax_label: String,
    #[serde(default)]
    pub notes: String,
    pub status: String,
    #[serde(default)]
    pub sent_at: Option<NaiveDate>,
    #[serde(default)]
    pub paid_at: Option<NaiveDate>,
    #[serde(default)]
    pub paid_note: Option<String>,
    #[serde(default = "empty")]
    pub credits: Json<Vec<CreditNote>>,
    #[serde(default)]
    #[sqlx(rename = "created_ms")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sample: bool,
}

pub const LINE_KINDS: &[&str] = &[
    "hours", "retainer", "overage", "covered", "expense", "custom",
];

impl Invoice {
    pub fn validate(&self) -> ApiResult<()> {
        required("Invoice number", &self.number)?;
        if self.seq <= 0 {
            return Err(ApiError::bad("seq must be positive."));
        }
        if !is_month(&self.period) {
            return Err(ApiError::bad("period must be YYYY-MM."));
        }
        if self.due_date < self.issue_date {
            return Err(ApiError::bad(
                "The due date can't be before the issue date.",
            ));
        }
        currency(&self.currency)?;
        one_of("status", &self.status, &["draft", "sent", "paid", "void"])?;
        non_negative("discount", self.discount)?;
        non_negative("taxRate", self.tax_rate)?;
        for l in self.lines.iter() {
            one_of("line kind", &l.kind, LINE_KINDS)?;
            if !l.amount.is_finite() {
                return Err(ApiError::bad("Every line needs a number amount."));
            }
        }
        for c in self.credits.iter() {
            if c.amount.is_nan() || c.amount <= 0.0 {
                return Err(ApiError::bad("Credit notes must be above zero."));
            }
        }
        Ok(())
    }
}

/* ---------------- Expenses ---------------- */

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Expense {
    pub id: String,
    pub date: NaiveDate,
    pub client_id: Option<String>,
    #[sqlx(rename = "description")]
    pub desc: String,
    pub amount: Decimal,
    #[serde(default)]
    pub markup: Decimal,
    pub billable: bool,
    pub status: String,
    pub invoice_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sample: bool,
}

impl Expense {
    pub fn validate(&self) -> ApiResult<()> {
        required("Expense description", &self.desc)?;
        non_negative("amount", self.amount)?;
        non_negative("markup", self.markup)?;
        one_of("status", &self.status, &["unbilled", "invoiced"])?;
        if (self.status == "invoiced") != self.invoice_id.is_some() {
            return Err(ApiError::bad("Only invoiced expenses have an invoiceId."));
        }
        Ok(())
    }
}

/* ---------------- Whole workspace ---------------- */

/// What `GET /api/bootstrap` returns and `POST /api/import` accepts.
#[derive(Debug, Serialize, Deserialize)]
pub struct DataState {
    pub workspace: Workspace,
    #[serde(default)]
    pub clients: Vec<Client>,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub invoices: Vec<Invoice>,
    #[serde(default)]
    pub expenses: Vec<Expense>,
}
