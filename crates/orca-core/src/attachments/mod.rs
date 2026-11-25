//! Prompt attachments (port of `OpenOrca.Core/Attachments`): files attached to the prompt
//! being typed (a pasted screenshot, a file copied in a file manager, a path dragged onto the
//! terminal), shown as chips such as `[image 1: shot.png]`, and the user message built from
//! them (images inline for a vision model, file paths otherwise). Also the clipboard readers,
//! pasted-path detection, the web UI's attached-files list and file-path links.
//!
//! The C# static classes are modules here: `message_builder::build`,
//! `file_link_resolver::find_paths`, `pasted_path_detector::try_parse`,
//! `web_prompt_attachments::{compose, extract}`, `image_files::is_image`.

pub mod clipboard;
pub mod file_link_resolver;
pub mod image_files;
pub mod message_builder;
pub mod pasted_path_detector;
mod prompt_attachment;
mod store;
mod tray;
pub mod web_prompt_attachments;

pub use file_link_resolver::FileReference;
pub use message_builder::AttachmentMessage;
pub use prompt_attachment::{AttachmentKind, PromptAttachment, chip_pattern};
pub use store::AttachmentStore;
pub use tray::{AttachmentTray, PasteResult};

/// The file name of a path (`Path.GetFileName`: after the last '/', or '\\' on Windows).
pub(crate) fn file_name(path: &str) -> &str {
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    path.rsplit(seps).next().unwrap_or(path)
}

/// `Path.GetExtension` (with the dot), or "".
pub(crate) fn extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => &name[i..],
        _ => "",
    }
}

/// `Path.GetFullPath(path, base)` as a string (lexical, like .NET).
pub(crate) fn full_path(base: &str, path: &str) -> String {
    orca_abstractions::full_path(std::path::Path::new(base), path)
        .to_string_lossy()
        .into_owned()
}

/// `~/x` or `~\x` under the user profile; anything else unchanged.
pub(crate) fn expand_home(path: &str) -> String {
    if (path.starts_with("~/") || path.starts_with("~\\"))
        && let Some(home) = orca_tools_common::path_safety::user_profile()
    {
        return std::path::Path::new(&home)
            .join(&path[2..])
            .to_string_lossy()
            .into_owned();
    }
    path.to_string()
}

/// `Path.IsPathRooted`.
pub(crate) fn is_rooted(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    if cfg!(windows) {
        let b = path.as_bytes();
        return b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    }
    false
}

#[cfg(test)]
mod tests;
