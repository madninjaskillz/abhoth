//! Standalone `abhoth-hostd` binary. `abhoth hostd` in the main CLI runs the same code once
//! wired in.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    std::process::exit(rt.block_on(orca_hostd::cli::main(&args)));
}
