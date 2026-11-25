//! Velopack (PORTING.md D136): the installer's hooks and auto-update from GitHub Releases.
//!
//! The Windows Setup.exe and the Linux AppImage are Velopack packages, cross-built on Linux by
//! CI's `package` job. Installed, the app lives in `%LocalAppData%\Abhoth\current` (Windows).
//! [`startup`] runs first in both exes: Setup.exe / Update.exe start the main exe with
//! `--veloapp-install` and friends, which add or remove that folder on the user PATH and exit
//! at once. The desktop app checks the public releases of the abhoth repo in the background
//! ([`spawn_checker`]); a newer release is downloaded, and the tray offers to restart into it.
//! Outside a Velopack install (a dev build, `cargo run`) all of this is a no-op.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

/// Where releases are published (`abhoth-v*` tags; CI attaches the Velopack feed files).
pub const RELEASES_REPO: &str = "https://github.com/madninjaskillz/abhoth";

/// How often the desktop app looks for a new release.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

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

/// A downloaded update, ready to apply.
#[derive(Clone)]
pub struct ReadyUpdate {
    manager: UpdateManager,
    info: UpdateInfo,
}

impl ReadyUpdate {
    pub fn version(&self) -> String {
        self.info.TargetFullRelease.Version.clone()
    }

    /// Hands over to Velopack's updater, which waits for this process to exit, applies the
    /// update and starts the new version. Does not return on success.
    pub fn apply_and_restart(&self) -> Result<(), String> {
        self.manager
            .apply_updates_and_restart(&self.info)
            .map_err(|e| e.to_string())
    }
}

/// The slot the checker fills when an update is downloaded.
pub type Slot = Arc<Mutex<Option<ReadyUpdate>>>;

/// Starts the background check (now, then every [`CHECK_EVERY`]) when this is a Velopack
/// install. `on_ready` is called once with the version when an update has been downloaded.
pub fn spawn_checker(slot: Slot, on_ready: impl Fn(String) + Send + 'static) {
    let manager =
        match UpdateManager::new(GithubSource::new(RELEASES_REPO, None, false), None, None) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("updates: not a Velopack install ({e}); no update checks");
                return;
            }
        };
    let _ = std::thread::Builder::new()
        .name("abhoth-update-check".into())
        .spawn(move || {
            loop {
                match check_and_download(&manager) {
                    Ok(Some(ready)) => {
                        let version = ready.version();
                        tracing::info!("updates: {version} downloaded, ready to apply");
                        if let Ok(mut s) = slot.lock() {
                            *s = Some(ready);
                        }
                        on_ready(version);
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("updates: check failed: {e}"),
                }
                std::thread::sleep(CHECK_EVERY);
            }
        });
}

fn check_and_download(manager: &UpdateManager) -> Result<Option<ReadyUpdate>, String> {
    match manager.check_for_updates().map_err(|e| e.to_string())? {
        UpdateCheck::UpdateAvailable(info) => {
            manager
                .download_updates(&info, None)
                .map_err(|e| e.to_string())?;
            Ok(Some(ReadyUpdate {
                manager: manager.clone(),
                info: *info,
            }))
        }
        UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => Ok(None),
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
