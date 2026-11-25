//! The desktop app's own title bar (D143): on Windows the window is frameless (tao, no
//! decorations, DWM shadow kept) and the web UI draws the title bar. The page drives the window
//! through wry IPC: minimise, maximise/restore, close (as the OS close button: to the tray),
//! move, resize from the top edge, and switching to the OS title bar
//! (`desktop.systemTitleBar`). Only the app's own page can send them: the bridge is injected into
//! the top frame of the app's origin only, every message carries a per-launch nonce it keeps in a
//! closure, and the handler checks the sender's URL against the origin as well.
//!
//! Linux and macOS keep the OS decorations (not tested frameless there); the bridge still exists
//! so the page knows it runs in the desktop app.

use std::path::Path;

use tao::window::ResizeDirection;

/// What the page asks the window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowCommand {
    /// Send the window state back (on page load); also re-reads `desktop.systemTitleBar`.
    State,
    Minimize,
    /// Maximise, or restore when maximised.
    ToggleMaximize,
    /// As the OS close button: hide to the tray (or quit with `desktop.closeToTray` off).
    Close,
    /// Move the window with the mouse (pressed on the title bar).
    Drag,
    /// Resize from an edge (pressed on the title bar's top edge or corners).
    Resize(ResizeDirection),
    /// The OS title bar on (true) or the app's own (false).
    SystemTitleBar(bool),
    /// The window's title (taskbar, Alt+Tab): the open project and the product (D144).
    Title(String),
}

/// The longest window title the page may set (characters).
pub const MAX_TITLE: usize = 200;

/// Whether the window is frameless (the page draws the title bar): Windows only, unless the user
/// asked for the OS title bar.
pub fn frameless(system_title_bar: bool) -> bool {
    cfg!(windows) && !system_title_bar
}

/// `desktop` from `<home>/config.json` (defaults when missing or unreadable; never written here).
pub fn desktop_config(home: &Path) -> orca_core::config::DesktopConfig {
    let Ok(bytes) = std::fs::read(home.join("config.json")) else {
        return Default::default();
    };
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice::<orca_core::config::OrcaConfig>(bytes)
        .map(|c| c.desktop)
        .unwrap_or_default()
}

/// The app's origin from the URL the window opens (`http://127.0.0.1:port/?t=...`).
pub fn origin_of(url: &str) -> String {
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            format!("{}://{}", &url[..i], &rest[..end])
        }
        None => url.to_string(),
    }
}

/// Whether a message came from a page of the app's own origin (not another site, not a frame
/// with an opaque origin).
pub fn sender_allowed(sender_url: &str, origin: &str) -> bool {
    origin_of(sender_url) == origin
}

/// Parses an IPC message; `None` for a wrong nonce or anything unknown.
pub fn parse_command(body: &str, nonce: &str) -> Option<WindowCommand> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("nonce")?.as_str()? != nonce || nonce.is_empty() {
        return None;
    }
    Some(match v.get("cmd")?.as_str()? {
        "state" => WindowCommand::State,
        "minimize" => WindowCommand::Minimize,
        "maximize" => WindowCommand::ToggleMaximize,
        "close" => WindowCommand::Close,
        "drag" => WindowCommand::Drag,
        "resize" => WindowCommand::Resize(match v.get("dir")?.as_str()? {
            "n" => ResizeDirection::North,
            "s" => ResizeDirection::South,
            "e" => ResizeDirection::East,
            "w" => ResizeDirection::West,
            "ne" => ResizeDirection::NorthEast,
            "nw" => ResizeDirection::NorthWest,
            "se" => ResizeDirection::SouthEast,
            "sw" => ResizeDirection::SouthWest,
            _ => return None,
        }),
        "systemTitleBar" => WindowCommand::SystemTitleBar(v.get("value")?.as_bool()?),
        "title" => {
            let t: String = v
                .get("value")?
                .as_str()?
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_TITLE)
                .collect();
            WindowCommand::Title(t)
        }
        _ => return None,
    })
}

/// The window state the page shows (pushed after each change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct WindowState {
    pub frameless: bool,
    pub maximized: bool,
    pub focused: bool,
}

/// The script that hands the state to the page's bridge.
pub fn state_script(state: WindowState) -> String {
    format!(
        "window.abhothDesktop&&window.abhothDesktop._set({})",
        serde_json::to_string(&state).unwrap_or_else(|_| "{}".into())
    )
}

/// The bridge (`window.abhothDesktop`), injected before the page's scripts: only in the top frame
/// of the app's origin, the nonce kept in a closure.
pub fn bridge_script(origin: &str, nonce: &str, state: WindowState) -> String {
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function(){{
if (window.top !== window || location.origin !== {origin}) return;
var nonce = {nonce};
var state = {state};
var subs = [];
function post(m) {{ try {{ window.ipc.postMessage(JSON.stringify(m)); }} catch (e) {{}} }}
Object.defineProperty(window, "abhothDesktop", {{ value: Object.freeze({{
  platform: {platform},
  state: function () {{ return state; }},
  subscribe: function (fn) {{ subs.push(fn); return function () {{ subs = subs.filter(function (f) {{ return f !== fn; }}); }}; }},
  send: function (cmd, extra) {{ var m = {{}}; for (var k in (extra || {{}})) m[k] = extra[k]; m.cmd = cmd; m.nonce = nonce; post(m); }},
  _set: function (s) {{ state = Object.freeze(Object.assign({{}}, state, s)); subs.slice().forEach(function (f) {{ try {{ f(state); }} catch (e) {{}} }}); }}
}}) }});
}})();"#,
        origin = q(origin),
        nonce = q(nonce),
        state = serde_json::to_string(&state).unwrap_or_else(|_| "{}".into()),
        platform = q(platform),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(
            origin_of("http://127.0.0.1:5123/?t=abc"),
            "http://127.0.0.1:5123"
        );
        assert_eq!(origin_of("http://127.0.0.1:5123"), "http://127.0.0.1:5123");
        assert_eq!(
            origin_of("http://127.0.0.1:5123#x"),
            "http://127.0.0.1:5123"
        );
        let o = "http://127.0.0.1:5123";
        assert!(sender_allowed("http://127.0.0.1:5123/settings", o));
        assert!(!sender_allowed("http://127.0.0.1:51234/", o));
        assert!(!sender_allowed("http://127.0.0.1:5123.evil.test/", o));
        assert!(!sender_allowed("https://example.com/", o));
        assert!(!sender_allowed("about:blank", o));
        assert!(!sender_allowed("null", o));
    }

    #[test]
    fn commands_need_the_nonce() {
        let n = "k1";
        let p = |s: &str| parse_command(s, n);
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"minimize"}"#),
            Some(WindowCommand::Minimize)
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"maximize"}"#),
            Some(WindowCommand::ToggleMaximize)
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"close"}"#),
            Some(WindowCommand::Close)
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"drag"}"#),
            Some(WindowCommand::Drag)
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"state"}"#),
            Some(WindowCommand::State)
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"resize","dir":"ne"}"#),
            Some(WindowCommand::Resize(ResizeDirection::NorthEast))
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"systemTitleBar","value":true}"#),
            Some(WindowCommand::SystemTitleBar(true))
        );
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"title","value":"orca · Abhoth"}"#),
            Some(WindowCommand::Title("orca · Abhoth".into()))
        );
        // Control characters go, and a title is capped.
        assert_eq!(
            p(r#"{"nonce":"k1","cmd":"title","value":"a\nb\u0007"}"#),
            Some(WindowCommand::Title("ab".into()))
        );
        let long = format!(
            r#"{{"nonce":"k1","cmd":"title","value":"{}"}}"#,
            "x".repeat(500)
        );
        assert_eq!(p(&long), Some(WindowCommand::Title("x".repeat(MAX_TITLE))));
        assert_eq!(p(r#"{"nonce":"k1","cmd":"title","value":3}"#), None);
        assert_eq!(p(r#"{"nonce":"k2","cmd":"close"}"#), None);
        assert_eq!(p(r#"{"cmd":"close"}"#), None);
        assert_eq!(p(r#"{"nonce":"k1","cmd":"resize","dir":"up"}"#), None);
        assert_eq!(p(r#"{"nonce":"k1","cmd":"systemTitleBar"}"#), None);
        assert_eq!(p(r#"{"nonce":"k1","cmd":"rm -rf"}"#), None);
        assert_eq!(p("close"), None);
        assert_eq!(parse_command(r#"{"nonce":"","cmd":"close"}"#, ""), None);
    }

    #[test]
    fn frameless_only_on_windows_and_only_without_the_system_bar() {
        assert!(!frameless(true));
        assert_eq!(frameless(false), cfg!(windows));
    }

    #[test]
    fn the_title_bar_setting_is_read_from_config_json() {
        let home = tempfile::tempdir().unwrap();
        assert!(!desktop_config(home.path()).system_title_bar);
        std::fs::write(
            home.path().join("config.json"),
            r#"{"desktop":{"systemTitleBar":true}}"#,
        )
        .unwrap();
        let d = desktop_config(home.path());
        assert!(d.system_title_bar);
        assert!(d.close_to_tray);
    }

    #[test]
    fn the_bridge_is_for_the_app_origin_only_and_keeps_the_nonce_private() {
        let s = WindowState {
            frameless: true,
            maximized: false,
            focused: true,
        };
        let js = bridge_script("http://127.0.0.1:5123", "n\"once", s);
        assert!(js.contains(r#"location.origin !== "http://127.0.0.1:5123""#));
        assert!(js.contains(r#"var nonce = "n\"once";"#));
        assert!(js.contains(r#""frameless":true"#));
        assert!(!js.contains("window.nonce"));
        assert_eq!(
            state_script(s),
            r#"window.abhothDesktop&&window.abhothDesktop._set({"frameless":true,"maximized":false,"focused":true})"#
        );
    }
}
