//! Running a program the way the C# tools do with `ProcessStartInfo.Arguments`: one argument
//! string, output captured, no window.
//!
//! On Windows the string goes to `CreateProcess` unchanged (as .NET passes it), so the child
//! parses it exactly as it would under C#. Elsewhere .NET splits it into argv with its own
//! rules ([`split_dotnet_arguments`]), which are reproduced here.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

/// `ParseArgumentsIntoList` from .NET's `Process.Unix`: splits on spaces/tabs outside quotes;
/// backslashes are literal unless they precede a quote (then pairs become one backslash and an
/// odd one escapes the quote); `""` inside quotes is a literal quote.
pub fn split_dotnet_arguments(arguments: &str) -> Vec<String> {
    let chars: Vec<char> = arguments.chars().collect();
    let mut results = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        if i == chars.len() {
            break;
        }
        results.push(next_argument(&chars, &mut i));
        i += 1;
    }
    results
}

fn next_argument(chars: &[char], i: &mut usize) -> String {
    let mut current = String::new();
    let mut in_quotes = false;
    while *i < chars.len() {
        let mut backslashes = 0;
        while *i < chars.len() && chars[*i] == '\\' {
            *i += 1;
            backslashes += 1;
        }
        if backslashes > 0 {
            if *i >= chars.len() || chars[*i] != '"' {
                current.extend(std::iter::repeat_n('\\', backslashes));
            } else {
                current.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 != 0 {
                    current.push('"');
                    *i += 1;
                }
            }
            continue;
        }
        let c = chars[*i];
        if c == '"' {
            if in_quotes && *i < chars.len() - 1 && chars[*i + 1] == '"' {
                current.push('"');
                *i += 1;
            } else {
                in_quotes = !in_quotes;
            }
            *i += 1;
            continue;
        }
        if (c == ' ' || c == '\t') && !in_quotes {
            break;
        }
        current.push(c);
        *i += 1;
    }
    current
}

/// A `tokio` command for `program` with a .NET-style argument string, output piped, no
/// console window, killed when dropped.
pub fn command(
    program: &str,
    arguments: &str,
    working_directory: &Path,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        cmd.raw_arg(arguments);
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    cmd.args(split_dotnet_arguments(arguments));
    cmd.current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// A finished run's exit code and output (lossy UTF-8, as the other ported tools decode it).
#[derive(Debug, Clone)]
pub struct Captured {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Why [`run_captured`] gave no [`Captured`].
#[derive(Debug)]
pub enum RunError {
    /// The program could not be started.
    Spawn(std::io::Error),
    /// The time ran out while the output was still open.
    TimedOutReading,
    /// The output closed but the process had not exited when the time ran out.
    TimedOutWaiting,
    /// Reading the output or waiting failed.
    Io(std::io::Error),
}

/// Runs `cmd` to the end within `timeout` (C#: `ReadToEndAsync` on both streams, then
/// `WaitForExitAsync`, all under one timeout token). The process is killed on a timeout.
pub async fn run_captured(
    mut cmd: tokio::process::Command,
    timeout: Duration,
) -> Result<Captured, RunError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let read = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a.and(b).map(|_| (out, err))
    };
    let (out, err) = match tokio::time::timeout_at(deadline, read).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(RunError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            return Err(RunError::TimedOutReading);
        }
    };
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(RunError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            return Err(RunError::TimedOutWaiting);
        }
    };
    Ok(Captured {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_dotnet_on_unix() {
        assert_eq!(
            split_dotnet_arguments("commit -m \"initial commit\""),
            vec!["commit", "-m", "initial commit"]
        );
        assert_eq!(
            split_dotnet_arguments("  a\tb  "),
            vec!["a".to_string(), "b".to_string()]
        );
        // GitHelper.EscapeArg doubles backslashes: not before a quote they stay literal.
        assert_eq!(split_dotnet_arguments(r#""C:\\x""#), vec![r"C:\\x"]);
        assert_eq!(
            split_dotnet_arguments(r#""say \"hi\"""#),
            vec![r#"say "hi""#]
        );
        assert_eq!(split_dotnet_arguments(r#""a\\""#), vec![r"a\"]);
        assert_eq!(split_dotnet_arguments(r#""a""b""#), vec![r#"a"b"#]);
        assert_eq!(split_dotnet_arguments("x\"\" y"), vec!["x", "y"]);
        assert!(split_dotnet_arguments("   ").is_empty());
    }
}
