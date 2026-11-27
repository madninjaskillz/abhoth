//! `abhoth engine …`: package tools for authors and admins.
//!
//! - `abhoth engine validate <dir|abhoth-engine.json> [--private] [--json]`: the checks a store
//!   runs (shape, templates, pins, metadata only). Exit 0 when valid, 1 when refused.
//! - `abhoth engine index <store-dir> [--name <name>] [--check]`: rebuild a store's
//!   `index.json` (or, with `--check`, fail when it is stale).
//! - `abhoth engine fit <dir> [--card <id>] [--json]`: which method this machine would use.
//! - `abhoth engine schema`: print the manifest's JSON Schema.

use std::path::PathBuf;

use crate::host::{self, CardSel, MethodFilter};
use crate::manifest::MANIFEST_FILE;
use crate::store;
use crate::validate::{self, Mode};

const USAGE: &str = "usage:
  abhoth engine validate <package-dir|abhoth-engine.json> [--private] [--json]
  abhoth engine index <store-dir> [--name <name>] [--check]
  abhoth engine fit <package-dir> [--card <gpu-index|cpu>] [--json]
  abhoth engine schema";

pub async fn main(args: &[String]) -> i32 {
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let positional: Vec<&String> = {
        let mut out = vec![];
        let mut skip = false;
        for a in args.iter().skip(1) {
            if skip {
                skip = false;
                continue;
            }
            if a == "--name" || a == "--card" {
                skip = true;
                continue;
            }
            if !a.starts_with("--") {
                out.push(a);
            }
        }
        out
    };
    match args.first().map(String::as_str) {
        Some("validate") => {
            let Some(p) = positional.first() else {
                eprintln!("{USAGE}");
                return 2;
            };
            let mode = if flag("--private") {
                Mode::Private
            } else {
                Mode::Store
            };
            let r = validate::validate_path(&PathBuf::from(p), mode);
            if flag("--json") {
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
            } else {
                print!("{}", r.text());
            }
            if r.ok { 0 } else { 1 }
        }
        Some("index") => {
            let Some(p) = positional.first() else {
                eprintln!("{USAGE}");
                return 2;
            };
            let dir = PathBuf::from(p);
            let name = opt("--name").unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "store".into())
            });
            let (index, reports) = store::build_index(&dir, &name);
            let mut bad = 0;
            for (d, r) in &reports {
                if !r.ok {
                    bad += 1;
                    eprintln!("{d}: refused\n{}", r.text());
                }
            }
            let text = serde_json::to_string_pretty(&index).unwrap_or_default() + "\n";
            let file = dir.join(store::INDEX_FILE);
            if flag("--check") {
                let cur = std::fs::read_to_string(&file).unwrap_or_default();
                let same = serde_json::from_str::<store::Index>(&cur).ok() == Some(index.clone());
                if !same {
                    eprintln!(
                        "{} is stale: run abhoth engine index {}",
                        file.display(),
                        dir.display()
                    );
                    return 1;
                }
                println!(
                    "{}: {} packages, current",
                    file.display(),
                    index.packages.len()
                );
                return if bad > 0 { 1 } else { 0 };
            }
            if let Err(e) = std::fs::write(&file, text) {
                eprintln!("{}: {e}", file.display());
                return 1;
            }
            println!("{}: {} packages", file.display(), index.packages.len());
            if bad > 0 { 1 } else { 0 }
        }
        Some("fit") => {
            let Some(p) = positional.first() else {
                eprintln!("{USAGE}");
                return 2;
            };
            let r = validate::validate_path(&PathBuf::from(p), Mode::Private);
            let Some(m) = r.manifest else {
                print!("{}", r.text());
                return 1;
            };
            let h = host::detect_local().await;
            let cards: Vec<CardSel> = match opt("--card").as_deref() {
                Some("cpu") => vec![CardSel::cpu("CPU", &h)],
                Some(i) => h
                    .gpus
                    .iter()
                    .filter(|g| g.index.to_string() == i)
                    .map(|g| CardSel::for_gpu(&format!("GPU{}", g.index), g))
                    .collect(),
                None => {
                    let mut c: Vec<CardSel> = h
                        .gpus
                        .iter()
                        .map(|g| CardSel::for_gpu(&format!("GPU{}", g.index), g))
                        .collect();
                    c.push(CardSel::cpu("CPU", &h));
                    c
                }
            };
            let fits: Vec<_> = cards
                .iter()
                .map(|c| {
                    (
                        c.id().to_string(),
                        host::fit(&m, &h, c, &MethodFilter::local()),
                    )
                })
                .collect();
            if flag("--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"host": h, "fits": fits}))
                        .unwrap_or_default()
                );
            } else {
                println!("{} on {}/{}:", m.key(), h.os, h.arch);
                for (c, f) in &fits {
                    match &f.method_label {
                        Some(l) => println!("  {c}: {l}"),
                        None => println!("  {c}: no method fits"),
                    }
                    for rj in &f.rejections {
                        println!("      {}: {}", rj.label, rj.reasons.join("; "));
                    }
                    for a in &f.approvals {
                        println!("      needs approval: {a}");
                    }
                }
            }
            0
        }
        Some("schema") => {
            println!("{}", store::embedded::schema());
            0
        }
        _ => {
            eprintln!(
                "{USAGE}\n\nA package is a folder with {MANIFEST_FILE} (docs/ENGINE-PACKAGES.md)."
            );
            2
        }
    }
}
