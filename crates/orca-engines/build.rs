//! Embeds the default engine store (`stores/default/`) and the manifest schema, so the router
//! lists the starter packages with no network. Text files only (the store is metadata only).

use std::path::{Path, PathBuf};

fn walk(base: &Path, d: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    v.sort();
    for p in v {
        if p.is_dir() {
            walk(base, &p, out);
        } else if p.is_file() {
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, p));
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("..")
        .join("..")
        .join("stores");
    let store = root.join("default");
    let schema = root.join("schema").join("abhoth-engine.schema.json");
    println!("cargo:rerun-if-changed={}", store.display());
    println!("cargo:rerun-if-changed={}", schema.display());
    let mut files = vec![];
    walk(&store, &store, &mut files);
    let mut src = String::from("pub static DEFAULT_STORE_FILES: &[(&str, &str)] = &[\n");
    for (rel, p) in &files {
        println!("cargo:rerun-if-changed={}", p.display());
        // only text: a binary in the store fails `abhoth engine validate` and the tests
        if std::fs::read_to_string(p).is_ok() {
            src.push_str(&format!(
                "    ({rel:?}, include_str!({:?})),\n",
                p.to_string_lossy()
            ));
        }
    }
    src.push_str("];\n");
    if schema.is_file() {
        src.push_str(&format!(
            "pub static MANIFEST_SCHEMA: &str = include_str!({:?});\n",
            schema.to_string_lossy()
        ));
    } else {
        src.push_str("pub static MANIFEST_SCHEMA: &str = \"{}\";\n");
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("default_store.rs");
    std::fs::write(out, src).expect("write default_store.rs");
}
