//! The inventory over HTTP (D152): the view, the engine catalog, Hugging Face search and model
//! details from a mock (never the real site), the token (set, never shown, audited), downloads
//! refused without a model folder, the migration at start-up, and admins only.

use std::path::PathBuf;

use orca_router::config::Snapshot;
use orca_router::server::{self, Mode};
use orca_router::state::AppState;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

const TOML: &str = r#"# inventory tests (placeholders only)
version = 1

[router]
listen = "127.0.0.1:0"
state_dir = "STATE"

[control]
enabled = false

[secrets]
ssh_gpu = { file = "/run/secrets/ssh-gpu" }

[hf]
endpoint = "HF"

[servers.gpu]
title = "GPU host"
host = "gpu-host"
exec = "ssh"
ssh = { user = "abhoth-exec", key = "secret:ssh_gpu", host_key = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }

[cards.A]
server = "gpu"
gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
vram_gib = 24.0
on_demand = true

[engines.llamacpp]
title = "llama.cpp"
driver = "docker"
image = "local/llama:test"
container = "llama-{card_lc}"
run = { docker = ["{image}"] }
port = { A = 8081 }
[engines.llamacpp.profiles.two]
lanes = 2
vram_gib = 2.0

[engines.llamacpp-fork]
title = "my llama.cpp fork"
driver = "docker"
image = "local/fork:test"
container = "fork-{card_lc}"
run = { docker = ["{image}"] }
port = { A = 8181 }
caps = { inherits = "llamacpp" }
[engines.llamacpp-fork.profiles.two]
lanes = 2
vram_gib = 2.0

[models.tiny]
title = "Tiny"
arch = "qwen3"
[models.tiny.artifacts.llamacpp]
file = "Tiny-Q4_K_M.gguf"
vram_gib = 5.0
"#;

const JSON: &str = r#"{
  "upstreams": { "A": { "url": "http://127.0.0.1:9", "lanes": 2 } },
  "deployments": [ { "name": "tiny", "cards": ["A"], "servedId": "tiny", "modelInfo": {} } ]
}
"#;

async fn mock_hf() -> String {
    use axum::{Router, extract::Path, routing::get};
    let app = Router::new()
        .route(
            "/api/models",
            get(|| async {
                axum::Json(json!([{"id": "example/Tiny-GGUF", "pipeline_tag": "text-generation", "tags": ["gguf", "license:mit"], "downloads": 5}]))
            }),
        )
        .route(
            "/api/models/{o}/{n}",
            get(|Path((o, n)): Path<(String, String)>| async move {
                if format!("{o}/{n}") != "example/Tiny-GGUF" {
                    return Err(axum::http::StatusCode::NOT_FOUND);
                }
                Ok(axum::Json(json!({
                    "id": "example/Tiny-GGUF", "sha": "r1", "pipeline_tag": "text-generation", "tags": ["gguf", "license:mit"],
                    "gguf": {"architecture": "qwen3", "context_length": 32768},
                    "siblings": [
                        {"rfilename": "Tiny-Q4_K_M.gguf", "size": 5_000_000_000u64},
                        {"rfilename": "Tiny-Q8_0.gguf", "size": 30_000_000_000u64}
                    ]
                })))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    format!("http://{a}")
}

struct Router {
    url: String,
    dir: tempfile::TempDir,
    toml: PathBuf,
}

async fn start() -> Router {
    start_with(|t| t, None).await
}

/// `edit` changes the config text; `exec` installs these executors before the router starts.
async fn start_with(
    edit: impl Fn(String) -> String,
    exec: Option<orca_router::control::downloads::ExecFor>,
) -> Router {
    let hf = mock_hf().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let toml = dir.path().join("router.toml");
    std::fs::write(
        &toml,
        edit(
            TOML.replace("STATE", &state.display().to_string().replace('\\', "/"))
                .replace("\"HF\"", &format!("\"{hf}\"")),
        ),
    )
    .unwrap();
    if let Some(exec) = exec {
        std::fs::create_dir_all(&state).unwrap();
        let store = std::sync::Arc::new(
            orca_router::control::store::Store::open(&state.join("router.db")).unwrap(),
        );
        let backend = orca_router::control_api::live::ConfigBackend::new(&toml, &state, store);
        orca_router::control::downloads::install_with(&toml, &state, backend, exec);
    }
    let json = dir.path().join("router.json");
    std::fs::write(&json, JSON).unwrap();
    let st = AppState::new(Snapshot::load(&json).unwrap(), Some(json.clone()));
    orca_router::control::startup::start(&st, &toml)
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve(l, st, Mode::Gateway, std::future::pending()));
    Router { url, dir, toml }
}

async fn admin(r: &Router) -> HeaderMap {
    let out = r.dir.path().join("k.key");
    let code = orca_router::cli::run(
        [
            "admin-key",
            "create",
            "ops",
            "--control",
            &r.toml.display().to_string(),
            "--out",
            &out.display().to_string(),
        ]
        .map(String::from)
        .to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    let key = std::fs::read_to_string(out).unwrap().trim().to_string();
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.5"));
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
    );
    h
}

#[tokio::test]
async fn the_inventory_view_catalog_and_migration() {
    let r = start().await;
    let h = admin(&r).await;
    let c = reqwest::Client::new();
    // the file was migrated at start-up, with a backup
    let text = std::fs::read_to_string(&r.toml).unwrap();
    assert!(text.contains("[inventory]") && text.contains("# inventory tests"));
    assert!(
        std::fs::read_dir(r.dir.path().join("state/backups"))
            .unwrap()
            .count()
            >= 1
    );
    let v: Value = c
        .get(format!("{}/admin/api/inventory", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["migrated"], true);
    assert_eq!(v["servers"][0]["cards"][0]["id"], "A");
    assert_eq!(
        v["servers"][0]["cards"][0]["engines"]["llm"],
        json!(["llamacpp", "llamacpp-fork"])
    );
    let tiny = &v["models"][0];
    // the fork runs it by capability, with no model edit
    assert_eq!(tiny["order"], json!(["llamacpp", "llamacpp-fork"]));
    let fork = tiny["supported"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["engine"] == "llamacpp-fork")
        .unwrap();
    assert_eq!(fork["reason"], "gguf + qwen3 ✓");
    let eng = v["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "llamacpp-fork")
        .unwrap();
    assert_eq!(eng["caps"]["base"], "llamacpp");
    assert_eq!(eng["usedBy"], json!(["tiny"]));
    let cat: Value = c
        .get(format!("{}/admin/api/catalog", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        cat.as_array()
            .unwrap()
            .iter()
            .any(|k| k["id"] == "llamacpp")
    );
    // admins only
    let anon = c
        .get(format!("{}/admin/api/inventory", r.url))
        .header("x-forwarded-for", "10.0.0.5")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}

#[tokio::test]
async fn hugging_face_search_details_fit_and_the_token() {
    let r = start().await;
    let h = admin(&r).await;
    let c = reqwest::Client::new();
    let s: Value = c
        .get(format!("{}/admin/api/hf/search?q=tiny", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s[0]["repo"], "example/Tiny-GGUF");
    assert_eq!(s[0]["licence"], "mit");
    let d: Value = c
        .get(format!(
            "{}/admin/api/hf/model?repo=example/Tiny-GGUF",
            r.url
        ))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["arch"], "qwen3");
    assert_eq!(d["context"], 32768);
    let q4 = d["choices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["quant"] == "Q4_K_M")
        .unwrap();
    let id = q4["id"].as_str().unwrap();
    assert_eq!(d["fit"][id][0]["fits"], true, "5 GB fits a 24 GiB card");
    let q8 = d["choices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["quant"] == "Q8_0")
        .unwrap();
    assert_eq!(d["fit"][q8["id"].as_str().unwrap()][0]["fits"], false);
    let nf = c
        .get(format!("{}/admin/api/hf/model?repo=example/none", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), 502);
    // the token: set, never shown back, audited without the value
    let t = c
        .put(format!("{}/admin/api/hf/token", r.url))
        .headers(h.clone())
        .json(&json!({"token": "hf_dummytokenvalue"}))
        .send()
        .await
        .unwrap();
    assert_eq!(t.status(), 200);
    let body = t.text().await.unwrap();
    assert!(body.contains("\"set\":true") && !body.contains("dummytoken"));
    let v: Value = c
        .get(format!("{}/admin/api/inventory", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["hf"]["set"], true);
    assert!(!v.to_string().contains("dummytoken"));
    assert!(
        !std::fs::read_to_string(&r.toml)
            .unwrap()
            .contains("dummytoken"),
        "never in the config"
    );
    let audit: Value = c
        .get(format!("{}/admin/api/audit", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(audit.to_string().contains("hf-token.set"));
    assert!(!audit.to_string().contains("dummytoken"));
    // downloads need a model folder on the server
    let dl = c
        .post(format!("{}/admin/api/downloads", r.url))
        .headers(h.clone())
        .json(&json!({"server": "gpu", "repo": "example/Tiny-GGUF", "files": [{"path": "Tiny-Q4_K_M.gguf", "size": 5}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 422);
    assert!(dl.text().await.unwrap().contains("model folder"));
    let list: Value = c
        .get(format!("{}/admin/api/downloads", r.url))
        .headers(h)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list, json!([]));
}

/// The edits the inventory forms send (client/inventoryModel.ts) are valid router.toml.
#[tokio::test]
async fn the_forms_edits_check_clean() {
    let r = start().await;
    let h = admin(&r).await;
    let c = reqwest::Client::new();
    let edits = json!([
        {"path": ["servers", "gpu", "model_dir"], "value": "/srv/models"},
        {"path": ["cards", "B"], "value": {"server": "gpu", "gpu_uuid": "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "label": "RTX 4090", "managed": true, "on_demand": true, "engines": {"llm": ["llamacpp"]}, "vram_gib": 24}},
        {"path": ["cards", "A", "engines"], "value": {"llm": ["llamacpp", "llamacpp-fork"], "embedding": ["llamacpp"]}},
        {"path": ["engines", "llamacpp-fork", "caps"], "value": {"inherits": "llamacpp", "add": {"archs": ["qwen3next"]}}},
        {"path": ["models", "tiny", "primary"], "value": "llamacpp-fork"},
        {"path": ["models", "tiny", "prefer"], "value": ["llamacpp"]},
        {"path": ["models", "tiny-chat"], "value": {
            "title": "Tiny-Chat-GGUF", "kind": "llm", "format": "gguf", "primary": "llamacpp", "servers": [], "size_gib": 5.4,
            "source": {"hf": "example/Tiny-Chat-GGUF", "revision": "abc", "files": ["Tiny-Q4_K_M.gguf"]},
            "arch": "qwen3", "quant": "Q4_K_M", "context": 40960, "vision": true, "licence": "apache-2.0",
            "artifacts": {"llamacpp": {"file": "/srv/models/example/Tiny-Chat-GGUF/Tiny-Q4_K_M.gguf", "vram_gib": 5.9, "options": {"mmproj": "/srv/models/example/Tiny-Chat-GGUF/mmproj-F16.gguf"}}}
        }}
    ]);
    let v: Value = c
        .post(format!("{}/admin/api/config/edit", r.url))
        .headers(h)
        .json(&json!({"edits": edits}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let errors: Vec<&Value> = v["check"]["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["severity"] == "error")
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(v["check"]["ok"], true, "{v}");
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("[models.tiny-chat]") && text.contains("inherits = \"llamacpp\""));
}

/// Folder browsing on a local server: listing, stat, new folder (audited), refusals.
#[tokio::test]
async fn folders_are_browsed_on_the_server() {
    let models = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(models.path())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let root = root.strip_prefix("//?/").unwrap_or(&root).to_string();
    std::fs::write(format!("{root}/m-Q4_K_M.gguf"), vec![0u8; 10]).unwrap();
    std::fs::create_dir(format!("{root}/sub")).unwrap();
    let r2 = root.clone();
    let r = start_with(
        move |t| {
            let ssh = t
                .lines()
                .find(|l| l.starts_with("ssh = {"))
                .unwrap()
                .to_string();
            t.replace("exec = \"ssh\"", "exec = \"local\"")
                .replace(&ssh, &format!("browse_roots = [\"{r2}\"]"))
        },
        None,
    )
    .await;
    let h = admin(&r).await;
    let c = reqwest::Client::new();
    let url = |p: &str| format!("{}/admin/api/inventory/{p}", r.url);
    let v: Value = c
        .get(url(&format!("browse?server=gpu&path={root}")))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["entries"][0]["name"], "sub", "{v}");
    assert_eq!(v["entries"][1]["model"], true);
    let out = c
        .get(url(&format!("browse?server=gpu&path={root}/../")))
        .headers(h.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(out.status(), 422);
    let s: Value = c
        .get(url(&format!("stat?server=gpu&path={root}/sub")))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["dir"], true);
    let m = c
        .post(url("mkdir"))
        .headers(h.clone())
        .json(&json!({"server": "gpu", "path": format!("{root}/new-folder")}))
        .send()
        .await
        .unwrap();
    assert_eq!(m.status(), 200);
    assert!(std::path::Path::new(&format!("{root}/new-folder")).is_dir());
    let audit: Value = c
        .get(format!("{}/admin/api/audit", r.url))
        .headers(h.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(audit.to_string().contains("folder.create"));
    let anon = c
        .get(url(&format!("browse?server=gpu&path={root}")))
        .header("x-forwarded-for", "10.0.0.5")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}

/// A strict host whose allowlist lacks the builtins: "not enabled", not an error page.
#[tokio::test]
async fn a_host_without_browsing_says_so() {
    use orca_router::control::exec::{ExecError, Executor, FakeExecutor};
    use std::sync::Arc;
    let fake = Arc::new(FakeExecutor::new());
    fake.on(
        "list-dir",
        vec![Err(ExecError::Refused(
            "builtin list-dir is not allowed".into(),
        ))],
    );
    let f2 = fake.clone();
    let exec: orca_router::control::downloads::ExecFor =
        Arc::new(move |_c, _s| Ok(f2.clone() as Arc<dyn Executor>));
    let r = start_with(|t| t, Some(exec)).await;
    let h = admin(&r).await;
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/admin/api/inventory/browse?server=gpu&path=/srv//models/",
            r.url
        ))
        .headers(h)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 501);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["enabled"], false);
    assert!(v["error"].as_str().unwrap().contains("isn't enabled"));
    // the path went as a normalised argument, never as shell text
    let req = serde_json::to_value(&fake.requests()[0]).unwrap();
    assert_eq!(req["args"]["path"], "/srv/models", "{req}");
}
