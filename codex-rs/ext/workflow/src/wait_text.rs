//! Text budgets and shared builders for workflow wait responses.
//!
//! `WaitWorkflow` and the `WaitWorkflows` winner describe the same runs, so their
//! identity, summary, and error budgets are resolved here once instead of being
//! restated per tool and drifting apart.
//!
//! Depends only on `model_text` and the snapshot type, so result and status tools can
//! both use it without an import cycle.

use crate::model_text::TRUNCATION_MARKER;
use crate::model_text::truncate_model_text;
use crate::service::WorkflowTaskSnapshot;

/// Budget for run summary text and result read/write errors in a wait response.
pub(super) const WAIT_OUTPUT_TEXT_MAX_BYTES: usize = 96;
/// Budget for a terminal run error inside a wait response.
///
/// This matches the `ListWorkflows` text budget so the two tools start from the same
/// view of a failure, until an over-cap wait response has to step its error down
/// through `WAIT_ERROR_BUDGET_LADDER`.
pub(super) const WAIT_ERROR_TEXT_MAX_BYTES: usize = 160;
/// Budget for a workflow name inside any wait response.
///
/// Shared by `WaitWorkflow` and the `WaitWorkflows` winner so the two tools name the
/// same run alike, until a bounding rung has to stub one of them.
pub(super) const WAIT_WORKFLOW_NAME_MAX_BYTES: usize = 64;
/// Stub budget applied to identity text once a wait response has to shrink.
///
/// Derived from the marker so a stub always shows that it is one. A budget below the
/// marker emits a bare prefix, which for a run error reads as the complete failure
/// reason and quietly replaces it with a shorter, wrong one. This is an upper bound:
/// a multi-byte char at the cut point floors the result one byte lower.
pub(super) const COMPACT_WAIT_TEXT_MAX_BYTES: usize = TRUNCATION_MARKER.len() + 1;
/// Successively smaller budgets for a run error inside an over-cap wait response.
///
/// The error gives way last and in stages because a failed run can also carry a
/// partial result; dropping straight to a stub would leave a large result with no
/// explanation.
pub(super) const WAIT_ERROR_BUDGET_LADDER: [usize; 2] =
    [WAIT_OUTPUT_TEXT_MAX_BYTES, COMPACT_WAIT_TEXT_MAX_BYTES];

pub(super) fn bounded_output_text(value: &str) -> String {
    truncate_model_text(value, WAIT_OUTPUT_TEXT_MAX_BYTES)
}

/// Bounds a terminal run error for a wait response.
///
/// Errors get a larger budget than summaries because a failed run's error text is
/// the field the model actually needs, and `ListWorkflows` already shows 160 bytes
/// of the same string.
pub(super) fn bounded_error_text(value: &str) -> String {
    truncate_model_text(value, WAIT_ERROR_TEXT_MAX_BYTES)
}

/// Reduces wait identity text to a stub.
pub(super) fn compact_wait_text(value: &str) -> String {
    truncate_model_text(value, COMPACT_WAIT_TEXT_MAX_BYTES)
}

/// Bounded identity text for one run inside a wait response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WaitRunText {
    pub(super) workflow_name: String,
    pub(super) summary: String,
    pub(super) error: Option<String>,
}

impl WaitRunText {
    pub(super) fn from_snapshot(snapshot: &WorkflowTaskSnapshot) -> Self {
        Self {
            workflow_name: truncate_model_text(
                &snapshot.workflow_name,
                WAIT_WORKFLOW_NAME_MAX_BYTES,
            ),
            summary: bounded_output_text(&snapshot.summary),
            error: snapshot.error.as_deref().map(bounded_error_text),
        }
    }
}

#[cfg(test)]
#[path = "wait_text_tests.rs"]
mod tests;
