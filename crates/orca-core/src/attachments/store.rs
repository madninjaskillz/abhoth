//! `AttachmentStore`: where pasted images are saved.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

type Clock = Box<dyn Fn() -> NaiveDateTime + Send + Sync>;

/// Where pasted images are saved: `<project>/.orca/attachments/paste-yyyyMMdd-HHmmss.png` (the
/// project's `.orca` folder, like plans and memory). The folder carries its own `.gitignore`
/// (`*`), so pastes stay out of git without touching the project's ignores. The model can
/// read the files with its tools, and they outlive the session for reference.
pub struct AttachmentStore {
    folder: PathBuf,
    clock: Clock,
}

impl std::fmt::Debug for AttachmentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentStore")
            .field("folder", &self.folder)
            .finish()
    }
}

impl AttachmentStore {
    /// `base_directory`: the folder holding `.orca` (the project root or working directory).
    pub fn new(base_directory: impl AsRef<Path>) -> Self {
        Self::with_clock(base_directory, || chrono::Local::now().naive_local())
    }

    /// With a clock (local time) for the file names, for tests.
    pub fn with_clock(
        base_directory: impl AsRef<Path>,
        clock: impl Fn() -> NaiveDateTime + Send + Sync + 'static,
    ) -> Self {
        Self {
            folder: base_directory.as_ref().join(".orca").join("attachments"),
            clock: Box::new(clock),
        }
    }

    /// The attachments folder.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Saves pasted image bytes and returns the file's absolute path. `extension` includes the
    /// dot, e.g. ".png" (C#'s default).
    pub fn save_image(&self, bytes: &[u8], extension: &str) -> std::io::Result<String> {
        self.ensure_directory()?;
        let stamp = (self.clock)().format("%Y%m%d-%H%M%S").to_string();
        let mut path = self.folder.join(format!("paste-{stamp}{extension}"));
        let mut n = 2;
        while path.exists() {
            path = self.folder.join(format!("paste-{stamp}-{n}{extension}"));
            n += 1;
        }
        std::fs::write(&path, bytes)?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn ensure_directory(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.folder)?;
        let ignore = self.folder.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(
                ignore,
                "# Pasted prompt attachments (Abhoth); not project files.\n*\n",
            )?;
        }
        Ok(())
    }
}
