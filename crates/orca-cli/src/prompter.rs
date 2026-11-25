//! The console answer to `ask_user` (C# `ToolWiring` wires a Spectre.Console selection prompt).

use std::io::{BufRead, IsTerminal, Write};

use orca_tools_utility::UserPrompter;

const OTHER: &str = "Other (type custom response)";

/// Shows the question on stderr; when stdin and stderr are a terminal, lists the options
/// (plus "Other") and reads the choice. Without a terminal it fails with the Spectre message,
/// which is what C# `--print` reports when its output is redirected.
pub struct ConsolePrompter;

#[async_trait::async_trait]
impl UserPrompter for ConsolePrompter {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        eprintln!("{question}");
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            return Err(
                "Cannot show selection prompt since the current terminal isn't interactive.".into(),
            );
        }
        let mut choices = options.to_vec();
        choices.push(OTHER.into());
        tokio::task::spawn_blocking(move || select(&choices))
            .await
            .map_err(|e| e.to_string())?
    }
}

fn read_line() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

fn select(choices: &[String]) -> Result<String, String> {
    let mut err = std::io::stderr();
    loop {
        let _ = writeln!(err, "Select an option:");
        for (i, c) in choices.iter().enumerate() {
            let _ = writeln!(err, "  {}. {c}", i + 1);
        }
        let _ = write!(err, "> ");
        let _ = err.flush();
        let answer = read_line()?;
        let picked = answer
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| choices.get(i))
            .or_else(|| choices.iter().find(|c| c.eq_ignore_ascii_case(&answer)));
        match picked {
            Some(c) if c == OTHER => {
                let _ = write!(err, "Your response: ");
                let _ = err.flush();
                return read_line();
            }
            Some(c) => return Ok(c.clone()),
            None => continue,
        }
    }
}
