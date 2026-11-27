//! `abhoth-bridge`: the headless bridge (the abhoth-bridge image). Same as `abhoth bridge`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("abhoth-bridge {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(orca_bridge::cli::main(&args, env!("CARGO_PKG_VERSION")));
    std::process::exit(code);
}
