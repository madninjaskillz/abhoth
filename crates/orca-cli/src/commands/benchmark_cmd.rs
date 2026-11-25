//! `/benchmark [count=N] [models=all|name1,name2] [mode=pair]` (`CommandHandler.HandleBenchmarkAsync`).
//! The arguments are parsed as in C#; the runner (`BenchmarkRunner`: a scripted three-prompt
//! Python task per model in a temp folder, checked by running it, plus the pair mode) is not
//! ported yet.

use super::CommandContext;
use crate::ui::Console;
use crate::ui::markup::escape;

/// What `/benchmark` was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkArgs {
    pub runs_per_model: i32,
    /// `None` = the current model only; `Some(empty)` = all models.
    pub model_filter: Option<Vec<String>>,
    pub mode_pair: bool,
}

fn strip_prefix_ci<'a>(arg: &'a str, prefix: &str) -> Option<&'a str> {
    arg.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &arg[prefix.len()..])
}

/// Parse the arguments; `Err` holds the markup lines to print.
pub fn parse_args(args: &[String]) -> Result<BenchmarkArgs, Vec<String>> {
    let mut parsed = BenchmarkArgs {
        runs_per_model: 3,
        model_filter: None,
        mode_pair: false,
    };
    for arg in args {
        if let Some(value) = strip_prefix_ci(arg, "count=") {
            match value.trim().parse::<i32>() {
                Ok(n) if n >= 1 => parsed.runs_per_model = n,
                _ => {
                    return Err(vec![
                        "[red]Invalid count — must be a positive integer (e.g. count=5).[/]".into(),
                    ]);
                }
            }
        } else if let Some(value) = strip_prefix_ci(arg, "models=") {
            if value.eq_ignore_ascii_case("all") {
                parsed.model_filter = Some(Vec::new());
            } else {
                let models: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if models.is_empty() {
                    return Err(vec![
                        "[red]Invalid models — provide comma-separated names or 'all' (e.g. models=all).[/]".into(),
                    ]);
                }
                parsed.model_filter = Some(models);
            }
        } else if let Some(value) = strip_prefix_ci(arg, "mode=") {
            if value.eq_ignore_ascii_case("pair") {
                parsed.mode_pair = true;
            } else {
                return Err(vec![format!(
                    "[red]Unknown mode: {}. Supported: pair[/]",
                    escape(value)
                )]);
            }
        } else {
            return Err(vec![
                format!("[red]Unknown argument: {}[/]", escape(arg)),
                "[grey]Usage: /benchmark [[count=N]] [[models=all|name1,name2]] [[mode=pair]][/]"
                    .into(),
            ]);
        }
    }
    // Pair mode defaults to all models if none specified.
    if parsed.mode_pair && parsed.model_filter.is_none() {
        parsed.model_filter = Some(Vec::new());
    }
    Ok(parsed)
}

/// `/benchmark`: check the arguments, then (until the runner is ported) say so.
pub async fn handle(ctx: &mut CommandContext<'_>, args: &[String]) {
    run(ctx.console, args);
}

fn run(console: &Console, args: &[String]) -> Option<BenchmarkArgs> {
    match parse_args(args) {
        Err(lines) => {
            for l in lines {
                console.markup_line(&l);
            }
            None
        }
        Ok(parsed) => {
            console.markup_line("[yellow]The benchmark runner is not ported to Rust yet.[/]");
            Some(parsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_three_runs_of_the_current_model() {
        assert_eq!(
            parse_args(&[]),
            Ok(BenchmarkArgs {
                runs_per_model: 3,
                model_filter: None,
                mode_pair: false
            })
        );
    }

    #[test]
    fn count_models_and_pair_mode_are_read() {
        let p = parse_args(&args(&["COUNT=5", "models=a, b,,c"])).unwrap();
        assert_eq!(p.runs_per_model, 5);
        assert_eq!(
            p.model_filter,
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        let p = parse_args(&args(&["models=ALL"])).unwrap();
        assert_eq!(p.model_filter, Some(vec![]));
        let p = parse_args(&args(&["mode=Pair"])).unwrap();
        assert!(p.mode_pair);
        assert_eq!(p.model_filter, Some(vec![]));
    }

    #[test]
    fn bad_arguments_say_what_is_wrong() {
        let (console, out) = Console::buffer();
        assert_eq!(run(&console, &args(&["count=0"])), None);
        assert_eq!(run(&console, &args(&["count=x"])), None);
        assert_eq!(run(&console, &args(&["models=,"])), None);
        assert_eq!(run(&console, &args(&["mode=solo"])), None);
        assert_eq!(run(&console, &args(&["fast"])), None);
        let text = Console::buffer_text(&out);
        assert!(text.contains("Invalid count — must be a positive integer (e.g. count=5)."));
        assert!(text.contains(
            "Invalid models — provide comma-separated names or 'all' (e.g. models=all)."
        ));
        assert!(text.contains("Unknown mode: solo. Supported: pair"));
        assert!(text.contains("Unknown argument: fast"));
        assert!(text.contains("Usage: /benchmark [count=N] [models=all|name1,name2] [mode=pair]"));
        assert!(run(&console, &args(&["count=2"])).is_some());
        assert!(
            Console::buffer_text(&out).contains("The benchmark runner is not ported to Rust yet.")
        );
    }
}
