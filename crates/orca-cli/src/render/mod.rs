//! The interactive CLI's renderers (port of `OpenOrca.Cli/Rendering` and
//! `Repl/WelcomeRenderer.cs`): tool calls and results, markdown replies (whole and
//! streamed), friendly error lines, the welcome banner, the thinking spinner, and the
//! stream-time tag filters. Output goes through `ui::console::Console` as Spectre-style markup.

pub mod agent_badge;
pub mod diff;
pub mod friendly_errors;
pub mod links;
pub mod markdown;
pub mod tag_filters;
pub mod text;
pub mod thinking;
pub mod tool_call;
pub mod welcome;
