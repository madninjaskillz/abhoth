//! `--continue` / `--resume <id>` (Program.cs): which saved session a run starts from.
//!
//! The notices go to stderr, which is where the C# build's console writes them in `--print`
//! mode (stdout stays the JSON). An unknown `--resume` id ends the run with exit code 0, as in C#.

use std::path::Path;

use orca_core::client::ChatMessage;
use orca_core::session::{SessionManager, session_to_messages};

use crate::args::CliArgs;

/// A session to continue: its id and conversation (system prompt first, if it had one).
pub struct RestoredSession {
    pub id: String,
    pub messages: Vec<ChatMessage>,
}

pub enum Restore {
    /// Start a new conversation.
    Fresh,
    Resumed(RestoredSession),
    /// `--resume <id>` named no saved session: the run stops (exit code 0).
    NotFound,
    /// The session file could not be read: the run stops (exit code 1).
    Unreadable,
}

/// Pick the session for `-c` / `-r`, printing the same notices as C#.
pub fn restore_from_args(args: &CliArgs, sessions: &SessionManager, cwd: &Path) -> Restore {
    if !args.continue_session && args.resume_session_id.is_none() {
        return Restore::Fresh;
    }
    let data = if let Some(id) = &args.resume_session_id {
        match sessions.load(id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                eprintln!("Session not found: {id}");
                return Restore::NotFound;
            }
            Err(e) => {
                // C# lets the exception end the program.
                eprintln!("Session {id} could not be read: {e}");
                return Restore::Unreadable;
            }
        }
    } else {
        // --continue: the most recent session from this directory, else from anywhere.
        match sessions.latest_for(cwd) {
            Some((s, elsewhere)) => {
                if elsewhere {
                    eprintln!(
                        "No session for this directory \u{2014} resuming most recent from elsewhere."
                    );
                }
                s
            }
            None => {
                eprintln!("No saved sessions to continue.");
                return Restore::Fresh;
            }
        }
    };
    eprintln!(
        "Resumed session: {} ({} messages)",
        data.title,
        data.messages.len()
    );
    Restore::Resumed(RestoredSession {
        id: data.id.clone(),
        messages: session_to_messages(&data),
    })
}
