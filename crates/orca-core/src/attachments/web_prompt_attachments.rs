//! `WebPromptAttachments`: a prompt sent from the browser with attachments.
//!
//! The typed text, then an "Attached files:" list of project-relative paths. The list is
//! readable as is (in the transcript, and by a model that opens the files with its tools);
//! [`extract`] turns it back into attachments so images can go to a vision model as image
//! parts, the same as Alt+V in the TUI.

use super::{PromptAttachment, full_path, image_files};

const HEADER: &str = "Attached files";
const IMAGES_NOTE: &str = " (images: view or inspect them with your tools)";

/// The prompt with its attached-files list.
pub fn compose(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let note = if paths.iter().any(|p| image_files::is_image(p)) {
        IMAGES_NOTE
    } else {
        ""
    };
    let list = paths
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let head = if text.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", text.trim_end())
    };
    format!("{head}{HEADER}{note}:\n{list}")
}

/// The prompt with its attached-files list turned into chips, and the attachments for them;
/// the text unchanged and no attachments when there's no list or a listed file is missing.
/// `root`: the folder the listed paths are relative to.
pub fn extract(text: &str, root: &str) -> (String, Vec<PromptAttachment>) {
    let unchanged = || (text.to_string(), Vec::new());
    let Some(at) = text.rfind(HEADER) else {
        return unchanged();
    };
    if at > 0 && text.as_bytes()[at - 1] != b'\n' {
        return unchanged();
    }
    let lines: Vec<&str> = text[at..].split('\n').collect();
    let first = lines[0];
    if !first.ends_with(':')
        || (first != format!("{HEADER}:") && first != format!("{HEADER}{IMAGES_NOTE}:"))
        || lines.len() < 2
    {
        return unchanged();
    }
    let mut attachments: Vec<PromptAttachment> = Vec::new();
    let mut sb = String::from(&text[..at]);
    sb.push_str(first);
    for line in &lines[1..] {
        let Some(rel) = line.strip_prefix("- ") else {
            return unchanged();
        };
        let rel = rel.trim();
        if rel.contains('\0') {
            return unchanged();
        }
        let full = full_path(root, rel);
        if !std::path::Path::new(&full).is_file() {
            return unchanged();
        }
        let a = PromptAttachment::for_path(attachments.len() as u32 + 1, full);
        sb.push_str("\n- ");
        sb.push_str(&a.chip());
        attachments.push(a);
    }
    (sb, attachments)
}
