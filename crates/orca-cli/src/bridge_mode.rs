//! The desktop app's Bridge mode (ROUTER-MEDIA-PLAN.md 11): the same bridge as `abhoth bridge`,
//! run in-process while `desktop.bridgeMode` is on. It dials the router (no port opens on the
//! PC), so the router can start and stop engines here and send them requests. The settings are
//! read every few seconds, so turning the mode on or off, or changing the router or token,
//! applies without a restart. Off (or the app closed), the router does not use this PC.

use std::path::{Path, PathBuf};
use std::time::Duration;

use orca_bridge::client::{self, Timing};
use orca_bridge::config::BridgeConfig;
use orca_core::config::DesktopConfig;

/// The bridge config the settings ask for, or why there is none (None: the mode is off).
pub fn wanted(d: &DesktopConfig, home: &Path) -> Option<Result<BridgeConfig, String>> {
    if !d.bridge_mode {
        return None;
    }
    Some(build(d, home))
}

fn build(d: &DesktopConfig, home: &Path) -> Result<BridgeConfig, String> {
    let mut c = if d.bridge_config.trim().is_empty() {
        BridgeConfig::default()
    } else {
        let p = PathBuf::from(d.bridge_config.trim());
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        toml::from_str::<BridgeConfig>(&text).map_err(|e| format!("{}: {e}", p.display()))?
    };
    if !d.bridge_router.trim().is_empty() {
        c.bridge.router = d.bridge_router.trim().to_string();
    }
    if !d.bridge_id.trim().is_empty() {
        c.bridge.id = d.bridge_id.trim().to_string();
    }
    if c.bridge.id.is_empty() {
        c.bridge.id = "pc".into();
    }
    if c.bridge.title.is_empty() {
        c.bridge.title = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| c.bridge.id.clone());
    }
    if c.bridge.state_dir.is_empty() {
        c.bridge.state_dir = home.join("bridge").display().to_string();
    }
    if !d.bridge_token.trim().is_empty() {
        c.token_value = Some(d.bridge_token.trim().to_string());
    }
    c.check()?;
    Ok(c)
}

/// What the settings asked for last, without the token (to notice a change).
fn signature(c: &BridgeConfig) -> String {
    let mut c = c.clone();
    let token = c.token_value.take();
    format!(
        "{}|{}",
        toml::to_string(&c).unwrap_or_default(),
        token
            .map(|t| orca_bridge::proto::token_hash(&t))
            .unwrap_or_default()
    )
}

/// Watch the settings on a thread of its own and run the bridge while the mode is on. The
/// bridge's state goes to `<home>/bridge/status.json` (no token in it).
pub fn spawn_watcher(home: PathBuf, read: fn(&Path) -> DesktopConfig) {
    let spawned = std::thread::Builder::new()
        .name("abhoth-bridge-mode".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("bridge mode: no runtime: {e}");
                    return;
                }
            };
            rt.block_on(watch(home, read));
        });
    if let Err(e) = spawned {
        tracing::warn!("bridge mode: no thread: {e}");
    }
}

async fn watch(home: PathBuf, read: fn(&Path) -> DesktopConfig) {
    let log: orca_bridge::Log = std::sync::Arc::new(|line: &str| tracing::info!("bridge: {line}"));
    let mut running: Option<(String, client::Handle)> = None;
    let mut last_error = String::new();
    loop {
        let want = wanted(&read(&home), &home);
        match want {
            None => {
                if let Some((_, h)) = running.take() {
                    tracing::info!("bridge mode: off; stopping the bridge");
                    h.stop();
                }
                last_error.clear();
            }
            Some(Err(e)) => {
                if e != last_error {
                    tracing::warn!("bridge mode: {e}");
                    last_error = e;
                }
                if let Some((_, h)) = running.take() {
                    h.stop();
                }
            }
            Some(Ok(c)) => {
                last_error.clear();
                let sig = signature(&c);
                if running.as_ref().is_none_or(|(s, _)| *s != sig) {
                    if let Some((_, h)) = running.take() {
                        h.stop();
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    tracing::info!("bridge mode: on; dialling {}", c.bridge.router);
                    let h =
                        client::spawn(c, env!("CARGO_PKG_VERSION"), log.clone(), Timing::default());
                    running = Some((sig, h));
                }
            }
        }
        write_status(&home, running.as_ref().map(|(_, h)| h), &last_error);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn write_status(home: &Path, h: Option<&client::Handle>, error: &str) {
    let dir = home.join("bridge");
    if h.is_none() && error.is_empty() && !dir.exists() {
        return;
    }
    let v = match h {
        Some(h) => {
            let s = h.status();
            serde_json::json!({"on": true, "connected": s.connected, "server": s.server,
                "enabled": s.enabled, "router": s.router, "lastError": s.last_error, "connects": s.connects})
        }
        None => serde_json::json!({"on": false, "lastError": (!error.is_empty()).then_some(error)}),
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("status.json"), v.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_make_a_bridge_config_only_when_on() {
        let home = Path::new("/home/u/.abhoth");
        let mut d = DesktopConfig::default();
        assert!(wanted(&d, home).is_none());
        d.bridge_mode = true;
        let e = wanted(&d, home).unwrap().unwrap_err();
        assert!(e.contains("bridge.router"), "{e}");
        d.bridge_router = "http://deepthought:8010".into();
        d.bridge_token = "abb_secret_value_for_the_test_only_xxxxxxxxxx".into();
        let c = wanted(&d, home).unwrap().unwrap();
        assert_eq!(c.bridge.id, "pc");
        assert!(
            c.bridge
                .state_dir
                .replace('\\', "/")
                .ends_with(".abhoth/bridge")
        );
        assert_eq!(c.token().unwrap(), d.bridge_token);
        // the change signature never holds the token itself
        assert!(!signature(&c).contains("abb_secret"));
        let mut d2 = d.clone();
        d2.bridge_token = "abb_another_value_for_the_test_only_xxxxxxxxx".into();
        assert_ne!(
            signature(&c),
            signature(&wanted(&d2, home).unwrap().unwrap())
        );
    }

    #[test]
    fn an_engines_file_adds_engines_and_the_settings_win() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bridge.toml");
        std::fs::write(
            &f,
            r#"
            [bridge]
            id = "fromfile"
            router = "http://other:8010"
            token_file = "unused"
            [engines.comfyui]
            kinds = ["image"]
            cards = ["GPU0"]
            port = 18188
            health = "/system_stats"
            [engines.comfyui.process]
            command = ["python", "main.py", "--port", "{port}"]
            "#,
        )
        .unwrap();
        let d = DesktopConfig {
            bridge_mode: true,
            bridge_router: "http://deepthought:8010".into(),
            bridge_config: f.display().to_string(),
            ..DesktopConfig::default()
        };
        let c = wanted(&d, dir.path()).unwrap().unwrap();
        assert_eq!(c.bridge.router, "http://deepthought:8010");
        assert_eq!(c.bridge.id, "fromfile");
        assert_eq!(c.engines["comfyui"].method(), "process");
    }
}
