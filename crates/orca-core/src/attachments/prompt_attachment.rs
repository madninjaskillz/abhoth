//! `PromptAttachment`: a file attached to the prompt, shown as a chip.

use std::sync::LazyLock;

use regex::Regex;

use super::{file_name, image_files};

/// What a prompt attachment is: a picture the model may be able to see, or any other file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentKind {
    Image,
    File,
}

/// A file attached to the prompt being typed. It shows in the input as a chip such as
/// `[image 1: paste-20261117-2210.png]`; the chip text is the source of truth, so deleting the
/// chip drops the attachment. Shared by the TUI and the web UI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptAttachment {
    /// The number shown in the chip, unique within one prompt.
    pub number: u32,
    /// Absolute path of the file on disk.
    pub path: String,
    pub kind: AttachmentKind,
}

impl PromptAttachment {
    pub fn new(number: u32, path: impl Into<String>, kind: AttachmentKind) -> Self {
        Self {
            number,
            path: path.into(),
            kind,
        }
    }

    /// The attachment for `path`, an image when its extension is one (`For`).
    pub fn for_path(number: u32, path: impl Into<String>) -> Self {
        let path = path.into();
        let kind = if image_files::is_image(&path) {
            AttachmentKind::Image
        } else {
            AttachmentKind::File
        };
        Self { number, path, kind }
    }

    /// The file name shown in the chip.
    pub fn display_name(&self) -> &str {
        file_name(&self.path)
    }

    /// "image" or "file".
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            AttachmentKind::Image => "image",
            AttachmentKind::File => "file",
        }
    }

    /// The chip text shown in the input, e.g. `[image 1: shot.png]`.
    pub fn chip(&self) -> String {
        format!(
            "[{} {}: {}]",
            self.kind_label(),
            self.number,
            safe_name(self.display_name())
        )
    }
}

// A ']' or '[' in a file name would end the chip early; chips show a lookalike instead.
fn safe_name(name: &str) -> String {
    name.replace('[', "(").replace(']', ")")
}

static CHIP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(?<kind>image|file) (?<n>\d{1,3}): (?<name>[^\[\]\r\n]{1,200})\]")
        .expect("static regex")
});

/// A chip in some text: `[image 3: name.png]` or `[file 2: notes.txt]` (groups `kind`, `n`,
/// `name`).
pub fn chip_pattern() -> &'static Regex {
    &CHIP
}
