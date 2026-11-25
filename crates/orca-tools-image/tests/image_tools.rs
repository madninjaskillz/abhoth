//! Ported from `ImageGenerationToolTests` and `UncensoredImageModelTests`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_core::config::{AgentUncensoredConfig, ImageGenerationConfig};
use orca_testkit::FakeOpenAi;
use orca_tools_image::*;
use serde_json::{Value, json};

struct Dir(tempfile::TempDir);

impl Dir {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }

    fn ctx(&self) -> ToolContext {
        ToolContext::new(self.0.path())
    }

    /// A transparent RGBA PNG (`new Image<Rgba32>(w, h)`).
    fn create_test_image(&self, width: u32, height: u32) -> PathBuf {
        let path = self.path(&format!("test_{width}x{height}_{}.png", unique()));
        image::RgbaImage::new(width, height).save(&path).unwrap();
        path
    }

    /// A file with TIFF content (the test project in C# could still write TIFF).
    fn create_tiff(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        // Little-endian TIFF header and an empty IFD: detected as TIFF from the content.
        let mut bytes = b"II*\0\x08\0\0\0".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn p(path: &Path) -> String {
    path.display().to_string()
}

fn dims(path: &Path) -> (u32, u32) {
    image::image_dimensions(path).unwrap()
}

async fn run(tool: &dyn Tool, dir: &Dir, args: Value) -> ToolResult {
    tool.execute(&args, &dir.ctx()).await
}

// === GenerateImageTool Tests ===

#[tokio::test]
async fn generate_image_tool_returns_error_when_disabled() {
    let dir = Dir::new();
    let tool = GenerateImageTool::new(ImageGenerationConfig::default(), None);
    let r = run(&tool, &dir, json!({"prompt": "a test image"})).await;
    assert!(r.is_error);
    assert!(r.content.contains("disabled"));
}

#[tokio::test]
async fn generate_image_tool_returns_error_on_http_failure() {
    let dir = Dir::new();
    let tool = GenerateImageTool::new(
        ImageGenerationConfig {
            enabled: true,
            base_url: "http://127.0.0.1:9".into(), // nothing listening
            api_key: "test-key".into(),
            timeout_seconds: 2,
            ..ImageGenerationConfig::default()
        },
        None,
    );
    let r = run(&tool, &dir, json!({"prompt": "a test image"})).await;
    assert!(r.is_error);
}

#[test]
fn generate_image_tool_parameter_schema_requires_prompt() {
    let tool = GenerateImageTool::new(ImageGenerationConfig::default(), None);
    let schema = tool.parameter_schema();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("prompt"))
    );
}

// === ResizeImageTool Tests ===

#[tokio::test]
async fn resize_image_tool_resizes_image() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let output = dir.path("resized.png");
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": p(&source), "width": 100, "height": 50, "save_path": p(&output)}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&output), (100, 50));
    assert_eq!(
        r.content,
        format!(
            "Image resized: {}\nOriginal: 200x200 \u{2192} New: 100x50",
            p(&output)
        )
    );
}

#[tokio::test]
async fn resize_image_tool_returns_error_for_missing_file() {
    let dir = Dir::new();
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": "/nonexistent/image.png", "width": 100, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("not found"));
}

#[tokio::test]
async fn resize_image_tool_returns_error_for_invalid_dimensions() {
    let dir = Dir::new();
    let source = dir.create_test_image(100, 100);
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": p(&source), "width": 0, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("positive"));
}

#[tokio::test]
async fn resize_image_tool_overwrites_source_when_no_save_path() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": p(&source), "width": 150, "height": 150}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&source), (150, 150));
}

// === CropImageTool Tests ===

#[tokio::test]
async fn crop_image_tool_crops_image() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let output = dir.path("cropped.png");
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": 50, "y": 50, "width": 100, "height": 100, "save_path": p(&output)}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&output), (100, 100));
    assert_eq!(
        r.content,
        format!("Image cropped: {}\nCrop: (50,50) 100x100", p(&output))
    );
}

#[tokio::test]
async fn crop_image_tool_returns_error_for_missing_file() {
    let dir = Dir::new();
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": "/nonexistent/image.png", "x": 0, "y": 0, "width": 100, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("not found"));
}

#[tokio::test]
async fn crop_image_tool_returns_error_for_negative_coordinates() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": -10, "y": 0, "width": 100, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("non-negative"));
}

#[tokio::test]
async fn crop_image_tool_returns_error_for_invalid_dimensions() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": 0, "y": 0, "width": 0, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("positive"));
}

#[tokio::test]
async fn crop_image_tool_returns_error_when_exceeding_bounds() {
    let dir = Dir::new();
    let source = dir.create_test_image(100, 100);
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": 50, "y": 50, "width": 100, "height": 100}),
    )
    .await;
    assert!(r.is_error);
    assert!(r.content.contains("exceeds image bounds"));
    assert_eq!(
        r.content,
        "Crop rectangle (50,50) 100x100 exceeds image bounds (100x100)."
    );
}

#[tokio::test]
async fn crop_image_tool_overwrites_source_when_no_save_path() {
    let dir = Dir::new();
    let source = dir.create_test_image(200, 200);
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": 0, "y": 0, "width": 100, "height": 100}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&source), (100, 100));
}

// === Restricted formats (TIFF etc. are not decoded or encoded) ===

#[tokio::test]
async fn resize_image_tool_rejects_tiff_input() {
    let dir = Dir::new();
    let source = dir.create_tiff("input.tif");
    let output = dir.path("out.png");
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": p(&source), "width": 20, "height": 20, "save_path": p(&output)}),
    )
    .await;
    assert!(r.is_error);
    assert!(
        r.content.contains("Unsupported input image format"),
        "{}",
        r.content
    );
    assert!(!output.exists());
}

#[tokio::test]
async fn crop_image_tool_rejects_tiff_input_even_with_png_extension() {
    // TIFF bytes behind a .png name: the format is detected from content, not the name.
    let dir = Dir::new();
    let disguised = dir.create_tiff("disguised.png");
    let output = dir.path("out.png");
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&disguised), "x": 0, "y": 0, "width": 10, "height": 10, "save_path": p(&output)}),
    )
    .await;
    assert!(r.is_error);
    assert!(
        r.content.contains("Unsupported input image format"),
        "{}",
        r.content
    );
}

#[tokio::test]
async fn resize_and_crop_reject_unsupported_output_extension() {
    for name in ["out.tif", "out.tiff", "out"] {
        let dir = Dir::new();
        let source = dir.create_test_image(50, 50);
        let output = dir.path(name);
        let resize = run(
            &ResizeImageTool,
            &dir,
            json!({"path": p(&source), "width": 20, "height": 20, "save_path": p(&output)}),
        )
        .await;
        let crop = run(
            &CropImageTool,
            &dir,
            json!({"path": p(&source), "x": 0, "y": 0, "width": 10, "height": 10, "save_path": p(&output)}),
        )
        .await;
        assert!(resize.is_error);
        assert!(resize.content.contains("Unsupported output format"));
        assert!(crop.is_error);
        assert!(crop.content.contains("Unsupported output format"));
        assert!(!output.exists());
        if name == "out" {
            assert_eq!(
                crop.content,
                "Unsupported output format '(no extension)'. Supported formats: PNG, JPEG, WebP, GIF, BMP."
            );
        }
    }
}

#[tokio::test]
async fn resize_image_tool_jpeg_in_and_out_works() {
    let dir = Dir::new();
    let source = dir.path("photo.jpg");
    image::RgbImage::new(80, 60).save(&source).unwrap();
    let output = dir.path("photo_small.jpeg");
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": p(&source), "width": 40, "height": 30, "save_path": p(&output)}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&output), (40, 30));
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        image::ImageFormat::Jpeg
    );
}

#[tokio::test]
async fn crop_image_tool_png_to_webp_works() {
    let dir = Dir::new();
    let source = dir.create_test_image(80, 60);
    let output = dir.path("crop.webp");
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": p(&source), "x": 10, "y": 10, "width": 30, "height": 20, "save_path": p(&output)}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&output), (30, 20));
}

#[tokio::test]
async fn crop_and_resize_resolve_relative_paths_against_the_working_folder() {
    let dir = Dir::new();
    let source = dir.create_test_image(20, 10);
    let name = source.file_name().unwrap().to_string_lossy().to_string();
    let r = run(
        &CropImageTool,
        &dir,
        json!({"path": name, "x": 0, "y": 0, "width": 5, "height": 5, "save_path": "out/c.gif"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.content, "Image cropped: out/c.gif\nCrop: (0,0) 5x5");
    assert_eq!(dims(&dir.path("out/c.gif")), (5, 5));
    let r = run(
        &ResizeImageTool,
        &dir,
        json!({"path": "out/c.gif", "width": 3, "height": 3, "save_path": "c.bmp"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(dims(&dir.path("c.bmp")), (3, 3));
}

// === UncensoredImageModelTests (generate_image follows the /uncensored switch) ===

struct ImageServer {
    server: FakeOpenAi,
    dir: Dir,
}

impl ImageServer {
    async fn new() -> Self {
        Self {
            server: FakeOpenAi::start(Vec::new()).await,
            dir: Dir::new(),
        }
    }

    fn config(&self, model: Option<&str>) -> (ImageGenerationConfig, SharedUncensored) {
        let config = ImageGenerationConfig {
            enabled: true,
            base_url: self.server.base_url(),
            model: model.map(str::to_string),
            timeout_seconds: 10,
            output_directory: self.dir.0.path().display().to_string(),
            ..ImageGenerationConfig::default()
        };
        (
            config,
            Arc::new(Mutex::new(AgentUncensoredConfig::default())),
        )
    }

    /// Runs generate_image and returns the request body it sent, with the tool's answer.
    async fn generate(&self, tool: &GenerateImageTool, args: Value) -> (String, String) {
        let r = tool.execute(&args, &self.dir.ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        let last = self.server.image_requests().pop().unwrap();
        (last["body"].as_str().unwrap().to_string(), r.content)
    }
}

fn landscape() -> Value {
    json!({"prompt": "a landscape"})
}

fn with_model(model: &str) -> Value {
    json!({"prompt": "a landscape", "model": model})
}

fn model_of(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).unwrap()["model"]
        .as_str()
        .map(str::to_string)
}

fn model_description(tool: &GenerateImageTool) -> String {
    tool.parameter_schema()["properties"]["model"]["description"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn switch_off_the_request_and_schema_are_as_without_the_switch() {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(Some("qwen-image"));
    let plain = GenerateImageTool::new(config.clone(), None);
    let wired = GenerateImageTool::new(config, Some(unc));
    let (before, _) = s.generate(&plain, landscape()).await;
    let (after, after_result) = s.generate(&wired, landscape()).await;
    assert_eq!(before, after);
    assert_eq!(
        after,
        "{\"prompt\":\"a landscape\",\"model\":\"qwen-image\",\"n\":1,\"response_format\":\"b64_json\"}"
    );
    assert_eq!(plain.parameter_schema(), wired.parameter_schema());
    assert!(!after_result.contains("Model:"));
}

#[tokio::test]
async fn switch_on_the_uncensored_model_is_the_default_and_the_schema_names_it() {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(Some("qwen-image"));
    let tool = GenerateImageTool::new(config, Some(unc.clone()));
    assert!(model_description(&tool).contains("Configured default: qwen-image."));

    // Flipped mid-session: the same tool instance follows.
    unc.lock().unwrap().session = Some(true);
    let (body, result) = s.generate(&tool, landscape()).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image-uncensored"));
    assert!(model_description(&tool).contains("Configured default: qwen-image-uncensored."));
    assert!(result.contains("Model: qwen-image-uncensored"));

    unc.lock().unwrap().session = Some(false);
    let (body, _) = s.generate(&tool, landscape()).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image"));
    assert!(model_description(&tool).contains("Configured default: qwen-image."));
}

#[tokio::test]
async fn switch_on_from_config_enabled_also_counts() {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(Some("qwen-image"));
    unc.lock().unwrap().enabled = true;
    let tool = GenerateImageTool::new(config, Some(unc));
    let (body, _) = s.generate(&tool, landscape()).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image-uncensored"));
}

#[tokio::test]
async fn switch_on_an_explicit_other_model_is_respected() {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(Some("qwen-image"));
    unc.lock().unwrap().session = Some(true);
    let tool = GenerateImageTool::new(config, Some(unc));
    let (body, _) = s.generate(&tool, with_model("juggernaut")).await;
    assert_eq!(model_of(&body).as_deref(), Some("juggernaut"));
}

#[tokio::test]
async fn switch_on_an_explicit_normal_default_is_overridden() {
    for asked in ["qwen-image", "Qwen-Image ", ""] {
        let s = ImageServer::new().await;
        let (config, unc) = s.config(Some("qwen-image"));
        unc.lock().unwrap().session = Some(true);
        let tool = GenerateImageTool::new(config, Some(unc));
        let (body, _) = s.generate(&tool, with_model(asked)).await;
        assert_eq!(
            model_of(&body).as_deref(),
            Some("qwen-image-uncensored"),
            "{asked:?}"
        );
    }
}

#[tokio::test]
async fn switch_off_an_explicit_model_is_sent_as_given() {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(Some("qwen-image"));
    let tool = GenerateImageTool::new(config, Some(unc));
    let (body, _) = s.generate(&tool, with_model("qwen-image-uncensored")).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image-uncensored"));
    let (body, _) = s.generate(&tool, with_model("qwen-image")).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image"));
}

#[tokio::test]
async fn switch_on_a_blank_image_model_changes_nothing() {
    for image_model in [Some(""), Some("   "), None] {
        let s = ImageServer::new().await;
        let (config, unc) = s.config(Some("qwen-image"));
        let off_tool = GenerateImageTool::new(config.clone(), Some(unc.clone()));
        let (off, _) = s.generate(&off_tool, landscape()).await;
        let off_schema = off_tool.parameter_schema();

        {
            let mut u = unc.lock().unwrap();
            u.image_model = image_model.map(str::to_string);
            u.session = Some(true);
        }
        let tool = GenerateImageTool::new(config, Some(unc));
        let (on, result) = s.generate(&tool, landscape()).await;
        assert_eq!(off, on);
        assert_eq!(off_schema, tool.parameter_schema());
        assert!(!result.contains("Model:"));
    }
}

#[tokio::test]
async fn switch_on_with_no_configured_model_uses_the_uncensored_model_and_keeps_any_model_asked_for()
 {
    let s = ImageServer::new().await;
    let (config, unc) = s.config(None);
    let tool = GenerateImageTool::new(config, Some(unc.clone()));
    let (body, _) = s.generate(&tool, landscape()).await;
    assert_eq!(model_of(&body), None); // off: the server's default

    unc.lock().unwrap().session = Some(true);
    let (body, _) = s.generate(&tool, landscape()).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image-uncensored"));
    let (body, _) = s.generate(&tool, with_model("qwen-image")).await;
    assert_eq!(model_of(&body).as_deref(), Some("qwen-image"));
}

#[test]
fn edit_image_does_not_follow_the_switch() {
    // edit_image keeps imageGen.model: whether the uncensored model takes /images/edits is
    // not known (and the tool has no switch to follow).
    let config = ImageGenerationConfig {
        model: Some("qwen-image".into()),
        ..ImageGenerationConfig::default()
    };
    let tool = EditImageTool::new(config);
    let description = tool.parameter_schema()["properties"]["model"]["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(description.contains("Configured default: qwen-image."));
}

#[test]
fn resolve_image_model_cases() {
    type Case<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: [Case; 8] = [
        (None, Some("qwen-image"), None, Some("qwen-image")),
        (Some("other"), Some("qwen-image"), None, Some("other")),
        (None, None, None, None),
        (None, Some("qwen-image"), Some("unc"), Some("unc")),
        (
            Some("qwen-image"),
            Some("qwen-image"),
            Some("unc"),
            Some("unc"),
        ),
        (
            Some("other"),
            Some("qwen-image"),
            Some("unc"),
            Some("other"),
        ),
        (Some("other"), None, Some("unc"), Some("other")),
        (Some(" "), None, Some("unc"), Some("unc")),
    ];
    for (requested, configured, uncensored, expected) in cases {
        assert_eq!(
            resolve_image_model(requested, configured, uncensored).as_deref(),
            expected
        );
    }
}

#[test]
fn image_model_defaults_to_the_uncensored_qwen_model_and_is_only_active_while_on() {
    let mut u = AgentUncensoredConfig::default();
    assert_eq!(u.image_model.as_deref(), Some("qwen-image-uncensored"));
    assert_eq!(active_image_model(&u), None);
    u.session = Some(true);
    assert_eq!(
        active_image_model(&u).as_deref(),
        Some("qwen-image-uncensored")
    );
    u.image_model = Some(" custom ".into());
    assert_eq!(active_image_model(&u).as_deref(), Some("custom"));
}

// === The images API round trip (not in the C# tests) ===

#[tokio::test]
async fn generate_and_edit_save_the_returned_image() {
    let s = ImageServer::new().await;
    let (config, _) = s.config(Some("m"));
    let tool = GenerateImageTool::new(config.clone(), None);
    let r = tool
        .execute(
            &json!({"prompt": "a <red> fox", "size": "1x1", "save_path": "art/fox.png"}),
            &s.dir.ctx(),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(
        r.content,
        "Image saved: art/fox.png\nPrompt: a <red> fox\nSize: 1x1"
    );
    assert_eq!(dims(&s.dir.path("art/fox.png")), (1, 1));
    let req = s.server.image_requests().pop().unwrap();
    assert_eq!(
        req["body"],
        "{\"prompt\":\"a \\u003Cred\\u003E fox\",\"size\":\"1x1\",\"model\":\"m\",\"n\":1,\"response_format\":\"b64_json\"}"
    );
    assert_eq!(req["authorization"], "Bearer none");

    // Without save_path: <output dir>/<stamp>_<slug>.png
    let r = tool
        .execute(&json!({"prompt": "Two Words"}), &s.dir.ctx())
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("_two_words.png"), "{}", r.content);

    // A server error is reported with its status and body.
    let r = tool
        .execute(
            &json!({"prompt": "FAIL please", "save_path": "x.png"}),
            &s.dir.ctx(),
        )
        .await;
    assert!(r.is_error);
    assert_eq!(
        r.content,
        "Server returned 500 Internal Server Error: image backend exploded"
    );

    let edit = EditImageTool::new(config);
    let r = edit
        .execute(
            &json!({"image_path": "art/fox.png", "prompt": "make it blue", "mask_path": "missing.png"}),
            &s.dir.ctx(),
        )
        .await;
    assert_eq!(r.content, "Mask file not found: missing.png");
    let r = edit
        .execute(
            &json!({"image_path": "art/fox.png", "prompt": "make it blue", "size": "1x1", "save_path": "art/blue.png"}),
            &s.dir.ctx(),
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(
        r.content,
        "Edited image saved: art/blue.png\nSource: art/fox.png\nPrompt: make it blue"
    );
    let req = s.server.image_requests().pop().unwrap();
    assert_eq!(req["path"], "/v1/images/edits");
    let names: Vec<&str> = req["body"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["image", "prompt", "n", "response_format", "size", "model"]
    );
    assert_eq!(req["body"][0]["filename"], "fox.png");
    assert_eq!(req["body"][0]["content_type"], "image/png");
    assert_eq!(req["body"][1]["text"], "make it blue");
}
