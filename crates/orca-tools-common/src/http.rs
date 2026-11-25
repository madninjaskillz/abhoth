//! `HttpHelper` (the shared HTTP client for web tools) and `DomainRateLimiter`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

/// `HttpHelper.Client`: one long-lived client, 30 s timeout, `User-Agent: OpenOrca/<version>`
/// and `Accept: text/html, application/json, text/plain`.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/html, application/json, text/plain"),
        );
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("OpenOrca/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .build()
            .expect("HTTP client builds")
    })
}

/// `DomainRateLimiter`: at least [`min_delay`] between requests to the same host (lowercase),
/// across all tools and tasks.
struct Limiter {
    min_delay: Duration,
    hosts: HashMap<String, Arc<tokio::sync::Mutex<Option<Instant>>>>,
}

fn limiter() -> &'static Mutex<Limiter> {
    static LIMITER: OnceLock<Mutex<Limiter>> = OnceLock::new();
    LIMITER.get_or_init(|| {
        Mutex::new(Limiter {
            min_delay: Duration::from_millis(1500),
            hosts: HashMap::new(),
        })
    })
}

/// `DomainRateLimiter.MinDelay` (default 1.5 s).
pub fn min_delay() -> Duration {
    limiter().lock().unwrap().min_delay
}

/// Sets `DomainRateLimiter.MinDelay`.
pub fn set_min_delay(delay: Duration) {
    limiter().lock().unwrap().min_delay = delay;
}

/// `DomainRateLimiter.ThrottleAsync`: waits until the host's window has passed, then records
/// this request. URLs that do not parse are not throttled.
pub async fn throttle(url: &str) {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return;
    };
    let Some(host) = parsed.host_str() else {
        return;
    };
    let host = host.to_lowercase();
    let (slot, delay) = {
        let mut l = limiter().lock().unwrap();
        let delay = l.min_delay;
        (l.hosts.entry(host).or_default().clone(), delay)
    };
    let mut last = slot.lock().await;
    if let Some(t) = *last {
        let elapsed = t.elapsed();
        if elapsed < delay {
            tokio::time::sleep(delay - elapsed).await;
        }
    }
    *last = Some(Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn throttle_spaces_requests_to_one_host() {
        set_min_delay(Duration::from_millis(1500));
        let start = Instant::now();
        throttle("http://rate-limit-test.invalid/a").await;
        throttle("http://other-host.invalid/").await;
        assert!(start.elapsed() < Duration::from_millis(500));
        throttle("http://RATE-LIMIT-TEST.invalid/b").await;
        assert!(start.elapsed() >= Duration::from_millis(1400));
        // Not a URL: no throttling.
        throttle("not a url").await;
    }
}
