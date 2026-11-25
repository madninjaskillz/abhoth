//! File tools (port of `OpenOrca.Tools.FileSystem`). Phase 1 has the six core tools; the
//! rest of the plugin (multi_edit, apply_patch, copy/move/rename/delete, mkdir, cd) is phase 2.
//!
//! Output text follows the C# tools line for line, including `Environment.NewLine` line breaks
//! where C# uses `AppendLine`, so the cross-check harness can compare tool results directly.

use std::sync::Arc;

use orca_abstractions::Tool;

mod edit_file;
mod glob;
mod grep;
mod list_directory;
mod read_file;
mod util;
mod write_file;

pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_directory::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use write_file::WriteFileTool;

/// Every tool in this crate, in the C# registration order.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(EditFileTool),
        Arc::new(ListDirectoryTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
    ]
}

/// The edit_file fallback matchers, public for tests and the cross-check harness.
pub mod edit_file_matching {
    pub use crate::edit_file::{
        MatchResult, adjust_indentation, get_edit_diff, try_block_anchor_match,
        try_indentation_flexible_match, try_line_trimmed_match, try_multi_stage_match,
        try_whitespace_normalized_match,
    };
}
