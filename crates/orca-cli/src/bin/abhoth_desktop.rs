//! `abhoth-desktop`: the desktop app without a console window (Windows GUI subsystem), for the
//! Start-menu shortcut. Same as `abhoth --desktop` (D110).
#![windows_subsystem = "windows"]

fn main() {
    // Velopack first (D136): Setup.exe / Update.exe run this exe with `--veloapp-install <ver>`
    // and friends (the user PATH is edited, then the process exits); an update downloaded in an
    // earlier session is applied before the window opens.
    orca_cli::updater::startup(true);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Any other `--veloapp-*` call (none is expected) must not open a window either.
    if args.first().is_some_and(|a| a.starts_with("--veloapp-")) {
        std::process::exit(0);
    }
    if !args.iter().any(|a| a == "--desktop") {
        args.push("--desktop".into());
    }
    // The one-time copy of ~/.openorca (D89) before the file log creates ~/.abhoth (D133).
    let migrated = orca_core::config::manager::migrate_default_home();
    let log = orca_core::logging::FileLog::open(&orca_core::config::default_config_dir());
    orca_core::logging::set_process_log(log);
    if let Some(line) = migrated {
        tracing::info!("{line}");
    }
    std::process::exit(orca_cli::desktop::run(args));
}
