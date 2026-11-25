//! `TaskStore`: the in-memory, session-scoped task list behind `task_list`.
//!
//! C# keeps one static list per process; [`TaskStore::shared`] is that list. Tools take an
//! `Arc<TaskStore>` so tests can use their own.

use std::sync::{Arc, LazyLock, Mutex};

use crate::append_line;

#[derive(Debug, Clone)]
pub struct TaskItem {
    pub id: i32,
    pub description: String,
    pub is_completed: bool,
}

#[derive(Debug)]
pub struct TaskStore {
    /// The tasks and the next id.
    inner: Mutex<(Vec<TaskItem>, i32)>,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new((Vec::new(), 1)),
        }
    }
}

static SHARED: LazyLock<Arc<TaskStore>> = LazyLock::new(|| Arc::new(TaskStore::default()));

impl TaskStore {
    /// The process-wide store (the C# static class).
    pub fn shared() -> Arc<TaskStore> {
        SHARED.clone()
    }

    pub fn add(&self, description: &str) -> i32 {
        let mut g = self.inner.lock().unwrap();
        let id = g.1;
        g.1 += 1;
        g.0.push(TaskItem {
            id,
            description: description.to_string(),
            is_completed: false,
        });
        id
    }

    pub fn complete(&self, id: i32) -> bool {
        let mut g = self.inner.lock().unwrap();
        match g.0.iter_mut().find(|t| t.id == id) {
            Some(t) => {
                t.is_completed = true;
                true
            }
            None => false,
        }
    }

    pub fn remove(&self, id: i32) -> bool {
        let mut g = self.inner.lock().unwrap();
        let before = g.0.len();
        g.0.retain(|t| t.id != id);
        g.0.len() < before
    }

    pub fn list(&self) -> String {
        let g = self.inner.lock().unwrap();
        if g.0.is_empty() {
            return "No tasks.".into();
        }
        let completed = g.0.iter().filter(|t| t.is_completed).count();
        let mut sb = String::new();
        append_line(
            &mut sb,
            &format!("Tasks: {completed}/{} completed", g.0.len()),
        );
        append_line(&mut sb, "");
        for t in &g.0 {
            let check = if t.is_completed { "x" } else { " " };
            append_line(
                &mut sb,
                &format!("  [{check}] #{}: {}", t.id, t.description),
            );
        }
        sb
    }

    /// Clear the list and restart ids at 1.
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.0.clear();
        g.1 = 1;
    }
}
