//! What the tool pipeline does around a call besides running it (`ToolExecutionPipeline`):
//! pre-hooks, checkpoints before file modifications, post-hooks and the auto-formatter.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use orca_abstractions::{Tool, ToolCapabilities, ToolContext};

use crate::config::OrcaConfig;
use crate::hooks::{AutoFormatter, HookRunner};
use crate::session::{CheckpointManager, DEFAULT_CHECKPOINT_SESSION};

/// Everything is optional: an [`super::AgentSession`] starts with none of it.
#[derive(Default, Clone)]
pub struct ToolHooks {
    /// The active session (`ReplState.CurrentSessionId`); checkpoints go under it, or under
    /// `default` without one.
    pub session_id: Option<String>,
    pub checkpoints: Option<Arc<CheckpointManager>>,
    pub hooks: Option<Arc<HookRunner>>,
    /// Only when `formatter.enabled` (as C# wires it).
    pub formatter: Option<Arc<AutoFormatter>>,
}

impl ToolHooks {
    /// The CLI wiring: checkpoints under `config_dir/checkpoints`, the configured hooks and,
    /// when enabled, the formatter.
    pub fn from_config(config: &OrcaConfig, config_dir: &Path) -> Self {
        Self {
            session_id: None,
            checkpoints: Some(Arc::new(CheckpointManager::new(
                config_dir.join("checkpoints"),
            ))),
            hooks: Some(Arc::new(HookRunner::new(config.hooks.clone()))),
            formatter: config
                .formatter
                .enabled
                .then(|| Arc::new(AutoFormatter::new(config.formatter.clone()))),
        }
    }

    /// The pre-hook: false when it blocked the call.
    pub async fn pre_hook(&self, tool_name: &str, args_json: &str) -> bool {
        match &self.hooks {
            Some(h) => {
                let allowed = h.run_pre_hook(tool_name, args_json).await;
                if !allowed {
                    tracing::info!("Tool {tool_name} blocked by pre-hook");
                }
                allowed
            }
            None => true,
        }
    }

    /// `SnapshotCheckpointAsync`: back up the files a file-modifying tool is about to touch.
    /// Failures are logged only.
    pub fn snapshot(&self, tool: &dyn Tool, args: &Value, ctx: &ToolContext) {
        let Some(cp) = &self.checkpoints else {
            return;
        };
        if !tool
            .capabilities()
            .contains(ToolCapabilities::MODIFIES_FILES)
        {
            return;
        }
        let sid = self
            .session_id
            .as_deref()
            .unwrap_or(DEFAULT_CHECKPOINT_SESSION);
        for path in tool.affected_file_paths(args, ctx) {
            if let Err(e) = cp.snapshot(&ctx.full_path(&path), sid) {
                tracing::warn!("Failed to create checkpoint for {}: {e}", tool.name());
                return;
            }
        }
    }

    /// `RunPostHookInBackground`.
    pub fn post_hook(&self, tool_name: &str, args_json: &str, result: &str, is_error: bool) {
        if let Some(h) = &self.hooks {
            h.run_post_hook_in_background(tool_name, args_json, result, is_error);
        }
    }

    /// `AppendAutoFormatResultAsync`: the formatter's output for each modified file, appended
    /// as `\n\n[Auto-format] ...`.
    pub async fn append_auto_format(
        &self,
        tool: &dyn Tool,
        args: &Value,
        ctx: &ToolContext,
        content: String,
    ) -> String {
        let Some(f) = &self.formatter else {
            return content;
        };
        if !tool
            .capabilities()
            .contains(ToolCapabilities::MODIFIES_FILES)
        {
            return content;
        }
        let mut appendages = Vec::new();
        for path in tool.affected_file_paths(args, ctx) {
            if let Some(out) = f.format_file(&path, &ctx.cwd).await {
                appendages.push(out);
            }
        }
        if appendages.is_empty() {
            content
        } else {
            format!("{content}\n\n[Auto-format] {}", appendages.join("\n"))
        }
    }

    /// Wait (bounded) for post-hooks still running, before the process exits.
    pub async fn flush(&self, timeout: std::time::Duration) {
        if let Some(h) = &self.hooks {
            h.wait_for_pending_post_hooks(timeout).await;
        }
    }
}
