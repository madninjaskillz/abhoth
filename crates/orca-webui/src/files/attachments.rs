//! `AttachmentEndpoints`: files pasted or dropped into the chat (`POST attachments`, saved in
//! `.orca/attachments`, git-ignored) and raw project files for the chat's inline previews
//! (`GET/HEAD raw`, with single byte ranges so audio and video can seek).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use crate::audit::Caller;
use crate::paths;
use crate::projects::manager::ProjectRuntime;
use crate::server::AppState;

pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_FILES: usize = 10;
pub const FOLDER: &str = ".orca/attachments";

fn ext_of(path: &str) -> String {
    FsPath::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

/// The type a raw file is served with, and whether the browser may show it inline.
pub fn content_type_for(path: &str) -> (&'static str, bool) {
    let t = match ext_of(path).as_str() {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".bmp" => "image/bmp",
        ".ico" => "image/x-icon",
        ".avif" => "image/avif",
        ".pdf" => "application/pdf",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" | ".oga" | ".opus" => "audio/ogg",
        ".flac" => "audio/flac",
        ".m4a" => "audio/mp4",
        ".aac" => "audio/aac",
        ".mp4" | ".m4v" => "video/mp4",
        ".webm" => "video/webm",
        ".mov" => "video/quicktime",
        ".ogv" => "video/ogg",
        ".txt" | ".log" | ".md" | ".csv" | ".json" => "text/plain; charset=utf-8",
        _ => return ("application/octet-stream", false),
    };
    (t, true)
}

fn is_text(b: &[u8]) -> bool {
    let n = b.len().min(64 * 1024);
    let window = &b[..n];
    if window.contains(&0) {
        return false;
    }
    match std::str::from_utf8(window) {
        Ok(_) => true,
        // A UTF-8 sequence cut by the 64 KB window isn't an error.
        Err(e) => e.error_len().is_none(),
    }
}

/// "image", "file" (PDF or text) or `None` (not attachable).
pub fn classify(name: &str, b: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| b.starts_with(sig);
    if starts(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
        || starts(&[0xFF, 0xD8, 0xFF])
        || starts(b"GIF8")
        || (b.len() >= 12 && starts(b"RIFF") && &b[8..12] == b"WEBP")
    {
        return Some("image");
    }
    if starts(b"%PDF-") {
        return Some("file");
    }
    let ext = ext_of(name);
    if matches!(
        ext.as_str(),
        ".exe"
            | ".dll"
            | ".msi"
            | ".com"
            | ".scr"
            | ".bat"
            | ".cmd"
            | ".ps1"
            | ".vbs"
            | ".jar"
            | ".so"
            | ".dylib"
    ) {
        return None;
    }
    is_text(b).then_some("file")
}

/// A file name safe on every OS (no path, no reserved characters, not hidden, at most 80).
pub fn safe_name(file_name: Option<&str>) -> String {
    let raw = file_name.unwrap_or("").replace('\\', "/");
    let name = raw.rsplit('/').next().unwrap_or("");
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() {
        s = "pasted".into();
    }
    if s.chars().count() > 80 {
        let ext = FsPath::new(&s)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let keep = 80usize.saturating_sub(ext.chars().count());
        s = s.chars().take(keep).collect::<String>() + &ext;
    }
    s
}

fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| {
            if (' '..'\u{7f}').contains(&c) && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects/{pid}/raw", get(raw))
        .route(
            "/api/projects/{pid}/attachments",
            post(upload).layer(DefaultBodyLimit::max(
                MAX_FILES * MAX_FILE_BYTES + 1024 * 1024,
            )),
        )
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}

fn strip_file_url(p: &str) -> String {
    match p.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("file://") => {
            let rest = percent_encoding::percent_decode_str(&p[7..])
                .decode_utf8_lossy()
                .into_owned();
            // file:///C:/x -> C:/x on Windows; file:///x -> /x elsewhere.
            if cfg!(windows) {
                rest.trim_start_matches('/').to_string()
            } else {
                rest
            }
        }
        _ => p.to_string(),
    }
}

/// A project file: relative to the folder shown, or absolute inside the project or one of its
/// worktrees. `None` when outside.
pub async fn resolve(rt: &ProjectRuntime, folder: &FsPath, path: &str) -> Option<PathBuf> {
    if path.trim().is_empty() {
        return None;
    }
    let p = strip_file_url(path.trim());
    let pb = PathBuf::from(&p);
    let rooted = pb.is_absolute() || (cfg!(windows) && p.starts_with("\\\\"));
    if !rooted {
        return paths::resolve(folder, Some(&p));
    }
    let full = paths::normalize(&p);
    let mut roots = vec![folder.to_path_buf(), rt.root()];
    roots.extend(
        rt.worktrees()
            .await
            .into_iter()
            .map(|w| PathBuf::from(w.path)),
    );
    for root in roots {
        let root = paths::normalize_path(&root);
        if paths::is_under(&full, &root) {
            return paths::resolve(&root, Some(&paths::to_relative(&root, &full)));
        }
    }
    None
}

/// A generated image (the image tool's output folder): png/jpg/gif/webp that really is one.
pub fn resolve_generated_image(image_dir: Option<&FsPath>, path: &str) -> Option<PathBuf> {
    let dir = image_dir.filter(|d| d.is_absolute())?;
    let p = strip_file_url(path.trim());
    if p.contains('\0') || !FsPath::new(&p).is_absolute() {
        return None;
    }
    if !matches!(
        ext_of(&p).as_str(),
        ".png" | ".jpg" | ".jpeg" | ".gif" | ".webp"
    ) {
        return None;
    }
    let root = paths::normalize_path(dir);
    let full = paths::normalize(&p);
    if !paths::is_under(&full, &root) || full == root {
        return None;
    }
    let inside = paths::resolve(&root, Some(&paths::to_relative(&root, &full)))?;
    let mut head = [0u8; 12];
    let n = {
        use std::io::Read;
        let mut f = std::fs::File::open(&inside).ok()?;
        f.read(&mut head).ok()?
    };
    (classify(&inside.to_string_lossy(), &head[..n]) == Some("image")).then_some(inside)
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against `len`: the inclusive range, or `None`.
pub fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        (start, end)
    };
    (start <= end && start < len).then_some((start, end))
}

async fn raw(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(rt) = s.projects.get(&pid) else {
        return err(StatusCode::NOT_FOUND, "No such project or worktree.");
    };
    let Some(folder) = rt.resolve_worktree(q.get("wt").map(String::as_str)).await else {
        return err(StatusCode::NOT_FOUND, "No such project or worktree.");
    };
    let path = q.get("path").cloned().unwrap_or_default();
    let image_dir = s.options.image_directory.as_ref().and_then(|f| f());
    let full = match resolve(&rt, &folder, &path).await {
        Some(f) => Some(f),
        None => resolve_generated_image(image_dir.as_deref(), &path),
    };
    let Some(full) = full else {
        return err(StatusCode::FORBIDDEN, "That path is outside the project.");
    };
    let Ok(meta) = std::fs::metadata(&full) else {
        return err(StatusCode::NOT_FOUND, "No such file.");
    };
    if !meta.is_file() {
        return err(StatusCode::NOT_FOUND, "No such file.");
    }
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (mut ctype, inline) = content_type_for(&name);
    // An SVG loaded by an <img> is an image (an <img> never runs its scripts); else a download.
    if ctype == "application/octet-stream"
        && ext_of(&name) == ".svg"
        && headers.get("sec-fetch-dest").is_some_and(|v| v == "image")
    {
        ctype = "image/svg+xml";
    }
    let len = meta.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range(h, len));
    let data = match tokio::fs::read(&full).await {
        Ok(d) => d,
        Err(_) => return err(StatusCode::NOT_FOUND, "No such file."),
    };
    let (status, body) = match range {
        Some((a, b)) => (
            StatusCode::PARTIAL_CONTENT,
            data[a as usize..=b as usize].to_vec(),
        ),
        None => (StatusCode::OK, data),
    };
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some((a, b)) = range
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {a}-{b}/{len}"))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    if ctype != "application/pdf" {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'"));
    } else {
        // The viewer shows PDFs in a frame of this site (and only this site).
        h.insert(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        );
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-ancestors 'self'"),
        );
    }
    let disposition = format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        ascii(&name),
        crate::util::escape_data(&name)
    );
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    r
}

fn stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let iso = crate::util::iso_from_unix(now.as_secs() as i64, 0);
    // "2026-11-25T04:09:01Z" -> "20261125-040901"
    let digits: String = iso.chars().filter(char::is_ascii_digit).collect();
    format!("{}-{}", &digits[..8], &digits[8..14.min(digits.len())])
}

async fn upload(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    let Some(rt) = s.projects.get(&pid) else {
        return err(StatusCode::NOT_FOUND, "No such project.");
    };
    if rt.read_only {
        return err(StatusCode::FORBIDDEN, "This tab is read-only.");
    }
    let Ok(mut form) = multipart else {
        return err(
            StatusCode::BAD_REQUEST,
            "Send the files as multipart/form-data.",
        );
    };
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    loop {
        match form.next_field().await {
            Ok(Some(field)) => {
                let Some(file_name) = field.file_name().map(str::to_string) else {
                    continue;
                };
                match field.bytes().await {
                    Ok(b) => files.push((file_name, b.to_vec())),
                    Err(_) => {
                        return err(StatusCode::PAYLOAD_TOO_LARGE, "The upload is too large.");
                    }
                }
                if files.len() > MAX_FILES {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("At most {MAX_FILES} files at a time."),
                    );
                }
            }
            Ok(None) => break,
            Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "The upload is too large."),
        }
    }
    if files.is_empty() {
        return err(StatusCode::BAD_REQUEST, "No files.");
    }
    for (name, b) in &files {
        if b.len() > MAX_FILE_BYTES {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("{name} is over {} MB.", MAX_FILE_BYTES / (1024 * 1024)),
            );
        }
    }
    let root = rt.root();
    let dir = root.join(".orca").join("attachments");
    if std::fs::create_dir_all(&dir).is_err() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Couldn't save the files.",
        );
    }
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n"); // pastes stay out of git
    }
    let mut saved = Vec::new();
    for (file_name, bytes) in files {
        let name = safe_name(Some(&file_name));
        let Some(kind) = classify(&name, &bytes) else {
            return err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &format!("{file_name}: only images, PDFs and text files can be attached."),
            );
        };
        let file = dir.join(format!(
            "web-{}-{}-{name}",
            stamp(),
            crate::util::short_id(6)
        ));
        if tokio::fs::write(&file, &bytes).await.is_err() {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't save the files.",
            );
        }
        saved.push(json!({
            "name": name,
            "path": paths::to_relative(&root, &file),
            "kind": kind,
            "size": bytes.len(),
        }));
    }
    s.audit.write(
        Some(&Caller {
            ip: Some(addr.ip().to_string()),
            user: None,
        }),
        "attachments.upload",
        json!({"project": pid, "files": saved.len()}),
    );
    axum::Json(saved).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_magic_and_text() {
        assert_eq!(classify("x.bin", b"\x89PNG\r\n\x1a\nrest"), Some("image"));
        assert_eq!(classify("x", b"%PDF-1.7"), Some("file"));
        assert_eq!(classify("notes.md", "héllo".as_bytes()), Some("file"));
        assert_eq!(classify("run.exe", b"MZ text"), None);
        assert_eq!(classify("blob", b"a\0b"), None);
        assert_eq!(classify("page.svg", b"<svg/>"), Some("file"));
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_name(Some("C:\\x\\..\\a<b>.png")), "a_b_.png");
        assert_eq!(safe_name(Some(".env")), "env");
        assert_eq!(safe_name(None), "pasted");
        let long = format!("{}.txt", "a".repeat(100));
        assert_eq!(safe_name(Some(&long)).chars().count(), 80);
        assert!(safe_name(Some(&long)).ends_with(".txt"));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
        assert_eq!(content_type_for("a.PNG"), ("image/png", true));
        assert_eq!(
            content_type_for("a.svg"),
            ("application/octet-stream", false)
        );
    }
}
