//! Reading the system clipboard for a paste (`OpenOrca.Core/Attachments/Clipboard`).
//!
//! C# reads the Windows clipboard through P/Invoke (`WindowsClipboard`); the workspace forbids
//! `unsafe`, so on Windows the clipboard is read through Windows PowerShell
//! ([`CommandClipboard`] with [`Platform::Windows`]), in the same order: files, the "PNG"
//! format, a DIB (converted with [`image_codec`]), text.

mod command;
pub mod image_codec;

use std::path::PathBuf;

#[cfg(test)]
pub(crate) use command::parse_windows_output;
pub use command::{
    ClipboardToolRunner, CommandClipboard, Platform, parse_apple_script_data, parse_uri_list,
};

/// What the system clipboard holds, in the order a paste uses it: files (copied in a file
/// manager), else an image (always PNG bytes here), else text. `error` says why nothing could
/// be read (no clipboard tool installed, clipboard busy).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    pub files: Option<Vec<String>>,
    pub png: Option<Vec<u8>>,
    pub text: Option<String>,
    pub error: Option<String>,
}

impl ClipboardContent {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.as_ref().is_none_or(|f| f.is_empty())
            && self.png.as_ref().is_none_or(|p| p.is_empty())
            && self.text.as_ref().is_none_or(|t| t.is_empty())
    }
}

/// Reads the system clipboard (`IClipboardReader`). Faked in tests; [`create_default`] picks
/// the real one. An `Err` is a read that failed outright (C#: an exception).
#[async_trait::async_trait]
pub trait ClipboardReader: Send + Sync {
    async fn read(&self) -> Result<ClipboardContent, String>;
}

/// Test hook: when set, the clipboard is read from this folder instead of the system's
/// ([`DirectoryClipboard`]), so harness runs never touch the user's clipboard.
pub const DIRECTORY_VARIABLE: &str = "OPENORCA_CLIPBOARD_DIR";

/// `ClipboardReaders.CreateDefault`: the clipboard reader for this machine.
pub fn create_default() -> Box<dyn ClipboardReader> {
    if let Ok(dir) = std::env::var(DIRECTORY_VARIABLE)
        && !dir.is_empty()
    {
        return Box::new(DirectoryClipboard::new(dir));
    }
    if cfg!(windows) {
        return Box::new(CommandClipboard::new(Platform::Windows));
    }
    if cfg!(target_os = "macos") {
        return Box::new(CommandClipboard::new(Platform::MacOS));
    }
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty());
    Box::new(CommandClipboard::new(if wayland {
        Platform::Wayland
    } else {
        Platform::X11
    }))
}

/// A clipboard kept in a folder (for tests and the TUI harness): `files.txt` (one path per
/// line), `image.png`, `text.txt`, read in that order.
#[derive(Debug, Clone)]
pub struct DirectoryClipboard {
    directory: PathBuf,
}

impl DirectoryClipboard {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
}

#[async_trait::async_trait]
impl ClipboardReader for DirectoryClipboard {
    async fn read(&self) -> Result<ClipboardContent, String> {
        let files = self.directory.join("files.txt");
        if files.is_file() {
            let text = tokio::fs::read(&files).await.map_err(|e| e.to_string())?;
            let text = orca_tools_common::decode_text(&text);
            return Ok(ClipboardContent {
                files: Some(
                    orca_tools_common::split_lines(&text)
                        .into_iter()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect(),
                ),
                ..ClipboardContent::default()
            });
        }
        let image = self.directory.join("image.png");
        if image.is_file() {
            let png = tokio::fs::read(&image).await.map_err(|e| e.to_string())?;
            return Ok(ClipboardContent {
                png: Some(png),
                ..ClipboardContent::default()
            });
        }
        let text = self.directory.join("text.txt");
        if text.is_file() {
            let bytes = tokio::fs::read(&text).await.map_err(|e| e.to_string())?;
            return Ok(ClipboardContent {
                text: Some(orca_tools_common::decode_text(&bytes)),
                ..ClipboardContent::default()
            });
        }
        Ok(ClipboardContent::empty())
    }
}
