//! The Benchmarks page (`BenchRunner`, `BenchQueue`).

pub mod endpoints;

use std::path::PathBuf;
use std::sync::Arc;

use crate::audit::AuditLog;
use crate::projects::manager::ProjectManager;

#[derive(Clone)]
pub struct BenchOptions {
    pub dir: PathBuf,
}

pub struct BenchRunner {
    pub options: BenchOptions,
}

impl BenchRunner {
    pub fn new(
        options: BenchOptions,
        _projects: Arc<ProjectManager>,
        _audit: Arc<AuditLog>,
        _profiles: Option<Arc<orca_core::profiles::ProfileService>>,
    ) -> Arc<Self> {
        Arc::new(Self { options })
    }

    pub async fn stop_all(&self) {}
}
