//! ESPHome `web_server` v3 backend: REST `POST /number/<name>/set?value=N` to push inputs,
//! SSE `GET /events` to read state, digest auth (SUPERVISOR.md 5.3, Appendix A.4).
//!
//! The device runs the curve and owns the fail-safe, so `fail_safe` and `release` do
//! nothing: the device sees no fresh temperature and goes to 100 % after `stale_seconds`.
//! The encrypted native API is a later transport (15 Q2).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode, header};

use super::{BackendError, Caps, ChannelReading, FailSafeOwner, FanBackend};
use crate::config::{BackendKind, Channel, EspNames, FanMode};
use crate::digest::{Challenge, md5_hex};
use crate::esp::{EspState, SseParser, parse_state_event};

pub struct EspHome {
    id: String,
    base: String,
    user: String,
    password: String,
    pub names: EspNames,
    channels: Vec<u32>,
    client: reqwest::Client,
    auth: Mutex<Option<(Challenge, u32)>>,
    /// Latest SSE state, shared with [`EspHome::run_events`].
    pub state: Arc<Mutex<EspState>>,
    started: std::time::Instant,
}

/// Percent-encodes an entity name for the URL path (ESPHome matches on the name).
pub fn encode_name(name: &str) -> String {
    let mut s = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

impl EspHome {
    pub fn new(
        id: &str,
        base: &str,
        user: &str,
        password: &str,
        names: EspNames,
        channels: Vec<u32>,
    ) -> EspHome {
        EspHome {
            id: id.into(),
            base: base.trim_end_matches('/').into(),
            user: user.into(),
            password: password.into(),
            names,
            channels,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
            auth: Mutex::new(None),
            state: Arc::new(Mutex::new(EspState::default())),
            started: std::time::Instant::now(),
        }
    }

    fn cnonce(&self, nc: u32) -> String {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        md5_hex(&format!("{t}:{nc}:{:p}", self))[..16].to_string()
    }

    fn authorization(&self, method: &str, uri: &str) -> Option<String> {
        let mut g = self.auth.lock().unwrap_or_else(|e| e.into_inner());
        let (c, nc) = g.as_mut()?;
        *nc += 1;
        let n = *nc;
        let c = c.clone();
        drop(g);
        Some(c.authorization(&self.user, &self.password, method, uri, n, &self.cnonce(n)))
    }

    /// Sends one request, answering a digest challenge once. The challenge is cached and
    /// reused (nc counting up); a 401 on a cached nonce re-challenges.
    async fn request(
        &self,
        method: Method,
        uri: &str,
        stream: bool,
    ) -> Result<reqwest::Response, BackendError> {
        let url = format!("{}{}", self.base, uri);
        for attempt in 0..2 {
            let mut rb = self.client.request(method.clone(), &url);
            if method == Method::POST {
                // ESPHome answers 411 without a Content-Length.
                rb = rb
                    .header(header::CONTENT_LENGTH, "0")
                    .body(Vec::<u8>::new());
            }
            if stream {
                rb = rb.timeout(std::time::Duration::from_secs(24 * 3600));
            }
            if let Some(a) = self.authorization(method.as_str(), uri) {
                rb = rb.header(header::AUTHORIZATION, a);
            }
            let resp = rb
                .send()
                .await
                .map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))?;
            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let ch = resp
                    .headers()
                    .get_all(header::WWW_AUTHENTICATE)
                    .iter()
                    .filter_map(|h| h.to_str().ok())
                    .find_map(Challenge::parse)
                    .ok_or_else(|| {
                        BackendError::Io(format!("{}: 401 without a digest challenge", self.id))
                    })?;
                *self.auth.lock().unwrap_or_else(|e| e.into_inner()) = Some((ch, 0));
                continue;
            }
            if !resp.status().is_success() {
                return Err(BackendError::Io(format!(
                    "{}: {} {}",
                    self.id,
                    method,
                    resp.status()
                )));
            }
            return Ok(resp);
        }
        Err(BackendError::Io(format!(
            "{}: authentication refused",
            self.id
        )))
    }

    async fn set_number(&self, name: &str, value: &str) -> Result<(), BackendError> {
        let uri = format!("/number/{}/set?value={}", encode_name(name), value);
        self.request(Method::POST, &uri, false).await.map(|_| ())
    }

    /// Reads `/events` until the connection ends, updating [`EspHome::state`]. GET only.
    /// The caller loops it with a back-off.
    pub async fn run_events(&self) -> Result<(), BackendError> {
        let resp = self.request(Method::GET, "/events", true).await?;
        let mut body = resp.bytes_stream();
        let mut parser = SseParser::default();
        let mut buf = Vec::<u8>::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| BackendError::Io(format!("{}: {e}", self.id)))?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if let Some((ev, data)) = parser.line(&line)
                    && ev == "state"
                    && let Some((name, v)) = parse_state_event(&data)
                {
                    let now = self.started.elapsed().as_secs_f64();
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .apply(name, v, now);
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl FanBackend for EspHome {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Esphome
    }
    fn caps(&self) -> Caps {
        Caps {
            mode: FanMode::DeviceCurve,
            writable: true,
            reads_rpm: true,
            failsafe: FailSafeOwner::Device,
        }
    }

    async fn push_inputs(
        &self,
        ch: &Channel,
        temp: &str,
        load: Option<&str>,
    ) -> Result<(), BackendError> {
        let n = ch
            .index()
            .ok_or_else(|| BackendError::NoChannel(ch.to_string()))?;
        if let Some(l) = load {
            self.set_number(&EspNames::for_channel(&self.names.load, n), l)
                .await?;
        }
        self.set_number(&EspNames::for_channel(&self.names.temp, n), temp)
            .await
    }

    async fn set_setting(&self, entity: &str, value: f64) -> Result<(), BackendError> {
        if !crate::esp::SETTING_ENTITIES
            .iter()
            .any(|(n, _, _)| *n == entity)
        {
            return Err(BackendError::Io(format!(
                "{entity:?} is not a device setting"
            )));
        }
        self.set_number(entity, &format!("{value}")).await
    }

    async fn read(&self) -> Result<Vec<ChannelReading>, BackendError> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(self
            .channels
            .iter()
            .map(|&n| {
                let c = st.channel(&self.names, n);
                let mut extra = Vec::new();
                if let Some(f) = c.ff_duty {
                    extra.push(("ff_duty".into(), format!("{f}")));
                }
                ChannelReading {
                    channel: n.to_string(),
                    rpm: c.rpm,
                    duty: c.duty,
                    failsafe: c.failsafe,
                    stalled: c.stalled,
                    extra,
                }
            })
            .collect())
    }

    async fn fail_safe(&self, _ch: &Channel) -> Result<(), BackendError> {
        Ok(())
    }

    async fn release(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn names_encode() {
        assert_eq!(encode_name("GPU 0 Temp"), "GPU%200%20Temp");
        assert_eq!(encode_name("FF Max Duty"), "FF%20Max%20Duty");
    }

    /// A tiny fake ESP: answers 401 with a digest challenge unless the Authorization header
    /// carries the right response; records the request lines.
    async fn fake_esp(
        expect_user: &'static str,
        expect_pw: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let log = log2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or("").to_string();
                    let mut parts = first.split(' ');
                    let method = parts.next().unwrap_or("").to_string();
                    let uri = parts.next().unwrap_or("").to_string();
                    let auth = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .map(|l| l.split_once(':').unwrap().1.trim().to_string());
                    let ok = auth.as_deref().is_some_and(|a| {
                        let get = |k: &str| {
                            a.split(", ")
                                .find_map(|p| p.strip_prefix(&format!("{k}=")))
                                .map(|v| v.trim_matches('"').to_string())
                                .or_else(|| {
                                    a.split(' ')
                                        .find_map(|p| p.strip_prefix(&format!("{k}=")))
                                        .map(|v| {
                                            v.trim_matches(|c| c == '"' || c == ',').to_string()
                                        })
                                })
                        };
                        let ha1 = md5_hex(&format!("{expect_user}:asyncesp:{expect_pw}"));
                        let ha2 = md5_hex(&format!("{method}:{uri}"));
                        let resp = md5_hex(&format!(
                            "{ha1}:{}:{}:{}:auth:{ha2}",
                            get("nonce").unwrap_or_default(),
                            get("nc").unwrap_or_default(),
                            get("cnonce").unwrap_or_default()
                        ));
                        get("response").as_deref() == Some(resp.as_str())
                    });
                    log.lock()
                        .unwrap()
                        .push(format!("{method} {uri} auth={ok}"));
                    let out = if !ok {
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"asyncesp\", qop=\"auth\", nonce=\"6f2a\", opaque=\"0b1c\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if uri == "/events" {
                        let body = "event: state\r\ndata: {\"id\":\"sensor/Fan 1 RPM\",\"name\":\"Fan 1 RPM\",\"value\":2400,\"state\":\"2400 RPM\"}\r\n\r\nevent: state\r\ndata: {\"id\":\"binary_sensor/Failsafe Active\",\"name\":\"Failsafe Active\",\"value\":false,\"state\":\"OFF\"}\r\n\r\n";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = s.write_all(out.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), log)
    }

    #[tokio::test]
    async fn pushes_load_then_temp_with_digest_auth() {
        let (url, log) = fake_esp("fans", "pw").await;
        let esp = EspHome::new(
            "esp",
            &url,
            "fans",
            "pw",
            EspNames::default(),
            vec![0, 1, 2],
        );
        esp.push_inputs(&Channel::Index(1), "61.3", Some("42.0"))
            .await
            .unwrap();
        esp.push_inputs(&Channel::Index(1), "61.4", None)
            .await
            .unwrap();
        let l = log.lock().unwrap().clone();
        assert_eq!(
            l,
            vec![
                "POST /number/GPU%201%20Power/set?value=42.0 auth=false",
                "POST /number/GPU%201%20Power/set?value=42.0 auth=true",
                // The cached challenge is reused (nc counts up): no second 401.
                "POST /number/GPU%201%20Temp/set?value=61.3 auth=true",
                "POST /number/GPU%201%20Temp/set?value=61.4 auth=true",
            ]
        );
    }

    #[tokio::test]
    async fn wrong_password_is_an_error_and_events_are_read() {
        let (url, _) = fake_esp("fans", "pw").await;
        let bad = EspHome::new("esp", &url, "fans", "nope", EspNames::default(), vec![0]);
        assert!(
            bad.push_inputs(&Channel::Index(0), "40.0", None)
                .await
                .is_err()
        );
        let esp = EspHome::new("esp", &url, "fans", "pw", EspNames::default(), vec![0, 1]);
        esp.run_events().await.unwrap();
        let r = esp.read().await.unwrap();
        assert_eq!(r[1].rpm, Some(2400.0));
        assert_eq!(r[0].failsafe, Some(false));
        // No temp channel 5 → NoChannel.
        assert!(matches!(
            esp.push_inputs(&Channel::Name("x".into()), "1", None).await,
            Err(BackendError::NoChannel(_))
        ));
    }
}
