//! `AttachmentTray`: the attachments of the prompt being typed.

use std::sync::Mutex;

use super::clipboard::ClipboardReader;
use super::{AttachmentStore, PromptAttachment, full_path, message_builder, pasted_path_detector};

/// What a paste puts into the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteResult {
    /// Text to insert at the cursor: the pasted text, or attachment chips.
    pub insert: String,
    /// A one-line message for the user (what was attached, or why nothing was).
    pub notice: Option<String>,
    pub attached: Vec<PromptAttachment>,
}

impl PasteResult {
    fn notice(notice: impl Into<String>) -> Self {
        Self {
            insert: String::new(),
            notice: Some(notice.into()),
            attached: Vec::new(),
        }
    }
}

/// The attachments of the prompt being typed. Each gets the next number and a chip; when the
/// prompt is sent, [`AttachmentTray::take`] hands over those whose chip is still in the text
/// and starts over. Also turns clipboard contents and pasted paths into chips.
#[derive(Debug)]
pub struct AttachmentTray {
    store: AttachmentStore,
    items: Mutex<Vec<PromptAttachment>>,
}

impl AttachmentTray {
    pub fn new(store: AttachmentStore) -> Self {
        Self {
            store,
            items: Mutex::new(Vec::new()),
        }
    }

    pub fn store(&self) -> &AttachmentStore {
        &self.store
    }

    fn items(&self) -> std::sync::MutexGuard<'_, Vec<PromptAttachment>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attachments added since the last [`AttachmentTray::take`].
    pub fn pending(&self) -> Vec<PromptAttachment> {
        self.items().clone()
    }

    /// Adds a file and returns its attachment (chip number = next free one).
    pub fn add(&self, path: &str) -> PromptAttachment {
        let mut items = self.items();
        let number = items.iter().map(|i| i.number).max().map_or(1, |n| n + 1);
        let cwd = std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        let a = PromptAttachment::for_path(number, full_path(&cwd, path));
        items.push(a.clone());
        a
    }

    /// The attachments whose chips are in `text`; the tray is emptied.
    pub fn take(&self, text: &str) -> Vec<PromptAttachment> {
        let mut items = self.items();
        let used = message_builder::used_attachments(text, &items);
        items.clear();
        used
    }

    /// Forgets all pending attachments (prompt cancelled).
    pub fn clear(&self) {
        self.items().clear();
    }

    /// What Alt+V inserts: files copied in a file manager become chips, an image is saved under
    /// `.orca/attachments` and becomes a chip, text that is only paths to existing images
    /// becomes chips, and any other text is pasted as is.
    pub async fn paste_from(
        &self,
        clipboard: &dyn ClipboardReader,
        working_directory: &str,
    ) -> PasteResult {
        let content = match clipboard.read().await {
            Ok(c) => c,
            Err(e) => return PasteResult::notice(format!("Couldn't read the clipboard: {e}")),
        };

        if let Some(error) = content.error {
            return PasteResult::notice(error);
        }

        if let Some(files) = content.files.filter(|f| !f.is_empty()) {
            let existing: Vec<String> = files
                .into_iter()
                .filter(|f| std::path::Path::new(f).is_file())
                .collect();
            if existing.is_empty() {
                return PasteResult::notice(
                    "The copied items aren't files (folders can't be attached).",
                );
            }
            return self.chips(&existing);
        }

        if let Some(png) = content.png.filter(|p| !p.is_empty()) {
            return match self.store.save_image(&png, ".png") {
                Ok(path) => self.chips(&[path]),
                Err(e) => PasteResult::notice(format!("Couldn't save the pasted image: {e}")),
            };
        }

        if let Some(text) = content.text.filter(|t| !t.is_empty()) {
            if let Some(paths) = pasted_path_detector::try_parse(&text, working_directory, true) {
                return self.chips(&paths);
            }
            return PasteResult {
                insert: text,
                notice: None,
                attached: Vec::new(),
            };
        }

        PasteResult::notice("The clipboard is empty.")
    }

    /// Chips for `paths`, separated by spaces, with a notice naming them.
    pub fn chips(&self, paths: &[String]) -> PasteResult {
        let added: Vec<PromptAttachment> = paths.iter().map(|p| self.add(p)).collect();
        let insert = added
            .iter()
            .map(PromptAttachment::chip)
            .collect::<Vec<_>>()
            .join(" ")
            + " ";
        let what = if added.len() == 1 {
            format!("{} {}", added[0].kind_label(), added[0].number)
        } else {
            format!("{} files", added.len())
        };
        PasteResult {
            insert,
            notice: Some(format!(
                "Attached {what}. Backspace over a chip removes it."
            )),
            attached: added,
        }
    }
}
