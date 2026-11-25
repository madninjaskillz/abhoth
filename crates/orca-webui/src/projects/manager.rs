//! `ProjectRuntime` and `ProjectManager`: the projects (tabs) the web UI shows, the persisted
//! list plus the primary project (the folder the session runs in, always present and first).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::files::edit_tracker::{EditMarkers, EditTracker};
use crate::files::watcher::FolderWatcher;
use crate::git::tokens::{APP_LINK, GitTokenStore};
use crate::git::worktrees::{self, WorktreeInfo};
use crate::live::{LiveSession, SessionFactory};
use crate::paths;
use crate::projects::store::{EngineChoiceStore, ProjectInfo, ProjectStore};

/// Something that changed, for the hub to tell the browsers.
#[derive(Clone)]
pub enum ProjectEvent {
    /// A project's session was created (the server starts forwarding its events).
    RuntimeCreated(Arc<ProjectRuntime>),
    /// A project's edit markers changed.
    Markers(String, EditMarkers),
    /// A project's files changed: (project, worktree folder, changed folders or `None`).
    Files(String, String, Option<Vec<String>>),
    /// The project list changed (added or closed).
    ListChanged,
}

pub type ProjectListener = Arc<dyn Fn(ProjectEvent) + Send + Sync>;

#[derive(Default)]
struct Listeners(Mutex<Vec<ProjectListener>>);

impl Listeners {
    fn emit(&self, e: ProjectEvent) {
        let all = self.0.lock().unwrap().clone();
        for l in all {
            l(e.clone());
        }
    }
}

/// One open project: its folder, its own live session, its edit markers and its watchers.
pub struct ProjectRuntime {
    pub info: ProjectInfo,
    pub is_primary: bool,
    /// A bench run's tab: watched, not driven.
    pub read_only: bool,
    pub session: Arc<dyn LiveSession>,
    pub edits: EditTracker,
    watchers: Mutex<HashMap<String, FolderWatcher>>,
    worktrees: tokio::sync::Mutex<Option<(Instant, Vec<WorktreeInfo>)>>,
    listeners: Arc<Listeners>,
}

impl ProjectRuntime {
    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.info.path)
    }

    /// The project's worktrees (cached for two seconds; git is not free on big repos).
    pub async fn worktrees(&self) -> Vec<WorktreeInfo> {
        let mut cache = self.worktrees.lock().await;
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < Duration::from_secs(2)
        {
            return list.clone();
        }
        let root = self.root();
        let list = if root.is_dir() {
            worktrees::list(&root).await
        } else {
            vec![WorktreeInfo::own(&self.info.path, &self.info.path)]
        };
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    /// Forget the cached worktree list (one was just added or removed).
    pub async fn forget_worktrees(&self) {
        *self.worktrees.lock().await = None;
    }

    /// The folder a request names with `wt` (empty = the project's own), only if it is one of
    /// this project's worktrees.
    pub async fn resolve_worktree(&self, wt: Option<&str>) -> Option<PathBuf> {
        let Some(wt) = wt.filter(|w| !w.trim().is_empty()) else {
            return Some(self.root());
        };
        let full = paths::normalize(wt).to_string_lossy().to_string();
        if paths::eq(&full, &self.info.path) {
            return Some(self.root());
        }
        let find = |list: &[WorktreeInfo]| {
            list.iter()
                .find(|w| paths::same_path(&w.path, &full))
                .map(|w| PathBuf::from(&w.path))
        };
        if let Some(p) = find(&self.worktrees().await) {
            return Some(p);
        }
        self.forget_worktrees().await; // maybe just created: look again once
        find(&self.worktrees().await)
    }

    /// Watch a worktree folder (once) so the browser hears about changes.
    pub fn ensure_watching(&self, folder: &Path) {
        let key = paths::key(&folder.to_string_lossy());
        let mut w = self.watchers.lock().unwrap();
        if w.contains_key(&key) {
            return;
        }
        let (listeners, id, f) = (
            self.listeners.clone(),
            self.info.id.clone(),
            folder.to_string_lossy().to_string(),
        );
        let watcher = FolderWatcher::start(
            folder,
            Arc::new(move |dirs| listeners.emit(ProjectEvent::Files(id.clone(), f.clone(), dirs))),
        );
        w.insert(key, watcher);
    }

    fn stop_watching(&self) {
        self.watchers.lock().unwrap().clear();
    }
}

/// Gives the GitHub sign-in's current access token.
pub type AppTokenFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct ProjectManager {
    store: ProjectStore,
    factory: Option<SessionFactory>,
    runtimes: Mutex<HashMap<String, Arc<ProjectRuntime>>>,
    order: Mutex<Vec<ProjectInfo>>,
    transient: Mutex<Vec<ProjectInfo>>,
    listeners: Arc<Listeners>,
    pub primary: ProjectInfo,
    /// Per-project GitHub tokens (next to projects.json, mode 600).
    pub tokens: GitTokenStore,
    /// Each project's engine choice.
    pub engines: EngineChoiceStore,
    /// Each project's inference profile; none = the config's own.
    pub profiles: EngineChoiceStore,
    /// Each project's agent mode; none = its profile's own.
    pub modes: EngineChoiceStore,
    /// The GitHub sign-in's access token for projects linked to it (`None`: not set up).
    pub github_app_token: Mutex<Option<AppTokenFn>>,
}

impl ProjectManager {
    pub fn new(
        store: ProjectStore,
        primary: Arc<dyn LiveSession>,
        factory: Option<SessionFactory>,
    ) -> Arc<Self> {
        let dir = store.state_dir();
        let info = store.add(&primary.working_directory(), None);
        let pm = Arc::new(Self {
            tokens: GitTokenStore::new(&dir),
            engines: EngineChoiceStore::engines(&dir),
            profiles: EngineChoiceStore::new(&dir, "project-profiles.json", ""),
            modes: EngineChoiceStore::new(&dir, "project-modes.json", ""),
            store,
            factory,
            runtimes: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            transient: Mutex::new(Vec::new()),
            listeners: Arc::new(Listeners::default()),
            primary: info.clone(),
            github_app_token: Mutex::new(None),
        });
        pm.create(info, primary, true, false);
        pm.reload();
        pm
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    pub fn can_add_projects(&self) -> bool {
        self.factory.is_some()
    }

    /// Listen to runtime, marker, file and list changes.
    pub fn subscribe(&self, listener: ProjectListener) {
        self.listeners.0.lock().unwrap().push(listener);
    }

    /// The token git should use for a project's fetch, pull and push: its own saved token, or,
    /// for a project linked to the GitHub sign-in, that sign-in's access token.
    pub fn git_token(&self, project_id: &str) -> Result<Option<String>, String> {
        let saved = self.tokens.get(project_id);
        if saved.as_deref() != Some(APP_LINK) {
            return Ok(saved);
        }
        let f = self.github_app_token.lock().unwrap().clone();
        match f {
            None => Err(
                "This project uses the GitHub sign-in, which isn't set up on this server.".into(),
            ),
            Some(f) => Ok(f()),
        }
    }

    /// The projects in tab order (primary first, then the saved order).
    pub fn list(&self) -> Vec<Arc<ProjectRuntime>> {
        self.reload();
        let order = self.order.lock().unwrap().clone();
        order.iter().filter_map(|i| self.get(&i.id)).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<ProjectRuntime>> {
        if id.is_empty() {
            return None;
        }
        if let Some(rt) = self.runtimes.lock().unwrap().get(id) {
            return Some(rt.clone());
        }
        let info = self
            .order
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.id == id)
            .cloned()?;
        let factory = self.factory.clone()?;
        let session = factory(&info.path);
        Some(self.create(info, session, false, false))
    }

    /// Add a folder on this machine as a project. Err: why it is not a usable folder.
    pub fn add(&self, path: &str, name: Option<&str>) -> Result<Arc<ProjectRuntime>, String> {
        if self.factory.is_none() {
            return Err("This server shows a single project.".into());
        }
        if path.trim().is_empty() {
            return Err("Choose a folder.".into());
        }
        let expanded = expand_env(path.trim().trim_matches('"'));
        let full = paths::normalize(&expanded);
        if !full.is_dir() {
            return Err(format!(
                "No such folder on this machine: {}",
                full.display()
            ));
        }
        if let Some(problem) = paths::read_write_problem(&full) {
            return Err(format!(
                "{problem} OpenOrca needs to read and write the project folder."
            ));
        }
        let info = self.store.add(&full.to_string_lossy(), name);
        self.reload();
        self.listeners.emit(ProjectEvent::ListChanged);
        self.get(&info.id)
            .ok_or_else(|| "Project vanished.".to_string())
    }

    /// A read-only tab for a session this server started itself (a bench run).
    pub fn add_transient(
        &self,
        path: &str,
        name: &str,
        session: Arc<dyn LiveSession>,
    ) -> Arc<ProjectRuntime> {
        let info = ProjectInfo {
            id: format!("bench-{}", crate::util::short_id(10)),
            name: name.into(),
            path: paths::normalize(path).to_string_lossy().to_string(),
            added_at: crate::util::utc_now_iso(),
        };
        self.transient.lock().unwrap().push(info.clone());
        let rt = self.create(info, session, false, true);
        self.reload();
        self.listeners.emit(ProjectEvent::ListChanged);
        rt
    }

    /// Close a project's tab: forget it (the folder stays) and stop its session.
    pub async fn remove(&self, id: &str) -> Result<bool, String> {
        if id == self.primary.id {
            return Err(
                "This project is the session openorca was started in; it can't be closed here."
                    .into(),
            );
        }
        let removed = !id.starts_with("bench-") && self.store.remove(id);
        self.transient.lock().unwrap().retain(|p| p.id != id);
        let rt = self.runtimes.lock().unwrap().remove(id);
        self.order.lock().unwrap().retain(|p| p.id != id);
        if let Some(rt) = &rt {
            rt.stop_watching();
            rt.session.shutdown().await;
        }
        self.tokens.remove(id);
        self.engines.remove(id);
        self.profiles.remove(id);
        self.modes.remove(id);
        if removed || rt.is_some() {
            self.listeners.emit(ProjectEvent::ListChanged);
        }
        Ok(removed || rt.is_some())
    }

    fn reload(&self) {
        let saved = self.store.load();
        let transient = self.transient.lock().unwrap().clone();
        let mut order = self.order.lock().unwrap();
        order.clear();
        order.push(
            saved
                .iter()
                .find(|p| p.id == self.primary.id)
                .cloned()
                .unwrap_or_else(|| self.primary.clone()),
        );
        order.extend(saved.into_iter().filter(|p| p.id != self.primary.id));
        order.extend(transient);
    }

    fn create(
        &self,
        info: ProjectInfo,
        session: Arc<dyn LiveSession>,
        primary: bool,
        read_only: bool,
    ) -> Arc<ProjectRuntime> {
        if !read_only {
            if let Some(engine) = self.engines.get(&info.id) {
                session.set_preferred_engine(Some(&engine));
            }
            if let Some(profile) = self.profiles.get(&info.id) {
                session.set_preferred_profile(Some(&profile));
            }
            if let Some(mode) = self.modes.get(&info.id).filter(|m| !m.is_empty()) {
                session.set_preferred_mode(Some(&mode));
            }
        }
        let bus = session.bus();
        let edits = EditTracker::new(&bus, &info.path);
        let (listeners, id) = (self.listeners.clone(), info.id.clone());
        edits.set_on_changed(move |m| listeners.emit(ProjectEvent::Markers(id.clone(), m)));
        let rt = Arc::new(ProjectRuntime {
            info: info.clone(),
            is_primary: primary,
            read_only,
            session,
            edits,
            watchers: Mutex::new(HashMap::new()),
            worktrees: tokio::sync::Mutex::new(None),
            listeners: self.listeners.clone(),
        });
        self.runtimes
            .lock()
            .unwrap()
            .insert(info.id.clone(), rt.clone());
        self.listeners
            .emit(ProjectEvent::RuntimeCreated(rt.clone()));
        rt
    }

    /// Runtimes created so far (the server forwards their events).
    pub fn created(&self) -> Vec<Arc<ProjectRuntime>> {
        self.runtimes.lock().unwrap().values().cloned().collect()
    }

    /// Stop every project's session except the primary one (which belongs to the CLI).
    pub async fn shutdown(&self) {
        let all: Vec<_> = self
            .runtimes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, v)| v)
            .collect();
        for rt in all {
            rt.stop_watching();
            if !rt.is_primary {
                rt.session.shutdown().await;
            }
        }
    }
}

/// `Environment.ExpandEnvironmentVariables`: `%NAME%` references (left as is when unset).
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => {
                        out.push_str(&v);
                        rest = &after[end + 1..];
                    }
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        rest = &after[end..];
                    }
                }
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
