//! `ImageFiles`: which files count as images a model can be sent, and their media types.

use super::extension;

/// The largest image sent inline (the Anthropic API's per-image limit, 5 MB).
pub const MAX_INLINE_BYTES: u64 = 5 * 1024 * 1024;

const MEDIA_TYPES: [(&str, &str); 5] = [
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
];

// Shown as image chips (and so, with vision, read inline when they're a sendable type).
const IMAGE_EXTENSIONS: [&str; 6] = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"];

/// True when the file name has an image extension.
pub fn is_image(path: &str) -> bool {
    let ext = extension(path);
    IMAGE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// The media type to send the image as, or `None` when models don't take that format (.bmp).
pub fn media_type_for(path: &str) -> Option<&'static str> {
    let ext = extension(path);
    MEDIA_TYPES
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|(_, t)| *t)
}
