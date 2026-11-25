//! agent.earlyReadOnlyTools for the main loop (`ToolCallExecutor.BeginEarlyRuns`,
//! `EndEarlyRuns` and `IsEarlyEligible`): a native call that is complete while the response is
//! still streaming starts at once when it and every call before it may run early.
//!
//! C# runs the whole tool pipeline early (and records the call when that run finishes). Here
//! the early task only does what can't be repeated (the pre-hook and the tool itself); the
//! rest of the pipeline (permission, validation, recording, formatting) runs when the loop
//! executes the call, so records keep the call order and a call the loop never executes
//! (doom loop, dropped response) is not recorded. Calls whose permission, argument check or
//! directory restriction would refuse them are not started early.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_abstractions::{ToolCapabilities, ToolResult, ToolRiskLevel};
use serde_json::Value;

use super::session::{AgentSession, TOOL_EXECUTION_TIMEOUT_SECONDS, validate_required_args};
use super::{permissions, policies};
use crate::chat::EarlyToolStarter;
use crate::client::ToolCall;

/// What an early run yields.
pub(crate) struct EarlyOutcome {
    /// The pre-hook let the call run.
    pub hook_allowed: bool,
    pub result: ToolResult,
    /// How long the tool ran.
    pub elapsed_ms: i64,
}

pub(crate) type EarlyRuns = EarlyToolStarter<EarlyOutcome>;

impl AgentSession {
    /// `BeginEarlyRuns`: before a streaming request, a fresh starter for its read-only calls
    /// (none when agent.earlyReadOnlyTools is off or tools are not native).
    pub(super) fn begin_early_runs(&mut self) {
        self.end_early_runs();
        if !self.config.agent.early_read_only_tools || !self.native_tools {
            return;
        }
        let eligible = self.early_eligibility();
        let registry = self.registry.clone();
        let ctx = self.ctx.clone();
        let hooks = self.tool_hooks.clone();
        let run = move |name: String,
                        args: String|
              -> futures_util::future::BoxFuture<'static, EarlyOutcome> {
            let registry = registry.clone();
            let ctx = ctx.clone();
            let hooks = hooks.clone();
            Box::pin(async move {
                if !hooks.pre_hook(&name, &args).await {
                    return EarlyOutcome {
                        hook_allowed: false,
                        result: ToolResult::blocked("Tool blocked by hook."),
                        elapsed_ms: 0,
                    };
                }
                let Some(tool) = registry.resolve(&name) else {
                    return EarlyOutcome {
                        hook_allowed: true,
                        result: ToolResult::error(format!("Unknown tool: {name}")),
                        elapsed_ms: 0,
                    };
                };
                let parsed: Value = serde_json::from_str(&args)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
                let started = Instant::now();
                let exec = tool.execute_streaming(&parsed, &ctx, &|_| {});
                // The same timeout rule as the normal run (`run_tool`).
                let result = if tool.capabilities().contains(ToolCapabilities::LONG_RUNNING) {
                    exec.await
                } else {
                    match tokio::time::timeout(
                        Duration::from_secs(TOOL_EXECUTION_TIMEOUT_SECONDS),
                        exec,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => ToolResult::error(format!(
                            "Error: Tool '{}' timed out after {TOOL_EXECUTION_TIMEOUT_SECONDS} seconds.",
                            tool.name()
                        )),
                    }
                };
                EarlyOutcome {
                    hook_allowed: true,
                    result,
                    elapsed_ms: started.elapsed().as_millis() as i64,
                }
            })
        };
        self.early_runs =
            Some(EarlyToolStarter::new(eligible, run).with_normalize(super::turn::strip_reason));
    }

    /// `EndEarlyRuns`: abort early runs nobody used.
    pub(super) fn end_early_runs(&mut self) {
        if let Some(e) = self.early_runs.take()
            && (e.started_count() > 0 || e.used_count() > 0)
        {
            tracing::info!(
                target: super::log_lines::EXECUTOR,
                "Early tool runs: {} used, {} unused",
                e.used_count(),
                e.started_count()
            );
        }
    }

    /// The early run of this call, when one was started for the same tool and arguments.
    pub(super) async fn take_early_run(&mut self, call: &ToolCall) -> Option<EarlyOutcome> {
        let task =
            self.early_runs
                .as_mut()?
                .try_take(Some(&call.id), &call.name, &call.arguments)?;
        self.early_tool_runs_used += 1;
        Some(match task.await {
            Ok(outcome) => {
                tracing::debug!(
                    target: super::log_lines::EXECUTOR,
                    "Tool {} used its early run",
                    call.name
                );
                outcome
            }
            Err(e) => {
                tracing::warn!("Early run of {} failed: {e}", call.name);
                EarlyOutcome {
                    hook_allowed: true,
                    result: ToolResult::error(format!("Error: {e}")),
                    elapsed_ms: 0,
                }
            }
        })
    }

    /// `IsEarlyEligible`: read-only (the read-only command allowlist), an existing tool that
    /// is not interactive, auto-approved, and no policy blocks it. Also not when the normal
    /// pipeline would refuse it (permission rules, missing arguments, directory restriction).
    pub(super) fn early_eligibility(&self) -> impl Fn(&str, &str) -> bool + Send + Sync + 'static {
        let config = Arc::new(self.config.clone());
        let registry = self.registry.clone();
        let ctx = self.ctx.clone();
        let plan_mode = self.plan_mode;
        let approved = self.approved_scopes.clone();
        let orchestrator = self.orchestrator.clone();
        move |name: &str, args: &str| {
            if crate::orchestration::read_only_command::refusal(name, args).is_some() {
                return false;
            }
            let Some(tool) = registry.resolve(name) else {
                return false;
            };
            if registry.has_capability(name, ToolCapabilities::INTERACTIVE) {
                return false;
            }
            let perms = &config.permissions;
            let auto_approved = perms.auto_approve_all
                || (perms.auto_approve_read_only && tool.risk_level() == ToolRiskLevel::ReadOnly);
            if !auto_approved {
                return false;
            }
            let types = orchestrator.as_ref().map(|o| o.type_registry().clone());
            if policies::check_blocked(
                &config,
                &registry,
                plan_mode,
                true,
                name,
                args,
                types.as_ref(),
            )
            .is_some()
            {
                return false;
            }
            if let Some(o) = &orchestrator {
                let advertised = registry.resolve(policies::LEAD_MODE_TOOL_NAME).is_some();
                match policies::check_auto_lead(
                    &config,
                    o.lead_request_mode().as_deref(),
                    advertised,
                    name,
                ) {
                    policies::AutoLeadVerdict::Run => {}
                    // C# records the hands-on choice while checking eligibility, as the
                    // policy does when the call runs.
                    policies::AutoLeadVerdict::ChooseHandsOn(reason) => {
                        tracing::info!(target: "orca_core::Program",
                            "Lead mode for this request: {} \u{2014} {reason}",
                            policies::LEAD_MODE_HANDS_ON
                        );
                        o.set_lead_request_mode(Some(policies::LEAD_MODE_HANDS_ON));
                    }
                    policies::AutoLeadVerdict::Blocked(_) => return false,
                }
            }
            let parsed: Value = match serde_json::from_str(args) {
                Ok(v @ Value::Object(_)) => v,
                _ => return false,
            };
            if !matches!(
                permissions::evaluate_permission(
                    perms,
                    tool.name(),
                    tool.risk_level(),
                    tool.permission_arg_name(),
                    &parsed,
                    &approved,
                ),
                permissions::PermissionVerdict::Allow
            ) {
                return false;
            }
            validate_required_args(tool.as_ref(), &parsed).is_none()
                && super::session::directory_restriction(
                    config.allowed_directory.as_deref(),
                    &ctx,
                    tool.as_ref(),
                    &parsed,
                )
                .is_none()
        }
    }
}
