//! Just enough `multipart/form-data` for OpenAI's transcription uploads.

use axum::body::Bytes;

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Bytes,
}

impl Part {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).trim().to_string()
    }
}

/// The boundary from a `content-type` header.
pub fn boundary(content_type: &str) -> Option<String> {
    let (mime, params) = content_type.split_once(';')?;
    if !mime.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
            .filter(|b| !b.is_empty())
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= hay.len() {
        let p = hay[i..hay.len() - needle.len() + 1]
            .iter()
            .position(|b| *b == first)?;
        i += p;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn param(disposition: &str, key: &str) -> Option<String> {
    disposition.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

pub fn parse(content_type: &str, body: &Bytes) -> Result<Vec<Part>, String> {
    let b = boundary(content_type).ok_or("the body must be multipart/form-data")?;
    let delim = format!("--{b}").into_bytes();
    let next_delim = format!("\r\n--{b}").into_bytes();
    let mut at = find(body, &delim, 0).ok_or("no multipart boundary in the body")? + delim.len();
    let mut parts = Vec::new();
    loop {
        if body[at..].starts_with(b"--") {
            break;
        }
        if body[at..].starts_with(b"\r\n") {
            at += 2;
        }
        let head_end = find(body, b"\r\n\r\n", at).ok_or("a multipart part has no headers")?;
        let head = String::from_utf8_lossy(&body[at..head_end]).to_string();
        let start = head_end + 4;
        let end = find(body, &next_delim, start).ok_or("a multipart part is not closed")?;
        let mut name = String::new();
        let mut filename = None;
        let mut content_type = None;
        for line in head.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            match k.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    name = param(v, "name").unwrap_or_default();
                    filename = param(v, "filename");
                }
                "content-type" => content_type = Some(v.trim().to_string()),
                _ => {}
            }
        }
        parts.push(Part {
            name,
            filename,
            content_type,
            data: body.slice(start..end),
        });
        at = end + next_delim.len();
        if at >= body.len() {
            break;
        }
    }
    Ok(parts)
}
