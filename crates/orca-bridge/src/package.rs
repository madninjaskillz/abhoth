//! Engines from engine packages (D153, ROUTER-MEDIA-PLAN.md 13.3) through the shared runner
//! `orca-engines`: `package = "kokoro"` in an engine's config picks the manifest from the default
//! store, the method that fits this host and card (docker or compose unless the engine allows
//! host processes), installs it on first start (every artifact pinned and checked) and runs it
//! under the same name scheme as the bridge's own containers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orca_engines::exec::{HttpFetch, LocalExec};
use orca_engines::host::{CardSel, HostFacts, MethodFilter, detect_local};
use orca_engines::manifest::GpuKind;
use orca_engines::render::{Limits, RunSpec as PkgSpec};
use orca_engines::runner::{LocalRunner, Runner as PkgRunner};
use orca_engines::state::{Approvals, Phase};
use orca_engines::store::{Package, embedded};
use tokio::sync::OnceCell;

use crate::runner::{Handle, RunSpec, Runner};

/// The package an engine names: `id` or `id@version` (the newest in the store if no version).
pub fn find(want: &str) -> Result<Package, String> {
    let (id, version) = match want.split_once('@') {
        Some((i, v)) => (i, Some(v)),
        None => (want, None),
    };
    let mut all: Vec<Package> = embedded::packages()
        .into_iter()
        .filter(|p| p.manifest.id == id && version.is_none_or(|v| p.manifest.version == v))
        .collect();
    all.sort_by(|a, b| a.manifest.version.cmp(&b.manifest.version));
    all.pop()
        .ok_or_else(|| format!("no engine package `{want}` in the default store"))
}

fn cpus(spec: &RunSpec) -> Option<f64> {
    spec.cfg
        .docker
        .as_ref()
        .and_then(|d| d.cpus.as_deref())
        .and_then(|c| c.parse::<f64>().ok())
}

/// Fill what an engine with `package = "..."` leaves out from its manifest: kinds, api, caps
/// (`inherits` the package id), the health path or `tcp`, and the start timeout. The package
/// becomes `id@version`. An unknown package is an error.
pub fn fill(c: &mut crate::config::BridgeConfig) -> Result<(), String> {
    for (eid, e) in c.engines.iter_mut() {
        let Some(want) = e.package.clone() else {
            continue;
        };
        let p = find(&want).map_err(|x| format!("engines.{eid}: {x}"))?;
        let m = &p.manifest;
        if e.kinds.is_empty() {
            e.kinds = m.kinds.clone();
        }
        if e.api.is_none() && !m.api.is_empty() {
            e.api = Some(m.api.clone());
        }
        if e.caps.is_none() {
            e.caps = Some(
                serde_json::json!({"inherits": m.caps.inherits.clone().unwrap_or_else(|| m.id.clone())}),
            );
        }
        if e.docker.is_none()
            && e.process.is_none()
            && let Some(h) = m.methods.first().map(|x| &x.health)
        {
            if h.tcp && e.health == "/health" {
                e.health = "tcp".into();
            } else if let Some(path) = h.http.as_ref().filter(|_| e.health == "/health") {
                e.health = path.clone();
            }
            if let Some(s) = h.start_seconds {
                e.start_timeout_seconds = e.start_timeout_seconds.max(s);
            }
        }
        e.package = Some(m.key());
    }
    Ok(())
}

/// The card as the shared runner sees it.
pub fn card_sel(spec: &RunSpec, facts: &HostFacts) -> CardSel {
    match &spec.gpu {
        Some(g) => CardSel::Gpu {
            card: spec.card.clone(),
            index: g.index,
            uuid: g.uuid.clone(),
            vendor: GpuKind::Cuda,
            vram_gib: g.vram_gib,
        },
        None => CardSel::Cpu {
            card: spec.card.clone(),
            cores: cpus(spec)
                .map(|c| c.ceil() as u32)
                .unwrap_or(facts.cpu.cores),
            ram_gib: spec.cfg.mem_gib.unwrap_or(facts.cpu.ram_gib),
        },
    }
}

/// The shared runner's start spec for one engine on one card.
pub fn pkg_spec(spec: &RunSpec, card: CardSel) -> PkgSpec {
    PkgSpec {
        engine: spec.engine.clone(),
        card,
        port: spec.port,
        ports: Default::default(),
        host: "127.0.0.1".into(),
        model: None,
        params: spec.cfg.params.clone().into_iter().collect(),
        limits: Limits {
            cpus: cpus(spec),
            mem_gib: spec.cfg.mem_gib,
        },
        model_dir: spec.model_dir.clone(),
        state: None,
        package_dir: None,
        name: spec.container(),
    }
}

/// [`Runner`] over `orca_engines::LocalRunner` (made on first use: detection runs commands).
pub struct PackageRunner {
    state_dir: PathBuf,
    inner: OnceCell<Arc<LocalRunner>>,
}

impl PackageRunner {
    pub fn new(state_dir: PathBuf) -> PackageRunner {
        PackageRunner {
            state_dir,
            inner: OnceCell::new(),
        }
    }

    async fn runner(&self) -> Arc<LocalRunner> {
        self.inner
            .get_or_init(|| async {
                let facts = detect_local().await;
                Arc::new(LocalRunner::new(
                    self.state_dir.clone(),
                    Arc::new(LocalExec),
                    Arc::new(HttpFetch::default()),
                    true,
                    facts,
                ))
            })
            .await
            .clone()
    }
}

fn wrap(h: orca_engines::Handle) -> Handle {
    Handle::Package {
        handle: Box::new(h),
    }
}

fn unwrap(h: &Handle) -> Result<&orca_engines::Handle, String> {
    match h {
        Handle::Package { handle } => Ok(handle),
        _ => Err("not a package handle".into()),
    }
}

#[async_trait]
impl Runner for PackageRunner {
    async fn start(&self, spec: &RunSpec) -> Result<Handle, String> {
        let want = spec
            .cfg
            .package
            .as_deref()
            .ok_or("this engine has no package")?;
        let p = find(want)?;
        let r = self.runner().await;
        let card = card_sel(spec, r.facts());
        let filter = MethodFilter {
            host_processes: spec.cfg.host_processes,
            only: None,
        };
        let (method, _) = orca_engines::choose_method(&p.manifest, r.facts(), &card, &filter)
            .map_err(|rej| {
                let why: Vec<String> = rej
                    .iter()
                    .map(|x| format!("{}: {}", x.label, x.reasons.join(", ")))
                    .collect();
                format!("no method of {} fits here ({})", p.key(), why.join("; "))
            })?;
        let installed = match r.layout.find_installed(&p.manifest.id, &p.manifest.version) {
            Some(i) if i.method == method => i,
            _ => r
                .install(&p, method, &card, &Approvals::default())
                .await
                .map_err(|e| format!("installing {}: {e}", p.key()))?,
        };
        r.start(&p.manifest, &installed, &pkg_spec(spec, card))
            .await
            .map(wrap)
            .map_err(|e| e.to_string())
    }

    async fn prepare(&self, spec: &RunSpec) -> Result<String, String> {
        let want = spec
            .cfg
            .package
            .as_deref()
            .ok_or("this engine has no package")?;
        let p = find(want)?;
        let r = self.runner().await;
        let card = card_sel(spec, r.facts());
        let filter = MethodFilter {
            host_processes: spec.cfg.host_processes,
            only: None,
        };
        let (method, _) = orca_engines::choose_method(&p.manifest, r.facts(), &card, &filter)
            .map_err(|_| format!("no method of {} fits here", p.key()))?;
        match r.layout.find_installed(&p.manifest.id, &p.manifest.version) {
            Some(i) if i.method == method => Ok(format!("{} is installed", p.key())),
            _ => r
                .install(&p, method, &card, &Approvals::default())
                .await
                .map(|_| format!("installed {}", p.key()))
                .map_err(|e| {
                    format!(
                        "installing {} failed ({e}); its image may need loading with `docker load`",
                        p.key()
                    )
                }),
        }
    }

    async fn stop(&self, h: &Handle, grace: Duration) -> Result<(), String> {
        let h = unwrap(h)?;
        self.runner()
            .await
            .stop(h, grace)
            .await
            .map_err(|e| e.to_string())
    }

    async fn running(&self, h: &Handle) -> bool {
        let Ok(h) = unwrap(h) else {
            return false;
        };
        self.runner().await.status(h).await.is_ok_and(|s| {
            matches!(
                s.phase,
                Phase::Starting | Phase::Running | Phase::Healthy | Phase::Unhealthy
            )
        })
    }

    async fn logs(&self, h: &Handle, lines: u32) -> Result<String, String> {
        let h = unwrap(h)?;
        self.runner()
            .await
            .logs(h, lines)
            .await
            .map_err(|e| e.to_string())
    }

    async fn adopt(&self, spec: &RunSpec) -> Option<Handle> {
        let name = spec.container();
        let r = self.runner().await;
        r.adopt()
            .await
            .into_iter()
            .find(|h| h.name() == name)
            .map(wrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_engine_takes_its_kinds_api_and_health_from_the_manifest() {
        let mut c: crate::config::BridgeConfig = toml::from_str(
            r#"
            [bridge]
            id = "nas"
            [engines.whisper]
            package = "whisper"
            cards = ["CPU"]
            port = 10300
            [engines.kokoro]
            package = "kokoro"
            kinds = ["tts"]
            health = "/ready"
            cards = ["CPU"]
            port = 18880
            "#,
        )
        .unwrap();
        fill(&mut c).unwrap();
        let w = &c.engines["whisper"];
        assert_eq!(w.kinds, ["stt"]);
        assert_eq!(w.api.as_deref(), Some("wyoming"));
        assert_eq!(w.health, "tcp");
        assert!(w.package.as_deref().unwrap().starts_with("whisper@"));
        assert_eq!(w.method(), "package");
        // what the config says wins
        assert_eq!(c.engines["kokoro"].health, "/ready");
        c.engines.get_mut("kokoro").unwrap().package = Some("nope".into());
        assert!(fill(&mut c).unwrap_err().contains("engines.kokoro"));
    }

    #[test]
    fn the_default_store_has_the_speech_packages() {
        for id in ["kokoro", "whisper", "chatterbox"] {
            let p = find(id).unwrap();
            assert_eq!(p.manifest.id, id);
            assert!(find(&p.key()).is_ok());
        }
        assert!(
            find("kokoro@0.0.0")
                .unwrap_err()
                .contains("no engine package")
        );
    }
}
