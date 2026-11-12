use std::sync::Arc;

use taskx_backend::{app, config::Config, connect, screenshots::DiskStore, AppState, MIGRATOR};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "taskx_backend=info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });

    let db = connect(&cfg.database_url).await.unwrap_or_else(|e| {
        eprintln!("Couldn't connect to the database: {e}");
        std::process::exit(1)
    });
    MIGRATOR.run(&db).await.unwrap_or_else(|e| {
        eprintln!("Migration failed: {e}");
        std::process::exit(1)
    });

    let mailer: Arc<dyn taskx_backend::mail::Mailer> = if cfg.smtp_url.is_empty() {
        tracing::warn!(
            "SMTP_URL is not set: reset and invite emails are written to the log instead of sent"
        );
        Arc::new(taskx_backend::mail::LogMailer)
    } else {
        Arc::new(
            taskx_backend::mail::SmtpMailer::new(&cfg.smtp_url, &cfg.mail_from).unwrap_or_else(
                |e| {
                    eprintln!("{e}");
                    std::process::exit(1)
                },
            ),
        )
    };
    let screenshots = Arc::new(DiskStore {
        root: cfg.screenshot_dir.clone().into(),
    });
    let router = app(
        AppState {
            db,
            workspace_id: cfg.workspace_id.clone(),
            auth_required: cfg.auth_required,
            limiter: Arc::new(taskx_backend::auth::LoginLimiter::default()),
            mailer,
            app_url: cfg.app_url.clone(),
            screenshots,
        },
        &cfg.cors_origins,
    );
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Can't listen on {addr}: {e}");
            std::process::exit(1)
        });
    tracing::info!(
        "TaskX API on http://{addr} ({}, CORS {:?}, screenshots in {})",
        if cfg.auth_required {
            "accounts on".to_string()
        } else {
            format!("accounts off, workspace '{}'", cfg.workspace_id)
        },
        cfg.cors_origins,
        cfg.screenshot_dir
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await
        .expect("server error");
}
