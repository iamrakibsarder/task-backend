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
    /// Built from `SMTP_USER` + `SMTP_PASSWORD` (+ `SMTP_HOST`, default Gmail) when not set.
    pub smtp_url: String,
    pub mail_from: String,
    /// Bearer token for `POST /api/cron/reminders`. Empty = endpoint off.
    pub cron_secret: String,
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
            smtp_url: smtp_url(
                &var("SMTP_URL"),
                &var("SMTP_HOST"),
                &var("SMTP_USER"),
                &env::var("SMTP_PASSWORD").unwrap_or_default(),
            ),
            mail_from: Some(var("MAIL_FROM"))
                .filter(|f| !f.is_empty())
                .or_else(|| {
                    let user = var("SMTP_USER");
                    user.contains('@').then(|| format!("TaskX <{user}>"))
                })
                .unwrap_or_else(|| "TaskX <no-reply@localhost>".into()),
            cron_secret: var("CRON_SECRET"),
            screenshot_dir: env::var("SCREENSHOT_DIR")
                .unwrap_or_else(|_| "./storage/screenshots".into()),
        })
    }
}

fn var(name: &str) -> String {
    env::var(name).unwrap_or_default().trim().to_string()
}

/// `SMTP_URL` as given, or one built from a user and password so nothing needs URL-encoding
/// by hand (Gmail: `SMTP_USER=you@gmail.com`, `SMTP_PASSWORD=` a 16-letter app password).
pub fn smtp_url(url: &str, host: &str, user: &str, password: &str) -> String {
    if !url.is_empty() || user.is_empty() || password.is_empty() {
        return url.to_string();
    }
    let host = if host.is_empty() {
        "smtp.gmail.com"
    } else {
        host
    };
    // Google shows app passwords in groups of four ("abcd efgh ijkl mnop").
    let password = if host == "smtp.gmail.com" {
        password.replace(' ', "")
    } else {
        password.to_string()
    };
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    // `host:587` = STARTTLS, anything else = TLS from the start (465).
    match host.rsplit_once(':') {
        Some((h, "587")) => format!(
            "smtp://{}:{}@{h}:587?tls=required",
            enc(user),
            enc(&password)
        ),
        Some((h, port)) => format!("smtps://{}:{}@{h}:{port}", enc(user), enc(&password)),
        None => format!("smtps://{}:{}@{host}:465", enc(user), enc(&password)),
    }
}

#[cfg(test)]
mod tests {
    use super::smtp_url;

    #[test]
    fn smtp_url_from_user_and_password() {
        assert_eq!(
            smtp_url("", "", "me@gmail.com", "abcd efgh ijkl mnop"),
            "smtps://me%40gmail.com:abcdefghijklmnop@smtp.gmail.com:465"
        );
        assert_eq!(
            smtp_url("", "mail.example.com:587", "me", "p@ss word"),
            "smtp://me:p%40ss%20word@mail.example.com:587?tls=required"
        );
        assert_eq!(smtp_url("smtp://x:25", "", "me", "pw"), "smtp://x:25");
        assert_eq!(smtp_url("", "", "me", ""), "");
        // lettre accepts what we build.
        let url = smtp_url("", "", "me@gmail.com", "abcd efgh ijkl mnop");
        assert!(crate::mail::SmtpMailer::new(&url, "TaskX <me@gmail.com>").is_ok());
    }
}
