use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Every failure the API can return. The body is always `{ "error": "<message>" }`,
/// and the message is written for a person, so the UI can show it as is.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// The change breaks a business rule (locked month, invoiced task…).
    #[error("{0}")]
    Conflict(String),
    #[error("Database error")]
    Db(#[from] sqlx::Error),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ApiError::BadRequest(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            ApiError::Db(e) => db_error(e),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

/// Turns Postgres constraint errors into readable 409/422s; anything else is a logged 500.
fn db_error(e: &sqlx::Error) -> (StatusCode, String) {
    if let sqlx::Error::Database(d) = e {
        let constraint = d.constraint().unwrap_or_default();
        match d.code().as_deref() {
            Some("23503") => {
                let msg = if d.message().contains("still referenced")
                    || d.message().contains("is still referenced")
                {
                    "This record is still in use by other records."
                } else {
                    "This refers to a record that doesn't exist."
                };
                return (StatusCode::CONFLICT, format!("{msg} ({constraint})"));
            }
            Some("23505") => {
                return (
                    StatusCode::CONFLICT,
                    format!("That already exists ({constraint})."),
                );
            }
            Some("23514") | Some("23502") | Some("22P02") | Some("22003") => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "Invalid value ({constraint}{}).",
                        if constraint.is_empty() {
                            d.message()
                        } else {
                            ""
                        }
                    ),
                );
            }
            _ => {}
        }
    }
    tracing::error!(error = %e, "database error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on the server.".into(),
    )
}
