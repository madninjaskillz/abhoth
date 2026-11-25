//! `abhoth-desktop`: the desktop app without a console window (Windows GUI subsystem), for the
//! Start-menu shortcut. Same as `abhoth --desktop` (D110).
#![windows_subsystem = "windows"]

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--desktop") {
        args.push("--desktop".into());
    }
    let log = orca_core::logging::FileLog::open(&orca_core::config::default_config_dir());
    orca_core::logging::set_process_log(log);
    if let Some(line) = orca_core::config::manager::migrate_default_home() {
        tracing::info!("{line}");
    }
    std::process::exit(orca_cli::desktop::run(args));
}
