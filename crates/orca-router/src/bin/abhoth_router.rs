//! The router alone (the Docker image's binary): `abhoth-router [router] --config router.json ...`.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("router") {
        args.remove(0);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(orca_router::cli::run(args)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("abhoth router: {e:#}");
            std::process::exit(2);
        }
    }
}
