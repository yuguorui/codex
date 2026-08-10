//! Shared argument validation for the workflow wait tools.
//!
//! `WaitWorkflow` and `WaitWorkflows` expose the same `timeoutMs` knob and the
//! same run-id contract. Keeping both in one module prevents the two tools from
//! drifting apart when the configured wait window or the run-id shape changes.

use codex_core::config::Config;
use codex_extension_api::FunctionCallError;
use std::collections::BTreeSet;

/// Upper bound on how many runs one `WaitWorkflows` call may observe.
pub(crate) const MAX_WAIT_WORKFLOW_ITEMS: usize = 8;
/// Upper bound on a single run id accepted by either wait tool.
pub(crate) const MAX_WAIT_WORKFLOW_ID_BYTES: usize = 128;

/// Clamps `timeoutMs` into the configured wait window.
///
/// An omitted value uses the configured default; a shorter value is raised to
/// the configured minimum; a longer value is rejected so the model retries with
/// a supported window instead of silently waiting less than requested.
pub(crate) fn resolve_timeout_ms(
    config: &Config,
    requested_timeout_ms: Option<i64>,
) -> Result<i64, FunctionCallError> {
    let min_timeout_ms = config.multi_agent_v2.min_wait_timeout_ms;
    let max_timeout_ms = config.multi_agent_v2.max_wait_timeout_ms;
    match requested_timeout_ms {
        Some(timeout_ms) if timeout_ms > max_timeout_ms => Err(FunctionCallError::RespondToModel(
            "choose timeoutMs within the configured wait window or omit it to use the server default"
                .to_string(),
        )),
        Some(timeout_ms) => Ok(timeout_ms.max(min_timeout_ms)),
        None => Ok(config.multi_agent_v2.default_wait_timeout_ms),
    }
}

/// Validates one run id supplied to `WaitWorkflow`.
pub(crate) fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.is_empty() || run_id.len() > MAX_WAIT_WORKFLOW_ID_BYTES {
        return Err(format!(
            "provide the workflow run id as 1..={MAX_WAIT_WORKFLOW_ID_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

/// Validates the focused, unique run-id set supplied to `WaitWorkflows`.
pub(crate) fn validate_run_ids(run_ids: &[String]) -> Result<(), String> {
    if run_ids.is_empty() || run_ids.len() > MAX_WAIT_WORKFLOW_ITEMS {
        return Err(
            "provide a focused, non-empty set of runIds; split larger sets across additional WaitWorkflows calls"
                .to_string(),
        );
    }
    for run_id in run_ids {
        validate_run_id(run_id)?;
    }
    if run_ids.iter().collect::<BTreeSet<_>>().len() != run_ids.len() {
        return Err("provide each workflow run id once in runIds".to_string());
    }
    Ok(())
}

#[cfg(test)]
#[path = "wait_args_tests.rs"]
mod tests;
