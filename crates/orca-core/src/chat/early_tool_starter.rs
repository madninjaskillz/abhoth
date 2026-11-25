//! Starts read-only tool calls while the model is still writing the rest of its response
//! (`EarlyToolStarter`, agent.earlyReadOnlyTools). The client only surfaces a tool call when the
//! whole stream ends, but the raw updates carry each call's deltas by index: call i is complete
//! as soon as call i+1 starts. A complete call is started at once when it, and every call before
//! it in the response, is eligible (read-only, so nothing earlier can change what it sees). The
//! executor later takes the running task by call id instead of running the call again; calls
//! never taken (blocked by a policy, response discarded) are aborted on drop. Their only effect
//! was a read.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::task::JoinHandle;

use orca_abstractions::ToolResult;

use crate::client::ToolCallDelta;

type Eligible = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;
type Runner<T> = Arc<dyn Fn(String, String) -> BoxFuture<'static, T> + Send + Sync>;
type Normalize = Box<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Default)]
struct Pending {
    id: Option<String>,
    name: Option<String>,
    args: String,
    done: bool,
}

struct Started<T> {
    name: String,
    args: Option<Value>,
    task: JoinHandle<T>,
}

/// See the module docs. `T` is what a run yields (a [`ToolResult`] by default).
pub struct EarlyToolStarter<T: Send + 'static = ToolResult> {
    eligible: Eligible,
    run: Runner<T>,
    normalize: Normalize,
    calls: BTreeMap<usize, Pending>,
    started: HashMap<String, Started<T>>,
    blocked: bool,
    used: usize,
}

impl<T: Send + 'static> EarlyToolStarter<T> {
    /// `eligible`: (tool name, arguments JSON) -> may this call run before the response ends?
    /// `run`: runs the call (tool name, arguments JSON) on a tokio task.
    pub fn new(
        eligible: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
        run: impl Fn(String, String) -> BoxFuture<'static, T> + Send + Sync + 'static,
    ) -> Self {
        Self {
            eligible: Box::new(eligible),
            run: Arc::new(run),
            normalize: Box::new(str::to_string),
            calls: BTreeMap::new(),
            started: HashMap::new(),
            blocked: false,
            used: 0,
        }
    }

    /// Applied to the arguments before a run and before matching a taken call (the executor
    /// strips `_reason`).
    pub fn with_normalize(
        mut self,
        normalize: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        self.normalize = Box::new(normalize);
        self
    }

    /// Calls started early and not taken yet.
    pub fn started_count(&self) -> usize {
        self.started.len()
    }

    /// Calls whose early result was used.
    pub fn used_count(&self) -> usize {
        self.used
    }

    /// Feed every raw tool-call fragment of the stream.
    pub fn observe(&mut self, u: &ToolCallDelta) {
        if !self.calls.contains_key(&u.index) {
            // A new call index: every earlier call is complete.
            let earlier: Vec<usize> = self
                .calls
                .iter()
                .filter(|(i, c)| **i < u.index && !c.done)
                .map(|(i, _)| *i)
                .collect();
            for i in earlier {
                self.complete(i);
            }
            self.calls.insert(u.index, Pending::default());
        }
        let call = self.calls.get_mut(&u.index).expect("inserted above");
        if let Some(id) = u.id.as_deref().filter(|s| !s.is_empty()) {
            call.id = Some(id.to_string());
        }
        if let Some(name) = u.name.as_deref().filter(|s| !s.is_empty()) {
            call.name = Some(name.to_string());
        }
        if let Some(delta) = u.arguments.as_deref().filter(|s| !s.is_empty()) {
            call.args.push_str(delta);
        }
    }

    fn complete(&mut self, index: usize) {
        let Some(call) = self.calls.get_mut(&index) else {
            return;
        };
        call.done = true;
        if self.blocked {
            return;
        }
        let raw = if call.args.is_empty() {
            "{}"
        } else {
            call.args.as_str()
        };
        let args = (self.normalize)(raw);
        let parsed = serde_json::from_str::<Value>(&args).ok();
        let ok = match (&call.id, &call.name, &parsed) {
            (Some(_), Some(name), Some(Value::Object(_))) => (self.eligible)(name, &args),
            _ => false,
        };
        if !ok {
            // A later call might depend on this one (a write, then a read of the same file):
            // stop here.
            self.blocked = true;
            return;
        }
        let (id, name) = (
            call.id.clone().unwrap_or_default(),
            call.name.clone().unwrap_or_default(),
        );
        tracing::debug!(
            target: crate::agent::log_lines::EXECUTOR,
            "Starting {name} ({id}) while the response is still streaming"
        );
        let task = tokio::spawn((self.run)(name.clone(), args));
        self.started.insert(
            id,
            Started {
                name,
                args: parsed,
                task,
            },
        );
    }

    /// The early run for this call, if one was started for the same tool and the same
    /// arguments; `None` means run it normally. Each early run is handed out once.
    pub fn try_take(
        &mut self,
        call_id: Option<&str>,
        tool_name: &str,
        args_json: &str,
    ) -> Option<JoinHandle<T>> {
        let started = self.started.remove(call_id?)?;
        let json = if args_json.trim().is_empty() {
            "{}"
        } else {
            args_json
        };
        let args = serde_json::from_str::<Value>(&(self.normalize)(json)).ok()?;
        if started.name != tool_name || started.args.as_ref() != Some(&args) {
            return None;
        }
        self.used += 1;
        Some(started.task)
    }
}

impl<T: Send + 'static> Drop for EarlyToolStarter<T> {
    /// Abort early runs nobody took.
    fn drop(&mut self) {
        if !self.started.is_empty() {
            tracing::debug!("{} early tool run(s) were not used", self.started.len());
        }
        for s in self.started.values() {
            s.task.abort();
        }
    }
}
