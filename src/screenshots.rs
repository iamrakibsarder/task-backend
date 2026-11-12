//! Timer screenshots.
//!
//! `POST /api/screenshots?user=&client=&project=&takenAt=YYYY-MM-DDTHH:MM:SS` with the image as the body.
//! Saved under `SCREENSHOT_DIR` as
//!   <user>/clients/<client>/<project>/<YYYY-MM>/week-<n>/<YYYY-MM-DD_HH-MM-SS>.jpg
//!   <user>/internal/<project>/…   (internal projects send an empty client)
//! The frontend builds the same path (src/features/capture/paths.ts) when it saves to a local folder.
//!
//! Storage sits behind `ScreenshotStore`, so a Google Drive store can replace the disk one later.

use std::path::PathBuf;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::routes::{AppState, Ws};

/// Safe folder/file name: no slashes or characters macOS, Windows or Drive reject.
pub fn safe_name(s: &str, fallback: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = collapsed
        .trim_matches('.')
        .trim()
        .chars()
        .take(80)
        .collect();
    let trimmed = trimmed.trim().to_string();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// Week of the month, weeks starting Monday; the week holding the 1st is week 1.
pub fn week_of_month(d: NaiveDate) -> u32 {
    let first = d.with_day(1).expect("day 1 exists");
    let offset = first.weekday().num_days_from_monday();
    (d.day() - 1 + offset) / 7 + 1
}

/// Relative path segments (folders + file name) for a screenshot.
pub fn relative_path(
    user: &str,
    client: &str,
    project: &str,
    at: NaiveDateTime,
    ext: &str,
) -> Vec<String> {
    let mut parts = vec![safe_name(user, "me")];
    if client.trim().is_empty() {
        parts.push("internal".into());
    } else {
        parts.push("clients".into());
        parts.push(safe_name(client, "unnamed"));
    }
    parts.push(safe_name(project, "unnamed"));
    parts.push(at.format("%Y-%m").to_string());
    parts.push(format!("week-{}", week_of_month(at.date())));
    parts.push(format!("{}.{ext}", at.format("%Y-%m-%d_%H-%M-%S")));
    parts
}

pub trait ScreenshotStore: Send + Sync {
    /// Saves the image under `workspace/<parts…>` and returns a display path.
    fn save<'a>(
        &'a self,
        workspace: &'a str,
        parts: &'a [String],
        bytes: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<String>> + Send + 'a>>;
}

/// Writes to a folder on the server's disk.
pub struct DiskStore {
    pub root: PathBuf,
}

impl ScreenshotStore for DiskStore {
    fn save<'a>(
        &'a self,
        workspace: &'a str,
        parts: &'a [String],
        bytes: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<String>> + Send + 'a>>
    {
        Box::pin(async move {
            let (file, dirs) = parts.split_last().expect("path has a file name");
            let mut dir = self.root.join(safe_name(workspace, "default"));
            for d in dirs {
                dir.push(d);
            }
            tokio::fs::create_dir_all(&dir).await?;
            let path = dir.join(file);
            tokio::fs::write(&path, bytes).await?;
            Ok(parts.join("/"))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotQuery {
    #[serde(default)]
    user: String,
    #[serde(default)]
    client: String,
    #[serde(default)]
    project: String,
    /// The user's local time, `YYYY-MM-DDTHH:MM:SS`, so folders follow their day.
    taken_at: String,
}

const MAX_BYTES: usize = 8 * 1024 * 1024;

pub async fn upload(
    State(state): State<AppState>,
    ws: Ws,
    Query(q): Query<ShotQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ext = match headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
    {
        t if t.starts_with("image/jpeg") => "jpg",
        t if t.starts_with("image/png") => "png",
        t if t.starts_with("image/webp") => "webp",
        _ => {
            return Err(ApiError::bad(
                "Send the screenshot as image/jpeg, image/png or image/webp.",
            ))
        }
    };
    if body.is_empty() {
        return Err(ApiError::bad("The screenshot is empty."));
    }
    if body.len() > MAX_BYTES {
        return Err(ApiError::bad("Screenshots must be under 8 MB."));
    }
    let at = NaiveDateTime::parse_from_str(&q.taken_at, "%Y-%m-%dT%H:%M:%S")
        .map_err(|_| ApiError::bad("takenAt must look like 2026-11-06T14:05:00."))?;
    let parts = relative_path(&q.user, &q.client, &q.project, at, ext);
    let path = state
        .screenshots
        .save(&ws.id, &parts, &body)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "saving screenshot failed");
            ApiError::conflict("The server couldn't save the screenshot.")
        })?;
    tracing::debug!(%path, bytes = body.len(), "screenshot saved");
    Ok(Json(json!({ "path": path })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_start_monday() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(week_of_month(d(2026, 11, 1)), 1); // Sunday
        assert_eq!(week_of_month(d(2026, 11, 2)), 2); // Monday
        assert_eq!(week_of_month(d(2026, 11, 30)), 6);
        assert_eq!(week_of_month(d(2026, 6, 7)), 1);
        assert_eq!(week_of_month(d(2026, 6, 8)), 2);
    }

    #[test]
    fn builds_the_same_path_as_the_frontend() {
        let at = NaiveDateTime::parse_from_str("2026-11-06T02:15:09", "%Y-%m-%dT%H:%M:%S").unwrap();
        let p = relative_path("Alex", "Northwind Bakery", "Website refresh", at, "jpg").join("/");
        assert_eq!(
            p,
            "Alex/clients/Northwind Bakery/Website refresh/2026-11/week-2/2026-11-06_02-15-09.jpg"
        );
        let p = relative_path("Alex", "", "Admin", at, "jpg");
        assert_eq!(&p[..3], &["Alex", "internal", "Admin"]);
    }

    #[test]
    fn names_cant_escape_the_folder() {
        let n = safe_name("../../etc", "x");
        assert!(!n.contains('/') && n != ".." && n != ".");
        assert_eq!(safe_name("..", "x"), "x");
        assert_eq!(safe_name("A/B: \"C\"?", "x"), "A B C");
        assert_eq!(safe_name("...", "unnamed"), "unnamed");
    }
}
