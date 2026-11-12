//! Outgoing email: password reset links and team invitations.
//!
//! - `SMTP_URL` set → `SmtpMailer` (e.g. `smtps://user:pass@smtp.example.com:465`).
//! - Not set → `LogMailer`: the message is written to the server log, so local setups work
//!   without a mail server. Don't run production like that: reset links would sit in logs.
//! - Tests use `MemoryMailer` to read what would have been sent.
//!
//! Sending happens in the background (`send_later`), so a slow mail server never slows a
//! request, and response times don't reveal whether an email address has an account.

use std::{future::Future, pin::Pin, sync::Arc, sync::Mutex};

use lettre::{
    message::{header::ContentType, Mailbox},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

pub type SendResult = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
}

pub trait Mailer: Send + Sync {
    fn send(&self, email: Email) -> SendResult;
    /// True when messages really leave the server.
    fn delivers(&self) -> bool;
}

/// Sends without waiting; failures are logged.
pub fn send_later(mailer: &Arc<dyn Mailer>, email: Email) {
    let m = mailer.clone();
    tokio::spawn(async move {
        let to = email.to.clone();
        if let Err(e) = m.send(email).await {
            tracing::error!(%to, error = %e, "sending email failed");
        }
    });
}

/* ---------------- SMTP ---------------- */

pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(url: &str, from: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| format!("SMTP_URL is invalid: {e}"))?
            .build();
        let from = from
            .parse()
            .map_err(|e| format!("MAIL_FROM '{from}' is not an email address: {e}"))?;
        Ok(Self { transport, from })
    }
}

impl Mailer for SmtpMailer {
    fn send(&self, email: Email) -> SendResult {
        let transport = self.transport.clone();
        let msg = email
            .to
            .parse::<Mailbox>()
            .map_err(|e| e.to_string())
            .and_then(|to| {
                Message::builder()
                    .from(self.from.clone())
                    .to(to)
                    .subject(email.subject)
                    .header(ContentType::TEXT_PLAIN)
                    .body(email.body)
                    .map_err(|e| e.to_string())
            });
        Box::pin(async move {
            transport.send(msg?).await.map_err(|e| e.to_string())?;
            Ok(())
        })
    }
    fn delivers(&self) -> bool {
        true
    }
}

/* ---------------- Log only ---------------- */

pub struct LogMailer;

impl Mailer for LogMailer {
    fn send(&self, email: Email) -> SendResult {
        tracing::warn!(
            to = %email.to,
            "email is not configured (SMTP_URL); this message was not sent:\n{}\n\n{}",
            email.subject,
            email.body
        );
        Box::pin(async { Ok(()) })
    }
    fn delivers(&self) -> bool {
        false
    }
}

/* ---------------- Tests ---------------- */

#[derive(Default)]
pub struct MemoryMailer {
    pub sent: Mutex<Vec<Email>>,
}

impl MemoryMailer {
    pub fn last_to(&self, to: &str) -> Option<Email> {
        self.sent
            .lock()
            .expect("mailer lock")
            .iter()
            .rev()
            .find(|e| e.to == to)
            .cloned()
    }
}

impl Mailer for MemoryMailer {
    fn send(&self, email: Email) -> SendResult {
        self.sent.lock().expect("mailer lock").push(email);
        Box::pin(async { Ok(()) })
    }
    fn delivers(&self) -> bool {
        true
    }
}

/// The token at the end of the first line in `body` that contains `marker` (for tests).
pub fn token_after(body: &str, marker: &str) -> Option<String> {
    body.lines()
        .find(|l| l.contains(marker))
        .and_then(|l| l.split(marker).nth(1))
        .map(|t| t.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_url_and_sender_are_checked() {
        assert!(SmtpMailer::new(
            "smtps://u:p@smtp.example.com:465",
            "TaskX <hello@example.com>"
        )
        .is_ok());
        assert!(SmtpMailer::new("not a url", "hello@example.com").is_err());
        assert!(SmtpMailer::new("smtp://localhost:25", "nope").is_err());
    }

    #[test]
    fn tokens_can_be_read_back_from_links() {
        let body = "Hi\nOpen http://app/reset-password?token=abc123\nThanks";
        assert_eq!(token_after(body, "token=").as_deref(), Some("abc123"));
    }
}
