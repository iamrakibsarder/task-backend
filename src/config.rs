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
            screenshot_dir: env::var("SCREENSHOT_DIR")
                .unwrap_or_else(|_| "./storage/screenshots".into()),
        })
    }
}
