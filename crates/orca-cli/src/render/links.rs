//! `Hyperlinks`: file paths in output as clickable OSC 8 hyperlinks. The links go in as
//! `[link=file:///…]` markup (see `ui::markup`), so only the visible text counts toward
//! widths. Only paths that exist are linked; with `ui.hyperlinks` off (or "auto" in a terminal
//! not known to support them, or when output is redirected) everything renders as plain text.
//!
//! The switch is process-wide, as in C# (`Hyperlinks.Enabled`): the REPL calls [`configure`]
//! once at startup. The `*_in` functions take the switch and folder explicitly (tests).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use orca_core::attachments::file_link_resolver::{find_paths, resolve, to_file_uri};

use crate::ui::markup;

static ENABLED: AtomicBool = AtomicBool::new(false);
static WORKING_DIRECTORY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Whether links are emitted.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The folder relative paths resolve against (default: the process's current directory).
pub fn working_directory() -> String {
    WORKING_DIRECTORY
        .lock()
        .unwrap()
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

/// Set the folder paths resolve against (`None`: follow the current directory).
pub fn set_working_directory(dir: Option<PathBuf>) {
    *WORKING_DIRECTORY.lock().unwrap() = dir;
}

/// `Hyperlinks.Configure`: applies `ui.hyperlinks` ("auto" / "on" / "off"). Links also need
/// an ANSI console.
pub fn configure(mode: &str, console_is_ansi: bool) {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let redirected = !std::io::IsTerminal::is_terminal(&std::io::stdout());
    set_enabled(console_is_ansi && should_enable(Some(mode), &env, redirected));
}

/// The decision for a mode ("auto" / "on" / "off") in an environment.
pub fn should_enable(
    mode: Option<&str>,
    env: &[(String, String)],
    output_redirected: bool,
) -> bool {
    match mode.map(|m| m.trim().to_lowercase()).as_deref() {
        Some("on" | "true" | "always") => return true,
        Some("off" | "false" | "never") => return false,
        _ => {}
    }
    !output_redirected && terminal_supports_links(env)
}

/// Terminals known to handle OSC 8 (and to ignore it safely when they don't).
pub fn terminal_supports_links(env: &[(String, String)]) -> bool {
    let get = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let non_empty = |name: &str| get(name).is_some_and(|v| !v.is_empty());
    let term = get("TERM").unwrap_or("");
    if term == "dumb" {
        return false;
    }
    // Inside tmux / screen the link needs passthrough the multiplexer may not give.
    if non_empty("TMUX") || term.starts_with("screen") || term.starts_with("tmux") {
        return false;
    }
    if non_empty("WT_SESSION") {
        return true; // Windows Terminal
    }
    if let Some(program) = get("TERM_PROGRAM")
        && matches!(
            program,
            "vscode" | "iTerm.app" | "WezTerm" | "Hyper" | "ghostty" | "Tabby" | "rio"
        )
    {
        return true;
    }
    if non_empty("KITTY_WINDOW_ID") || non_empty("KONSOLE_VERSION") || non_empty("DOMTERM") {
        return true;
    }
    if ["kitty", "wezterm", "ghostty", "foot", "alacritty"]
        .iter()
        .any(|t| term.contains(t))
    {
        return true;
    }
    // GNOME Terminal & other VTE terminals: OSC 8 since VTE 0.50.
    get("VTE_VERSION")
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|v| v >= 5000)
}

/// Markup for a link around already-escaped markup text.
fn wrap(markup_text: &str, full_path: &str) -> String {
    format!("[link={}]{markup_text}[/]", to_file_uri(full_path))
}

/// `PathMarkup`: `text` escaped, and linked to the file `path` names (default: the text
/// itself) when links are on and the file exists.
pub fn path_markup(text: &str, path: Option<&str>) -> String {
    path_markup_in(enabled(), &working_directory(), text, path)
}

pub fn path_markup_in(enabled: bool, cwd: &str, text: &str, path: Option<&str>) -> String {
    let escaped = markup::escape(text);
    if !enabled {
        return escaped;
    }
    match resolve(path.unwrap_or(text), cwd) {
        Some(full) => wrap(&escaped, &full),
        None => escaped,
    }
}

/// `SummaryMarkup`: a one-line summary that contains `path` (a tool header such as
/// `src/app.cs:10-40`) with only the path part linked, when it exists.
pub fn summary_markup(shown: &str, path: Option<&str>) -> String {
    summary_markup_in(enabled(), &working_directory(), shown, path)
}

pub fn summary_markup_in(enabled: bool, cwd: &str, shown: &str, path: Option<&str>) -> String {
    let Some(path) = path.filter(|p| enabled && !p.is_empty()) else {
        return markup::escape(shown);
    };
    let Some(at) = shown.find(path) else {
        // Ellipsized: the visible start of the path ("src/very/long/pa…").
        let visible = shown.trim_end_matches('…');
        if visible.len() < shown.len()
            && !visible.is_empty()
            && path.starts_with(visible)
            && let Some(cut) = resolve(path, cwd)
        {
            return wrap(&markup::escape(visible), &cut) + "…";
        }
        return markup::escape(shown);
    };
    let Some(full) = resolve(path, cwd) else {
        return markup::escape(shown);
    };
    format!(
        "{}{}{}",
        markup::escape(&shown[..at]),
        wrap(&markup::escape(path), &full),
        markup::escape(&shown[at + path.len()..])
    )
}

/// `TextMarkup`: prose escaped, with every path that names an existing file linked.
pub fn text_markup(text: &str) -> String {
    text_markup_in(enabled(), &working_directory(), text)
}

pub fn text_markup_in(enabled: bool, cwd: &str, text: &str) -> String {
    if !enabled || text.is_empty() {
        return markup::escape(text);
    }
    let refs = find_paths(text, cwd);
    if refs.is_empty() {
        return markup::escape(text);
    }
    let mut sb = String::with_capacity(text.len() + refs.len() * 48);
    let mut pos = 0;
    for r in refs {
        sb.push_str(&markup::escape(&text[pos..r.index]));
        sb.push_str(&wrap(
            &markup::escape(&text[r.index..r.index + r.length]),
            &r.full_path,
        ));
        pos = r.index + r.length;
    }
    sb.push_str(&markup::escape(&text[pos..]));
    sb
}

#[cfg(test)]
mod tests {
    //! Ported from HyperlinkAndPasteTests.cs (the mode / terminal / markup cases; the chip,
    //! paste and console-capability cases belong to the line editor and `ui` modules).
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn mode_on_off_auto() {
        let wt = env(&[("WT_SESSION", "1")]);
        for (mode, redirected, expected) in [
            (Some("on"), false, true),
            (Some("on"), true, true),
            (Some("off"), false, false),
            (Some("auto"), false, true),
            (Some("auto"), true, false),
            (None, false, true),
        ] {
            assert_eq!(should_enable(mode, &wt, redirected), expected, "{mode:?}");
        }
    }

    #[test]
    fn auto_known_terminals() {
        for (name, value, expected) in [
            ("WT_SESSION", "abc", true),
            ("TERM_PROGRAM", "vscode", true),
            ("TERM_PROGRAM", "iTerm.app", true),
            ("TERM_PROGRAM", "Apple_Terminal", false),
            ("VTE_VERSION", "7200", true),
            ("VTE_VERSION", "4600", false),
            ("KITTY_WINDOW_ID", "1", true),
            ("TERM", "xterm-256color", false),
        ] {
            assert_eq!(
                terminal_supports_links(&env(&[(name, value)])),
                expected,
                "{name}={value}"
            );
        }
    }

    #[test]
    fn auto_off_inside_tmux_even_in_windows_terminal() {
        assert!(!terminal_supports_links(&env(&[
            ("WT_SESSION", "1"),
            ("TMUX", "/tmp/tmux-1/default,1,0")
        ])));
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("app.cs"), "class App {}").unwrap();
        dir
    }

    #[test]
    fn path_markup_existing_file_emits_osc8_file_link() {
        let dir = project();
        let cwd = dir.path().to_string_lossy().to_string();
        let m = path_markup_in(true, &cwd, "src/app.cs", None);
        let full = dir.path().join("src").join("app.cs");
        assert_eq!(
            m,
            format!(
                "[link={}]src/app.cs[/]",
                to_file_uri(&full.to_string_lossy())
            )
        );
        let ansi = markup::render(&format!("see {m}"), true);
        assert!(ansi.contains("\x1b]8;;file://"));
        assert!(ansi.contains("\x1b]8;;\x1b\\"));
        assert_eq!(crate::ui::console::strip_ansi(&ansi), "see src/app.cs");
    }

    #[test]
    fn path_markup_missing_file_or_links_off_is_plain_text() {
        let dir = project();
        let cwd = dir.path().to_string_lossy().to_string();
        assert_eq!(
            path_markup_in(true, &cwd, "src/none.cs", None),
            "src/none.cs"
        );
        assert_eq!(
            path_markup_in(false, &cwd, "src/app.cs", None),
            "src/app.cs"
        );
    }

    #[test]
    fn summary_markup_links_only_the_path_part_even_when_ellipsized() {
        let dir = project();
        let cwd = dir.path().to_string_lossy().to_string();
        let uri = to_file_uri(&dir.path().join("src").join("app.cs").to_string_lossy());
        assert_eq!(
            summary_markup_in(true, &cwd, "src/app.cs:10-40", Some("src/app.cs")),
            format!("[link={uri}]src/app.cs[/]:10-40")
        );
        assert!(summary_markup_in(true, &cwd, "src/ap…", Some("src/app.cs")).starts_with("[link="));
        assert_eq!(summary_markup_in(true, &cwd, "ls -la", None), "ls -la");
    }

    #[test]
    fn text_markup_prose_keeps_markup_characters_escaped() {
        let dir = project();
        let cwd = dir.path().to_string_lossy().to_string();
        let m = text_markup_in(true, &cwd, "edit [src/app.cs] and README.md");
        assert!(m.starts_with("edit [["));
        assert!(m.contains("[link=file://"));
        assert_eq!(markup::remove(&m), "edit [src/app.cs] and README.md");
    }
}
