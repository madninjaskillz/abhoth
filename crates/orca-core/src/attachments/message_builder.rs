//! `AttachmentMessageBuilder`: typed text with attachment chips -> the user message.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use super::{AttachmentKind, PromptAttachment, chip_pattern, full_path, image_files, is_rooted};
use crate::client::{ChatMessage, ImagePart};

/// Told to a model without vision when images went as paths.
pub const NO_VISION_NOTE: &str = "(This model can't view images directly; the attached image(s) are files on disk: inspect them with your tools if needed.)";

/// The user message built from a prompt and its attachments.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentMessage {
    pub message: ChatMessage,
    /// The message text (chips replaced by paths, plus any note).
    pub text: String,
    pub images_inline: usize,
    /// Images that went as file paths only (no vision, or too big / unsupported format).
    pub images_as_paths: usize,
}

/// Turns typed text with attachment chips into the user message sent to the model. Each chip
/// becomes `[image 1: path]` in the text, so the model (and a saved session, which keeps only
/// text) always has the path for its tools. When the model takes images (`vision`), each image
/// also goes as an image part; when it doesn't, a line tells the model the image is a file it
/// can't see directly. Attachments whose chip was deleted are ignored; paths under
/// `working_directory` are shown relative.
pub fn build(
    text: &str,
    attachments: &[PromptAttachment],
    vision: bool,
    working_directory: &str,
) -> AttachmentMessage {
    let used = used_attachments(text, attachments);
    if used.is_empty() {
        return AttachmentMessage {
            message: ChatMessage::user(text),
            text: text.to_string(),
            images_inline: 0,
            images_as_paths: 0,
        };
    }

    let by_chip: HashMap<(&str, u32), &PromptAttachment> = used
        .iter()
        .map(|a| ((a.kind_label(), a.number), a))
        .collect();
    let body = chip_pattern().replace_all(text, |c: &regex::Captures<'_>| {
        let key = c["n"].parse::<u32>().ok().map(|n| (&c["kind"], n));
        match key.and_then(|k| by_chip.get(&k)) {
            Some(a) => format!(
                "[{} {}: {}]",
                a.kind_label(),
                a.number,
                display_path(&a.path, working_directory)
            ),
            None => c[0].to_string(),
        }
    });

    let mut images = Vec::new();
    let mut as_paths = 0;
    for a in used.iter().filter(|a| a.kind == AttachmentKind::Image) {
        match vision.then(|| try_load(&a.path)).flatten() {
            Some(data) => images.push(data),
            None => as_paths += 1,
        }
    }

    let mut final_text = body.into_owned();
    if as_paths > 0 && !vision {
        final_text.push_str("\n\n");
        final_text.push_str(NO_VISION_NOTE);
    }
    let images_inline = images.len();
    AttachmentMessage {
        message: ChatMessage::user(final_text.clone()).with_images(images),
        text: final_text,
        images_inline,
        images_as_paths: as_paths,
    }
}

/// The attachments whose chip is still in `text`.
pub fn used_attachments(text: &str, attachments: &[PromptAttachment]) -> Vec<PromptAttachment> {
    if attachments.is_empty() {
        return Vec::new();
    }
    let chips: HashSet<&str> = chip_pattern().find_iter(text).map(|m| m.as_str()).collect();
    attachments
        .iter()
        .filter(|a| chips.contains(a.chip().as_str()))
        .cloned()
        .collect()
}

/// The image as a data part (named by its path), or `None` when it is missing, too big, or a
/// format models don't take.
pub fn try_load(path: &str) -> Option<ImagePart> {
    let media_type = image_files::media_type_for(path)?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > image_files::MAX_INLINE_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    Some(ImagePart {
        data,
        media_type: media_type.to_string(),
        name: Some(path.to_string()),
    })
}

/// The path relative to the working directory when inside it, else absolute; '/' separators.
pub fn display_path(path: &str, working_directory: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    let full = full_path(&cwd, path);
    let base = full_path(&cwd, working_directory);
    let rel = relative_path(&base, &full);
    let shown = if rel.starts_with("..") || is_rooted(&rel) {
        full
    } else {
        rel
    };
    shown.replace('\\', "/")
}

/// `Path.GetRelativePath` for two full paths: the full path when the roots differ.
pub(crate) fn relative_path(base: &str, full: &str) -> String {
    let same = |a: &Component<'_>, b: &Component<'_>| {
        if cfg!(windows) {
            a.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
        } else {
            a == b
        }
    };
    let b: Vec<Component<'_>> = Path::new(base).components().collect();
    let f: Vec<Component<'_>> = Path::new(full).components().collect();
    let root_len = |c: &[Component<'_>]| {
        c.iter()
            .take_while(|x| matches!(x, Component::Prefix(_) | Component::RootDir))
            .count()
    };
    let (rb, rf) = (root_len(&b), root_len(&f));
    if rb != rf || !b[..rb].iter().zip(&f[..rf]).all(|(x, y)| same(x, y)) {
        return full.to_string();
    }
    let common = b.iter().zip(&f).take_while(|(x, y)| same(x, y)).count();
    let sep = std::path::MAIN_SEPARATOR_STR;
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), b.len() - common).collect();
    parts.extend(
        f[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join(sep)
    }
}
