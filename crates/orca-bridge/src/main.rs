//! `abhoth-bridge`: the headless bridge under its old name (the image keeps it). Same as
//! `abhoth-migo` and `abhoth migo` (docs/NAMES.md).

fn main() {
    orca_bridge::cli::standalone("abhoth-bridge");
}
