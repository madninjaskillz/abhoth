//! Finds the router admin pages' bundle (`client/dist-router`, D151) for `rust-embed`
//! (`ABHOTH_ROUTER_UI_DIST`, relative to this crate). An `ABHOTH_ROUTER_UI_DIST` set in the
//! environment wins. Without the folder the router still builds (an empty one is made in
//! OUT_DIR) and `/admin/` says the pages are not built in.

use std::path::Path;

const CANDIDATE: &str = "../../client/dist-router";

fn main() {
    println!("cargo:rerun-if-env-changed=ABHOTH_ROUTER_UI_DIST");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = match std::env::var("ABHOTH_ROUTER_UI_DIST") {
        Ok(set) if !set.trim().is_empty() => set,
        _ if Path::new(&manifest)
            .join(CANDIDATE)
            .join("index.html")
            .is_file() =>
        {
            CANDIDATE.to_string()
        }
        _ => {
            let out = std::env::var("OUT_DIR").expect("OUT_DIR");
            let empty = Path::new(&out).join("no-router-ui");
            std::fs::create_dir_all(&empty).expect("empty ui folder");
            empty.to_string_lossy().into_owned()
        }
    };
    println!("cargo:rerun-if-changed={dist}");
    println!("cargo:rustc-env=ABHOTH_ROUTER_UI_DIST={dist}");
}
