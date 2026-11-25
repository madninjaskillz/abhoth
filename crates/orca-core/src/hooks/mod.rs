//! Tool hooks and the auto-formatter (port of `OpenOrca.Core/Hooks`).

pub mod formatter;
pub mod runner;

pub use formatter::AutoFormatter;
pub use runner::HookRunner;

/// A command that opens no console window on Windows.
fn hidden_command(program: &str) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}
