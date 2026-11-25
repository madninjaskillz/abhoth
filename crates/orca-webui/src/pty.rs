//! The terminal panel (`PtyEndpoint`): `/api/projects/{pid}/pty?cols=&rows=&wt=` is a
//! websocket bridged to a shell on a pseudo-console (portable-pty: ConPTY on Windows, a Unix
//! pty elsewhere) in the project's folder or one of its worktrees. Server to client: binary
//! frames of raw VT output. Client to server: text frames of JSON, `{"t":"i","d":"…"}` input
//! and `{"t":"r","c":120,"r":30}` resize (binary frames are raw input). The shell lives as long
//! as the socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

use crate::audit::Caller;
use crate::server::AppState;

type SharedChild = Arc<Mutex<Box<dyn Child + Send + Sync>>>;

/// Terminals open in browsers; all closed when the server stops.
#[derive(Default)]
pub struct PtyRegistry {
    open: Mutex<HashMap<u64, SharedChild>>,
    next: Mutex<u64>,
}

impl PtyRegistry {
    fn add(&self, child: SharedChild) -> u64 {
        let mut n = self.next.lock().unwrap();
        *n += 1;
        self.open.lock().unwrap().insert(*n, child);
        *n
    }

    fn remove(&self, id: u64) {
        self.open.lock().unwrap().remove(&id);
    }

    pub fn count(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    pub fn close_all(&self) {
        let all: Vec<SharedChild> = self.open.lock().unwrap().drain().map(|(_, c)| c).collect();
        for c in all {
            let _ = c.lock().unwrap().kill();
        }
    }
}

/// ConPTY (Windows 10 1809+) or a Unix pseudo-terminal.
pub fn is_supported() -> bool {
    cfg!(any(windows, unix))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/projects/{pid}/pty", get(open))
}

fn clamp(v: Option<&String>, fallback: u16, min: u16, max: u16) -> u16 {
    v.and_then(|s| s.parse::<i64>().ok())
        .map(|n| n.clamp(min as i64, max as i64) as u16)
        .unwrap_or(fallback)
}

/// A command line split on spaces, double quotes grouping (enough for "a shell and its flags").
pub fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in command.chars() {
        if c == '"' {
            quoted = !quoted;
            any = true;
            continue;
        }
        if c == ' ' && !quoted {
            if any {
                parts.push(std::mem::take(&mut current));
            }
            current.clear();
            any = false;
            continue;
        }
        current.push(c);
        any = true;
    }
    if any {
        parts.push(current);
    }
    if parts.is_empty() {
        parts.push("/bin/sh".into());
    }
    parts
}

fn find_on_path(exe: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// Windows: pwsh when installed, else Windows PowerShell, else cmd. Elsewhere: $SHELL, else
/// bash, else sh, as a login shell.
pub fn default_shell() -> String {
    if !cfg!(windows) {
        let mut sh = std::env::var("SHELL").unwrap_or_default();
        if sh.is_empty() || !std::path::Path::new(&sh).is_file() {
            sh = if std::path::Path::new("/bin/bash").is_file() {
                "/bin/bash".into()
            } else {
                "/bin/sh".into()
            };
        }
        return if sh.contains(' ') {
            format!("\"{sh}\" -l")
        } else {
            format!("{sh} -l")
        };
    }
    for candidate in ["pwsh.exe", "powershell.exe"] {
        if let Some(p) = find_on_path(candidate) {
            return format!("\"{}\" -NoLogo", p.display());
        }
    }
    std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into())
}

async fn open(
    State(s): State<Arc<AppState>>,
    Path(pid): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !is_supported() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "The terminal panel needs Windows 10 1809+ (ConPTY) or Linux with setsid (util-linux).",
        )
            .into_response();
    }
    let cols = clamp(q.get("cols"), 100, 20, 500);
    let rows = clamp(q.get("rows"), 30, 5, 300);
    let Some(rt) = s.projects.get(&pid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(folder) = rt.resolve_worktree(q.get("wt").map(String::as_str)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cwd = if folder.is_dir() {
        folder
    } else {
        std::env::current_dir().unwrap_or(folder)
    };
    s.audit.write(
        Some(&Caller {
            ip: Some(addr.ip().to_string()),
            user: None,
        }),
        "terminal.open",
        json!({"project": pid, "cwd": cwd.to_string_lossy()}),
    );
    let shell = s.options.shell.clone().unwrap_or_else(default_shell);
    let registry = s.ptys.clone();
    ws.on_upgrade(move |socket| run(socket, shell, cwd, cols, rows, registry))
}

struct Pty {
    master: Box<dyn MasterPty + Send>,
    child: SharedChild,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
}

fn start(shell: &str, cwd: &std::path::Path, cols: u16, rows: u16) -> anyhow::Result<Pty> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let parts = split_command(shell);
    let mut cmd = CommandBuilder::new(&parts[0]);
    cmd.args(&parts[1..]);
    cmd.cwd(cwd);
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    Ok(Pty {
        master: pair.master,
        child: Arc::new(Mutex::new(child)),
        writer,
        reader,
    })
}

async fn run(
    socket: WebSocket,
    shell: String,
    cwd: std::path::PathBuf,
    cols: u16,
    rows: u16,
    registry: Arc<PtyRegistry>,
) {
    let (mut sink, mut stream) = socket.split();
    let started = {
        let (shell, cwd) = (shell.clone(), cwd.clone());
        tokio::task::spawn_blocking(move || start(&shell, &cwd, cols, rows)).await
    };
    let pty = match started {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            let msg = format!("\r\nCould not start the shell ({shell}): {e}\r\n");
            let _ = sink.send(Message::Binary(msg.into_bytes().into())).await;
            let _ = sink.close().await;
            return;
        }
        Err(e) => {
            tracing::warn!("pty start task failed: {e}");
            return;
        }
    };
    let Pty {
        master,
        child,
        mut writer,
        mut reader,
    } = pty;
    let id = registry.add(child.clone());

    // Output: a blocking read on its own thread, forwarded as binary frames.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // Exit watcher: the shell exiting ends the session.
    let exited = {
        let child = child.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if !matches!(child.lock().unwrap().try_wait(), Ok(None)) {
                    break;
                }
            }
        }
    };
    tokio::pin!(exited);
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    let _ = writer.write_all(&b);
                    let _ = writer.flush();
                }
                Some(Ok(Message::Text(t))) => control(&*master, &mut writer, t.as_str()),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = &mut exited => {
                // Drain what the shell printed last.
                while let Ok(bytes) = out_rx.try_recv() {
                    let _ = sink.send(Message::Binary(bytes.into())).await;
                }
                break;
            }
        }
    }
    registry.remove(id);
    let _ = child.lock().unwrap().kill();
    drop(writer);
    drop(master);
    let _ = sink.close().await;
}

fn control(master: &dyn MasterPty, writer: &mut Box<dyn Write + Send>, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return; // malformed control frames are ignored
    };
    match v["t"].as_str() {
        Some("i") => {
            if let Some(d) = v["d"].as_str() {
                let _ = writer.write_all(d.as_bytes());
                let _ = writer.flush();
            }
        }
        Some("r") => {
            if let (Some(c), Some(r)) = (v["c"].as_i64(), v["r"].as_i64()) {
                let _ = master.resize(PtySize {
                    rows: r.clamp(2, 300) as u16,
                    cols: c.clamp(2, 500) as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_groups_quotes() {
        assert_eq!(
            split_command("\"C:\\Program Files\\pwsh.exe\" -NoLogo"),
            vec!["C:\\Program Files\\pwsh.exe", "-NoLogo"]
        );
        assert_eq!(split_command("  "), vec!["/bin/sh"]);
        assert_eq!(split_command("sh -l"), vec!["sh", "-l"]);
    }

    #[test]
    fn sizes_are_clamped() {
        assert_eq!(clamp(Some(&"9999".to_string()), 100, 20, 500), 500);
        assert_eq!(clamp(Some(&"x".to_string()), 100, 20, 500), 100);
        assert_eq!(clamp(None, 30, 5, 300), 30);
    }
}
