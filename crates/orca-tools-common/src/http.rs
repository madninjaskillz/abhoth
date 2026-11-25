//! `HttpHelper` (the shared HTTP client of the web tools) and `DomainRateLimiter` (a minimum
//! delay between requests to the same host). Behind the `http` feature.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// `HttpHelper.Client`: 30 s timeout, `User-Agent: OpenOrca/<version>`, and an Accept header
/// for HTML, JSON and plain text.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/html, application/json, text/plain"),
        );
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(format!("OpenOrca/{}", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .build()
            .expect("the HTTP client builds")
    });
    &CLIENT
}

/// `DomainRateLimiter.MinDelay`.
pub const MIN_DELAY: Duration = Duration::from_millis(1500);

type Slot = Arc<tokio::sync::Mutex<Option<Instant>>>;
static DOMAINS: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(Default::default);

/// `DomainRateLimiter.ThrottleAsync`: waits until [`MIN_DELAY`] has passed since the last
/// request to the same host. URLs that do not parse are not throttled.
pub async fn throttle(url: &str) {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return;
    };
    let domain = parsed.host_str().unwrap_or("").to_lowercase();
    let slot = DOMAINS
        .lock()
        .unwrap()
        .entry(domain)
        .or_insert_with(Default::default)
        .clone();
    let mut last = slot.lock().await;
    if let Some(t) = *last {
        let elapsed = t.elapsed();
        if elapsed < MIN_DELAY {
            tokio::time::sleep(MIN_DELAY - elapsed).await;
        }
    }
    *last = Some(Instant::now());
}
