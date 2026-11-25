//! `SessionManager`: save, load, list, fork and delete sessions in `<config dir>/sessions`.

use std::path::{Path, PathBuf};

use super::data::{SessionData, messages_to_session, new_session_id};

/// The bridge engine ("claude", "codex") a conversation ran on and that CLI's own session id;
/// both `None` for the native loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineRef {
    pub engine: Option<String>,
    pub engine_session_id: Option<String>,
}

impl EngineRef {
    pub fn of(session: &SessionData) -> Self {
        Self {
            engine: session.engine.clone(),
            engine_session_id: session.engine_session_id.clone(),
        }
    }
}
use crate::client::{ChatMessage, Role};
use crate::config::{OrcaConfig, default_config_dir, dotnet_json};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid session file: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct SessionManager {
    dir: PathBuf,
}

impl Default for SessionManager {
    /// `<config dir>/sessions` (`OPENORCA_HOME` or `~/.openorca`).
    fn default() -> Self {
        Self::new(&default_config_dir())
    }
}

impl SessionManager {
    /// Sessions under `config_dir/sessions`.
    pub fn new(config_dir: &Path) -> Self {
        Self::with_dir(config_dir.join("sessions"))
    }

    /// Sessions directly in `dir` (the C# test constructor).
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// `SaveAsync`: write the conversation (system prompt first) as `existing_id` or a new id.
    /// The title defaults to the first user message (see [`generate_title`]).
    pub fn save(
        &self,
        messages: &[ChatMessage],
        title: Option<&str>,
        existing_id: Option<&str>,
    ) -> Result<String, SessionError> {
        self.save_with_engine(messages, title, existing_id, &EngineRef::default())
    }

    /// [`Self::save`] recording the bridge engine that ran the conversation and its own session
    /// id (C# `ConversationToSession` takes `EngineId` / `EngineSessionId`).
    pub fn save_with_engine(
        &self,
        messages: &[ChatMessage],
        title: Option<&str>,
        existing_id: Option<&str>,
        engine: &EngineRef,
    ) -> Result<String, SessionError> {
        std::fs::create_dir_all(&self.dir)?;
        let mut session = messages_to_session(messages);
        session.engine = engine.engine.clone();
        session.engine_session_id = engine.engine_session_id.clone();
        if let Some(id) = existing_id {
            session.id = id.to_string();
        }
        session.title = title
            .map(str::to_string)
            .unwrap_or_else(|| generate_title(messages));
        session.working_directory = current_dir_string();
        self.write(&session)?;
        tracing::info!("Session saved: {} ({})", session.id, session.title);
        Ok(session.id)
    }

    /// Write a session file as it is (atomically: temp file, then rename).
    pub fn write(&self, session: &SessionData) -> Result<(), SessionError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&session.id);
        let json = dotnet_json::to_string_dotnet(session)?;
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// `LoadAsync`: `None` when there is no such session.
    pub fn load(&self, id: &str) -> Result<Option<SessionData>, SessionError> {
        let path = self.path(id);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(read_session(&path)?))
    }

    /// `List`: every readable session, most recently updated first.
    pub fn list(&self) -> Vec<SessionData> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                    && p.is_file()
            })
            .collect();
        files.sort();
        let mut sessions: Vec<SessionData> = files
            .iter()
            .filter_map(|f| match read_session(f) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("Failed to read session file {}: {e}", f.display());
                    None
                }
            })
            .collect();
        // Stable, like OrderByDescending.
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        sessions
    }

    /// `Delete`: false when there is no such session.
    pub fn delete(&self, id: &str) -> Result<bool, SessionError> {
        let path = self.path(id);
        if !path.is_file() {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        tracing::info!("Session deleted: {id}");
        Ok(true)
    }

    /// `ForkAsync`: the conversation as a new session that records its parent and fork point.
    pub fn fork(
        &self,
        messages: &[ChatMessage],
        title: Option<&str>,
        parent_id: &str,
        message_index: i32,
    ) -> Result<String, SessionError> {
        let mut session = messages_to_session(messages);
        session.id = new_session_id();
        session.title = title
            .map(str::to_string)
            .unwrap_or_else(|| format!("Fork of {parent_id}"));
        session.working_directory = current_dir_string();
        session.parent_session_id = Some(parent_id.to_string());
        session.fork_point_message_index = Some(message_index);
        self.write(&session)?;
        tracing::info!(
            "Session forked: {} from parent {parent_id} at message {message_index}",
            session.id
        );
        Ok(session.id)
    }

    /// `GetSessionTree`: roots (most recent first) each followed by their forks, depth-first,
    /// with the depth for indentation. Forks whose parent is gone are not listed (as in C#).
    pub fn session_tree(&self) -> Vec<(SessionData, usize)> {
        let all = self.list();
        let mut roots = Vec::new();
        let mut children: std::collections::HashMap<String, Vec<SessionData>> = Default::default();
        for s in all {
            match s.parent_session_id.clone() {
                None => roots.push(s),
                Some(p) => children.entry(p).or_default().push(s),
            }
        }
        fn walk(
            s: SessionData,
            depth: usize,
            children: &mut std::collections::HashMap<String, Vec<SessionData>>,
            out: &mut Vec<(SessionData, usize)>,
        ) {
            let kids = children.remove(&s.id);
            out.push((s, depth));
            if let Some(mut kids) = kids {
                kids.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
                for k in kids {
                    walk(k, depth + 1, children, out);
                }
            }
        }
        let mut out = Vec::new();
        for r in roots {
            walk(r, 0, &mut children, &mut out);
        }
        out
    }

    /// The `--continue` pick: the most recent session whose working directory is `cwd` (any
    /// case), else the most recent one. The flag says whether it is from another directory.
    pub fn latest_for(&self, cwd: &Path) -> Option<(SessionData, bool)> {
        let sessions = self.list();
        let cwd = cwd.display().to_string();
        let same = |s: &SessionData| {
            s.working_directory
                .as_deref()
                .is_some_and(|w| w.to_lowercase() == cwd.to_lowercase())
        };
        if let Some(s) = sessions.iter().find(|s| same(s)) {
            return Some((s.clone(), false));
        }
        sessions.into_iter().next().map(|s| (s, true))
    }

    /// `ReplLoop.AutoSaveSessionAsync`: save under the current id (a new one the first time)
    /// when `session.autoSave` is on. Failures are logged, not returned.
    pub fn auto_save(
        &self,
        config: &OrcaConfig,
        messages: &[ChatMessage],
        current_id: &mut Option<String>,
    ) {
        if !config.session.auto_save {
            return;
        }
        match self.save(messages, None, current_id.as_deref()) {
            Ok(id) => *current_id = Some(id),
            Err(e) => tracing::warn!("Failed to auto-save session: {e}"),
        }
    }
}

fn read_session(path: &Path) -> Result<SessionData, SessionError> {
    let bytes = std::fs::read(path)?;
    // File.ReadAllText drops a UTF-8 byte order mark.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    Ok(serde_json::from_slice(bytes)?)
}

fn current_dir_string() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|d| d.display().to_string())
}

/// `GenerateTitle`: the first user message, cut to 57 UTF-16 units plus "..." when it is
/// longer than 60.
pub fn generate_title(messages: &[ChatMessage]) -> String {
    let text = messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_else(|| "Untitled".into());
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > 60 {
        format!("{}...", String::from_utf16_lossy(&units[..57]))
    } else {
        text
    }
}
