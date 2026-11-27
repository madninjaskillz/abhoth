//! Sothoth, the router alone (the abhoth-sothoth image's binary):
//! `abhoth-sothoth [sothoth] --config sothoth.json ...`. The same program as `abhoth sothoth` and
//! the old `abhoth-router` (docs/NAMES.md).

fn main() {
    orca_router::cli::standalone();
}
