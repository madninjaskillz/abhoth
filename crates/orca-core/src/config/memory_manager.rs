//! Port of `MemoryManager.cs`: auto-learned memory files in `<config dir>/memory` (global) and
//! `<project root>/.orca/memory` (project).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::model::OrcaConfig;
use super::project_instructions;

pub struct MemoryManager {
    max_memory_files: i32,
    global_dir: PathBuf,
    skip_project_dir: bool,
    /// The directory the project root is searched from (C#: the process's current directory).
    cwd: Option<PathBuf>,
}

impl MemoryManager {
    /// Global memory under `<config dir>/memory`, project memory under the current project.
    pub fn new(config: &OrcaConfig) -> Self {
        Self {
            max_memory_files: config.memory.max_memory_files,
            global_dir: super::default_config_dir().join("memory"),
            skip_project_dir: false,
            cwd: None,
        }
    }

    /// Test seam: an explicit global directory, optionally without project memory.
    pub fn with_global_dir(
        config: &OrcaConfig,
        global_dir: impl Into<PathBuf>,
        skip_project_dir: bool,
    ) -> Self {
        Self {
            max_memory_files: config.memory.max_memory_files,
            global_dir: global_dir.into(),
            skip_project_dir,
            cwd: None,
        }
    }

    /// Resolve the project from `cwd` instead of the process's current directory.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    /// `LoadAllMemoryAsync`: project memory first, then global, trimmed.
    pub fn load_all_memory(&self) -> String {
        self.load_memory_sources()
            .into_iter()
            .map(|(_, c)| c)
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_string()
    }

    /// `LoadMemorySourcesAsync`: memory content per directory (project first, then global),
    /// skipping empty ones.
    pub fn load_memory_sources(&self) -> Vec<(PathBuf, String)> {
        let mut dirs = Vec::new();
        if let Some(p) = self.project_memory_dir() {
            dirs.push(p);
        }
        dirs.push(self.global_dir.clone());
        let mut result = Vec::new();
        for dir in dirs {
            let mut sb = String::new();
            for file in md_files_newest_first(&dir) {
                let Ok(content) = read_text(&file) else {
                    continue;
                };
                if !content.trim().is_empty() {
                    // AppendLine(content.Trim()); AppendLine();
                    sb.push_str(content.trim());
                    sb.push_str(orca_tools_common::NEWLINE);
                    sb.push_str(orca_tools_common::NEWLINE);
                }
            }
            let content = sb.trim().to_string();
            if !content.is_empty() {
                result.push((dir, content));
            }
        }
        result
    }

    /// `SaveLearningsAsync`: write `content` to `<yyyyMMdd>-<hash6>.md` in the project memory
    /// directory (global without a project), then prune to `memory.maxMemoryFiles`.
    pub fn save_learnings(&self, content: &str) -> std::io::Result<()> {
        if content.trim().is_empty() {
            return Ok(());
        }
        let dir = self
            .project_memory_dir()
            .unwrap_or_else(|| self.global_dir.clone());
        std::fs::create_dir_all(&dir)?;
        let digest = Sha256::digest(content.as_bytes());
        let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let name = format!("{}-{}.md", utc_date_yyyymmdd(SystemTime::now()), &hash[..6]);
        std::fs::write(dir.join(name), content)?;
        self.prune(&dir);
        Ok(())
    }

    /// `ListAsync`: every memory file with its first line (without leading `#`/spaces) as preview.
    pub fn list(&self) -> Vec<(PathBuf, String)> {
        let mut results = Vec::new();
        if let Some(p) = self.project_memory_dir() {
            list_from_dir(&p, &mut results);
        }
        list_from_dir(&self.global_dir, &mut results);
        results
    }

    /// `ClearAsync`: delete every memory file in both directories.
    pub fn clear(&self) {
        if let Some(p) = self.project_memory_dir() {
            clear_dir(&p);
        }
        clear_dir(&self.global_dir);
    }

    fn project_memory_dir(&self) -> Option<PathBuf> {
        if self.skip_project_dir {
            return None;
        }
        let cwd = match &self.cwd {
            Some(c) => c.clone(),
            None => std::env::current_dir().ok()?,
        };
        project_instructions::find_project_root(&cwd).map(|r| r.join(".orca").join("memory"))
    }

    fn prune(&self, dir: &Path) {
        let files = md_files_newest_first(dir);
        let keep = self.max_memory_files.max(0) as usize;
        if files.len() <= keep {
            return;
        }
        for f in files.into_iter().skip(keep) {
            let _ = std::fs::remove_file(f); // best effort
        }
    }
}

/// `Directory.GetFiles(dir, "*.md").OrderByDescending(File.GetLastWriteTimeUtc)` (stable: ties
/// keep name order).
fn md_files_newest_first(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.ends_with(".md")
        })
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (e.path(), mtime)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by_key(|a| std::cmp::Reverse(a.1));
    files.into_iter().map(|(p, _)| p).collect()
}

fn list_from_dir(dir: &Path, results: &mut Vec<(PathBuf, String)>) {
    for file in md_files_newest_first(dir) {
        let preview = read_text(&file)
            .ok()
            .and_then(|t| {
                t.split('\n')
                    .next()
                    .map(|l| l.trim_end_matches('\r').to_string())
            })
            .map(|l| l.trim_start_matches(['#', ' ']).to_string())
            .map(|p| {
                if orca_tools_common::utf16_len(&p) > 80 {
                    let mut units = 0;
                    let cut: String = p
                        .chars()
                        .take_while(|c| {
                            units += c.len_utf16();
                            units <= 77
                        })
                        .collect();
                    format!("{cut}...")
                } else {
                    p
                }
            })
            .unwrap_or_default();
        results.push((file, preview));
    }
}

fn clear_dir(dir: &Path) {
    for f in md_files_newest_first(dir) {
        let _ = std::fs::remove_file(f); // best effort
    }
}

fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `DateTime.UtcNow.ToString("yyyyMMdd")`.
fn utc_date_yyyymmdd(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}{m:02}{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
