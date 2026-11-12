use std::env;

/// Settings from environment variables (a `.env` file works too).
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub host: String,
    pub port: u16,
    /// Browser origins allowed to call the API, e.g. the Vite dev server.
    pub cors_origins: Vec<String>,
    pub workspace_id: String,
    /// Accounts on (default): every request needs a signed-in user, and each user has their own
    /// workspace. `AUTH=off` = single-business mode, everything goes to `workspace_id`.
    pub auth_required: bool,
    /// Where the web app lives, for links in emails (reset password, invites).
    pub app_url: String,
    /// `smtps://user:pass@host:465` or `smtp://host:25`. Empty = emails go to the log.
    pub smtp_url: String,
    pub mail_from: String,
    /// Folder for timer screenshots.
    pub screenshot_dir: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is not set. Copy .env.example to .env.".to_string())?;
        let port = env::var("PORT")
            .ok()
            .map(|p| p.parse().map_err(|_| format!("PORT '{p}' is not a number")))
            .transpose()?
            .unwrap_or(8080);
        Ok(Self {
            database_url,
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port,
            cors_origins: env::var("CORS_ORIGINS")
                .unwrap_or_else(|_| "http://localhost:5173,http://127.0.0.1:5173".into())
                .split(',')
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            workspace_id: env::var("WORKSPACE_ID").unwrap_or_else(|_| "default".into()),
            auth_required: !matches!(
                env::var("AUTH")
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
                    .as_str(),
                "off" | "false" | "0" | "no"
            ),
            app_url: env::var("APP_URL")
                .unwrap_or_else(|_| "http://localhost:5173".into())
                .trim()
                .trim_end_matches('/')
                .to_string(),
            smtp_url: env::var("SMTP_URL").unwrap_or_default().trim().to_string(),
            mail_from: env::var("MAIL_FROM")
                .unwrap_or_else(|_| "TaskX <no-reply@localhost>".into()),
            screenshot_dir: env::var("SCREENSHOT_DIR")
                .unwrap_or_else(|_| "./storage/screenshots".into()),
        })
    }
}
