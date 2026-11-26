use super::*;

#[test]
fn paths_are_normalised_and_escapes_refused() {
    assert_eq!(clean("/srv//models/./qwen/").unwrap(), "/srv/models/qwen");
    assert_eq!(clean("  /  ").unwrap(), "/");
    assert_eq!(
        clean("/a b/c'd/$(x)").unwrap(),
        "/a b/c'd/$(x)",
        "odd names are names, never shell text"
    );
    for bad in [
        "",
        "srv/models",
        "./x",
        "~/models",
        "/srv/../etc",
        "/srv/..",
        "/a\u{0}b",
        "/a\nb",
        "$(rm -rf /)",
    ] {
        assert!(clean(bad).is_err(), "{bad:?}");
    }
    #[cfg(not(windows))]
    assert!(clean(r"/a\b").is_err());
    assert_eq!(parent("/srv/models"), Some("/srv".into()));
    assert_eq!(parent("/srv"), Some("/".into()));
    assert_eq!(parent("/"), None);
}

#[test]
fn roots_and_hidden_trees() {
    let any = Roots { roots: vec![] };
    assert!(any.allows("/srv/models"));
    for h in [
        "/proc",
        "/proc/1/environ",
        "/sys/kernel",
        "/dev/sda",
        "/run/secrets",
    ] {
        assert!(!any.allows(h), "{h}");
    }
    assert!(
        any.allows("/devices"),
        "a sibling with the same prefix is not hidden"
    );
    let r = Roots {
        roots: vec!["/srv/models".into(), "/mnt/data/".into()],
    };
    assert!(r.allows("/srv/models") && r.allows("/srv/models/a/b") && r.allows("/mnt/data/x"));
    assert!(!r.allows("/srv") && !r.allows("/srv/models2") && !r.allows("/etc"));
    // a bad root never widens anything
    assert!(
        !Roots {
            roots: vec!["../".into()]
        }
        .allows("/srv")
    );
    assert!(
        is_model_file("Q.GGUF")
            && is_model_file("m-00001-of-00002.safetensors")
            && !is_model_file("README.md")
    );
}

/// A temp folder as a path the browse functions take (`C:/…` on Windows, `/…` elsewhere).
fn tmp() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    let p = std::fs::canonicalize(d.path())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
    (d, p)
}

#[test]
fn listing_puts_folders_first_marks_model_files_and_pages() {
    let (_d, root) = tmp();
    std::fs::create_dir(format!("{root}/zeta")).unwrap();
    std::fs::create_dir(format!("{root}/Alpha")).unwrap();
    std::fs::write(format!("{root}/model-Q4_K_M.gguf"), vec![0u8; 1234]).unwrap();
    std::fs::write(format!("{root}/notes.txt"), b"x").unwrap();
    let roots = Roots {
        roots: vec![root.clone()],
    };
    let v = list(&root, &roots, 0, 200).unwrap();
    let names: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Alpha", "zeta", "model-Q4_K_M.gguf", "notes.txt"]);
    let g = &v["entries"][2];
    assert_eq!(g["model"], true);
    assert_eq!(g["size"], 1234);
    assert_eq!(v["entries"][0]["dir"], true);
    assert_eq!(v["parent"], Value::Null, "the parent is outside the roots");
    assert_eq!(v["writable"], true);
    // pages
    let p = list(&root, &roots, 1, 2).unwrap();
    assert_eq!(p["entries"].as_array().unwrap().len(), 2);
    assert_eq!(p["entries"][0]["name"], "zeta");
    assert_eq!(p["more"], true);
    assert_eq!(p["total"], 4);
    // outside the roots, and .. never gets there
    assert!(list(&format!("{root}/../"), &roots, 0, 10).is_err());
    let sub = Roots {
        roots: vec![format!("{root}/zeta")],
    };
    assert!(list(&root, &sub, 0, 10).unwrap_err().contains("outside"));
}

#[cfg(unix)]
#[test]
fn symlinks_out_of_the_roots_are_not_followed() {
    let (_d, root) = tmp();
    let (_o, other) = tmp();
    std::fs::create_dir(format!("{root}/in")).unwrap();
    std::os::unix::fs::symlink(&other, format!("{root}/out")).unwrap();
    std::os::unix::fs::symlink(format!("{root}/in"), format!("{root}/inside-link")).unwrap();
    let roots = Roots {
        roots: vec![root.clone()],
    };
    let v = list(&root, &roots, 0, 10).unwrap();
    let out = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "out")
        .unwrap();
    assert_eq!(out["outside"], true);
    let inl = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "inside-link")
        .unwrap();
    assert_eq!(inl["outside"], false);
    assert!(
        list(&format!("{root}/out"), &roots, 0, 10)
            .unwrap_err()
            .contains("leads outside")
    );
    assert!(mkdir(&format!("{root}/out/x"), &roots).is_err());
}

#[test]
fn stat_and_mkdir() {
    let (_d, root) = tmp();
    let roots = Roots {
        roots: vec![root.clone()],
    };
    let s = stat(&root, &roots).unwrap();
    assert_eq!(s["exists"], true);
    assert_eq!(s["dir"], true);
    assert_eq!(s["writable"], true);
    let n = stat(&format!("{root}/new/deeper"), &roots).unwrap();
    assert_eq!(n["exists"], false);
    assert_eq!(n["parentExists"], root.as_str());
    assert_eq!(stat("/etc", &roots).unwrap()["allowed"], false);
    let m = mkdir(&format!("{root}/models"), &roots).unwrap();
    assert_eq!(m["exists"], true);
    assert!(
        mkdir(&format!("{root}/models"), &roots)
            .unwrap_err()
            .contains("already exists")
    );
    assert!(
        mkdir(&format!("{root}/a/b"), &roots).is_err(),
        "one level only: the parent must exist"
    );
    assert!(mkdir(&format!("{root}/.hidden"), &roots).is_err());
    assert!(mkdir("/etc/evil", &roots).is_err());
}
