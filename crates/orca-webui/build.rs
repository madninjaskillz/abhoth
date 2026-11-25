//! Finds the React client's `dist` folder for `rust-embed` (`ABHOTH_CLIENT_DIST`, relative to
//! this crate). It lives at `client/dist` in the abhoth repo layout and at
//! `src/OpenOrca.WebUi/client/dist` in the openorca repo (D130). An `ABHOTH_CLIENT_DIST` set in
//! the environment wins.

use std::path::Path;

const CANDIDATES: [&str; 2] = [
    "../../client/dist",
    "../../../src/OpenOrca.WebUi/client/dist",
];

fn main() {
    println!("cargo:rerun-if-env-changed=ABHOTH_CLIENT_DIST");
    let dist = match std::env::var("ABHOTH_CLIENT_DIST") {
        Ok(set) if !set.trim().is_empty() => set,
        _ => {
            let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
            CANDIDATES
                .iter()
                .find(|c| Path::new(&manifest).join(c).join("index.html").is_file())
                .unwrap_or(&CANDIDATES[0])
                .to_string()
        }
    };
    println!("cargo:rerun-if-changed={dist}");
    println!("cargo:rustc-env=ABHOTH_CLIENT_DIST={dist}");
}
