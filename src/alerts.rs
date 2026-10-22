//! Notifications: a healthchecks.io-style dead-man's switch, ntfy push messages and webhooks
//! (Slack, Discord or plain JSON).
//!
//! Sending an alert never fails the operation that triggered it: problems are logged.

use crate::config::AlertsConfig;
use crate::report::Report;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::time::Duration;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Backup,
    Upload,
    Clean,
    Drill,
    Anomaly,
    Test,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Backup => "backup",
            EventKind::Upload => "upload",
            EventKind::Clean => "clean",
            EventKind::Drill => "drill",
            EventKind::Anomaly => "anomaly",
            EventKind::Test => "test",
        }
    }
}

/// Something that happened and may be worth telling someone about.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub success: bool,
    /// One line, e.g. "backup-incr-20261021-120000.tar.zst created (1.2 GiB)".
    pub summary: String,
    /// Optional longer text (error chain, drill report...).
    pub details: Option<String>,
}

impl Event {
    pub fn success(kind: EventKind, summary: impl Into<String>) -> Self {
        Self {
            kind,
            success: true,
            summary: summary.into(),
            details: None,
        }
    }

    pub fn failure(kind: EventKind, error: &anyhow::Error) -> Self {
        Self {
            kind,
            success: false,
            summary: format!("{} failed", kind.as_str()),
            details: Some(format!("{:#}", error)),
        }
    }

    /// For a backup flagged by the anomaly checks.
    pub fn anomaly(backup: &str, anomalies: &[String]) -> Self {
        Self {
            kind: EventKind::Anomaly,
            success: false,
            summary: format!("backup {} looks wrong; old backups are kept", backup),
            details: Some(format!(
                "{}\n\nIf this was expected, run `zesty-backup accept-changes`.",
                anomalies
                    .iter()
                    .map(|a| format!("- {}", a))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        }
    }

    pub fn from_drill(backup: Option<&str>, report: &Report) -> Self {
        Self {
            kind: EventKind::Drill,
            success: report.passed(),
            summary: format!(
                "restore drill of {}: {}",
                backup.unwrap_or("(no backup)"),
                report.summary()
            ),
            details: Some(report.to_string()),
        }
    }

    /// From the result of an operation.
    pub fn from_result<T>(
        kind: EventKind,
        result: &Result<T>,
        summary: impl FnOnce(&T) -> String,
    ) -> Self {
        match result {
            Ok(value) => Self::success(kind, summary(value)),
            Err(e) => Self::failure(kind, e),
        }
    }
}

/// Which channel a delivery went to, and how it went (used by `test-alerts`).
#[derive(Debug)]
pub struct Delivery {
    pub channel: &'static str,
    pub result: Result<()>,
}

pub struct Alerter {
    config: Option<AlertsConfig>,
    server: String,
    client: reqwest::Client,
}

impl Alerter {
    pub fn new(config: Option<&AlertsConfig>) -> Self {
        let server = config
            .and_then(|c| c.server_name.clone())
            .unwrap_or_else(hostname);
        let client = reqwest::Client::builder()
            .user_agent(concat!("zesty-backup/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");
        Self {
            config: config.cloned(),
            server,
            client,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.config.as_ref().is_some_and(|c| {
            c.healthchecks_url.is_some()
                || c.healthchecks_drill_url.is_some()
                || c.ntfy_url.is_some()
                || c.webhook_url.is_some()
        })
    }

    /// Send `event` to every channel that wants it. Errors are logged, never returned.
    pub async fn notify(&self, event: &Event) {
        for delivery in self.send(event).await {
            match delivery.result {
                Ok(()) => debug!("Alert sent to {}", delivery.channel),
                Err(e) => warn!("Could not send alert to {}: {:#}", delivery.channel, e),
            }
        }
    }

    /// Send `event` and report what happened on each channel.
    pub async fn send(&self, event: &Event) -> Vec<Delivery> {
        let Some(config) = &self.config else {
            return Vec::new();
        };
        let mut deliveries = Vec::new();
        let is_test = event.kind == EventKind::Test;

        // Dead-man's switch: successes keep it quiet, failures (and silence) raise the alarm.
        // Only a newly created backup proves the system is alive; an upload of nothing or a
        // paused clean must not keep the dead-man's switch quiet.
        let counts_as_alive = matches!(event.kind, EventKind::Backup | EventKind::Test);
        let ping_url = if event.kind == EventKind::Drill {
            config
                .healthchecks_drill_url
                .as_ref()
                // Drill failures also flag the main check; drill successes don't count as backups.
                .or(config.healthchecks_url.as_ref().filter(|_| !event.success))
        } else {
            config
                .healthchecks_url
                .as_ref()
                .filter(|_| !event.success || counts_as_alive)
        };
        if let Some(url) = ping_url {
            deliveries.push(Delivery {
                channel: "healthchecks",
                result: self.ping(url, event).await,
            });
        }

        let wanted = !event.success || config.notify_success.unwrap_or(false) || is_test;
        if wanted {
            if let Some(url) = &config.ntfy_url {
                deliveries.push(Delivery {
                    channel: "ntfy",
                    result: self.ntfy(url, config.ntfy_token.as_deref(), event).await,
                });
            }
            if let Some(url) = &config.webhook_url {
                deliveries.push(Delivery {
                    channel: "webhook",
                    result: self.webhook(url, event).await,
                });
            }
        }
        deliveries
    }

    fn title(&self, event: &Event) -> String {
        if event.kind == EventKind::Anomaly {
            return format!("zesty-backup on {}: backup anomaly", self.server);
        }
        let status = if event.success { "OK" } else { "FAILED" };
        format!(
            "zesty-backup on {}: {} {}",
            self.server,
            event.kind.as_str(),
            status
        )
    }

    fn body(&self, event: &Event) -> String {
        match &event.details {
            Some(d) if !d.is_empty() => format!("{}\n\n{}", event.summary, truncate(d, 3000)),
            _ => event.summary.clone(),
        }
    }

    async fn ping(&self, url: &str, event: &Event) -> Result<()> {
        let url = healthchecks_url(url, event.success);
        let response = self
            .client
            .post(&url)
            .body(truncate(&self.body(event), 10_000))
            .send()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("request to {} failed", host_of(&url)))?;
        check(response).await
    }

    async fn ntfy(&self, url: &str, token: Option<&str>, event: &Event) -> Result<()> {
        let mut request = self
            .client
            .post(url)
            .header("Title", self.title(event))
            .header("Priority", if event.success { "default" } else { "high" })
            .header(
                "Tags",
                if event.success {
                    "white_check_mark"
                } else {
                    "rotating_light"
                },
            )
            .body(self.body(event));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("request to {} failed", host_of(url)))?;
        check(response).await
    }

    async fn webhook(&self, url: &str, event: &Event) -> Result<()> {
        let payload = webhook_payload(
            url,
            &self.title(event),
            &self.body(event),
            &self.server,
            event,
        );
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("request to {} failed", host_of(url)))?;
        check(response).await
    }
}

async fn check(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    bail!("HTTP {}: {}", status, truncate(body.trim(), 300));
}

/// healthchecks.io: `<url>` signals success, `<url>/fail` failure (query string kept).
pub fn healthchecks_url(base: &str, success: bool) -> String {
    if success {
        return base.trim_end_matches('/').to_string();
    }
    match url::Url::parse(base) {
        Ok(mut url) => {
            let path = format!("{}/fail", url.path().trim_end_matches('/'));
            url.set_path(&path);
            url.to_string()
        }
        Err(_) => format!("{}/fail", base.trim_end_matches('/')),
    }
}

/// Only the host: alert URLs often contain secrets (check UUIDs, topics, webhook tokens).
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "the alert service".to_string())
}

/// Slack and Discord get their own message format; anything else gets structured JSON.
pub fn webhook_payload(
    url: &str,
    title: &str,
    body: &str,
    server: &str,
    event: &Event,
) -> serde_json::Value {
    let text = format!("*{}*\n{}", title, body);
    if url.contains("hooks.slack.com") {
        json!({ "text": text })
    } else if url.contains("discord.com/api/webhooks")
        || url.contains("discordapp.com/api/webhooks")
    {
        // Discord messages are limited to 2000 characters.
        json!({ "content": truncate(&text.replacen('*', "**", 2), 1900) })
    } else {
        json!({
            "source": "zesty-backup",
            "server": server,
            "event": event.kind.as_str(),
            "success": event.success,
            "title": title,
            "summary": event.summary,
            "details": event.details,
            "text": text,
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthchecks_urls() {
        assert_eq!(
            healthchecks_url("https://hc-ping.com/abc/", true),
            "https://hc-ping.com/abc"
        );
        assert_eq!(
            healthchecks_url("https://hc-ping.com/abc", false),
            "https://hc-ping.com/abc/fail"
        );
        assert_eq!(
            healthchecks_url("https://hc-ping.com/key/slug?create=1", false),
            "https://hc-ping.com/key/slug/fail?create=1"
        );
        assert_eq!(
            host_of("https://hooks.slack.com/services/SECRET"),
            "hooks.slack.com"
        );
    }

    #[test]
    fn payload_formats() {
        let event = Event::failure(EventKind::Upload, &anyhow::anyhow!("boom"));
        let slack = webhook_payload(
            "https://hooks.slack.com/services/x",
            "T",
            "B",
            "srv",
            &event,
        );
        assert_eq!(slack, json!({ "text": "*T*\nB" }));
        let discord = webhook_payload(
            "https://discord.com/api/webhooks/1/x",
            "T",
            "B",
            "srv",
            &event,
        );
        assert_eq!(discord, json!({ "content": "**T**\nB" }));
        let generic = webhook_payload("https://example.com/hook", "T", "B", "srv", &event);
        assert_eq!(generic["event"], "upload");
        assert_eq!(generic["success"], false);
        assert_eq!(generic["details"], "boom");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
