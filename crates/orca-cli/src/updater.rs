//! Velopack (PORTING.md D136): the installer's hooks and auto-update from GitHub Releases.
//!
//! The Windows Setup.exe and the Linux AppImage are Velopack packages, cross-built on Linux by
//! CI's `package` job. Installed, the app lives in `%LocalAppData%\Abhoth\current` (Windows).
//! [`startup`] runs first in both exes: Setup.exe / Update.exe start the main exe with
//! `--veloapp-install` and friends, which add or remove that folder on the user PATH and exit
//! at once. The desktop app checks the public releases of the abhoth repo in the background
//! (`update_service`; Settings -> Updates checks on demand); a newer release is downloaded,
//! and the tray and the page offer to restart into it.
//! Outside a Velopack install (a dev build, `cargo run`) all of this is a no-op.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

use crate::update_service::Feed;
pub use crate::update_service::RELEASES_REPO;

/// How often the desktop app looks for a new release (when `desktop.autoUpdate` is on).
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Velopack's startup work, first thing in `main`. The install / update / uninstall hooks run
/// here and exit the process (Velopack kills a hook after 15-30 s). `apply_pending` lets the
/// desktop app apply an update downloaded in an earlier session before it starts; the console
/// exe never restarts itself under the user's feet.
pub fn startup(apply_pending: bool) {
    let app = VelopackApp::build().set_auto_apply_on_startup(apply_pending);
    // The install hooks exist on Windows only (the AppImage has no install step).
    #[cfg(windows)]
    let app = app
        .on_after_install_fast_callback(|_| user_path::edit_for_current_exe(true))
        .on_after_update_fast_callback(|_| user_path::edit_for_current_exe(true))
        .on_before_uninstall_fast_callback(|_| user_path::edit_for_current_exe(false));
    let mut app = app;
    app.run();
}

/// Velopack's GitHub feed as the update service's [`Feed`] (settings -> Updates, the tray).
pub struct VelopackFeed {
    manager: UpdateManager,
    found: Mutex<Option<UpdateInfo>>,
}

/// The feed of an installed app; `None` outside a Velopack install (a dev build, `cargo run`).
pub fn feed() -> Option<Arc<dyn Feed>> {
    match UpdateManager::new(GithubSource::new(RELEASES_REPO, None, false), None, None) {
        Ok(manager) => Some(Arc::new(VelopackFeed {
            manager,
            found: Mutex::new(None),
        })),
        Err(e) => {
            tracing::debug!("updates: not a Velopack install ({e})");
            None
        }
    }
}

impl Feed for VelopackFeed {
    fn current(&self) -> String {
        self.manager.get_current_version_as_string()
    }
    fn channel(&self) -> String {
        // GithubSource(prerelease = false): published releases, no pre-releases
        "stable".into()
    }
    fn check(&self) -> Result<Option<String>, String> {
        match self
            .manager
            .check_for_updates()
            .map_err(|e| e.to_string())?
        {
            UpdateCheck::UpdateAvailable(info) => {
                let v = info.TargetFullRelease.Version.clone();
                *self.found.lock().unwrap() = Some(*info);
                Ok(Some(v))
            }
            UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => Ok(None),
        }
    }
    fn download(&self, progress: Arc<dyn Fn(u8) + Send + Sync>) -> Result<(), String> {
        let info = self
            .found
            .lock()
            .unwrap()
            .clone()
            .ok_or("nothing to download (check first)")?;
        let (tx, rx) = std::sync::mpsc::channel::<i16>();
        let reader = std::thread::spawn(move || {
            for p in rx {
                progress(p.clamp(0, 100) as u8);
            }
        });
        let r = self
            .manager
            .download_updates(&info, Some(tx))
            .map_err(|e| e.to_string());
        let _ = reader.join();
        r
    }
    fn apply(&self) -> Result<(), String> {
        let info = self
            .found
            .lock()
            .unwrap()
            .clone()
            .ok_or("no update is downloaded")?;
        self.manager
            .apply_updates_and_restart(&info)
            .map_err(|e| e.to_string())
    }
}

/// The install folder on the user PATH, so `abhoth` works in a terminal (as the MSI did).
#[cfg(windows)]
mod user_path {
    /// Adds (or removes) the folder of the running exe to the user PATH. At hook time that is
    /// `%LocalAppData%\Abhoth\current`.
    pub fn edit_for_current_exe(add: bool) {
        let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        else {
            return;
        };
        if let Err(e) = edit(&dir.to_string_lossy(), add) {
            tracing::warn!("updates: could not edit the user PATH: {e}");
        }
    }

    /// PowerShell, so the value keeps its REG_EXPAND_SZ type and `%VARS%` stay unexpanded, and
    /// setting a variable through .NET broadcasts WM_SETTINGCHANGE so new terminals see it.
    /// The folder goes in through the environment, never into the script text.
    const SCRIPT: &str = r#"
$k = Get-Item 'HKCU:\Environment'
$p = [string]$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$d = $env:ABHOTH_PATH_DIR.TrimEnd('\')
$parts = @($p -split ';' | Where-Object { $_ -ne '' -and $_.TrimEnd('\') -ine $d })
if ($env:ABHOTH_PATH_OP -eq 'add') { $parts += $d }
Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value ($parts -join ';') -Type ExpandString
[Environment]::SetEnvironmentVariable('ABHOTH_PATH_REFRESH', '1', 'User')
[Environment]::SetEnvironmentVariable('ABHOTH_PATH_REFRESH', $null, 'User')
"#;

    fn edit(dir: &str, add: bool) -> std::io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let status = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                SCRIPT,
            ])
            .env("ABHOTH_PATH_DIR", dir)
            .env("ABHOTH_PATH_OP", if add { "add" } else { "remove" })
            .creation_flags(CREATE_NO_WINDOW)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "powershell exited with {status}"
            )))
        }
    }
}
