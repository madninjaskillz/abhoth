//! `crop_image` and `resize_image` (`CropImageTool`, `ResizeImageTool`).

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::{get_i32_lenient, get_str, required_str};
use serde_json::Value;

use crate::formats::{self, LoadError};

fn hints(short: &'static str) -> Option<ToolRenderHints> {
    Some(ToolRenderHints {
        emoji: "\u{1f3a8}",
        short_description: short,
        summary_arg_keys: &["path"],
        aliases: &[],
    })
}

/// A required integer argument (lenient about numbers sent as strings).
fn required_int(args: &Value, name: &str) -> Result<i64, String> {
    match args.get(name) {
        None => Err(format!("Missing required argument: {name}")),
        v => Ok(i64::from(get_i32_lenient(v, 0))),
    }
}

/// `save_path` when given (not blank), else the source path.
fn output_path<'a>(args: &'a Value, path: &'a str) -> &'a str {
    match get_str(args, "save_path") {
        Some(p) if !p.trim().is_empty() => p,
        _ => path,
    }
}

/// Create the output's folder (before loading, as C# does).
fn ensure_parent(output: &std::path::Path) -> Result<(), String> {
    match output.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())
        }
        _ => Ok(()),
    }
}

#[derive(Default)]
pub struct CropImageTool;

#[async_trait::async_trait]
impl Tool for CropImageTool {
    fn name(&self) -> &str {
        "crop_image"
    }
    fn description(&self) -> &str {
        "Crop an image to a specified rectangular region. Always provide a full absolute save_path to avoid overwriting the original."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn parameter_schema(&self) -> Value {
        serde_json::from_str(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Full absolute path to the source image file"
            },
            "x": {
                "type": "integer",
                "description": "X coordinate of the top-left corner of the crop rectangle (pixels from left edge)"
            },
            "y": {
                "type": "integer",
                "description": "Y coordinate of the top-left corner of the crop rectangle (pixels from top edge)"
            },
            "width": {
                "type": "integer",
                "description": "Width of the crop rectangle in pixels"
            },
            "height": {
                "type": "integer",
                "description": "Height of the crop rectangle in pixels"
            },
            "save_path": {
                "type": "string",
                "description": "Full absolute path to save the cropped image. If omitted, overwrites the source file."
            }
        },
        "required": ["path", "x", "y", "width", "height"]
    }"#,
        )
        .expect("valid schema")
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Crop image")
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let parsed = (|| -> Result<_, String> {
            Ok((
                required_str(args, "path")?,
                required_int(args, "x")?,
                required_int(args, "y")?,
                required_int(args, "width")?,
                required_int(args, "height")?,
            ))
        })();
        let (path, x, y, width, height) = match parsed {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let source = ctx.full_path(path);
        if !source.is_file() {
            return ToolResult::error(format!("Image file not found: {path}"));
        }
        if width <= 0 || height <= 0 {
            return ToolResult::error("Crop width and height must be positive integers.");
        }
        if x < 0 || y < 0 {
            return ToolResult::error("Crop x and y coordinates must be non-negative.");
        }
        let output = output_path(args, path);
        if let Some(e) = formats::check_output_path(output) {
            return ToolResult::error(e);
        }
        let out_full = ctx.full_path(output);
        let failed = |e: String| ToolResult::error(format!("Failed to crop image: {e}"));
        if let Err(e) = ensure_parent(&out_full) {
            return failed(e);
        }
        let image = match formats::load(&source) {
            Ok(i) => i,
            Err(LoadError::Unsupported) => {
                return ToolResult::error(formats::unsupported_input(path));
            }
            Err(LoadError::Failed(e)) => return failed(e),
        };
        let (w, h) = (i64::from(image.width()), i64::from(image.height()));
        if x + width > w || y + height > h {
            return ToolResult::error(format!(
                "Crop rectangle ({x},{y}) {width}x{height} exceeds image bounds ({w}x{h})."
            ));
        }
        let cropped = image.crop_imm(x as u32, y as u32, width as u32, height as u32);
        match formats::save(&cropped, &out_full) {
            Ok(()) => ToolResult::success(format!(
                "Image cropped: {output}\nCrop: ({x},{y}) {width}x{height}"
            )),
            Err(e) => failed(e),
        }
    }
}

#[derive(Default)]
pub struct ResizeImageTool;

#[async_trait::async_trait]
impl Tool for ResizeImageTool {
    fn name(&self) -> &str {
        "resize_image"
    }
    fn description(&self) -> &str {
        "Resize an image to the specified width and height in pixels. Stretches to exact dimensions. Always provide a full absolute save_path to avoid overwriting the original."
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }
    fn parameter_schema(&self) -> Value {
        serde_json::from_str(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Full absolute path to the source image file"
            },
            "width": {
                "type": "integer",
                "description": "Target width in pixels"
            },
            "height": {
                "type": "integer",
                "description": "Target height in pixels"
            },
            "save_path": {
                "type": "string",
                "description": "Full absolute path to save the resized image. If omitted, overwrites the source file."
            }
        },
        "required": ["path", "width", "height"]
    }"#,
        )
        .expect("valid schema")
    }
    fn render_hints(&self) -> Option<ToolRenderHints> {
        hints("Resize image")
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let parsed = (|| -> Result<_, String> {
            Ok((
                required_str(args, "path")?,
                required_int(args, "width")?,
                required_int(args, "height")?,
            ))
        })();
        let (path, width, height) = match parsed {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let source = ctx.full_path(path);
        if !source.is_file() {
            return ToolResult::error(format!("Image file not found: {path}"));
        }
        if width <= 0 || height <= 0 {
            return ToolResult::error("Width and height must be positive integers.");
        }
        let output = output_path(args, path);
        if let Some(e) = formats::check_output_path(output) {
            return ToolResult::error(e);
        }
        let out_full = ctx.full_path(output);
        let result = (|| -> Result<Result<(u32, u32), String>, LoadError> {
            if let Err(e) = ensure_parent(&out_full) {
                return Ok(Err(e));
            }
            let image = formats::load(&source)?;
            let original = (image.width(), image.height());
            // ImageSharp's default resampler is bicubic; Catmull-Rom is the `image` crate's.
            let resized = image.resize_exact(
                width as u32,
                height as u32,
                image::imageops::FilterType::CatmullRom,
            );
            Ok(formats::save(&resized, &out_full).map(|()| original))
        })();
        match result {
            Ok(Ok((ow, oh))) => ToolResult::success(format!(
                "Image resized: {output}\nOriginal: {ow}x{oh} \u{2192} New: {width}x{height}"
            )),
            Ok(Err(e)) => ToolResult::error(format!("Failed to resize image: {e}")),
            Err(LoadError::Unsupported) => ToolResult::error(formats::unsupported_input(path)),
            Err(LoadError::Failed(e)) => ToolResult::error(format!("Failed to resize image: {e}")),
        }
    }
}
