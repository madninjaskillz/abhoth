//! `POST /v1/audio/transcriptions` (OpenAI's speech to text, multipart): to a Wyoming engine
//! (whisper; the router speaks the protocol itself) or an engine with OpenAI's endpoint.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::json;

use super::multipart;
use super::place::{self, Want};
use super::send::{self, Patience, Prepared, Tried};
use crate::bridges::Upstream;
use crate::control::schema::{MediaApi, ModelKind};

#[derive(Debug, Clone)]
pub struct Ask {
    pub audio: Bytes,
    pub model: Option<String>,
    pub server: Option<String>,
    pub language: Option<String>,
    pub format: String,
}

pub fn parse(content_type: &str, body: &Bytes) -> Result<Ask, String> {
    let parts = multipart::parse(content_type, body)?;
    let text = |n: &str| {
        parts
            .iter()
            .find(|p| p.name == n)
            .map(|p| p.text())
            .filter(|s| !s.is_empty())
    };
    let audio = parts
        .iter()
        .find(|p| p.name == "file")
        .map(|p| p.data.clone())
        .filter(|d| !d.is_empty())
        .ok_or("the form has no file")?;
    let (model, server) = match text("model") {
        Some(m) => {
            let (m, s) = place::split_pin(&m);
            (Some(m.to_string()), s.map(str::to_string))
        }
        None => (None, None),
    };
    Ok(Ask {
        audio,
        model,
        server,
        language: text("language"),
        format: text("response_format").unwrap_or_else(|| "json".into()),
    })
}

/// The answer in the format asked for.
pub fn answer(
    text: &str,
    format: &str,
    seconds: f64,
    language: Option<&str>,
) -> axum::response::Response {
    match format {
        "text" => {
            let mut r = axum::response::Response::new(axum::body::Body::from(format!("{text}\n")));
            r.headers_mut().insert(
                "content-type",
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            r
        }
        "verbose_json" => crate::proxy::json_response(
            200,
            &json!({"task": "transcribe", "language": language.unwrap_or("en"),
                    "duration": (seconds * 1000.0).round() / 1000.0, "text": text, "segments": []}),
        ),
        _ => crate::proxy::json_response(200, &json!({"text": text})),
    }
}

pub async fn run(cx: &super::Cx<'_>, content_type: &str, raw: &Bytes) -> Tried {
    let fail = |status: u16, msg: &str| Tried {
        response: super::error(status, msg),
        target: None,
        failed: vec![],
    };
    let a = match parse(content_type, raw) {
        Ok(a) => a,
        Err(e) => return fail(400, &e),
    };
    if !matches!(a.format.as_str(), "json" | "text" | "verbose_json") {
        return fail(
            400,
            "response_format must be json, text or verbose_json here",
        );
    }
    let ts = place::candidates(
        cx.c,
        &Want {
            kinds: &[ModelKind::Stt],
            apis: &[MediaApi::Wyoming, MediaApi::OpenaiStt],
            model: a.model.as_deref(),
            server: a.server.as_deref(),
        },
        &cx.bridge_priority(),
    );
    let mut failed = Vec::new();
    let mut pcm: Option<Result<super::wav::Pcm, String>> = None;
    for t in &ts {
        match t.api {
            MediaApi::Wyoming => {
                // a Wyoming engine on a bridge: its raw TCP stream (13.1)
                let up = match crate::bridges::upstream_for(cx.c, &t.server, &t.engine, &t.card) {
                    Some(u) => u,
                    None => continue,
                };
                let p = pcm.get_or_insert_with(|| super::wav::decode(&a.audio, 16_000));
                let p = match p {
                    Ok(p) => p,
                    Err(e) => {
                        failed.push(format!("{}: {e}", t.key()));
                        continue;
                    }
                };
                let busy = place::begin(t);
                let patience = Duration::from_secs(300);
                let got = match &up {
                    Upstream::Http(u) => {
                        super::wyoming::transcribe(
                            u.trim_start_matches("http://"),
                            &p.data,
                            p.rate,
                            a.language.as_deref(),
                            patience,
                        )
                        .await
                    }
                    Upstream::Bridge {
                        bridge,
                        engine,
                        card,
                    } => match cx.hub {
                        Some(hub) => {
                            super::wyoming::transcribe_bridged(
                                hub,
                                (bridge, engine, card),
                                &p.data,
                                p.rate,
                                a.language.as_deref(),
                                patience,
                            )
                            .await
                        }
                        None => Err(format!("{}: no bridges here", t.key())),
                    },
                };
                drop(busy);
                match got {
                    Ok(text) => {
                        let mut r = answer(&text, &a.format, p.seconds(), a.language.as_deref());
                        send::mark(&mut r, t);
                        return Tried {
                            response: r,
                            target: Some(t.clone()),
                            failed,
                        };
                    }
                    Err(e) => {
                        tracing::info!("media: speech to text: {e}; trying the next");
                        place::cool(t);
                        failed.push(e);
                    }
                }
            }
            MediaApi::OpenaiStt => {
                let mut h = HeaderMap::new();
                if let Ok(v) = HeaderValue::from_str(content_type) {
                    h.insert("content-type", v);
                }
                let p = Prepared {
                    method: "POST".into(),
                    path_q: "/v1/audio/transcriptions".into(),
                    headers: h,
                    body: raw.clone(),
                };
                match send::send(
                    cx.http,
                    cx.hub,
                    cx.c,
                    t,
                    p,
                    Patience(Duration::from_secs(600)),
                )
                .await
                {
                    send::Sent::Answer(r) => {
                        return Tried {
                            response: r,
                            target: Some(t.clone()),
                            failed,
                        };
                    }
                    send::Sent::Retry(why, _) => {
                        place::cool(t);
                        failed.push(why);
                    }
                }
            }
            _ => {}
        }
    }
    // a file that is not WAV, with only Wyoming engines: the client's to fix
    if let Some(Err(e)) = &pcm
        && !ts.iter().any(|t| t.api == MediaApi::OpenaiStt)
    {
        return Tried {
            response: super::error(415, e),
            target: None,
            failed,
        };
    }
    let msg = if ts.is_empty() {
        "no engine serves speech to text here".to_string()
    } else {
        format!(
            "no engine for speech to text answered: {}",
            failed.join("; ")
        )
    };
    Tried {
        response: super::error(503, &msg),
        target: None,
        failed,
    }
}
