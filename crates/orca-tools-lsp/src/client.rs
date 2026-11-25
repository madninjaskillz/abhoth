//! `LspClientManager`: one language server process per language, started lazily, spoken to
//! over stdio with `Content-Length` framed JSON-RPC.
//!
//! The C# client uses StreamJsonRpc with its default (Newtonsoft) formatter, which ignores the
//! protocol types' `[JsonPropertyName]` attributes (so it sends `TextDocument`/`Position`),
//! wraps notification parameters in an array, and cannot read a result into a `JsonElement`.
//! This port sends what the LSP specification says (see the port's decisions log).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

use crate::protocol::{Diagnostic, PublishDiagnosticsParams};

/// How long one request may take before it fails (C# waits forever).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// One configured language server (`LspServerConfig`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub root_uri: Option<String>,
}

/// A failed request: the server's error message (`RemoteInvocationException.Message`) or a
/// transport failure.
#[derive(Debug, Clone, PartialEq)]
pub struct LspError(pub String);

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;
type DiagnosticStore = Arc<Mutex<HashMap<String, HashMap<String, Vec<Diagnostic>>>>>;

/// A running server connection.
pub struct LspConnection {
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    exited: Arc<AtomicBool>,
}

impl LspConnection {
    /// Whether the server process has gone (its output closed).
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Send a request and wait for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send(&msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError("The language server connection was lost.".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(LspError(format!("'{method}' timed out.")))
            }
        }
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn send(&self, msg: &Value) -> Result<(), LspError> {
        let body = msg.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| LspError(e.to_string()))?;
        stdin.flush().await.map_err(|e| LspError(e.to_string()))
    }

    async fn shutdown(&self) {
        if !self.has_exited() {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                self.request("shutdown", Value::Null),
            )
            .await;
            let _ = self.notify("exit", Value::Null).await;
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

/// Manages the language servers per language (`LspClientManager`).
pub struct LspClientManager {
    configs: HashMap<String, LspServerConfig>,
    root_path: PathBuf,
    connections: tokio::sync::Mutex<HashMap<String, Arc<LspConnection>>>,
    versions: Mutex<HashMap<String, HashMap<String, i32>>>,
    diagnostics: DiagnosticStore,
}

impl LspClientManager {
    pub fn new(configs: HashMap<String, LspServerConfig>, root_path: impl Into<PathBuf>) -> Self {
        Self {
            configs,
            root_path: root_path.into(),
            connections: tokio::sync::Mutex::new(HashMap::new()),
            versions: Mutex::new(HashMap::new()),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The connection for `language_id`, starting the server on first use; `None` when no
    /// server is configured or it fails to start (logged).
    pub async fn get_client(&self, language_id: &str) -> Option<Arc<LspConnection>> {
        let mut connections = self.connections.lock().await;
        if let Some(existing) = connections.get(language_id)
            && !existing.has_exited()
        {
            return Some(existing.clone());
        }
        let config = self.configs.get(language_id)?;
        match self.start_server(language_id, config).await {
            Ok(c) => {
                connections.insert(language_id.to_string(), c.clone());
                Some(c)
            }
            Err(e) => {
                tracing::warn!("Failed to start LSP server for {language_id}: {e}");
                None
            }
        }
    }

    /// `textDocument/didOpen` (version 1) for the file's language server, if there is one.
    pub async fn notify_did_open(&self, file_path: &Path, content: &str) {
        let Some(lang) = detect_language(file_path) else {
            return;
        };
        let Some(conn) = self.get_client(lang).await else {
            return;
        };
        let uri = path_to_uri(file_path);
        self.versions
            .lock()
            .unwrap()
            .entry(lang.to_string())
            .or_default()
            .insert(uri.clone(), 1);
        let params = json!({"textDocument": {"uri": uri, "languageId": lang, "version": 1, "text": content}});
        if let Err(e) = conn.notify("textDocument/didOpen", params).await {
            tracing::debug!("Failed to send didOpen for {}: {e}", file_path.display());
        }
    }

    /// `textDocument/didChange` with the whole new content and the next version.
    pub async fn notify_did_change(&self, file_path: &Path, content: &str) {
        let Some(lang) = detect_language(file_path) else {
            return;
        };
        let Some(conn) = self.get_client(lang).await else {
            return;
        };
        let uri = path_to_uri(file_path);
        let version = {
            let mut versions = self.versions.lock().unwrap();
            let v = versions
                .entry(lang.to_string())
                .or_default()
                .entry(uri.clone())
                .and_modify(|v| *v += 1)
                .or_insert(2);
            *v
        };
        let params = json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": content}]});
        if let Err(e) = conn.notify("textDocument/didChange", params).await {
            tracing::debug!("Failed to send didChange for {}: {e}", file_path.display());
        }
    }

    /// The latest diagnostics a server published for the file.
    pub fn get_diagnostics(&self, file_path: &Path) -> Vec<Diagnostic> {
        let uri = path_to_uri(file_path);
        let store = self.diagnostics.lock().unwrap();
        store
            .values()
            .find_map(|by_uri| by_uri.get(&uri).cloned())
            .unwrap_or_default()
    }

    /// Whether a server is configured for the language.
    pub fn has_server(&self, language_id: &str) -> bool {
        self.configs.contains_key(language_id)
    }

    /// Shut every server down (`shutdown`, `exit`, then kill after 3 s).
    pub async fn dispose(&self) {
        let connections: Vec<_> = self.connections.lock().await.drain().collect();
        for (_, c) in connections {
            c.shutdown().await;
        }
    }

    async fn start_server(
        &self,
        language_id: &str,
        config: &LspServerConfig,
    ) -> Result<Arc<LspConnection>, String> {
        tracing::info!(
            "Starting LSP server for {language_id}: {} {}",
            config.command,
            config.args.join(" ")
        );
        let mut child = tokio::process::Command::new(&config.command)
            .args(&config.args)
            .current_dir(&self.root_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to start LSP server: {}: {e}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            let lang = language_id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("LSP {lang} stderr: {line}");
                }
            });
        }
        let conn = Arc::new(LspConnection {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicI64::new(1),
            exited: Arc::new(AtomicBool::new(false)),
        });
        tokio::spawn(read_loop(
            stdout,
            conn.clone(),
            language_id.to_string(),
            self.diagnostics.clone(),
        ));

        let root_uri = match config.root_uri.as_deref().filter(|r| !r.is_empty()) {
            Some(r) => path_to_uri(&orca_abstractions::full_path(&self.root_path, r)),
            None => path_to_uri(&self.root_path),
        };
        let caps = json!({"dynamicRegistration": false});
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {"textDocument": {
                "definition": caps, "references": caps, "hover": caps,
                "documentSymbol": caps, "callHierarchy": caps
            }}
        });
        conn.request("initialize", params)
            .await
            .map_err(|e| e.to_string())?;
        conn.notify("initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("LSP server for {language_id} initialized successfully");
        Ok(conn)
    }
}

/// Reads the server's messages: responses complete their requests, published diagnostics are
/// stored, and server requests are refused (method not found), as StreamJsonRpc does for
/// methods it has no handler for.
async fn read_loop(
    stdout: ChildStdout,
    conn: Arc<LspConnection>,
    language_id: String,
    diagnostics: DiagnosticStore,
) {
    let mut reader = BufReader::new(stdout);
    while let Some(msg) = read_frame(&mut reader).await {
        let method = msg.get("method").and_then(Value::as_str);
        match (msg.get("id"), method) {
            (Some(id), None) => {
                let Some(id) = id.as_i64() else { continue };
                let Some(tx) = conn.pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let result = match msg.get("error") {
                    Some(err) if !err.is_null() => Err(LspError(
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Unknown error")
                            .to_string(),
                    )),
                    _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            (None, Some("textDocument/publishDiagnostics")) => {
                if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(
                    msg.get("params").cloned().unwrap_or(Value::Null),
                ) {
                    diagnostics
                        .lock()
                        .unwrap()
                        .entry(language_id.clone())
                        .or_default()
                        .insert(p.uri, p.diagnostics);
                }
            }
            (Some(id), Some(method)) => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601,
                    "message": format!("No method by the name '{method}' is found.")}});
                let _ = conn.send(&reply).await;
            }
            _ => {}
        }
    }
    conn.exited.store(true, Ordering::SeqCst);
    // Fail whatever is still waiting.
    conn.pending.lock().unwrap().clear();
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Option<Value> {
    loop {
        let mut len: Option<usize> = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().ok();
            }
        }
        let Some(len) = len else { continue };
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await.ok()?;
        if let Ok(v) = serde_json::from_slice(&body) {
            return Some(v);
        }
    }
}

/// `DetectLanguage`: the language id for the file's extension (case-insensitive).
pub fn detect_language(file_path: &Path) -> Option<&'static str> {
    let ext = file_path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "cs" => "csharp",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "go" => "go",
        "rs" => "rust",
        "java" => "java",
        "rb" => "ruby",
        "cpp" | "cc" | "cxx" | "hpp" | "h" => "cpp",
        "c" => "c",
        "lua" => "lua",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        _ => return None,
    })
}

/// `PathToUri`: `file:///C:/x/y` on Windows, `file:///x/y` elsewhere; only spaces escaped.
pub fn path_to_uri(file_path: &Path) -> String {
    let full = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        std::path::absolute(file_path).unwrap_or_else(|_| file_path.to_path_buf())
    };
    let s = full.display().to_string();
    if cfg!(windows) {
        format!("file:///{}", s.replace('\\', "/").replace(' ', "%20"))
    } else {
        format!("file://{}", s.replace(' ', "%20"))
    }
}

/// `UriToPath`: the reverse of [`path_to_uri`], percent-decoding the rest.
pub fn uri_to_path(uri: &str) -> String {
    if cfg!(windows)
        && let Some(rest) = uri.strip_prefix("file:///")
    {
        return unescape_data_string(rest).replace('/', "\\");
    }
    if let Some(rest) = uri.strip_prefix("file://") {
        return unescape_data_string(rest);
    }
    uri.to_string()
}

/// `Uri.UnescapeDataString`: `%XX` sequences decoded as UTF-8; invalid ones kept as written.
fn unescape_data_string(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}
