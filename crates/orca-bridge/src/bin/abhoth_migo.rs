//! `abhoth-migo`: Migo, the headless bridge (the abhoth-migo image). Same as `abhoth migo` and
//! the old `abhoth-bridge` (docs/NAMES.md).

fn main() {
    orca_bridge::cli::standalone("abhoth-migo");
}
