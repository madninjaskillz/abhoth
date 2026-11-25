fn main() {
    // Velopack's install hooks exit here (D136); the console exe never auto-applies an update.
    #[cfg(feature = "desktop")]
    orca_cli::updater::startup(false);
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // The one-time copy of ~/.openorca into ~/.abhoth (D89), before anything creates ~/.abhoth.
    if let Some(line) = orca_core::config::manager::migrate_default_home() {
        eprintln!("{line}");
    }
    // The file log (~/.abhoth/logs/openorca-{date}.log, FileLoggerProvider): every run; the
    // per-request "Slot usage" lines go there too. Stderr only when asked for (RUST_LOG=debug),
    // so --print stdout stays JSON.
    let log = orca_core::logging::FileLog::open(&orca_core::config::default_config_dir());
    orca_core::logging::set_process_log(log.clone());
    let stderr = std::env::var_os("RUST_LOG").is_some().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(tracing_subscriber::EnvFilter::from_default_env())
    });
    tracing_subscriber::registry()
        .with(orca_cli::file_log::FileLogLayer::new(log))
        .with(stderr)
        .init();
    // The desktop app owns the main thread (its window's event loop) and runs the host itself.
    #[cfg(feature = "desktop")]
    if std::env::args().any(|a| a == "--desktop") {
        std::process::exit(orca_cli::desktop::run(std::env::args().skip(1).collect()));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(orca_cli::main_with_args(std::env::args().skip(1).collect()));
    orca_cli::log_provider_usage();
    orca_tools_shell::process::kill_all();
    match code {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}
