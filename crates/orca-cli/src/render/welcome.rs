//! `WelcomeRenderer`: the startup banner (orca art, version line) and the connection status.
//!
//! [`show_banner`] asks the server for its models itself (`model_discovery::available_models`,
//! cached five minutes); [`write_status`] is the same output for a given model list (tests).

use orca_core::config::OrcaConfig;

use crate::ui::console::{Console, cell_width, strip_ansi};
use crate::ui::markup::escape;

/// The art is this many columns wide; narrower terminals skip it (it wrapped into noise).
const ART_WIDTH: usize = 70;

const BANNER_LINES: [&str; 7] = [
    "\x1b[48;5;69m        \x1b[38;5;69;48;5;69m\u{2584}\x1b[38;5;24;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\x1b[38;5;24;48;5;69m\u{2584}\u{2584}\x1b[38;5;25;48;5;69m\u{2584}\x1b[48;5;69m                                                      \x1b[0m",
    "\x1b[48;5;69m       \x1b[38;5;234;48;5;69m\u{2584}\x1b[38;5;0;48;5;234m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;0;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\x1b[48;5;69m   \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\u{2584}\x1b[48;5;69m                    \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\u{2584}\x1b[48;5;69m                       \x1b[0m",
    "\x1b[48;5;69m   \x1b[38;5;235;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;234m\u{2584}\x1b[38;5;0;48;5;0m\u{2584}\x1b[48;5;0m \x1b[38;5;234;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;0m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\u{2584}\x1b[48;5;69m   \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;69m  \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;69m\u{2584}\x1b[48;5;69m \x1b[38;5;0;48;5;69m\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;69m\u{2584}\x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\u{2584}\x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;69m  \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\u{2584}\x1b[48;5;0m \x1b[48;5;69m \x1b[38;5;0;48;5;69m\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[48;5;69m  \x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;69m\u{2584}\x1b[48;5;69m      \x1b[0m",
    "\x1b[48;5;69m \x1b[38;5;24;48;5;69m\u{2584}\x1b[38;5;0;48;5;234m\u{2584}\x1b[38;5;0;48;5;0m\u{2584}\x1b[48;5;0m \x1b[38;5;15;48;5;0m\u{2584}\u{2584}\x1b[48;5;0m  \x1b[38;5;0;48;5;0m\u{2584}\x1b[48;5;0m   \x1b[48;5;69m \x1b[38;5;69;48;5;69m\u{2584}\u{2584}\x1b[48;5;69m  \x1b[48;5;0m \x1b[48;5;69m    \x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m   \x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\u{2584}\x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m  \x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m    \x1b[48;5;0m \x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m    \x1b[48;5;0m \x1b[48;5;69m    \x1b[38;5;0;48;5;69m\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\u{2584}\u{2584}\x1b[48;5;0m \x1b[48;5;69m      \x1b[0m",
    "\x1b[48;5;69m \x1b[38;5;69;48;5;24m\u{2584}\x1b[38;5;15;48;5;0m\u{2584}\u{2584}\u{2584}\u{2584}\x1b[38;5;0;48;5;0m\u{2584}\x1b[48;5;0m  \x1b[38;5;15;48;5;0m\u{2584}\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\x1b[48;5;69m     \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;0m \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;69m \x1b[48;5;0m \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;69m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;0m \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m  \x1b[48;5;0m \x1b[48;5;69m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;0m \x1b[38;5;0;48;5;69m\u{2584}\u{2584}\x1b[48;5;0m \x1b[38;5;69;48;5;0m\u{2584}\x1b[48;5;69m \x1b[48;5;0m \x1b[48;5;69m    \x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[48;5;69m \x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;0;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[48;5;0m \x1b[48;5;69m      \x1b[0m",
    "\x1b[48;5;69m  \x1b[38;5;69;48;5;69m\u{2584}\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;236;48;5;0m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\x1b[38;5;69;48;5;0m\u{2584}\x1b[38;5;236;48;5;0m\u{2584}\x1b[38;5;69;48;5;69m\u{2584}\u{2584}\u{2584}\x1b[48;5;69m             \x1b[48;5;0m \x1b[48;5;69m                                            \x1b[0m",
    "\x1b[49;38;5;69m\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\x1b[0m",
];

/// `ShowBannerAsync`: the art (when the terminal is wide enough and takes colour), the
/// version line, the endpoint / model status and the image server status.
pub async fn show_banner(console: &Console, config: &OrcaConfig) {
    let width = console.width();
    if console.is_ansi() && width > ART_WIDTH {
        console.raw(&banner_art(width));
    }
    console.blank();
    console.markup_line(&version_line(width));

    let models = if config.demo_mode {
        Vec::new()
    } else {
        crate::model_discovery::available_models(config).await
    };
    let image_gen_reachable = if config.image_gen.enabled {
        Some(ping_image_server(&config.image_gen.base_url).await)
    } else {
        None
    };
    write_status(console, config, &models, image_gen_reachable);
    console.blank();
}

/// `ShowMinimal`.
pub fn show_minimal(console: &Console) {
    console.markup_line("[magenta bold]Abhoth[/] [grey]— Type /help for commands[/]\n");
}

/// The art centred in `width` columns, the sides filled in the art's colours.
pub fn banner_art(width: usize) -> String {
    let mut out = String::new();
    for (i, line) in BANNER_LINES.iter().enumerate() {
        let visible = cell_width(&strip_ansi(line));
        let total = width.saturating_sub(visible);
        let left = total / 2;
        let right = total - left;
        let body = line.replace("\x1b[0m", "");
        if i < BANNER_LINES.len() - 1 {
            out.push_str(&format!(
                "\x1b[48;5;69m{}{body}{}\x1b[0m\n",
                " ".repeat(left),
                " ".repeat(right)
            ));
        } else {
            out.push_str(&format!(
                "\x1b[49;38;5;69m{}{body}{}\x1b[0m\n",
                "\u{2580}".repeat(left),
                "\u{2580}".repeat(right)
            ));
        }
    }
    out
}

fn version_line(width: usize) -> String {
    let version = format!("v{}", crate::version());
    if width > 84 {
        format!(
            "[magenta bold]Abhoth[/] [grey]{version} · /help · Tab completes /commands and @files · !cmd runs a shell[/]"
        )
    } else {
        format!("[magenta bold]Abhoth[/] [grey]{version} · /help for commands[/]")
    }
}

/// `RenderConnectionStatusAsync` for a known model list (empty: none loaded, which is also
/// what an unreachable server gives, as in C#) and image server state (`None`: disabled).
pub fn write_status(
    console: &Console,
    config: &OrcaConfig,
    models: &[String],
    image_gen_reachable: Option<bool>,
) {
    if config.demo_mode {
        console.markup_line("[grey]Endpoint:[/]  [green]● Demo mode[/]");
        let demo = config.lm_studio.primary_model().unwrap_or("demo-model");
        console.markup_line(&format!("[grey]Model:[/]     [cyan]{}[/]", escape(demo)));
    } else {
        console.markup(&format!(
            "[grey]Endpoint:[/]  [white]{}[/]  ",
            escape(&config.lm_studio.base_url)
        ));
        if models.is_empty() {
            console.markup_line("[yellow]● No models loaded[/]");
        } else {
            console.markup_line("[green]● Connected[/]");
            let configured = &config.lm_studio.models;
            if configured.len() == 1 {
                console.markup_line(&format!(
                    "[grey]Model:[/]     [cyan]{}[/]",
                    escape(&configured[0])
                ));
            } else if configured.len() > 1 {
                console.markup_line(&format!(
                    "[grey]Models:[/]    [cyan]{}[/]",
                    escape(&configured.join(" > "))
                ));
            } else if models.len() == 1 {
                console.markup_line(&format!(
                    "[grey]Model:[/]     [cyan]{}[/]",
                    escape(&models[0])
                ));
            } else {
                let first: Vec<&str> = models.iter().take(3).map(String::as_str).collect();
                console.markup_line(&format!(
                    "[grey]Models:[/]    [cyan]{} available[/] — {}{}",
                    models.len(),
                    escape(&first.join(", ")),
                    if models.len() > 3 { ", ..." } else { "" }
                ));
            }
        }
    }

    if let Some(reachable) = image_gen_reachable {
        console.markup(&format!(
            "[grey]Image Gen:[/] [white]{}[/]  ",
            escape(&config.image_gen.base_url)
        ));
        console.markup_line(if reachable {
            "[green]● Connected[/]"
        } else {
            "[red]● Unreachable[/]"
        });
    }
}

/// C# counts any HTTP answer from `{baseUrl}/models` within five seconds as connected.
async fn ping_image_server(base_url: &str) -> bool {
    let Ok(http) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    else {
        return false;
    };
    http.get(format!("{}/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    //! WelcomeRenderer has no C# tests; these cover the port.
    use super::*;

    fn config() -> OrcaConfig {
        let mut c = OrcaConfig::default();
        c.lm_studio.base_url = "http://localhost:1234/v1".into();
        c.lm_studio.models.clear();
        c.image_gen.enabled = false;
        c
    }

    fn status(c: &OrcaConfig, models: &[&str], image: Option<bool>) -> String {
        let (console, buf) = Console::buffer();
        let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();
        write_status(&console, c, &models, image);
        Console::buffer_text(&buf)
    }

    #[test]
    fn art_lines_are_seventy_columns_and_centre() {
        for line in BANNER_LINES {
            assert_eq!(cell_width(&strip_ansi(line)), ART_WIDTH);
        }
        let art = banner_art(100);
        for line in strip_ansi(&art).lines() {
            assert_eq!(cell_width(line), 100);
        }
    }

    #[test]
    fn many_models_list_the_first_three() {
        let out = status(&config(), &["a", "b", "c", "d"], None);
        assert_eq!(
            out,
            "Endpoint:  http://localhost:1234/v1  ● Connected\nModels:    4 available — a, b, c, ...\n"
        );
    }

    #[test]
    fn configured_models_win_and_none_loaded_is_yellow() {
        let mut c = config();
        c.lm_studio.models = vec!["m1".into(), "m2".into()];
        assert!(status(&c, &["x"], None).contains("Models:    m1 > m2"));
        assert!(status(&c, &[], None).contains("● No models loaded"));
        c.lm_studio.models = vec!["m1".into()];
        assert!(status(&c, &["x"], None).contains("Model:     m1"));
    }

    #[test]
    fn demo_mode_and_image_server() {
        let mut c = config();
        c.demo_mode = true;
        let out = status(&c, &[], Some(false));
        assert!(out.starts_with("Endpoint:  ● Demo mode\nModel:     demo-model\n"));
        assert!(out.contains("Image Gen:"));
        assert!(out.ends_with("● Unreachable\n"));
    }

    #[test]
    fn minimal_and_version_lines() {
        let (console, buf) = Console::buffer();
        show_minimal(&console);
        assert_eq!(
            Console::buffer_text(&buf),
            "Abhoth — Type /help for commands\n\n"
        );
        assert!(version_line(100).contains("Tab completes"));
        assert!(version_line(80).contains("/help for commands"));
    }
}
