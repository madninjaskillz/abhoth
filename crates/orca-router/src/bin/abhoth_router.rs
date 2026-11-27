//! The router alone under its old name (the Docker image's binary):
//! `abhoth-router [router] --config router.json ...`. Same as `abhoth-sothoth` (docs/NAMES.md).

fn main() {
    orca_router::cli::standalone();
}
