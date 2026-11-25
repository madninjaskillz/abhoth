//! D140: each web UI session's tools keep their own background processes, task list and Pi
//! jobs, so one user's agent can't list, read or stop another's. The CLI keeps the
//! process-wide state.

use std::sync::Arc;

use orca_abstractions::ToolContext;
use orca_cli::{ToolScope, build_registry_scoped};
use orca_core::config::OrcaConfig;
use serde_json::json;

struct NoAsk;

#[async_trait::async_trait]
impl orca_tools_utility::UserPrompter for NoAsk {
    async fn ask(&self, _q: &str, _o: &[String]) -> Result<String, String> {
        Err("no".into())
    }
}

#[tokio::test]
async fn two_sessions_do_not_share_tasks_processes_or_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let config = OrcaConfig::default();
    let (sa, sb) = (ToolScope::new(), ToolScope::new());
    assert!(!Arc::ptr_eq(&sa.jobs, &sb.jobs));
    assert!(!Arc::ptr_eq(&sa.tasks, &sb.tasks));
    assert_ne!(sa.id, sb.id);
    let (ra, _sa) = build_registry_scoped(&config, dir.path(), Arc::new(NoAsk), Some(sa));
    let (rb, _sb) = build_registry_scoped(&config, dir.path(), Arc::new(NoAsk), Some(sb));
    let ctx = ToolContext::new(dir.path());
    let (ta, tb) = (
        ra.resolve("task_list").unwrap(),
        rb.resolve("task_list").unwrap(),
    );
    let added = ta
        .execute(&json!({"action": "add", "task": "ann's secret task"}), &ctx)
        .await;
    assert!(!added.is_error, "{}", added.content);
    let mine = ta.execute(&json!({"action": "list"}), &ctx).await;
    assert!(mine.content.contains("ann's secret task"));
    let theirs = tb.execute(&json!({"action": "list"}), &ctx).await;
    assert!(
        !theirs.content.contains("ann's secret task"),
        "{}",
        theirs.content
    );
    // Processes: B's list_processes doesn't show A's.
    let start = ra.resolve("start_background_process").unwrap();
    let cmd = if cfg!(windows) {
        "ping -n 20 127.0.0.1"
    } else {
        "sleep 20"
    };
    let r = start.execute(&json!({"command": cmd}), &ctx).await;
    assert!(!r.is_error, "{}", r.content);
    let id = r.content.split('"').nth(1).unwrap().to_string();
    let la = ra
        .resolve("list_processes")
        .unwrap()
        .execute(&json!({}), &ctx)
        .await;
    let lb = rb
        .resolve("list_processes")
        .unwrap()
        .execute(&json!({}), &ctx)
        .await;
    assert!(la.content.contains(&id));
    assert!(!lb.content.contains(&id));
    let stop_b = rb
        .resolve("stop_process")
        .unwrap()
        .execute(&json!({"process_id": id}), &ctx)
        .await;
    assert!(stop_b.is_error);
    let stop_a = ra
        .resolve("stop_process")
        .unwrap()
        .execute(&json!({"process_id": id}), &ctx)
        .await;
    assert!(!stop_a.is_error, "{}", stop_a.content);
}
