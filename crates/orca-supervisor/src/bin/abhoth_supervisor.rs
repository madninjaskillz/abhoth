//! `abhoth-supervisor run|check --config FILE`.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("abhoth supervisor: runtime: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit(rt.block_on(orca_supervisor::run::main(&args)));
}
