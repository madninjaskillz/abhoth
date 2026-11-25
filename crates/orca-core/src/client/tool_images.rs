//! Images in requests to an OpenAI-compatible server (`ToolImagesChatClient`, `ToolResultImages`
//! and `VisionSupport` in C#).
//!
//! A tool message there is text only, so the images a tool returned (Pi `read` on a .png, MCP
//! image blocks) move into a user message right after the run of tool messages, as `image_url`
//! parts with a base64 data URI. Only the newest images are sent; older ones become a one-line
//! note, so a long run doesn't fill the window with pictures. A server that takes no images
//! (vision unknown or off) gets a note instead of the image, and a prompt's attached images
//! become lines naming their files.
//!
//! Vision comes from `lmStudio.vision` when set, else from the server: llama.cpp / Strata
//! `/props` `modalities.vision`, `/v1/models` `input_modalities`, or LM Studio's
//! `/api/v0/models` type "vlm" (probed once per server for the process).
//!
//! A server that refuses an image request (400 "no images", ninfer `vision_disabled`) is
//! remembered as taking none for the rest of the process, and the request is sent once more
//! with notes in place of the images ([`set_on_vision_refused`] tells the user once).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use orca_abstractions::{ToolResult, ToolResultPart};
use regex::Regex;
use serde_json::Value;

use super::openai::ChatError;
use super::types::{ChatMessage, ContentPart, ImagePart, Role};
use crate::config::OrcaConfig;

/// Images kept in a request (the newest ones): at most [`MAX_IMAGES`], at least
/// [`KEEP_IMAGES`] once there are more.
pub const KEEP_IMAGES: usize = 4;

/// Images a request may carry before the oldest are dropped. They are dropped in steps (from
/// `MAX_IMAGES + 1` down to `KEEP_IMAGES`), not one per new image: turning an old image into a
/// note changes the prompt at that image, so the server re-reads everything after it.
pub const MAX_IMAGES: usize = 8;

pub const IMAGES_INTRO: &str = "Image(s) returned by the tool call(s) above:";
pub const DROPPED_NOTE: &str =
    "[An earlier tool result had an image here; only the newest images are kept.]";
pub const NO_VISION_NOTE: &str =
    "[The tool result had an image, but this model server does not take images.]";
/// `{0}` is the attachment's path.
pub const NO_VISION_USER_NOTE: &str =
    "[Attached image {0}: this model server does not take images; use your tools on the file.]";

/// The Anthropic API's per-image limit (5 MB), `ToolResultImages.MaxBytes`.
pub const MAX_TOOL_IMAGE_BYTES: usize = 5 * 1024 * 1024;

const TOOL_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// `ToolResultImages.From`: the image parts of a tool result that go to the model, next to the
/// result in the tool message. None when `lmStudio.vision` is false (OpenAI-compatible), for an
/// error, or for a part that is too big or a format models don't take.
pub fn tool_result_images(result: &ToolResult, config: &OrcaConfig) -> Vec<ImagePart> {
    if !config.lm_studio.tool_images() || result.is_error {
        return Vec::new();
    }
    let Some(parts) = &result.parts else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|p| match p {
            ToolResultPart::Image { data, mime_type }
                if !data.is_empty()
                    && data.len() <= MAX_TOOL_IMAGE_BYTES
                    && TOOL_IMAGE_TYPES
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(mime_type)) =>
            {
                Some(ImagePart::new(data.clone(), mime_type.clone()))
            }
            _ => None,
        })
        .collect()
}

/// How many of the oldest of `total` images are not sent; changes only every few images.
pub fn images_to_skip(total: usize) -> usize {
    if total <= MAX_IMAGES {
        return 0;
    }
    let step = MAX_IMAGES - KEEP_IMAGES + 1;
    ((total - MAX_IMAGES - 1) / step + 1) * step
}

fn is_image(p: &ContentPart) -> bool {
    matches!(p, ContentPart::Image(i) if i.media_type.len() >= 6 && i.media_type[..6].eq_ignore_ascii_case("image/"))
}

/// Tool-result images, or images the user attached to a prompt (`HasImages`).
pub fn has_images(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .any(|m| m.role != Role::System && m.parts.iter().any(is_image))
}

/// `HasToolImages`.
pub fn has_tool_images(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .any(|m| m.role == Role::Tool && m.parts.iter().any(is_image))
}

/// The messages with tool images moved into a user message after each run of tool messages.
/// Without vision, or past the newest [`KEEP_IMAGES`], an image becomes a note; without vision
/// a prompt's attached images become lines naming their files.
pub fn rewrite(messages: &[ChatMessage], vision: bool) -> Vec<ChatMessage> {
    let total: usize = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.parts.iter().filter(|p| is_image(p)).count())
        .sum();
    let skip = images_to_skip(total);
    let mut seen = 0;

    let mut output = Vec::with_capacity(messages.len() + 4);
    let mut pending: Vec<ContentPart> = Vec::new();
    let flush = |output: &mut Vec<ChatMessage>, pending: &mut Vec<ContentPart>| {
        if !pending.is_empty() {
            let mut m = ChatMessage::user(IMAGES_INTRO);
            m.parts = std::mem::take(pending);
            output.push(m);
        }
    };

    for message in messages {
        if message.role != Role::Tool {
            flush(&mut output, &mut pending);
            output.push(
                if !vision && message.role == Role::User && message.parts.iter().any(is_image) {
                    without_user_images(message)
                } else {
                    message.clone()
                },
            );
            continue;
        }
        if !message.parts.iter().any(is_image) {
            output.push(message.clone());
            continue;
        }

        let mut kept = Vec::new();
        let mut notes: Vec<&str> = Vec::new();
        for part in &message.parts {
            if !is_image(part) {
                kept.push(part.clone());
                continue;
            }
            seen += 1;
            if !vision {
                notes.push(NO_VISION_NOTE);
            } else if seen <= skip {
                notes.push(DROPPED_NOTE);
            } else {
                pending.push(part.clone());
            }
        }
        let mut copy = message.clone();
        copy.parts = kept;
        if !notes.is_empty() {
            let mut distinct: Vec<&str> = Vec::new();
            for n in notes {
                if !distinct.contains(&n) {
                    distinct.push(n);
                }
            }
            let result = copy.content.take().unwrap_or_default();
            copy.content = Some(format!("{result}\n{}", distinct.join("\n")));
        }
        output.push(copy);
    }
    flush(&mut output, &mut pending);
    output
}

/// A prompt's attached images for a server that takes none: each becomes a line naming the
/// file (the attachment's path is its `name`), so the model can still use its tools on it.
fn without_user_images(message: &ChatMessage) -> ChatMessage {
    let mut copy = message.clone();
    copy.parts = message
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Image(i) if is_image(p) => ContentPart::Text(
                NO_VISION_USER_NOTE.replace("{0}", i.name.as_deref().unwrap_or("(unsaved)")),
            ),
            other => other.clone(),
        })
        .collect();
    copy
}

// ── vision support ──────────────────────────────────────────────────────

fn server_key(url: &str) -> String {
    url.trim_end_matches('/').to_lowercase()
}

/// Servers that refused an image request: no images for them, whatever the config or the
/// probe said (key: lowercased URL without a trailing '/'; value: the refusal's first line).
static REFUSED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

type ProbeCell = Arc<tokio::sync::OnceCell<bool>>;
static PROBES: LazyLock<Mutex<HashMap<String, ProbeCell>>> = LazyLock::new(Default::default);

/// The `OnVisionRefused` hook: gets the line for the user.
pub type VisionRefusedCallback = Arc<dyn Fn(&str) + Send + Sync>;
static ON_VISION_REFUSED: LazyLock<RwLock<Option<VisionRefusedCallback>>> =
    LazyLock::new(Default::default);

/// `ToolImagesChatClient.OnVisionRefused`: called once per server when it refuses images, with
/// a line for the user (plain text). `None` = say nothing (the model still gets the note).
pub fn set_on_vision_refused(callback: Option<VisionRefusedCallback>) {
    *ON_VISION_REFUSED.write().unwrap_or_else(|e| e.into_inner()) = callback;
}

/// Whether `server_url` refused an image request earlier in this process.
pub fn has_refused_images(server_url: &str) -> bool {
    REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&server_key(server_url))
}

static REFUSAL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)vision_disabled|vision is (?:disabled|not (?:supported|enabled|available))|image input is not supported|does not support (?:image|vision)|images? (?:are|is) not supported|(?:model|server) is not multimodal|multimodal.{0,40}not (?:supported|enabled)",
    )
    .expect("static regex")
});

/// A message that says the server takes no images.
pub fn is_vision_refusal_message(message: &str) -> bool {
    REFUSAL_PATTERN.is_match(message)
}

/// `IsVisionRefusal`: a 400 / 415 / 422 that says the server takes no images (ninfer's
/// `vision_disabled`, llama.cpp without an mmproj, the usual gateway wordings). An error that
/// broke an already-started stream is not one.
pub fn is_vision_refusal(error: &ChatError) -> bool {
    match error {
        ChatError::Http { status, body, .. } => {
            matches!(status, 400 | 415 | 422) && is_vision_refusal_message(body)
        }
        ChatError::Interrupted { partial, message } => {
            partial.updates == 0 && is_vision_refusal_message(message)
        }
        other => is_vision_refusal_message(&other.to_string()),
    }
}

/// The first line of the error as the .NET OpenAI client words it
/// (`HTTP 400 (invalid_request_error: vision_disabled)`).
fn refusal_first_line(error: &ChatError) -> String {
    if let ChatError::Http { status, body, .. } = error {
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let err = parsed.as_ref().and_then(|v| v.get("error"));
        let kind = err.and_then(|e| e.get("type")).and_then(Value::as_str);
        let code = err.and_then(|e| e.get("code")).and_then(Value::as_str);
        return match (kind, code) {
            (Some(t), Some(c)) => format!("HTTP {status} ({t}: {c})"),
            (Some(t), None) => format!("HTTP {status} ({t})"),
            (None, Some(c)) => format!("HTTP {status} ({c})"),
            (None, None) => format!("HTTP {status}"),
        };
    }
    error
        .to_string()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("HTTP 400")
        .to_string()
}

/// `MarkRefused`: remember the server and tell the user once.
pub(crate) fn mark_refused(base_url: &str, configured: Option<bool>, error: &ChatError) {
    let first = refusal_first_line(error);
    {
        let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
        let key = server_key(base_url);
        if refused.contains_key(&key) {
            return;
        }
        refused.insert(key, first.clone());
    }
    let callback = ON_VISION_REFUSED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(cb) = callback {
        let mut text = format!(
            "This model server does not take images ({first}). The request was sent again without them, and images go to it as file paths for the rest of this session."
        );
        if configured == Some(true) {
            text.push_str(
                " lmStudio.vision is true in the config: set it to false (or remove it) for this server.",
            );
        }
        cb(&text);
    }
}

/// Whether the OpenAI-compatible server at `server_url` takes images: `configured`
/// (`lmStudio.vision`) when set, else what the server says (probed once per server for the
/// process; a failed probe counts as "no images"). A server that refused an image request in
/// this process takes none, whatever the config says.
pub async fn supports_vision(
    server_url: &str,
    key: Option<&str>,
    configured: Option<bool>,
) -> bool {
    if has_refused_images(server_url) {
        return false;
    }
    if let Some(set) = configured {
        return set;
    }
    let url = server_url.trim_end_matches('/').to_string();
    let cell = PROBES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(url.to_lowercase())
        .or_default()
        .clone();
    let key = key.map(str::to_string);
    *cell
        .get_or_init(|| async move { detect(&url, key.as_deref()).await })
        .await
}

/// `VisionSupport.ForMainModelAsync`: whether the main model takes images, for building a
/// prompt with attachments. Anthropic: yes. An OpenAI-compatible server: `lmStudio.vision`
/// when set, else what the server reports; unknown counts as no, so images go as file paths.
pub async fn for_main_model(config: &OrcaConfig) -> bool {
    let lm = &config.lm_studio;
    if lm.is_anthropic() {
        return true;
    }
    let key = lm.resolve_api_key();
    supports_vision(&lm.base_url, Some(&key), lm.vision).await
}

static PROBE_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client builds with static settings")
});

/// `DetectAsync`: asks the server whether it takes images; false when nothing says so.
pub async fn detect(base_url: &str, api_key: Option<&str>) -> bool {
    let mut v1 = base_url.trim_end_matches('/').to_string();
    let ends_v1 = v1.len() >= 3 && v1[v1.len() - 3..].eq_ignore_ascii_case("/v1");
    let root = if ends_v1 {
        v1[..v1.len() - 3].to_string()
    } else {
        v1.clone()
    };
    if !ends_v1 {
        v1.push_str("/v1");
    }
    for url in [
        format!("{root}/props"),
        format!("{v1}/models"),
        format!("{root}/api/v0/models"),
    ] {
        let mut req = PROBE_HTTP.get(&url);
        if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
            req = req.bearer_auth(key);
        }
        let Ok(response) = req.send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text().await else {
            continue;
        };
        if let Some(found) = parse_vision(&body) {
            return found;
        }
    }
    false
}

/// `ParseVision`: vision support from a /props, /v1/models or /api/v0/models body; `None` when
/// it doesn't say.
pub fn parse_vision(json: &str) -> Option<bool> {
    let r: Value = serde_json::from_str(json).ok()?;
    let r = r.as_object()?;
    if let Some(v) = r
        .get("modalities")
        .and_then(Value::as_object)
        .and_then(|m| m.get("vision"))
        .and_then(Value::as_bool)
    {
        return Some(v);
    }
    let data = r.get("data")?.as_array()?;
    let mut any = None;
    for model in data {
        let Some(model) = model.as_object() else {
            continue;
        };
        if let Some(inputs) = model
            .get("architecture")
            .and_then(Value::as_object)
            .and_then(|a| a.get("input_modalities"))
            .and_then(Value::as_array)
        {
            if inputs.iter().any(|i| i.as_str() == Some("image")) {
                return Some(true);
            }
            any = Some(false);
        } else if let Some(t) = model
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| *t == "vlm" || *t == "llm")
        {
            // LM Studio lists downloaded models too: only a loaded one counts.
            let not_loaded = model
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "loaded");
            if not_loaded {
                continue;
            }
            if t == "vlm" {
                return Some(true);
            }
            any = Some(false);
        }
    }
    any
}
