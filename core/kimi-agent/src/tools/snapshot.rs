use std::sync::Arc;

use kaos::CachedKaos;
use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::{CallableTool2, ToolReturnValue, tool_ok};

use crate::soul::agent::Runtime;

const UNDO_DESC: &str = include_str!("desc/snapshot/undo.md");

pub struct Undo {
    cached_kaos: Arc<CachedKaos>,
}

impl Undo {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            cached_kaos: runtime.cached_kaos.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UndoParams {
    #[serde(default = "default_steps")]
    #[schemars(description = "Number of file writes to undo (default: 1).")]
    pub steps: usize,
}

fn default_steps() -> usize {
    1
}

#[async_trait::async_trait]
impl CallableTool2 for Undo {
    type Params = UndoParams;

    fn name(&self) -> &str {
        "Undo"
    }

    fn description(&self) -> &str {
        UNDO_DESC
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let steps = params.steps.max(1);
        let report = match self.cached_kaos.undo(steps).await {
            Ok(r) => r,
            Err(e) => {
                return kosong::tooling::tool_error("", format!("Undo failed: {e}"), "Undo error");
            }
        };

        let mut out = format!(
            "steps_requested: {steps}\nsteps_available: {}\nsteps_applied: {}\nrestored: {}\ndeleted: {}\nskipped: {}",
            report.steps_available,
            report.steps_applied,
            report.restored,
            report.deleted,
            report.skipped,
        );
        if !report.errors.is_empty() {
            out.push_str(&format!("\nerrors: {}", report.errors.len()));
            for e in &report.errors {
                out.push_str(&format!("\n  - {e}"));
            }
        }
        if report.steps_applied == 0 {
            out.push_str("\nnothing to undo");
        }

        let mut summary = format!(
            "Undid {} write(s) ({} restored, {} deleted)",
            report.steps_applied, report.restored, report.deleted
        );
        if report.skipped > 0 {
            summary.push_str(&format!(", {} skipped", report.skipped));
        }

        tool_ok(out, &summary, "")
    }
}
