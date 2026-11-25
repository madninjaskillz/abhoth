//! `SafeImageFormats`: the image formats the tools read and write (PNG, JPEG, WebP, GIF, BMP).
//! The input format is detected from the content, never the name; anything else (TIFF, ...)
//! is refused. The `image` crate is built with only these formats, and its decoders do not
//! parse embedded ICC/EXIF metadata.

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat};

pub const SUPPORTED_LIST: &str = "PNG, JPEG, WebP, GIF, BMP";

/// The output format for a file extension (ImageSharp's extension table for those formats).
fn format_for_extension(ext: &str) -> Option<ImageFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jfif" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        "bm" | "bmp" | "dip" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// `Path.GetExtension`: ".png", or "" without one.
pub fn extension(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i..].to_string(),
        _ => String::new(),
    }
}

/// `CheckOutputPath`: the error when the output path's extension is not a supported format.
pub fn check_output_path(output_path: &str) -> Option<String> {
    let ext = extension(output_path);
    if ext.is_empty() || format_for_extension(ext.trim_start_matches('.')).is_none() {
        let shown = if ext.is_empty() {
            "(no extension)"
        } else {
            ext.as_str()
        };
        return Some(format!(
            "Unsupported output format '{shown}'. Supported formats: {SUPPORTED_LIST}."
        ));
    }
    None
}

/// `UnsupportedInput`.
pub fn unsupported_input(path: &str) -> String {
    format!("Unsupported input image format: {path}. Supported formats: {SUPPORTED_LIST}.")
}

/// Why an image could not be loaded.
pub enum LoadError {
    /// Not one of the supported formats (`UnknownImageFormatException`).
    Unsupported,
    /// A supported format that failed to decode, or an I/O error.
    Failed(String),
}

/// Load an image with the restricted format set.
pub fn load(path: &Path) -> Result<DynamicImage, LoadError> {
    let bytes = std::fs::read(path).map_err(|e| LoadError::Failed(e.to_string()))?;
    let format = image::guess_format(&bytes).map_err(|_| LoadError::Unsupported)?;
    if !matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::WebP
            | ImageFormat::Gif
            | ImageFormat::Bmp
    ) {
        return Err(LoadError::Unsupported);
    }
    image::load_from_memory_with_format(&bytes, format).map_err(|e| match e {
        image::ImageError::Unsupported(_) => LoadError::Unsupported,
        other => LoadError::Failed(other.to_string()),
    })
}

/// Save in the format the output path's extension names (checked by [`check_output_path`]).
/// JPEG has no alpha channel and GIF and WebP take 8-bit RGBA, so the pixels are converted
/// where the encoder needs it.
pub fn save(image: &DynamicImage, output: &Path) -> Result<(), String> {
    let ext = output
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let format =
        format_for_extension(&ext).ok_or_else(|| check_output_path(&ext).unwrap_or_default())?;
    let eight_bit = matches!(
        image,
        DynamicImage::ImageRgba8(_)
            | DynamicImage::ImageRgb8(_)
            | DynamicImage::ImageLuma8(_)
            | DynamicImage::ImageLumaA8(_)
    );
    let float = matches!(
        image,
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
    );
    let converted = match format {
        ImageFormat::Jpeg => Some(DynamicImage::ImageRgb8(image.to_rgb8())),
        ImageFormat::Gif => Some(DynamicImage::ImageRgba8(image.to_rgba8())),
        ImageFormat::WebP | ImageFormat::Bmp if !eight_bit => {
            Some(DynamicImage::ImageRgba8(image.to_rgba8()))
        }
        ImageFormat::Png if float => Some(DynamicImage::ImageRgba8(image.to_rgba8())),
        _ => None,
    };
    let img = converted.as_ref().unwrap_or(image);
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, format).map_err(|e| e.to_string())?;
    std::fs::write(output, buf.into_inner()).map_err(|e| e.to_string())
}
