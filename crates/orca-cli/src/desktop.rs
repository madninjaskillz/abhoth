//! `abhoth --desktop` (PORTING.md phase 7, D110): the web UI in a native window. The web host runs
//! in this process on a loopback port with a per-launch token, on its own threads (tokio); the
//! main thread owns the window's event loop (tao) and the webview (wry: WebView2 on Windows). The
//! window is just another client of the same HTTP/websocket API. One instance per state home: a
//! second launch asks the first to show its window and exits. A tray icon offers Show and Quit.
//! Closing the window hides it to the tray (agents keep running; tray Quit exits), or quits with
//! `desktop.closeToTray` off (D113). Installed with Velopack, it checks GitHub Releases in the
//! background and the tray offers "Restart to update" once a new version is downloaded (D136).
//! On Windows the window is frameless and the web UI draws its title bar (D143, `desktop_chrome`).

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon, WindowBuilder};

use crate::desktop_chrome::{self as chrome, WindowCommand, WindowState};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

const ICON_PNG: &[u8] = include_bytes!("../../../assets/abhoth/icons/abhoth-256.png");
const LOCK_FILE: &str = "desktop.lock";
const INSTANCE_FILE: &str = "desktop.json";

#[derive(Debug)]
enum UserEvent {
    /// The host listens: open this URL (with the token).
    Ready(String),
    /// The host could not start.
    Failed(String),
    /// The host thread ended (its error, if it returned one) while the window was up.
    HostStopped(Option<String>),
    /// Another launch asked for the window.
    Focus,
    Menu(MenuId),
    TrayClick,
    /// A newer release has been downloaded (its version); the tray offers to restart into it.
    UpdateReady(String),
    /// The page's title bar asks the window for something (D143).
    Window(WindowCommand),
}

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Hide the window; the tray's Show (or a second launch) brings it back.
    Hide,
    /// Quit the app.
    Quit,
}

/// Closing the window: hide it to the tray when `desktop.closeToTray` is on (the default) and
/// there is a tray icon to bring it back from; otherwise quit.
pub fn on_close_requested(close_to_tray: bool, has_tray: bool) -> CloseAction {
    if close_to_tray && has_tray {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

/// `desktop.closeToTray` from `<home>/config.json`, read when the window is closed so a change in
/// the settings applies at once. A missing or unreadable file means the default (on); the file
/// is never written here.
pub fn close_to_tray(home: &Path) -> bool {
    chrome::desktop_config(home).close_to_tray
}

/// Why the app stops, on stderr and in a native error box: `abhoth-desktop.exe` has no console,
/// so without the box it would just vanish (D116).
fn report_error(text: &str) {
    eprintln!("abhoth: {text}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(orca_core::brand::PRODUCT_NAME)
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// What the error box says when the host thread ends on its own.
fn host_stopped_text(error: Option<&str>) -> String {
    match error {
        Some(e) => format!("The web host stopped: {e}"),
        None => "The web host stopped before the window could open.".into(),
    }
}

/// The arguments for the in-process host: `--desktop` becomes a loopback `--hostui` on a free
/// port (the token is made per launch by the host).
pub fn host_args(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = raw
        .iter()
        .filter(|a| {
            *a != "--desktop" && !a.starts_with("--hostui") && *a != "--print" && *a != "-p"
        })
        .cloned()
        .collect();
    out.push("--hostui=127.0.0.1:0".into());
    out
}

/// What a second launch reads to reach the first one.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct Instance {
    port: u16,
    key: String,
}

/// Holds the single-instance lock while the app runs.
pub struct InstanceLock {
    _file: std::fs::File,
    home: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.home.join(INSTANCE_FILE));
    }
}

/// Takes the lock in `home`, or `None` when another instance holds it.
pub fn try_lock(home: &Path) -> std::io::Result<Option<InstanceLock>> {
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(InstanceLock {
            _file: file,
            home: home.to_path_buf(),
        })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Asks the running instance (if it answers) to show its window.
pub fn signal_running(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(INSTANCE_FILE)) else {
        return false;
    };
    let Ok(inst) = serde_json::from_str::<Instance>(&text) else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, inst.port).into(),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(s, "{}", inst.key).is_err() {
        return false;
    }
    let mut answer = String::new();
    BufReader::new(s).read_line(&mut answer).is_ok() && answer.trim() == "ok"
}

/// Listens on loopback for "show yourself" from later launches; writes the instance file.
fn listen_for_focus(home: &Path, on_focus: impl Fn() + Send + 'static) -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let key = orca_webui::util::new_token();
    let inst = Instance {
        port: listener.local_addr()?.port(),
        key: key.clone(),
    };
    std::fs::write(
        home.join(INSTANCE_FILE),
        serde_json::to_string(&inst).unwrap_or_default(),
    )?;
    std::thread::Builder::new()
        .name("abhoth-desktop-focus".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut line).is_ok() && line.trim() == key {
                    on_focus();
                    let _ = (&stream).write_all(b"ok\n");
                }
            }
        })?;
    Ok(())
}

fn icon_rgba() -> (Vec<u8>, u32, u32) {
    match image::load_from_memory(ICON_PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            (rgba.into_raw(), w, h)
        }
        Err(_) => (vec![0; 4], 1, 1),
    }
}

/// The native folder dialog, for `/api/desktop/pick-folder`.
fn pick_folder_hook() -> orca_webui::server::PickFolderFn {
    Arc::new(|start: Option<PathBuf>| {
        Box::pin(async move {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Open a folder");
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            dialog.pick_folder().await.map(|h| h.path().to_path_buf())
        }) as futures_util::future::BoxFuture<'static, Option<PathBuf>>
    })
}

/// The folder the desktop app starts in when it was launched from its own install folder
/// (the Start-menu shortcut starts it in `%LocalAppData%\Abhoth\current`, D137): the last
/// opened project, else the user's home folder. `None` keeps `cwd` (a launch from a project).
pub fn start_folder(
    cwd: &Path,
    install_roots: &[PathBuf],
    last_opened: impl FnOnce() -> Option<PathBuf>,
    user_home: &Path,
) -> Option<PathBuf> {
    let inside = install_roots
        .iter()
        .any(|root| orca_webui::paths::is_under(cwd, root));
    inside.then(|| last_opened().unwrap_or_else(|| user_home.to_path_buf()))
}

/// The Velopack install root (`%LocalAppData%\Abhoth`): the parent of the exe's `current`
/// folder when Velopack's updater sits there, and on Windows `%LocalAppData%\Abhoth` itself.
fn install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let updater = if cfg!(windows) {
        "Update.exe"
    } else {
        "UpdateNix"
    };
    if let Some(root) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(Path::to_path_buf))
        && root.join(updater).is_file()
    {
        roots.push(root);
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA")
    {
        roots.push(PathBuf::from(local).join(orca_core::brand::PRODUCT_NAME));
    }
    roots
}

/// Whose last opened project the desktop app starts in: the user it signs in as (the first
/// admin, D142) when the accounts database exists, and the folder a session started in.
fn desktop_users() -> Vec<Option<i64>> {
    let db = orca_core::config::default_config_dir().join("abhoth.db");
    let admin = db
        .is_file()
        .then(|| orca_webui::login::db::UserDb::open(&db).ok())
        .flatten()
        .and_then(|db| db.first_admin())
        .map(|u| u.id);
    vec![admin, None]
}

/// Runs the desktop app; returns the process exit code.
pub fn run(raw: Vec<String>) -> i32 {
    use orca_webui::projects::store::ProjectStore;
    if let Ok(cwd) = std::env::current_dir()
        && let Some(dir) = start_folder(
            &cwd,
            &install_roots(),
            || ProjectStore::new(ProjectStore::default_path()).last_opened(&desktop_users()),
            &orca_core::config::manager::user_profile(),
        )
    {
        tracing::info!(
            "desktop: started in the install folder; opening {}",
            dir.display()
        );
        if let Err(e) = std::env::set_current_dir(&dir) {
            tracing::warn!("desktop: could not move to {}: {e}", dir.display());
        }
    }
    let home = orca_core::config::default_config_dir();
    let lock = match try_lock(&home) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            if signal_running(&home) {
                return 0;
            }
            report_error(&format!(
                "Another {} window is starting or not answering; try again.",
                orca_core::brand::PRODUCT_NAME
            ));
            return 1;
        }
        Err(e) => {
            report_error(&format!(
                "Could not take the desktop lock in {}: {e}",
                home.display()
            ));
            return 1;
        }
    };

    // This machine (D155): the app's own inventory and engines, and Bridge mode
    // (desktop.bridgeMode: this PC as a router-managed server while the app runs).
    #[cfg(all(feature = "bridge", feature = "router"))]
    let machine = Some(crate::machine::spawn(home.clone(), chrome::desktop_config));
    #[cfg(all(feature = "bridge", not(feature = "router")))]
    crate::bridge_mode::spawn_watcher(home.clone(), chrome::desktop_config);
    #[cfg(not(all(feature = "bridge", feature = "router")))]
    let machine = None;

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // The host: its own threads and tokio runtime.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    crate::hostui::set_desktop_link(crate::hostui::DesktopLink {
        ready: ready_tx,
        stop: stop_rx,
        hooks: orca_webui::server::DesktopHooks {
            pick_folder: pick_folder_hook(),
            machine,
        },
    });
    let args = host_args(&raw);
    let host_proxy = proxy.clone();
    let host = std::thread::Builder::new()
        .name("abhoth-desktop-host".into())
        .spawn(move || {
            let (code, error) = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => match runtime.block_on(crate::main_with_args(args)) {
                    Ok(code) => (code, None),
                    Err(e) => {
                        eprintln!("Error: {e:#}");
                        (1, Some(format!("{e:#}")))
                    }
                },
                Err(e) => (1, Some(e.to_string())),
            };
            // Unless the window is quitting (then nobody reads it), this ends the app.
            let _ = host_proxy.send_event(UserEvent::HostStopped(error));
            code
        })
        .expect("spawning the host thread");
    {
        // A host that stops before it is ready says why through `HostStopped`.
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let event = match ready_rx.recv() {
                Ok(Ok(url)) => UserEvent::Ready(url),
                Ok(Err(e)) => UserEvent::Failed(e),
                Err(_) => return,
            };
            let _ = proxy.send_event(event);
        });
    }
    {
        let proxy = proxy.clone();
        if let Err(e) = listen_for_focus(&home, move || {
            let _ = proxy.send_event(UserEvent::Focus);
        }) {
            tracing::warn!("desktop: no single-instance listener: {e}");
        }
    }
    {
        let proxy: EventLoopProxy<UserEvent> = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(e.id));
        }));
    }
    {
        let proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::DoubleClick { .. } = e {
                let _ = proxy.send_event(UserEvent::TrayClick);
            }
        }));
    }

    let (rgba, w, h) = icon_rgba();
    let mut frameless = chrome::frameless(chrome::desktop_config(&home).system_title_bar);
    let builder = WindowBuilder::new()
        .with_title(orca_core::brand::PRODUCT_NAME)
        .with_inner_size(LogicalSize::new(1360.0, 860.0))
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_window_icon(Icon::from_rgba(rgba.clone(), w, h).ok())
        .with_decorations(!frameless);
    // Frameless keeps the DWM shadow, the Windows 11 rounded corners and the resize borders.
    #[cfg(windows)]
    let builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder.with_undecorated_shadow(true)
    };
    let window = match builder.build(&event_loop) {
        Ok(w) => w,
        Err(e) => {
            report_error(&format!("Could not open a window: {e}"));
            return 1;
        }
    };

    let show_id = MenuId::new("show");
    let quit_id = MenuId::new("quit");
    let update_id = MenuId::new("update");
    let update_slot: crate::updater::Slot = Arc::new(std::sync::Mutex::new(None));
    {
        let proxy = proxy.clone();
        crate::updater::spawn_checker(update_slot.clone(), move |version| {
            let _ = proxy.send_event(UserEvent::UpdateReady(version));
        });
    }
    let mut menu: Option<Menu> = None;
    let mut tray: Option<TrayIcon> = None;
    let mut webview: Option<wry::WebView> = None;
    let mut web_context = wry::WebContext::new(Some(home.join("webview")));
    let nonce = orca_webui::util::new_token();
    let mut last_state: Option<WindowState> = None;
    let mut stop = Some(stop_tx);
    let mut host = Some(host);
    let mut lock = Some(lock);

    let quit = move |stop: &mut Option<tokio::sync::oneshot::Sender<()>>,
                     host: &mut Option<std::thread::JoinHandle<i32>>,
                     lock: &mut Option<InstanceLock>|
          -> i32 {
        if let Some(s) = stop.take() {
            let _ = s.send(());
        }
        let code = host.take().map(|h| h.join().unwrap_or(1)).unwrap_or(0);
        drop(lock.take());
        code
    };

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let tray_menu = Menu::new();
                let show = MenuItem::with_id(show_id.clone(), "Show", true, None);
                let quit_item = MenuItem::with_id(quit_id.clone(), "Quit", true, None);
                let _ = tray_menu.append_items(&[&show, &quit_item]);
                menu = Some(tray_menu.clone());
                tray = tray_icon::Icon::from_rgba(rgba.clone(), w, h)
                    .ok()
                    .and_then(|icon| {
                        TrayIconBuilder::new()
                            .with_menu(Box::new(tray_menu))
                            .with_tooltip(orca_core::brand::PRODUCT_NAME)
                            .with_icon(icon)
                            .build()
                            .map_err(|e| tracing::warn!("desktop: no tray icon: {e}"))
                            .ok()
                    });
            }
            Event::UserEvent(UserEvent::Ready(url)) => {
                let origin = url.split("/?").next().unwrap_or(&url).to_string();
                let app_origin = chrome::origin_of(&url);
                let state = window_state(&window, frameless);
                let ipc_proxy = proxy.clone();
                let ipc_origin = app_origin.clone();
                let ipc_nonce = nonce.clone();
                let builder = wry::WebViewBuilder::new_with_web_context(&mut web_context)
                    .with_url(&url)
                    .with_initialization_script_for_main_only(
                        chrome::bridge_script(&app_origin, &nonce, state),
                        true,
                    )
                    .with_ipc_handler(move |req: wry::http::Request<String>| {
                        // Only the app's own page (its origin, with the per-launch nonce).
                        if !chrome::sender_allowed(&req.uri().to_string(), &ipc_origin) {
                            return;
                        }
                        if let Some(cmd) = chrome::parse_command(req.body(), &ipc_nonce) {
                            let _ = ipc_proxy.send_event(UserEvent::Window(cmd));
                        }
                    })
                    .with_navigation_handler(move |to: String| {
                        // Links out of the app open in the system browser.
                        if to.starts_with(&origin) || to.starts_with("about:") {
                            true
                        } else {
                            let _ = crate::hostui::open_browser(&to);
                            false
                        }
                    });
                // Linux: into tao's GTK box, so it works on Wayland as well as X11 and follows
                // the window's size (`build` on a raw handle is X11-only and doesn't resize).
                #[cfg(target_os = "linux")]
                let built = {
                    use tao::platform::unix::WindowExtUnix;
                    use wry::WebViewBuilderExtUnix;
                    match window.default_vbox() {
                        Some(vbox) => builder.build_gtk(vbox),
                        None => builder.build(&window),
                    }
                };
                #[cfg(not(target_os = "linux"))]
                let built = builder.build(&window);
                match built {
                    Ok(v) => webview = Some(v),
                    Err(e) => {
                        report_error(&format!(
                            "Could not open the web view ({e}). On Windows the WebView2 runtime is needed: https://developer.microsoft.com/microsoft-edge/webview2/"
                        ));
                        let code = quit(&mut stop, &mut host, &mut lock);
                        std::process::exit(if code == 0 { 1 } else { code });
                    }
                }
            }
            Event::UserEvent(UserEvent::Failed(e)) => {
                report_error(&format!("The web host did not start: {e}"));
                let code = quit(&mut stop, &mut host, &mut lock);
                std::process::exit(if code == 0 { 1 } else { code });
            }
            Event::UserEvent(UserEvent::HostStopped(error)) => {
                // A clean stop with the window up (Ctrl+C in the console) just ends the app.
                let explain = error.is_some() || webview.is_none();
                drop(webview.take());
                drop(tray.take());
                window.set_visible(false);
                if explain {
                    report_error(&host_stopped_text(error.as_deref()));
                }
                let code = quit(&mut stop, &mut host, &mut lock);
                finish(if code == 0 { 1 } else { code });
            }
            // macOS: clicking the Dock icon while the window is hidden in the tray.
            Event::Reopen {
                has_visible_windows: false,
                ..
            }
            | Event::UserEvent(UserEvent::Focus)
            | Event::UserEvent(UserEvent::TrayClick) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            Event::UserEvent(UserEvent::UpdateReady(version)) => {
                let label = format!("Update available: restart into {version}");
                if let Some(m) = &menu {
                    let item = MenuItem::with_id(update_id.clone(), &label, true, None);
                    let _ = m.insert(&item, 0);
                }
                if let Some(t) = &tray {
                    let _ = t.set_tooltip(Some(format!(
                        "{} ({label})",
                        orca_core::brand::PRODUCT_NAME
                    )));
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == update_id {
                    let ready = update_slot.lock().ok().and_then(|s| s.clone());
                    if let Some(ready) = ready {
                        // Stop the host cleanly first; Velopack's updater waits for this
                        // process to exit, applies the update and starts the new version.
                        drop(webview.take());
                        drop(tray.take());
                        let _ = quit(&mut stop, &mut host, &mut lock);
                        if let Err(e) = ready.apply_and_restart() {
                            report_error(&format!("Could not apply the update: {e}"));
                        }
                        finish(0);
                    }
                } else if id == show_id {
                    window.set_visible(true);
                    window.set_minimized(false);
                    window.set_focus();
                } else if id == quit_id {
                    drop(webview.take());
                    drop(tray.take());
                    let code = quit(&mut stop, &mut host, &mut lock);
                    *control_flow = ControlFlow::Exit;
                    finish(code);
                }
            }
            Event::UserEvent(UserEvent::Window(cmd)) if cmd != WindowCommand::Close => {
                match cmd {
                    WindowCommand::State => {
                        // A (re)loaded page: pick up a title bar setting changed elsewhere.
                        let want =
                            chrome::frameless(chrome::desktop_config(&home).system_title_bar);
                        if want != frameless {
                            frameless = want;
                            window.set_decorations(!frameless);
                        }
                        last_state = None;
                    }
                    WindowCommand::Minimize => window.set_minimized(true),
                    WindowCommand::ToggleMaximize => window.set_maximized(!window.is_maximized()),
                    WindowCommand::Drag => {
                        let _ = window.drag_window();
                    }
                    WindowCommand::Resize(dir) => {
                        if frameless && !window.is_maximized() {
                            let _ = window.drag_resize_window(dir);
                        }
                    }
                    WindowCommand::SystemTitleBar(on) => {
                        let want = chrome::frameless(on);
                        if want != frameless {
                            frameless = want;
                            window.set_decorations(!frameless);
                        }
                    }
                    WindowCommand::Title(title) => window.set_title(&title),
                    WindowCommand::Close => {}
                }
                push_state(webview.as_ref(), &window, frameless, &mut last_state);
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::Focused(_),
                ..
            } => push_state(webview.as_ref(), &window, frameless, &mut last_state),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::UserEvent(UserEvent::Window(WindowCommand::Close)) => match on_close_requested(close_to_tray(&home), tray.is_some()) {
                CloseAction::Hide => window.set_visible(false),
                CloseAction::Quit => {
                    drop(webview.take());
                    drop(tray.take());
                    let code = quit(&mut stop, &mut host, &mut lock);
                    *control_flow = ControlFlow::Exit;
                    finish(code);
                }
            },
            _ => {}
        }
    })
}

fn window_state(window: &tao::window::Window, frameless: bool) -> WindowState {
    WindowState {
        frameless,
        maximized: window.is_maximized(),
        focused: window.is_focused(),
    }
}

/// Tells the page's title bar how the window is now (only when that changed).
fn push_state(
    webview: Option<&wry::WebView>,
    window: &tao::window::Window,
    frameless: bool,
    last: &mut Option<WindowState>,
) {
    let Some(webview) = webview else { return };
    let state = window_state(window, frameless);
    if *last != Some(state) {
        *last = Some(state);
        let _ = webview.evaluate_script(&chrome::state_script(state));
    }
}

/// What `main` does after a run: the provider usage lines, the background processes, exit.
fn finish(code: i32) -> ! {
    crate::log_provider_usage();
    orca_tools_shell::process::kill_all();
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_launch_from_the_install_folder_opens_the_last_project_else_home() {
        let (root, home) = if cfg!(windows) {
            (r"C:\Users\u\AppData\Local\Abhoth", r"C:\Users\u")
        } else {
            ("/home/u/.local/share/Abhoth", "/home/u")
        };
        let (root, home) = (PathBuf::from(root), PathBuf::from(home));
        let project = home.join("src").join("proj");
        let roots = [root.clone()];
        let last = || Some(project.clone());
        let start = |cwd: &Path, roots: &[PathBuf], last: &dyn Fn() -> Option<PathBuf>| {
            super::start_folder(cwd, roots, last, &home)
        };
        // From the install folder or anything inside it: the last opened project.
        assert_eq!(
            start(&root.join("current"), &roots, &last),
            Some(project.clone())
        );
        assert_eq!(start(&root, &roots, &last), Some(project.clone()));
        // No last project: the home folder.
        assert_eq!(
            start(&root.join("current"), &roots, &|| None),
            Some(home.clone())
        );
        // A launch from a project folder (or one that only shares the prefix) keeps it.
        assert_eq!(start(&project, &roots, &last), None);
        let sibling = PathBuf::from(format!("{}Other", root.display()));
        assert_eq!(start(&sibling, &roots, &last), None);
        // Not installed: nothing changes.
        assert_eq!(start(&root.join("current"), &[], &last), None);
    }

    use super::*;

    #[test]
    fn desktop_becomes_a_loopback_host_on_a_free_port() {
        let raw: Vec<String> = ["--desktop", "--profile", "x", "--hostui=0.0.0.0:80"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            host_args(&raw),
            vec!["--profile", "x", "--hostui=127.0.0.1:0"]
        );
    }

    #[test]
    fn a_second_lock_in_the_same_home_is_refused_until_the_first_is_dropped() {
        let home = tempfile::tempdir().unwrap();
        let first = try_lock(home.path()).unwrap();
        assert!(first.is_some());
        assert!(try_lock(home.path()).unwrap().is_none());
        drop(first);
        assert!(try_lock(home.path()).unwrap().is_some());
    }

    #[test]
    fn a_later_launch_reaches_the_running_one() {
        let home = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        listen_for_focus(home.path(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        assert!(signal_running(home.path()));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A wrong key is ignored.
        let mut inst: Instance = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(INSTANCE_FILE)).unwrap(),
        )
        .unwrap();
        inst.key = "nope".into();
        std::fs::write(
            home.path().join(INSTANCE_FILE),
            serde_json::to_string(&inst).unwrap(),
        )
        .unwrap();
        assert!(!signal_running(home.path()));
    }

    #[test]
    fn closing_hides_to_the_tray_only_when_asked_and_a_tray_exists() {
        assert_eq!(on_close_requested(false, true), CloseAction::Quit);
        assert_eq!(on_close_requested(false, false), CloseAction::Quit);
        assert_eq!(on_close_requested(true, true), CloseAction::Hide);
        // No tray icon (e.g. no StatusNotifier host on Linux): hiding would strand the window.
        assert_eq!(on_close_requested(true, false), CloseAction::Quit);
    }

    #[test]
    fn close_to_tray_is_read_from_config_json_and_defaults_to_on() {
        let home = tempfile::tempdir().unwrap();
        assert!(close_to_tray(home.path()));
        let path = home.path().join("config.json");
        std::fs::write(&path, r#"{"lmStudio":{}}"#).unwrap();
        assert!(close_to_tray(home.path()));
        std::fs::write(&path, r#"{"desktop":{"closeToTray":false}}"#).unwrap();
        assert!(!close_to_tray(home.path()));
        std::fs::write(&path, b"\xEF\xBB\xBF{\"desktop\":{\"closeToTray\":false}}").unwrap();
        assert!(!close_to_tray(home.path()));
        std::fs::write(&path, r#"{"desktop":{"closeToTray":true}}"#).unwrap();
        assert!(close_to_tray(home.path()));
        std::fs::write(&path, "not json").unwrap();
        assert!(close_to_tray(home.path()));
        // Reading never creates or rewrites the file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn a_stopped_host_is_explained() {
        assert_eq!(
            host_stopped_text(Some("no model")),
            "The web host stopped: no model"
        );
        assert!(host_stopped_text(None).contains("before the window could open"));
    }

    #[test]
    fn the_icon_decodes() {
        let (rgba, w, h) = icon_rgba();
        assert_eq!((w, h), (256, 256));
        assert_eq!(rgba.len(), 256 * 256 * 4);
    }
}
