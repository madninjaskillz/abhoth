//! `CommandClipboard`: the clipboard through command-line tools.
//!
//! `osascript` / `pngpaste` / `pbpaste` on macOS, `wl-paste` on Wayland, `xclip` on X11, and
//! (Rust only, in place of C#'s P/Invoke `WindowsClipboard`) Windows PowerShell on Windows.
//! When none is installed the read fails with a message saying what to install.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures_util::future::BoxFuture;

use super::{ClipboardContent, ClipboardReader, image_codec};
use crate::attachments::pasted_path_detector::file_uri_local_path;

/// Runs a clipboard tool: exit code and stdout bytes; `None` when the tool isn't installed.
pub type ClipboardToolRunner =
    Arc<dyn Fn(String, Vec<String>) -> BoxFuture<'static, Option<(i32, Vec<u8>)>> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Wayland,
    X11,
    /// Windows PowerShell (`powershell.exe`) with System.Windows.Forms.
    Windows,
}

pub struct CommandClipboard {
    platform: Platform,
    run: ClipboardToolRunner,
}

impl CommandClipboard {
    pub fn new(platform: Platform) -> Self {
        Self::with_runner(
            platform,
            Arc::new(|file, args| Box::pin(run_process(file, args))),
        )
    }

    /// With a fake tool runner (tests).
    pub fn with_runner(platform: Platform, run: ClipboardToolRunner) -> Self {
        Self { platform, run }
    }

    async fn run(&self, file: &str, args: &[&str]) -> Option<(i32, Vec<u8>)> {
        (self.run)(
            file.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        )
        .await
    }

    // wl-paste / xclip: list the offered types, then fetch the best one.
    async fn read_listed(
        &self,
        tool: &str,
        list_args: &[&str],
        type_args: impl Fn(&str) -> Vec<String>,
        text_args: &[&str],
        missing: &str,
    ) -> ClipboardContent {
        let Some(listed) = self.run(tool, list_args).await else {
            return ClipboardContent::failed(missing);
        };
        let listed = String::from_utf8_lossy(&listed.1).into_owned();
        let types: Vec<String> = listed
            .split('\n')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase)
            .collect();
        let has = |t: &str| types.iter().any(|x| x == t);
        let fetch = |t: &str| {
            let args = type_args(t);
            async move {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.run(tool, &refs).await
            }
        };

        if has("text/uri-list")
            && let Some((0, out)) = fetch("text/uri-list").await
        {
            let files = parse_uri_list(&String::from_utf8_lossy(&out));
            if !files.is_empty() {
                return ClipboardContent {
                    files: Some(files),
                    ..ClipboardContent::default()
                };
            }
        }

        if has("image/png")
            && let Some((0, png)) = fetch("image/png").await
            && image_codec::is_png(&png)
        {
            return ClipboardContent {
                png: Some(png),
                ..ClipboardContent::default()
            };
        }

        if let Some((0, text)) = self.run(tool, text_args).await
            && !text.is_empty()
        {
            return ClipboardContent {
                text: Some(String::from_utf8_lossy(&text).into_owned()),
                ..ClipboardContent::default()
            };
        }
        ClipboardContent::empty()
    }

    async fn read_mac(&self) -> ClipboardContent {
        // Finder copy: a file URL. ("as «class furl»" fails when the clipboard holds none.)
        if let Some((0, furl)) = self
            .run(
                "osascript",
                &["-e", "POSIX path of (the clipboard as «class furl»)"],
            )
            .await
        {
            let path = String::from_utf8_lossy(&furl).trim().to_string();
            if !path.is_empty() && std::path::Path::new(&path).is_file() {
                return ClipboardContent {
                    files: Some(vec![path]),
                    ..ClipboardContent::default()
                };
            }
        }

        let pngpaste = self.run("pngpaste", &["-"]).await;
        if let Some((0, p)) = &pngpaste
            && image_codec::is_png(p)
        {
            return ClipboardContent {
                png: Some(p.clone()),
                ..ClipboardContent::default()
            };
        }
        if pngpaste.is_none()
            && let Some((0, data)) = self
                .run("osascript", &["-e", "the clipboard as «class PNGf»"])
                .await
            && let Some(bytes) = parse_apple_script_data(&String::from_utf8_lossy(&data))
            && image_codec::is_png(&bytes)
        {
            return ClipboardContent {
                png: Some(bytes),
                ..ClipboardContent::default()
            };
        }

        match self.run("pbpaste", &[]).await {
            None => ClipboardContent::failed("pbpaste was not found."),
            Some((0, text)) if !text.is_empty() => ClipboardContent {
                text: Some(String::from_utf8_lossy(&text).into_owned()),
                ..ClipboardContent::default()
            },
            Some(_) => ClipboardContent::empty(),
        }
    }

    async fn read_windows(&self) -> ClipboardContent {
        let encoded = {
            let utf16: Vec<u8> = WINDOWS_SCRIPT
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            base64::engine::general_purpose::STANDARD.encode(utf16)
        };
        let args = [
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            encoded.as_str(),
        ];
        // OpenClipboard fails while another app holds it: try a second time.
        let mut last = None;
        for _ in 0..2 {
            match self.run("powershell.exe", &args).await {
                None => {
                    return ClipboardContent::failed(
                        "Windows PowerShell (powershell.exe) was not found; it is needed to paste from the clipboard.",
                    );
                }
                Some((0, out)) => return parse_windows_output(&String::from_utf8_lossy(&out)),
                Some(other) => last = Some(other),
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let _ = last;
        ClipboardContent::failed("The clipboard is in use by another app; try again.")
    }
}

#[async_trait::async_trait]
impl ClipboardReader for CommandClipboard {
    async fn read(&self) -> Result<ClipboardContent, String> {
        Ok(match self.platform {
            Platform::MacOS => self.read_mac().await,
            Platform::Wayland => {
                self.read_listed(
                    "wl-paste",
                    &["--list-types"],
                    |t| vec!["--no-newline".into(), "--type".into(), t.into()],
                    &["--no-newline"],
                    "Install wl-clipboard (wl-paste) to paste from the clipboard.",
                )
                .await
            }
            Platform::X11 => {
                self.read_listed(
                    "xclip",
                    &["-selection", "clipboard", "-t", "TARGETS", "-o"],
                    |t| {
                        vec![
                            "-selection".into(),
                            "clipboard".into(),
                            "-t".into(),
                            t.into(),
                            "-o".into(),
                        ]
                    },
                    &["-selection", "clipboard", "-o"],
                    "Install xclip to paste from the clipboard.",
                )
                .await
            }
            Platform::Windows => self.read_windows().await,
        })
    }
}

/// The PowerShell side of the Windows reader: one tag line (FILES / PNG / DIB / TEXT / EMPTY),
/// then base64 lines (file names as UTF-8, image bytes, UTF-8 text), so no console code page
/// gets in the way. The order is `WindowsClipboard`'s: CF_HDROP, "PNG", CF_DIBV5 / CF_DIB,
/// text.
const WINDOWS_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$c = [System.Windows.Forms.Clipboard]
$o = $c::GetDataObject()
function B([byte[]]$b) { [Convert]::ToBase64String($b) }
function S($x) { if ($x -is [System.IO.Stream]) { $m = New-Object System.IO.MemoryStream; $x.CopyTo($m); return ,$m.ToArray() }; if ($x -is [byte[]]) { return ,$x }; return $null }
if ($null -eq $o) { 'EMPTY'; exit 0 }
if ($c::ContainsFileDropList()) { 'FILES'; foreach ($f in $c::GetFileDropList()) { B ([Text.Encoding]::UTF8.GetBytes($f)) }; exit 0 }
if ($o.GetDataPresent('PNG')) { $b = S ($o.GetData('PNG')); if ($null -ne $b) { 'PNG'; B $b; exit 0 } }
foreach ($fmt in @('Format17', 'DeviceIndependentBitmap')) { if ($o.GetDataPresent($fmt)) { $b = S ($o.GetData($fmt)); if ($null -ne $b) { 'DIB'; B $b; exit 0 } } }
if ($c::ContainsText()) { 'TEXT'; B ([Text.Encoding]::UTF8.GetBytes($c::GetText())); exit 0 }
'EMPTY'
"#;

/// The Windows reader's output as clipboard content.
pub(crate) fn parse_windows_output(output: &str) -> ClipboardContent {
    let mut lines = output.lines().map(str::trim).filter(|l| !l.is_empty());
    let tag = lines.next().unwrap_or("EMPTY");
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok();
    match tag {
        "FILES" => {
            let files: Vec<String> = lines
                .filter_map(b64)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .collect();
            ClipboardContent {
                files: Some(files),
                ..ClipboardContent::default()
            }
        }
        "PNG" => match lines.next().and_then(b64) {
            Some(png) if image_codec::is_png(&png) => ClipboardContent {
                png: Some(png),
                ..ClipboardContent::default()
            },
            _ => ClipboardContent::empty(),
        },
        "DIB" => match lines
            .next()
            .and_then(b64)
            .and_then(|d| image_codec::dib_to_png(&d))
        {
            Some(png) => ClipboardContent {
                png: Some(png),
                ..ClipboardContent::default()
            },
            None => ClipboardContent::empty(),
        },
        "TEXT" => {
            let text = lines
                .next()
                .and_then(b64)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            // CF_UNICODETEXT ends at the first NUL.
            let text = text.split('\0').next().unwrap_or_default().to_string();
            ClipboardContent {
                text: Some(text),
                ..ClipboardContent::default()
            }
        }
        _ => ClipboardContent::empty(),
    }
}

/// The files in a `text/uri-list` (file:// lines; comments and other schemes skipped).
pub fn parse_uri_list(list: &str) -> Vec<String> {
    list.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_local_path)
        .collect()
}

/// AppleScript's `«data PNGf89504E47…»` as bytes.
pub fn parse_apple_script_data(output: &str) -> Option<Vec<u8>> {
    const OPEN: &str = "«data ";
    let start = output.find(OPEN)?;
    let hex_start = start + OPEN.len() + 4; // skip the four-letter type code
    if hex_start > output.len() || !output.is_char_boundary(hex_start) {
        return None;
    }
    let end = hex_start + output[hex_start..].find('»')?;
    let hex = &output[hex_start..end];
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

async fn run_process(file: String, args: Vec<String>) -> Option<(i32, Vec<u8>)> {
    let mut cmd = tokio::process::Command::new(&file);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = cmd.spawn().ok()?; // not installed
    // PowerShell takes a moment to start; the other tools get C#'s 5 seconds.
    let limit = if file.eq_ignore_ascii_case("powershell.exe") {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(5)
    };
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(out)) => Some((out.status.code().unwrap_or(-1), out.stdout)),
        Ok(Err(_)) | Err(_) => Some((-1, Vec::new())),
    }
}
