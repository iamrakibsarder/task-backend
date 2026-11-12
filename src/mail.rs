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
    message::{header::ContentType, Attachment as Part, Mailbox, MultiPart, SinglePart},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

pub type SendResult = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub cc: Vec<String>,
    /// Where replies go (e.g. the business, for invoices).
    pub reply_to: Option<String>,
    /// Display name for the sender; the address is always `MAIL_FROM`'s.
    pub from_name: Option<String>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
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
        let from = Mailbox::new(
            email.from_name.clone().or_else(|| self.from.name.clone()),
            self.from.email.clone(),
        );
        let msg = build(from, email);
        Box::pin(async move {
            transport.send(msg?).await.map_err(|e| e.to_string())?;
            Ok(())
        })
    }
    fn delivers(&self) -> bool {
        true
    }
}

fn build(from: Mailbox, email: Email) -> Result<Message, String> {
    let mb = |s: &str| s.parse::<Mailbox>().map_err(|e| format!("{s}: {e}"));
    let mut b = Message::builder()
        .from(from)
        .to(mb(&email.to)?)
        .subject(email.subject);
    for cc in &email.cc {
        b = b.cc(mb(cc)?);
    }
    if let Some(r) = &email.reply_to {
        b = b.reply_to(mb(r)?);
    }
    let text = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(email.body);
    let msg = if email.attachments.is_empty() {
        b.singlepart(text)
    } else {
        let mut parts = MultiPart::mixed().singlepart(text);
        for a in email.attachments {
            let ct = ContentType::parse(&a.content_type).map_err(|e| e.to_string())?;
            parts = parts.singlepart(Part::new(a.filename).body(a.bytes, ct));
        }
        b.multipart(parts)
    };
    msg.map_err(|e| e.to_string())
}

/* ---------------- Log only ---------------- */

pub struct LogMailer;

impl Mailer for LogMailer {
    fn send(&self, email: Email) -> SendResult {
        tracing::warn!(
            to = %email.to,
            attachments = email.attachments.len(),
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
    fn messages_carry_cc_reply_to_and_attachments() {
        let msg = build(
            "TaskX <no-reply@example.com>".parse().unwrap(),
            Email {
                to: "client@example.com".into(),
                subject: "Invoice".into(),
                body: "Hi".into(),
                cc: vec!["me@example.com".into()],
                reply_to: Some("studio@example.com".into()),
                attachments: vec![Attachment {
                    filename: "INV-1.pdf".into(),
                    content_type: "application/pdf".into(),
                    bytes: b"%PDF-1.4".to_vec(),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("Cc: me@example.com"));
        assert!(raw.contains("Reply-To: studio@example.com"));
        assert!(raw.contains("INV-1.pdf"));
        assert!(raw.contains("application/pdf"));
        assert!(build(
            "a@b.co".parse().unwrap(),
            Email {
                to: "nope".into(),
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn tokens_can_be_read_back_from_links() {
        let body = "Hi\nOpen http://app/reset-password?token=abc123\nThanks";
        assert_eq!(token_after(body, "token=").as_deref(), Some("abc123"));
    }
}
