//! Notifications to Telegram, email and webhooks. Secret values are masked before sending.

use crate::app::App;
use crate::store::Service;
use crate::util;
use anyhow::{Result, anyhow};
use rusqlite::params;
use serde_json::{Value, json};

pub struct Channel {
    pub id: String,
    kind: String,
    name: String,
    config: Value,
    events: Vec<String>,
}

pub fn channels(app: &App) -> Result<Vec<Channel>> {
    let rows = app.db.rows("SELECT id, kind, name, config, events FROM notification_channels WHERE enabled = 1", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Vec<u8>>(3)?, r.get::<_, String>(4)?))
    })?;
    rows.into_iter()
        .map(|(id, kind, name, config, events)| {
            let config = serde_json::from_str(&app.keys.open_str(&format!("notify:{id}"), &config)?)?;
            Ok(Channel { id, kind, name, config, events: serde_json::from_str(&events).unwrap_or_default() })
        })
        .collect()
}

pub fn save_channel(app: &App, kind: &str, name: &str, config: &Value, events: &[String]) -> Result<String> {
    let id = util::id();
    app.db.exec(
        "INSERT INTO notification_channels (id, kind, name, config, events, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, kind, name, app.keys.seal_str(&format!("notify:{id}"), &config.to_string())?, serde_json::to_string(events)?, util::now()],
    )?;
    Ok(id)
}

fn wants(c: &Channel, kind: &str) -> bool {
    c.events.iter().any(|e| e == "*" || kind.strip_prefix(e.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('.')))
}

/// Send an event to every channel subscribed to it. Never fails the caller.
pub async fn event(app: &App, kind: &str, title: &str, body: &str, service: Option<&Service>) {
    let Ok(chans) = channels(app) else { return };
    let masker = service.map(|s| crate::secrets::Masker::for_service(app, s));
    let body = masker.as_ref().map_or(body.into(), |m| m.mask(body));
    for c in chans.iter().filter(|c| wants(c, kind)) {
        if let Err(e) = send(c, kind, title, &body, service).await {
            tracing::warn!("notification to {} failed: {e:#}", c.name);
        }
    }
}

pub async fn send(c: &Channel, kind: &str, title: &str, body: &str, service: Option<&Service>) -> Result<()> {
    let client = util::http(15).build()?;
    match c.kind.as_str() {
        "telegram" => {
            let token = c.config["bot_token"].as_str().ok_or_else(|| anyhow!("missing bot token"))?;
            let chat = &c.config["chat_id"];
            let mut payload = json!({ "chat_id": chat, "text": format!("{title}\n{body}"), "disable_web_page_preview": true });
            if let Some(thread) = c.config.get("thread_id").filter(|v| !v.is_null()) {
                payload["message_thread_id"] = thread.clone();
            }
            client.post(format!("https://api.telegram.org/bot{token}/sendMessage")).json(&payload).send().await?.error_for_status()?;
        }
        "webhook" => {
            let url = c.config["url"].as_str().ok_or_else(|| anyhow!("missing url"))?;
            client
                .post(url)
                .json(&json!({ "event": kind, "title": title, "body": body, "service": service.map(|s| json!({ "id": s.id, "name": s.name, "project": s.project })), "at": util::now() }))
                .send()
                .await?
                .error_for_status()?;
        }
        "email" => send_email(c, title, body).await?,
        other => return Err(anyhow!("unknown channel kind {other}")),
    }
    Ok(())
}

async fn send_email(c: &Channel, title: &str, body: &str) -> Result<()> {
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, transport::smtp::authentication::Credentials};
    let cfg = &c.config;
    let host = cfg["host"].as_str().ok_or_else(|| anyhow!("missing SMTP host"))?;
    let mut builder = if cfg["tls"].as_str() == Some("starttls") {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(host)?
    };
    if let Some(port) = cfg["port"].as_u64() {
        builder = builder.port(u16::try_from(port).map_err(|_| anyhow!("the SMTP port {port} is out of range"))?);
    }
    if let (Some(u), Some(p)) = (cfg["username"].as_str(), cfg["password"].as_str()) {
        builder = builder.credentials(Credentials::new(u.into(), p.into()));
    }
    let msg = Message::builder()
        .from(cfg["from"].as_str().ok_or_else(|| anyhow!("missing from address"))?.parse()?)
        .to(cfg["to"].as_str().ok_or_else(|| anyhow!("missing to address"))?.parse()?)
        .subject(title)
        .body(body.to_string())?;
    builder.build().send(msg).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_match_prefixes() {
        let c = |e: &[&str]| Channel { id: String::new(), kind: String::new(), name: String::new(), config: Value::Null, events: e.iter().map(|s| s.to_string()).collect() };
        assert!(wants(&c(&["*"]), "service.oom"));
        assert!(wants(&c(&["service"]), "service.oom"));
        assert!(wants(&c(&["alert"]), "alert"));
        assert!(!wants(&c(&["service"]), "services.x"));
        assert!(!wants(&c(&["deploy"]), "alert"));
    }
}
