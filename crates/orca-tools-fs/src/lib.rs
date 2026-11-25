//! File tools (port of `OpenOrca.Tools.FileSystem`): read/write/edit, multi_edit, apply_patch,
//! list/glob/grep, and the file-management tools (cd, mkdir, create_directory, copy, move,
//! rename, delete).
//!
//! Output text follows the C# tools line for line, including `Environment.NewLine` line breaks
//! where C# uses `AppendLine`, so the cross-check harness can compare tool results directly.

use std::sync::Arc;

use orca_abstractions::Tool;

mod apply_patch;
mod edit_file;
mod fs_ops;
mod glob;
mod grep;
mod list_directory;
mod multi_edit;
mod read_file;
mod util;
mod write_file;

pub use apply_patch::{ApplyPatchTool, unified_diff};
pub use edit_file::EditFileTool;
pub use fs_ops::{
    CdTool, CopyFileTool, CreateDirectoryTool, DeleteFileTool, MkdirTool, MoveFileTool,
    RenameFileTool,
};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_directory::ListDirectoryTool;
pub use multi_edit::MultiEditTool;
pub use read_file::ReadFileTool;
pub use write_file::WriteFileTool;

/// Every tool in this crate, in the C# registration order (reflection lists the plugin's
/// types sorted by class name).
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ApplyPatchTool),
        Arc::new(CdTool),
        Arc::new(CopyFileTool),
        Arc::new(CreateDirectoryTool),
        Arc::new(DeleteFileTool),
        Arc::new(EditFileTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
        Arc::new(ListDirectoryTool),
        Arc::new(MkdirTool),
        Arc::new(MoveFileTool),
        Arc::new(MultiEditTool),
        Arc::new(ReadFileTool),
        Arc::new(RenameFileTool),
        Arc::new(WriteFileTool),
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
