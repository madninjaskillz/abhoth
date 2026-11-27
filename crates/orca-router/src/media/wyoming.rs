//! A Wyoming client for speech to text (whisper has no HTTP API): `transcribe`, `audio-start`,
//! `audio-chunk`s, `audio-stop`, then wait for `transcript`. An event is a JSON header line, then
//! `data_length` bytes of JSON data and `payload_length` bytes of payload.

use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// One second of 16 kHz 16-bit mono audio per chunk.
const CHUNK: usize = 32_000;
const MAX_LINE: usize = 1 << 20;

async fn write_event<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    kind: &str,
    data: Value,
    payload: &[u8],
) -> std::io::Result<()> {
    let data = data.to_string().into_bytes();
    let mut head = json!({"type": kind, "data_length": data.len()});
    if !payload.is_empty() {
        head["payload_length"] = json!(payload.len());
    }
    let mut line = head.to_string().into_bytes();
    line.push(b'\n');
    w.write_all(&line).await?;
    w.write_all(&data).await?;
    w.write_all(payload).await
}

/// One event read: type and data (inline `data` merged with the `data_length` bytes).
async fn read_event<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<(String, Value), String> {
    let mut line = Vec::new();
    let n = (&mut *r)
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("the engine closed the connection".into());
    }
    let head: Value =
        serde_json::from_slice(&line).map_err(|e| format!("a bad Wyoming header: {e}"))?;
    let kind = head["type"].as_str().unwrap_or_default().to_string();
    let mut data: Map<String, Value> = head
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let dl = head["data_length"].as_u64().unwrap_or(0) as usize;
    if dl > 0 {
        if dl > MAX_LINE {
            return Err("a Wyoming event's data is too large".into());
        }
        let mut b = vec![0u8; dl];
        r.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&b) {
            data.extend(m);
        }
    }
    let pl = head["payload_length"].as_u64().unwrap_or(0);
    if pl > 0 {
        tokio::io::copy(&mut (&mut *r).take(pl), &mut tokio::io::sink())
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok((kind, Value::Object(data)))
}

/// Transcribe 16-bit mono `pcm` at `rate` on the Wyoming engine at `addr` (`host:port`).
pub async fn transcribe(
    addr: &str,
    pcm: &[u8],
    rate: u32,
    language: Option<&str>,
    patience: Duration,
) -> Result<String, String> {
    let work = async {
        let s = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
            .await
            .map_err(|_| format!("{addr} did not accept the connection"))?
            .map_err(|e| format!("{addr}: {e}"))?;
        let (rd, mut wr) = s.into_split();
        let mut rd = BufReader::new(rd);
        let fmt = json!({"rate": rate, "width": 2, "channels": 1});
        let mut t = json!({});
        if let Some(l) = language {
            t["language"] = json!(l);
        }
        let io = |e: std::io::Error| format!("{addr}: {e}");
        write_event(&mut wr, "transcribe", t, &[])
            .await
            .map_err(io)?;
        write_event(&mut wr, "audio-start", fmt.clone(), &[])
            .await
            .map_err(io)?;
        for c in pcm.chunks(CHUNK) {
            write_event(&mut wr, "audio-chunk", fmt.clone(), c)
                .await
                .map_err(io)?;
        }
        write_event(&mut wr, "audio-stop", json!({}), &[])
            .await
            .map_err(io)?;
        wr.flush().await.map_err(io)?;
        let mut chunks = String::new();
        loop {
            let (kind, data) = read_event(&mut rd).await?;
            match kind.as_str() {
                "transcript" => {
                    return Ok(data["text"].as_str().unwrap_or_default().trim().to_string());
                }
                // streaming engines (Wyoming 1.7): chunks, then a stop
                "transcript-chunk" => chunks.push_str(data["text"].as_str().unwrap_or_default()),
                "transcript-stop" => return Ok(chunks.trim().to_string()),
                "error" => {
                    return Err(format!(
                        "{addr}: {}",
                        data["text"].as_str().unwrap_or("an error")
                    ));
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(patience, work)
        .await
        .map_err(|_| format!("{addr} gave no transcript in time"))?
}

/// A Wyoming engine for tests: answers every `audio-stop` with `transcript` = `text` followed by
/// the number of audio bytes it got.
#[cfg(test)]
pub async fn fake_engine(text: &'static str) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (rd, mut wr) = s.into_split();
                let mut rd = BufReader::new(rd);
                let mut got = 0usize;
                // payloads are read and counted here (not skipped)
                loop {
                    let mut line = Vec::new();
                    if rd.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let head: Value = serde_json::from_slice(&line).unwrap();
                    let dl = head["data_length"].as_u64().unwrap_or(0) as usize;
                    let mut d = vec![0u8; dl];
                    rd.read_exact(&mut d).await.unwrap();
                    let pl = head["payload_length"].as_u64().unwrap_or(0) as usize;
                    let mut p = vec![0u8; pl];
                    rd.read_exact(&mut p).await.unwrap();
                    got += pl;
                    if head["type"] == "audio-stop" {
                        // the old style: data inline in the header
                        let ev = json!({"type": "transcript", "data": {"text": format!("{text} {got}")}});
                        let mut b = ev.to_string().into_bytes();
                        b.push(b'\n');
                        wr.write_all(&b).await.unwrap();
                        return;
                    }
                }
            });
        }
    });
    addr
}
