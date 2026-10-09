//! Healthchecks.io pings: `<url>/start` before a run, `<url>` with a summary
//! after success, `<url>/fail` with the error otherwise.
//! https://healthchecks.io/docs/http_api/
//!
//! Pings never fail the run: errors are logged and the run goes on.

use std::time::Duration;

use proxmox_http::Body;
use proxmox_http::client::Client;

const TIMEOUT: Duration = Duration::from_secs(10);
const ATTEMPTS: u32 = 3;
/// Healthchecks keeps the first 100 kB of a request body.
const MAX_BODY: usize = 100_000;

pub struct Healthchecks {
    client: Client,
    url: String,
    /// Run ID, pairs the start ping with its success/fail ping.
    rid: String,
}

impl Healthchecks {
    pub fn new(url: &str) -> Self {
        Self {
            client: Client::new(),
            url: url.trim_end_matches('/').to_string(),
            rid: proxmox_uuid::Uuid::generate().to_string(),
        }
    }

    pub async fn start(&self) {
        self.ping("/start", String::new()).await
    }

    pub async fn success(&self, message: &str) {
        self.ping("", truncate(message)).await
    }

    pub async fn fail(&self, message: &str) {
        self.ping("/fail", truncate(message)).await
    }

    fn ping_url(&self, suffix: &str) -> String {
        format!("{}{suffix}?rid={}", self.url, self.rid)
    }

    async fn ping(&self, suffix: &str, body: String) {
        let url = self.ping_url(suffix);
        for attempt in 1..=ATTEMPTS {
            let request = self.client.post(
                &url,
                Some(Body::from(body.clone())),
                Some("text/plain; charset=utf-8"),
                None,
            );
            let error = match tokio::time::timeout(TIMEOUT, request).await {
                Ok(Ok(resp)) if resp.status().is_success() => return,
                Ok(Ok(resp)) => format!("HTTP {}", resp.status()),
                Ok(Err(err)) => format!("{err:#}"),
                Err(_) => "timed out".to_string(),
            };
            // The URL holds the check's secret, don't log it.
            log::warn!("healthchecks ping{suffix} failed (attempt {attempt}/{ATTEMPTS}): {error}");
            if attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(attempt.into())).await;
            }
        }
    }
}

fn truncate(message: &str) -> String {
    if message.len() <= MAX_BODY {
        return message.to_string();
    }
    let mut end = MAX_BODY;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_urls() {
        let hc = Healthchecks::new("https://hc-ping.com/uuid/");
        assert_eq!(
            hc.ping_url("/start"),
            format!("https://hc-ping.com/uuid/start?rid={}", hc.rid)
        );
        assert_eq!(
            hc.ping_url(""),
            format!("https://hc-ping.com/uuid?rid={}", hc.rid)
        );
    }

    #[test]
    fn truncate_at_char_boundary() {
        let s = "é".repeat(MAX_BODY); // 2 bytes each
        let t = truncate(&s);
        assert!(t.len() <= MAX_BODY && t.len() > MAX_BODY - 2);
        assert_eq!(truncate("short"), "short");
    }
}
