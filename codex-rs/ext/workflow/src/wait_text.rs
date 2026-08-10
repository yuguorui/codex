//! Text budgets and shared builders for workflow wait responses.
//!
//! `WaitWorkflow` and the `WaitWorkflows` winner describe the same runs, so their
//! identity budgets, error budgets, and inline result head are resolved here once
//! instead of being restated per tool and drifting apart.

use codex_protocol::ThreadId;

use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;
use crate::workflow_result_tool::RESULT_INLINE_MAX_BYTES;
use crate::workflow_result_tool::WorkflowResultData;
use crate::workflow_result_tool::truncate_model_text;
use crate::workflow_result_tool::workflow_result_is_available;

/// Budget for run summary text and result read/write errors in a wait response.
pub(super) const WAIT_OUTPUT_TEXT_MAX_BYTES: usize = 96;
/// Budget for a terminal run error inside a wait response.
///
/// This matches the `ListWorkflows` text budget so the two tools never disagree
/// about how much of the same failure a model gets to read.
pub(super) const WAIT_ERROR_TEXT_MAX_BYTES: usize = 160;
/// Budget for a workflow name inside any wait response.
///
/// Shared by `WaitWorkflow` and the `WaitWorkflows` winner so the same run is never
/// named differently by the two tools.
pub(super) const WAIT_WORKFLOW_NAME_MAX_BYTES: usize = 64;
/// Stub budget applied to identity text once a wait response has to shrink.
pub(super) const COMPACT_WAIT_TEXT_MAX_BYTES: usize = 8;
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

/// Reads the inline result head a wait response should carry for one run.
///
/// Shared by `WaitWorkflow` and the `WaitWorkflows` `mode: any` winner so both tools
/// describe the same artifact with the same budget and the same failure fallback.
pub(super) async fn read_wait_result_data(
    service: &WorkflowService,
    thread_id: ThreadId,
    snapshot: &WorkflowTaskSnapshot,
) -> WorkflowResultData {
    if !workflow_result_is_available(snapshot.status) {
        return WorkflowResultData::without_chunk(snapshot, /*result_error*/ None);
    }
    match service
        .read_result_chunk(
            thread_id,
            snapshot,
            /*offset*/ 0,
            RESULT_INLINE_MAX_BYTES,
        )
        .await
    {
        Ok(chunk) => {
            match WorkflowResultData::from_snapshot_with_result(snapshot, Some(&chunk), None) {
                Ok(data) => data,
                Err(error) => WorkflowResultData::without_chunk(snapshot, Some(&error.to_string())),
            }
        }
        Err(error) => WorkflowResultData::without_chunk(snapshot, Some(&error)),
    }
}

#[cfg(test)]
#[path = "wait_text_tests.rs"]
mod tests;
