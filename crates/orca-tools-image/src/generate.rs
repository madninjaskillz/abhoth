//! `generate_image` and `edit_image`: the OpenAI-compatible images API
//! (`/images/generations`, `/images/edits`), saving the returned `b64_json` image.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_core::config::{AgentUncensoredConfig, ImageGenerationConfig};
use orca_tools_common::required_str;
use serde_json::{Value, json};

const DISABLED: &str =
    "Image generation is disabled. Enable it in /config \u{2192} Image Generation.";

/// The `/uncensored` switch, shared with the host so a flip applies to the next call.
pub type SharedUncensored = Arc<Mutex<AgentUncensoredConfig>>;

/// `AgentUncensoredConfig.ActiveImageModel`: the image model while the switch is on.
pub fn active_image_model(u: &AgentUncensoredConfig) -> Option<String> {
    if !u.is_on() {
        return None;
    }
    u.image_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// `AgentUncensoredConfig.ResolveImageModel`: the model an image request is sent with.
/// Without an uncensored model: the argument, else the configured one. With one: it replaces
/// the default, and an argument that only names the configured default; any other model
/// asked for is kept.
pub fn resolve_image_model(
    requested: Option<&str>,
    configured: Option<&str>,
    uncensored_model: Option<&str>,
) -> Option<String> {
    let Some(unc) = uncensored_model else {
        return requested.or(configured).map(str::to_string);
    };
    let Some(req) = requested.filter(|r| !r.trim().is_empty()) else {
        return Some(unc.to_string());
    };
    match configured {
        Some(c) if req.trim().eq_ignore_ascii_case(c.trim()) => Some(unc.to_string()),
        _ => Some(req.to_string()),
    }
}

/// `ImageGenerationConfig.ResolveOutputDirectory`.
pub fn resolve_output_directory(config: &ImageGenerationConfig) -> PathBuf {
    if config.output_directory.trim().is_empty() {
        orca_core::config::manager::user_profile()
            .join(".openorca")
            .join("images")
    } else {
        PathBuf::from(&config.output_directory)
    }
}

/// A string as `Utf8JsonWriter` writes it with the default `JavaScriptEncoder`: printable
/// ASCII except the HTML-sensitive characters as is, everything else `\uXXXX` (UTF-16 units).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\u0022"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' if !matches!(c, '<' | '>' | '&' | '\'' | '+' | '`') => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04X}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// `BuildRequestBody`: `{"prompt", "size"?, "model"?, "n": 1, "response_format": "b64_json"}`.
fn generation_body(prompt: &str, size: Option<&str>, model: Option<&str>) -> String {
    let mut body = format!("{{\"prompt\":{}", json_string(prompt));
    if let Some(size) = size {
        body.push_str(&format!(",\"size\":{}", json_string(size)));
    }
    if let Some(model) = model {
        body.push_str(&format!(",\"model\":{}", json_string(model)));
    }
    body.push_str(",\"n\":1,\"response_format\":\"b64_json\"}");
    body
}

/// `MakeSlug`: letters and digits lowercased, spaces as single underscores, at most
/// `max_len` characters, no trailing underscore.
fn make_slug(text: &str, max_len: usize) -> String {
    let mut sb = String::new();
    let mut len = 0;
    for c in text.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                sb.push(l);
            }
            len += 1;
        } else if c == ' ' && len > 0 && !sb.ends_with('_') {
            sb.push('_');
            len += 1;
        }
        if len >= max_len {
            break;
        }
    }
    sb.trim_end_matches('_').to_string()
}

fn timestamp() -> String {
    orca_core::session::DotNetDateTime::now().compact_stamp()
}

/// A string argument that may be absent or null.
fn opt_str<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

fn client(config: &ImageGenerationConfig) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_seconds.max(1) as u64))
        .build()
        .map_err(|e| e.to_string())
}

/// Send the request and read the image out of `data[0].b64_json`; `Err` is the tool's error.
async fn send_and_decode(
    config: &ImageGenerationConfig,
    request: reqwest::RequestBuilder,
    what: &str,
) -> Result<Vec<u8>, String> {
    let response = match request
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Err(format!(
                "Request timed out after {}s. Check that the image generation server is running at {}.",
                config.timeout_seconds, config.base_url
            ));
        }
        Err(e) => return Err(format!("HTTP request failed: {}", error_chain(&e))),
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Server returned {} {}: {body}",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        ));
    }
    let text = match response.text().await {
        Ok(t) => t,
        Err(e) if e.is_timeout() => {
            return Err(format!(
                "Request timed out after {}s. Check that the image generation server is running at {}.",
                config.timeout_seconds, config.base_url
            ));
        }
        Err(e) => return Err(format!("Failed to read response: {e}")),
    };
    let b64 = match serde_json::from_str::<Value>(&text) {
        Ok(doc) => match doc
            .get("data")
            .and_then(|d| d.get(0))
            .map(|d| d.get("b64_json"))
        {
            Some(Some(Value::String(s))) => Some(s.clone()),
            Some(Some(Value::Null)) => None,
            _ => {
                return Err(parse_failure(
                    "The given key was not present in the dictionary.",
                    &text,
                ));
            }
        },
        Err(e) => return Err(parse_failure(&e.to_string(), &text)),
    };
    let Some(b64) = b64.filter(|b| !b.trim().is_empty()) else {
        return Err(format!(
            "Server returned an empty b64_json field. The image {what} may have failed."
        ));
    };
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("Failed to decode base64 image data: {e}"))
}

fn parse_failure(message: &str, response: &str) -> String {
    let units: Vec<u16> = response.encode_utf16().take(500).collect();
    let head = String::from_utf16_lossy(&units);
    format!("Failed to parse image response: {message}. Response: {head}")
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

/// Write the image, creating its folder; `Err` is the tool's error.
fn save(output: &str, full: &Path, bytes: &[u8]) -> Result<(), String> {
    let result = (|| {
        if let Some(dir) = full.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(full, bytes)
    })();
    result.map_err(|e| format!("Failed to save image to '{output}': {e}"))
}

// ── generate_image ──

pub struct GenerateImageTool {
    pub config: ImageGenerationConfig,
    /// The `/uncensored` switch (`None`: a tool built without the app's config).
    pub uncensored: Option<SharedUncensored>,
}

impl GenerateImageTool {
    pub fn new(config: ImageGenerationConfig, uncensored: Option<SharedUncensored>) -> Self {
        Self { config, uncensored }
    }

    fn uncensored_model(&self) -> Option<String> {
        self.uncensored
            .as_ref()
            .and_then(|u| active_image_model(&u.lock().unwrap()))
    }

    /// The model a call without a `model` argument uses right now.
    pub fn effective_default_model(&self) -> Option<String> {
        self.uncensored_model()
            .or_else(|| self.config.model.clone())
    }
}

#[async_trait::async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        "generate_image"
    }
    fn description(&self) -> &str {
        "Generate an image from a text prompt using the configured image generation server (OpenAI-compatible /v1/images/generations API). Saves the result as a PNG file and returns its path. Always provide a full absolute save_path so the image is saved in the correct project location \u{2014} omitting it will save to a default folder unrelated to your current project."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn parameter_schema(&self) -> Value {
        // Read per request, so the schema names the model in use after a /uncensored flip.
        let model_desc = match self.effective_default_model() {
            Some(m) => format!(
                "Model to use for generation. Configured default: {m}. Omit to use this default."
            ),
            None => "Model name to use for generation. Omit to use the server default.".into(),
        };
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Text description of the image to generate"},
                "size": {"type": "string", "description": "Image dimensions supported by your server (e.g. 1024x1024). Omit to let the server use its default."},
                "model": {"type": "string", "description": model_desc},
                "save_path": {"type": "string", "description": "Full absolute file path where the image should be saved (e.g. C:/projects/mygame/assets/hero.png). Always provide this so the image ends up in your project \u{2014} omitting it saves to an unrelated default folder."}
            },
            "required": ["prompt"]
        })
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f3a8}",
            short_description: "Generate image",
            summary_arg_keys: &["prompt"],
            aliases: &[],
        })
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        if !self.config.enabled {
            return ToolResult::error(DISABLED);
        }
        let prompt = match required_str(args, "prompt") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let size = opt_str(args, "size");
        let save_path = opt_str(args, "save_path");
        // /uncensored on: its image model is the default, and replaces an argument that only
        // names imageGen.model.
        let uncensored_model = self.uncensored_model();
        let model = resolve_image_model(
            opt_str(args, "model"),
            self.config.model.as_deref(),
            uncensored_model.as_deref(),
        );

        let url = format!(
            "{}/images/generations",
            self.config.base_url.trim_end_matches('/')
        );
        let http = match client(&self.config) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("HTTP request failed: {e}")),
        };
        let request = http
            .post(url)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(generation_body(prompt, size, model.as_deref()));
        let bytes = match send_and_decode(&self.config, request, "generation").await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(e),
        };

        let output = match save_path.filter(|p| !p.trim().is_empty()) {
            Some(p) => p.to_string(),
            None => resolve_output_directory(&self.config)
                .join(format!("{}_{}.png", timestamp(), make_slug(prompt, 30)))
                .display()
                .to_string(),
        };
        if let Err(e) = save(&output, &ctx.full_path(&output), &bytes) {
            return ToolResult::error(e);
        }
        let mut text = format!("Image saved: {output}\nPrompt: {prompt}");
        if let Some(size) = size {
            text.push_str(&format!("\nSize: {size}"));
        }
        if uncensored_model.is_some()
            && let Some(m) = &model
        {
            text.push_str(&format!("\nModel: {m}"));
        }
        ToolResult::success(text)
    }
}

// ── edit_image ──

pub struct EditImageTool {
    pub config: ImageGenerationConfig,
}

impl EditImageTool {
    pub fn new(config: ImageGenerationConfig) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl Tool for EditImageTool {
    fn name(&self) -> &str {
        "edit_image"
    }
    fn description(&self) -> &str {
        "Edit an existing image using an AI prompt via the configured image generation server (OpenAI-compatible /v1/images/edits API). Provide the full absolute path to the source image and a description of the desired changes. Always provide a full absolute save_path so the result is saved in the correct project location \u{2014} omitting it will save to a default folder unrelated to your current project."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn parameter_schema(&self) -> Value {
        let model_desc = match &self.config.model {
            Some(m) => format!(
                "Model to use for editing. Configured default: {m}. Omit to use this default."
            ),
            None => "Model name to use for editing. Omit to use the server default.".into(),
        };
        json!({
            "type": "object",
            "properties": {
                "image_path": {"type": "string", "description": "Full absolute path to the source image file (PNG)"},
                "prompt": {"type": "string", "description": "Text description of the desired changes to make to the image"},
                "mask_path": {"type": "string", "description": "Full absolute path to an optional mask PNG. Transparent areas indicate where edits should be applied."},
                "size": {"type": "string", "description": "Output image dimensions supported by your server (optional). Omit to let the server decide."},
                "model": {"type": "string", "description": model_desc},
                "save_path": {"type": "string", "description": "Full absolute file path where the edited image should be saved. Always provide this so the image ends up in your project \u{2014} omitting it saves to an unrelated default folder."}
            },
            "required": ["image_path", "prompt"]
        })
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f3a8}",
            short_description: "Edit image",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        if !self.config.enabled {
            return ToolResult::error(DISABLED);
        }
        let (image_path, prompt) = match (
            required_str(args, "image_path"),
            required_str(args, "prompt"),
        ) {
            (Ok(i), Ok(p)) => (i, p),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(e),
        };
        let mask_path = opt_str(args, "mask_path");
        let size = opt_str(args, "size");
        let model = opt_str(args, "model")
            .map(str::to_string)
            .or_else(|| self.config.model.clone());
        let save_path = opt_str(args, "save_path");

        let image_full = ctx.full_path(image_path);
        if !image_full.is_file() {
            return ToolResult::error(format!("Image file not found: {image_path}"));
        }
        let mask_full = mask_path.map(|m| ctx.full_path(m));
        if let (Some(m), Some(full)) = (mask_path, &mask_full)
            && !full.is_file()
        {
            return ToolResult::error(format!("Mask file not found: {m}"));
        }

        let file_part = |full: &Path| -> Result<reqwest::multipart::Part, String> {
            let bytes = std::fs::read(full).map_err(|e| format!("HTTP request failed: {e}"))?;
            let name = full
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            reqwest::multipart::Part::bytes(bytes)
                .file_name(name)
                .mime_str("image/png")
                .map_err(|e| format!("HTTP request failed: {e}"))
        };
        let mut form = match file_part(&image_full) {
            Ok(p) => reqwest::multipart::Form::new().part("image", p),
            Err(e) => return ToolResult::error(e),
        };
        form = form
            .text("prompt", prompt.to_string())
            .text("n", "1")
            .text("response_format", "b64_json");
        if let Some(size) = size {
            form = form.text("size", size.to_string());
        }
        if let Some(model) = &model {
            form = form.text("model", model.clone());
        }
        if let Some(full) = &mask_full {
            match file_part(full) {
                Ok(p) => form = form.part("mask", p),
                Err(e) => return ToolResult::error(e),
            }
        }

        let url = format!(
            "{}/images/edits",
            self.config.base_url.trim_end_matches('/')
        );
        let http = match client(&self.config) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("HTTP request failed: {e}")),
        };
        let bytes =
            match send_and_decode(&self.config, http.post(url).multipart(form), "edit").await {
                Ok(b) => b,
                Err(e) => return ToolResult::error(e),
            };

        let output = match save_path.filter(|p| !p.trim().is_empty()) {
            Some(p) => p.to_string(),
            None => {
                let stem = Path::new(image_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                resolve_output_directory(&self.config)
                    .join(format!("{}_{stem}_edited.png", timestamp()))
                    .display()
                    .to_string()
            }
        };
        if let Err(e) = save(&output, &ctx.full_path(&output), &bytes) {
            return ToolResult::error(e);
        }
        ToolResult::success(format!(
            "Edited image saved: {output}\nSource: {image_path}\nPrompt: {prompt}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escapes_like_utf8_json_writer() {
        assert_eq!(json_string("a b"), "\"a b\"");
        assert_eq!(
            json_string("<&>'+`\"\\"),
            "\"\\u003C\\u0026\\u003E\\u0027\\u002B\\u0060\\u0022\\\\\""
        );
        assert_eq!(json_string("caf\u{e9}\n"), "\"caf\\u00E9\\n\"");
        assert_eq!(json_string("\u{1f600}"), "\"\\uD83D\\uDE00\"");
    }

    #[test]
    fn generation_body_has_csharp_key_order() {
        assert_eq!(
            generation_body("a landscape", None, Some("qwen-image")),
            "{\"prompt\":\"a landscape\",\"model\":\"qwen-image\",\"n\":1,\"response_format\":\"b64_json\"}"
        );
        assert_eq!(
            generation_body("x", Some("512x512"), None),
            "{\"prompt\":\"x\",\"size\":\"512x512\",\"n\":1,\"response_format\":\"b64_json\"}"
        );
    }

    #[test]
    fn make_slug_matches_csharp() {
        assert_eq!(make_slug("A Red  Fox, jumping!", 30), "a_red_fox_jumping");
        assert_eq!(make_slug("  lead", 30), "lead");
        assert_eq!(
            make_slug("abcdefghij abcdefghij abcdefghij", 30),
            "abcdefghij_abcdefghij_abcdefgh"
        );
        assert_eq!(make_slug("word ", 5), "word");
    }
}
