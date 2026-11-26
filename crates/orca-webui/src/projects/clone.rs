//! `CloneService`: `git clone` into a folder on this machine, with progress the wizard polls.
//! A token typed in the wizard goes to git through the token file (never the URL, arguments
//! or .git/config) and is saved for the new project.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::git::service::{TokenFile, no_prompt_env, scrub_text};
use crate::git::tokens;
use crate::paths;
use crate::projects::manager::{ProjectManager, expand_env};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneStatus {
    pub id: String,
    pub url: String,
    pub destination: String,
    pub state: String,
    pub progress: Option<String>,
    pub log: Vec<String>,
    pub error: Option<String>,
    pub project_id: Option<String>,
}

struct Job {
    token: Option<String>,
    /// The user who started it (D140): the project is theirs, and only they see the job.
    owner: Option<i64>,
    /// What the new project's token entry becomes instead of a copy of the token: the GitHub
    /// sign-in (`@github-app`) or its owner's GitHub credentials (`@github-user:<id>`).
    link: Option<String>,
    /// Without a token, git gets none of the server's own credential helpers either.
    isolate: bool,
    status: Mutex<CloneStatus>,
    cancel: tokio::sync::Notify,
}

impl Job {
    fn line(&self, text: &str, progress: bool) {
        let text = scrub_text(text, self.token.as_deref());
        let mut s = self.status.lock().unwrap();
        s.progress = Some(text.clone());
        // Progress lines repeat with new percentages: keep the latest of each phase only.
        let phase = text.split(':').next().unwrap_or("").to_string();
        let same = s.log.last().is_some_and(|l| {
            l.split(':').next().unwrap_or("") == phase && (progress || l.contains('%'))
        });
        if same {
            *s.log.last_mut().unwrap() = text;
        } else {
            s.log.push(text);
        }
        if s.log.len() > 200 {
            s.log.remove(0);
        }
    }

    fn last_line(&self) -> Option<String> {
        let s = self.status.lock().unwrap();
        s.log
            .iter()
            .rev()
            .find(|l| {
                let l = l.to_lowercase();
                l.starts_with("fatal") || l.starts_with("error")
            })
            .or(s.log.last())
            .cloned()
    }

    fn fail(&self, error: &str) {
        let mut s = self.status.lock().unwrap();
        s.state = "failed".into();
        s.error = Some(scrub_text(error, self.token.as_deref()));
    }
}

/// How [`CloneService::start_opts`] gives git credentials.
#[derive(Debug, Clone, Default)]
pub struct CloneOptions {
    pub token: Option<String>,
    /// Saved for the new project instead of the token (see `Job::link`).
    pub link: Option<String>,
    /// No credential helper of the server's (`credential.helper=`) when there is no token.
    pub isolate: bool,
}

pub struct CloneService {
    projects: Arc<ProjectManager>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

/// The repository's folder name for a URL ("https://github.com/a/b.git" -> "b").
pub fn name_from_url(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/');
    if u.to_lowercase().ends_with(".git") {
        u = &u[..u.len() - 4];
    }
    let cut = u.rfind(['/', ':', '\\']);
    let name = match cut {
        Some(i) => &u[i + 1..],
        None => u,
    };
    if name.trim().is_empty() {
        "repository".into()
    } else {
        name.into()
    }
}

/// "owner/repo" -> the GitHub HTTPS URL; anything that already looks like a URL or path is kept.
pub fn expand_url(url: &str) -> String {
    static OWNER_REPO: OnceLock<regex::Regex> = OnceLock::new();
    let u = url.trim();
    let re = OWNER_REPO
        .get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$").unwrap());
    if re.is_match(u) {
        format!("https://github.com/{u}.git")
    } else {
        u.to_string()
    }
}

impl CloneService {
    pub fn new(projects: Arc<ProjectManager>) -> Self {
        Self {
            projects,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Check the request and start cloning. `Err` is a user-facing message (400).
    pub fn start(
        &self,
        url: &str,
        parent_folder: &str,
        name: Option<&str>,
        token: Option<&str>,
        branch: Option<&str>,
    ) -> Result<CloneStatus, String> {
        self.start_with(url, parent_folder, name, token, branch, false)
    }

    /// [`Self::start`]; `github_app`: the token is the GitHub sign-in's, and the new project is
    /// linked to the sign-in (fetch/pull/push use it, refreshed as needed).
    pub fn start_with(
        &self,
        url: &str,
        parent_folder: &str,
        name: Option<&str>,
        token: Option<&str>,
        branch: Option<&str>,
        github_app: bool,
    ) -> Result<CloneStatus, String> {
        self.start_opts(
            url,
            parent_folder,
            name,
            branch,
            CloneOptions {
                token: token.map(str::to_string),
                link: github_app.then(|| tokens::APP_LINK.to_string()),
                isolate: false,
            },
        )
    }

    /// Check the request and start cloning with these credentials.
    pub fn start_opts(
        &self,
        url: &str,
        parent_folder: &str,
        name: Option<&str>,
        branch: Option<&str>,
        options: CloneOptions,
    ) -> Result<CloneStatus, String> {
        let token = options.token.as_deref();
        static CRED: OnceLock<regex::Regex> = OnceLock::new();
        static BRANCH: OnceLock<regex::Regex> = OnceLock::new();
        let url = expand_url(url);
        if url.is_empty() {
            return Err("Enter a repository URL (or owner/repo for GitHub).".into());
        }
        if url.to_lowercase().starts_with("http")
            && CRED
                .get_or_init(|| regex::Regex::new(r"(?i)^[a-z+]+://[^/]*@").unwrap())
                .is_match(&url)
        {
            return Err("Put the token in the token field, not in the URL.".into());
        }
        let token = match token.filter(|t| !t.trim().is_empty()) {
            Some(t) => Some(tokens::validate(Some(t))?),
            None => None,
        };
        if url.starts_with('-') || url.chars().any(char::is_control) || url.contains(' ') {
            return Err("That doesn't look like a repository URL.".into());
        }
        if parent_folder.trim().is_empty() {
            return Err("Choose the folder to clone into.".into());
        }
        let parent = paths::normalize(&expand_env(parent_folder.trim().trim_matches('"')));
        if !parent.is_dir() {
            return Err(format!(
                "No such folder on this machine: {}",
                parent.display()
            ));
        }
        let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.to_string(),
            None => name_from_url(&url),
        };
        let invalid = |c: char| {
            (c as u32) < 32
                || (cfg!(windows) && matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        };
        if name == "." || name == ".." || name.contains(['/', '\\']) || name.chars().any(invalid) {
            return Err(format!("Not a valid folder name: {name}"));
        }
        let dest = parent.join(&name);
        let non_empty =
            dest.is_dir() && std::fs::read_dir(&dest).is_ok_and(|mut d| d.next().is_some());
        if dest.is_file() || non_empty {
            return Err(format!(
                "That folder already exists and isn't empty: {}",
                dest.display()
            ));
        }
        let branch = branch.map(str::trim).filter(|b| !b.is_empty());
        if let Some(b) = branch {
            let re = BRANCH.get_or_init(|| {
                regex::Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9._/+-]{0,250}$").unwrap()
            });
            if !re.is_match(b)
                || b.contains("..")
                || b.contains("//")
                || b.ends_with('/')
                || b.ends_with(".lock")
                || b.ends_with('.')
            {
                return Err(format!("Not a valid branch name: {b}"));
            }
        }
        let id = crate::util::short_id(12);
        let status = CloneStatus {
            id: id.clone(),
            url: url.clone(),
            destination: dest.to_string_lossy().to_string(),
            state: "running".into(),
            progress: None,
            log: Vec::new(),
            error: None,
            project_id: None,
        };
        let job = Arc::new(Job {
            token,
            owner: crate::audit::request_caller().and_then(|c| c.uid),
            link: options.link,
            isolate: options.isolate,
            status: Mutex::new(status.clone()),
            cancel: tokio::sync::Notify::new(),
        });
        self.jobs.lock().unwrap().insert(id, job.clone());
        let projects = self.projects.clone();
        let branch = branch.map(str::to_string);
        tokio::spawn(async move { run(job, projects, url, dest, branch).await });
        Ok(status)
    }

    /// Who started a job (`None`: no such job).
    pub fn owner_of(&self, id: &str) -> Option<Option<i64>> {
        self.jobs.lock().unwrap().get(id).map(|j| j.owner)
    }

    pub fn get(&self, id: &str) -> Option<CloneStatus> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(id).map(|j| j.status.lock().unwrap().clone())
    }

    pub fn cancel(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(id) {
            Some(j) if j.status.lock().unwrap().state == "running" => {
                j.cancel.notify_one();
                true
            }
            _ => false,
        }
    }
}

async fn run(
    job: Arc<Job>,
    projects: Arc<ProjectManager>,
    url: String,
    dest: std::path::PathBuf,
    branch: Option<String>,
) {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["-c", "color.ui=false"]);
    let _auth = match TokenFile::apply(&mut cmd, job.token.as_deref()) {
        Ok(a) => a,
        Err(e) => return job.fail(&e.to_string()),
    };
    if job.isolate && !_auth.is_active() {
        // Reset the helper list: none of the server's own sign-ins are offered to GitHub.
        cmd.args(["-c", "credential.helper="]);
    }
    cmd.args(["clone", "--progress"]);
    if let Some(b) = &branch {
        cmd.args(["--branch", b]);
    }
    cmd.arg("--").arg(&url).arg(&dest);
    cmd.env("LC_ALL", "C.UTF-8");
    no_prompt_env(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return job.fail(&format!("Could not start git: {e}")),
    };
    let err = read_progress(child.stderr.take(), job.clone());
    let out = read_progress(child.stdout.take(), job.clone());
    let wait = async {
        let (_, _) = tokio::join!(err, out);
        child.wait().await
    };
    let status = tokio::select! {
        s = wait => s,
        _ = job.cancel.notified() => {
            return job.fail("Cancelled.");
        }
    };
    let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    if code != 0 {
        let msg = job
            .last_line()
            .unwrap_or_else(|| format!("git clone failed (exit {code})."));
        return job.fail(&msg);
    }
    match projects.add(&dest.to_string_lossy(), None, job.owner) {
        Ok(rt) => {
            if let Some(link) = &job.link {
                projects.tokens.link(rt.id(), link);
            } else if let Some(t) = &job.token {
                let _ = projects.tokens.set(rt.id(), t);
            }
            let mut s = job.status.lock().unwrap();
            s.state = "done".into();
            s.project_id = Some(rt.id().to_string());
        }
        Err(e) => job.fail(&e),
    }
}

/// git writes progress with carriage returns: each \r or \n ends a line.
async fn read_progress<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>, job: Arc<Job>) {
    let Some(mut reader) = reader else { return };
    let mut buf = [0u8; 1024];
    let mut line: Vec<u8> = Vec::new();
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        for &c in &buf[..n] {
            if c == b'\r' || c == b'\n' {
                if !line.is_empty() {
                    job.line(&String::from_utf8_lossy(&line), c == b'\r');
                }
                line.clear();
            } else {
                line.push(c);
            }
        }
    }
    if !line.is_empty() {
        job.line(&String::from_utf8_lossy(&line), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_urls() {
        assert_eq!(name_from_url("https://github.com/a/b.git"), "b");
        assert_eq!(name_from_url("git@github.com:a/b"), "b");
        assert_eq!(name_from_url("/"), "repository");
        assert_eq!(expand_url("acme/tool"), "https://github.com/acme/tool.git");
        assert_eq!(expand_url("https://x/y"), "https://x/y");
    }
}
