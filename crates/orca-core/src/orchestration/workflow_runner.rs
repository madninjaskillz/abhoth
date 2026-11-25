//! Runs a workflow (`WorkflowRunner`): stages one after another, the agents of a stage in
//! parallel (bounded by agent.maxConcurrent through the orchestrator). A failed agent adds its
//! error to the stage output instead of stopping the run.

use std::sync::Arc;

use super::cancel::CancelToken;
use super::orchestrator::{AgentOrchestrator, ParallelAgentSpec};
use super::types::AgentStatus;
use super::workflow::{WorkflowDefinition, substitute_template};
use crate::config::dotnet_json::NEWLINE;

/// Run `workflow` on `input`; the last stage's output. `on_status` gets a line per stage.
/// Err: cancelled, or a stage could not start (the agent cap).
pub async fn run_workflow(
    orchestrator: &Arc<AgentOrchestrator>,
    workflow: &WorkflowDefinition,
    input: &str,
    on_status: Option<&(dyn Fn(&str) + Send + Sync)>,
    ct: &CancelToken,
) -> Result<String, String> {
    let mut stage_outputs: Vec<String> = Vec::new();
    let count = workflow.stages.len();
    for (i, stage) in workflow.stages.iter().enumerate() {
        if ct.is_cancelled() {
            return Err("The operation was canceled.".into());
        }
        let stage_label = stage
            .name
            .clone()
            .unwrap_or_else(|| format!("stage {}", i + 1));
        if let Some(f) = on_status {
            f(&format!(
                "[{}] {stage_label} ({}/{count}) \u{2014} {} agent(s)",
                workflow.name,
                i + 1,
                stage.agents.len()
            ));
        }
        tracing::info!(
            "Workflow {}: running {stage_label} with {} agents",
            workflow.name,
            stage.agents.len()
        );
        let specs: Vec<ParallelAgentSpec> = stage
            .agents
            .iter()
            .map(|a| {
                ParallelAgentSpec::new(
                    substitute_template(&a.task, input, &stage_outputs),
                    a.agent_type.clone(),
                )
            })
            .collect();
        // Workflow agents sit one level below the depth ceiling so they can't fan out further:
        // the workflow is the orchestration layer.
        let depth = (orchestrator.config().agent.max_spawn_depth - 1).max(0);
        let contexts = orchestrator.spawn_parallel(&specs, depth, ct).await?;
        let mut sb = String::new();
        for (a, context) in contexts.iter().enumerate() {
            let status = context.status();
            if contexts.len() > 1 {
                sb.push_str(&format!(
                    "### {stage_label} \u{2014} agent {} ({}, {status:?}){NEWLINE}",
                    a + 1,
                    context.agent_type
                ));
            }
            let body = match status {
                AgentStatus::Completed | AgentStatus::CompletedPartial => {
                    context.result().unwrap_or_else(|| "(no result)".into())
                }
                _ => format!("[agent failed: {}]", context.error().unwrap_or_default()),
            };
            sb.push_str(&body);
            sb.push_str(NEWLINE);
            sb.push_str(NEWLINE);
        }
        stage_outputs.push(sb.trim_end().to_string());
    }
    Ok(stage_outputs.pop().unwrap_or_default())
}
