use codex_core::config::Config;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolAvailability;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::TurnActivity;
use codex_extension_api::TurnActivitySubscription;
use codex_protocol::ThreadId;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::model_text::truncate_model_text;
use crate::service::WorkflowService;
use crate::service::WorkflowWaitOutcome;
use crate::wait_args::MAX_WAIT_WORKFLOW_ID_BYTES;
use crate::wait_args::resolve_timeout_ms;
use crate::wait_args::validate_run_id;
use crate::wait_text::WAIT_ERROR_BUDGET_LADDER;
use crate::wait_text::WaitRunText;
use crate::wait_text::compact_wait_text;
use crate::workflow_recovery::WorkflowRecoveryStatus;
use crate::workflow_recovery::workflow_recovery_status;
use crate::workflow_result_tool;
use crate::workflow_result_tool::WAIT_MODEL_CONTEXT_COMPACT_BYTES;
use crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES;
use crate::workflow_result_tool::WorkflowResultData;
use crate::workflow_result_tool::focused_response_error;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value_with_limit;
use crate::workflow_result_tool::read_wait_result_data;
use crate::workflow_result_write::resolve_result_write_target;
use crate::workflow_result_write::write_workflow_result;

pub const WAIT_WORKFLOW_TOOL_NAME: &str = "WaitWorkflow";

#[derive(Clone)]
pub(crate) struct WaitWorkflowToolExecutor {
    thread_id: ThreadId,
    config: Config,
    service: WorkflowService,
}

impl WaitWorkflowToolExecutor {
    pub(crate) fn new(thread_id: ThreadId, config: Config, service: WorkflowService) -> Self {
        Self {
            thread_id,
            config,
            service,
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for WaitWorkflowToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        wait_workflow_tool_spec(&self.config)
    }

    fn availability(&self) -> ToolAvailability {
        ToolAvailability::RootSessionOnly
    }

    fn handle<'a>(
        &'a self,
        invocation: ToolCall<'call>,
    ) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let args = parse_arguments(invocation.function_arguments()?)?;
            validate_run_id(&args.run_id).map_err(model_bounded_error)?;
            let timeout_ms = resolve_timeout_ms(&self.config, args.timeout_ms)?;
            let timeout_duration =
                Duration::from_millis(u64::try_from(timeout_ms).map_err(|error| {
                    model_bounded_error(format_args!("invalid WaitWorkflow timeout: {error}"))
                })?);
            let activity = invocation.turn_activity();
            let wait =
                self.service
                    .wait_for_terminal(self.thread_id, &args.run_id, timeout_duration);
            let (outcome, interrupted_by_user_input) =
                match race_with_turn_activity(wait, activity).await {
                    InterruptibleWait::Completed(outcome) => {
                        (outcome.map_err(model_bounded_error)?, false)
                    }
                    InterruptibleWait::InterruptedByUserInput => (
                        self.service
                            .wait_for_terminal(self.thread_id, &args.run_id, Duration::ZERO)
                            .await
                            .map_err(model_bounded_error)?,
                        true,
                    ),
                };
            let mut write_error = None;
            let write_requested = args.write_path.is_some();
            let written_result = if write_requested
                && workflow_result_tool::run_result_is_available(&outcome.snapshot)
            {
                let write_path = args.write_path.as_deref().expect("checked above");
                match async {
                    let verified = self
                        .service
                        .load_result(self.thread_id, &outcome.snapshot)
                        .await?;
                    let target = resolve_result_write_target(
                        &invocation.execution_environments(),
                        write_path,
                    )?;
                    write_workflow_result(
                        &target,
                        verified.serialized(),
                        &verified.artifact().sha256,
                    )
                    .await
                }
                .await
                {
                    Ok(write) => Some(write),
                    Err(error) => {
                        write_error = Some(error);
                        None
                    }
                }
            } else {
                if write_requested
                    && workflow_result_tool::workflow_result_is_available(outcome.snapshot.status)
                {
                    write_error =
                        Some("terminal workflow snapshot has no result artifact".to_string());
                }
                None
            };
            let result_data = if let Some(write) = written_result.as_ref() {
                WorkflowResultData::from_written_result(&outcome.snapshot, write)
            } else if let Some(error) = write_error.as_deref() {
                WorkflowResultData::from_write_error(&outcome.snapshot, error)
            } else if write_requested {
                WorkflowResultData::without_chunk(&outcome.snapshot, /*result_error*/ None)
            } else {
                read_wait_result_data(&self.service, self.thread_id, &outcome.snapshot).await
            };
            let mut output = WaitWorkflowOutput::from_outcome_with_result(
                outcome,
                timeout_ms,
                interrupted_by_user_input,
                result_data,
            )
            .map_err(|error| {
                model_bounded_error(format_args!(
                    "failed to serialize WaitWorkflow result: {error}"
                ))
            })?;
            bound_wait_workflow_output(&mut output)?;
            let value = model_bounded_json_value_with_limit(
                WAIT_WORKFLOW_TOOL_NAME,
                &output,
                WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES,
            )?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitWorkflowArgs {
    run_id: String,
    timeout_ms: Option<i64>,
    write_path: Option<String>,
}

fn parse_arguments(arguments: &str) -> Result<WaitWorkflowArgs, FunctionCallError> {
    serde_json::from_str(arguments).map_err(|error| {
        model_bounded_error(format_args!(
            "invalid {WAIT_WORKFLOW_TOOL_NAME} input: {error}"
        ))
    })
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct WaitWorkflowOutput {
    run_id: String,
    workflow_name: String,
    status: WorkflowTaskStatus,
    summary: String,
    error: Option<String>,
    failure_count: usize,
    usage: WorkflowUsage,
    completed_at: Option<i64>,
    timed_out: bool,
    interrupted_by_user_input: bool,
    timeout_ms: i64,
    recovery: WorkflowRecoveryStatus,
    #[serde(flatten)]
    result_data: WorkflowResultData,
}

impl WaitWorkflowOutput {
    #[cfg(test)]
    fn from_outcome(
        outcome: WorkflowWaitOutcome,
        timeout_ms: i64,
        interrupted_by_user_input: bool,
    ) -> serde_json::Result<Self> {
        let result_data =
            WorkflowResultData::without_chunk(&outcome.snapshot, /*result_error*/ None);
        Self::from_outcome_with_result(outcome, timeout_ms, interrupted_by_user_input, result_data)
    }

    /// Assembles the fixed wait response around already-resolved result metadata.
    ///
    /// The caller decides where result metadata came from (an inline head, a
    /// `writePath` write, or a read failure) so this builder stays total and the
    /// output shape never depends on argument combinations.
    pub(super) fn from_outcome_with_result(
        outcome: WorkflowWaitOutcome,
        timeout_ms: i64,
        interrupted_by_user_input: bool,
        result_data: WorkflowResultData,
    ) -> serde_json::Result<Self> {
        let snapshot = outcome.snapshot;
        let recovery = workflow_recovery_status(&snapshot);
        let text = WaitRunText::from_snapshot(&snapshot);
        let mut output = Self {
            run_id: snapshot.run_id,
            workflow_name: text.workflow_name,
            status: snapshot.status,
            summary: text.summary,
            error: text.error,
            failure_count: snapshot.failures.len(),
            usage: snapshot.usage,
            completed_at: snapshot.completed_at,
            timed_out: outcome.timed_out && !interrupted_by_user_input,
            interrupted_by_user_input,
            timeout_ms,
            recovery,
            result_data,
        };
        if serde_json::to_vec(&output)?.len() > WAIT_MODEL_CONTEXT_COMPACT_BYTES {
            // These three only ever shrink: `workflow_recovery_status` emits either the
            // five-item requirement list or nothing, so collapsing it to one requirement
            // cannot invent bytes, and both text fields truncate to a fixed stub budget.
            // They are spent in full even when result compaction has to be reverted
            // below, because reverting them too would hand back bytes the response
            // cannot afford.
            output.recovery.compact_for_wait();
            output.workflow_name = compact_wait_text(&output.workflow_name);
            output.summary = compact_wait_text(&output.summary);
            output.result_data.compact_for_wait_without_growing();
            // `error` is deliberately exempt from this step: a failed run can also
            // carry a partial result artifact, and shrinking the failure reason to a
            // stub would leave the model with a large result and no explanation.
            // `bound_wait_workflow_output` spends cheaper fields first and only
            // steps the error down if the response still exceeds the hard cap.
        }
        Ok(output)
    }
}

/// Shrinks a wait response until it fits the fixed model context item cap.
///
/// Builder-level compaction covers the common oversized case. This ladder is the
/// rescue for the combination that motivated keeping `error` readable — a failed run
/// that also carries a partial result artifact and observed restore
/// incompatibilities — where honest text alone can push past the cap and the whole
/// tool call would otherwise fail, costing the model even the run status.
///
/// Give-up order is least to most valuable: advisory restore metadata, the
/// re-obtainable result digest, the result read/write detail, and only then the run
/// error budget one notch at a time. Status, usage, failure count, and result
/// availability are never dropped.
///
/// The run error is last because it is the one field the model cannot recover
/// elsewhere, but the cap is fixed: a run that simultaneously fails, carries a
/// partial artifact, reports restore conflicts, and fails a `writePath` write can
/// still force it down to a stub. That is strictly better than the alternative,
/// which is a tool-level error that costs the model the run status as well.
fn bound_wait_workflow_output(output: &mut WaitWorkflowOutput) -> Result<(), FunctionCallError> {
    if wait_workflow_output_fits(output)? {
        return Ok(());
    }
    output.recovery.drop_observed_restore_incompatibilities();
    if wait_workflow_output_fits(output)? {
        return Ok(());
    }
    output.result_data.drop_digest_for_wait();
    if wait_workflow_output_fits(output)? {
        return Ok(());
    }
    output.result_data.stub_result_error_for_wait();
    if wait_workflow_output_fits(output)? {
        return Ok(());
    }
    for budget in WAIT_ERROR_BUDGET_LADDER {
        if let Some(error) = output.error.take() {
            output.error = Some(truncate_model_text(&error, budget));
        }
        if wait_workflow_output_fits(output)? {
            return Ok(());
        }
    }
    Err(focused_response_error(WAIT_WORKFLOW_TOOL_NAME))
}

fn wait_workflow_output_fits(output: &WaitWorkflowOutput) -> Result<bool, FunctionCallError> {
    let bytes = serde_json::to_vec(output).map_err(|error| {
        model_bounded_error(format_args!(
            "failed to measure {WAIT_WORKFLOW_TOOL_NAME} output: {error}"
        ))
    })?;
    Ok(bytes.len() <= WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES)
}

pub(crate) enum InterruptibleWait<T> {
    Completed(T),
    InterruptedByUserInput,
}

pub(crate) async fn race_with_turn_activity<F>(
    wait: F,
    activity: Option<Arc<dyn TurnActivitySubscription>>,
) -> InterruptibleWait<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    let Some(activity) = activity else {
        return InterruptibleWait::Completed(wait.await);
    };
    if matches!(activity.observed(), Some(TurnActivity::UserInput)) {
        return InterruptibleWait::InterruptedByUserInput;
    }

    tokio::pin!(wait);
    tokio::select! {
        biased;
        output = &mut wait => InterruptibleWait::Completed(output),
        observed = activity.wait() => match observed {
            Some(TurnActivity::UserInput) => InterruptibleWait::InterruptedByUserInput,
            None => InterruptibleWait::Completed(wait.await),
        },
    }
}

fn wait_workflow_tool_spec(_config: &Config) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "runId".to_string(),
            JsonSchema::string(Some(format!(
                "Workflow run id returned by the Workflow tool; 1..={MAX_WAIT_WORKFLOW_ID_BYTES} UTF-8 bytes."
            ))),
        ),
        (
            "timeoutMs".to_string(),
            JsonSchema::integer(Some(
                "Wait duration in milliseconds. Omit it to use the configured default; shorter values use the configured minimum."
                    .to_string(),
            )),
        ),
        (
            "writePath".to_string(),
            JsonSchema::string(Some(
                "Optional native path, relative to the primary selected execution environment cwd or absolute inside one of its workspace roots, where a terminal result should be written. The wait does not write before a result is available."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: WAIT_WORKFLOW_TOOL_NAME.to_string(),
        description: "Wait for one background workflow to reach a terminal status. The wait returns early for an already-terminal workflow and otherwise ends at completion, timeout, or new owning-turn user input. Repeated waits are safe. A focused terminal result is returned inline; use ReadWorkflowResult by runId when resultTruncated is true, or provide writePath to write and return only verified result metadata."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["runId".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "runId": { "type": "string" },
                "workflowName": { "type": "string" },
                "status": {
                    "enum": ["pending", "running", "completed", "failed", "paused", "killed"]
                },
                "summary": { "type": "string" },
                "error": { "type": ["string", "null"] },
                "failureCount": { "type": "integer", "minimum": 0 },
                "usage": {
                    "type": "object",
                    "properties": {
                        "totalTokens": { "type": "integer", "minimum": 0 },
                        "toolUses": { "type": "integer", "minimum": 0 },
                        "durationMs": { "type": "integer", "minimum": 0 },
                        "agentCount": { "type": "integer", "minimum": 0 },
                        "successfulAgentCount": { "type": "integer", "minimum": 0 },
                        "failedAgentCount": { "type": "integer", "minimum": 0 },
                        "skippedAgentCount": { "type": "integer", "minimum": 0 },
                        "nullAgentResultCount": { "type": "integer", "minimum": 0 }
                    },
                    "required": [
                        "totalTokens",
                        "toolUses",
                        "durationMs",
                        "agentCount",
                        "successfulAgentCount",
                        "failedAgentCount",
                        "skippedAgentCount",
                        "nullAgentResultCount"
                    ],
                    "additionalProperties": false
                },
                "completedAt": { "type": ["integer", "null"] },
                "timedOut": { "type": "boolean" },
                "interruptedByUserInput": { "type": "boolean" },
                "timeoutMs": { "type": "integer", "minimum": 0 },
                "recovery": {
                    "type": "object",
                    "description": "Recovery eligibility for this unfinished run. The resume target is this object's enclosing runId. A completed value is not presented as a recovery candidate even though Workflow may technically accept its runId for explicit replay.",
                    "properties": {
                        "recoveryEligible": { "type": "boolean" },
                        "reason": {
                            "enum": [
                                "pending",
                                "running",
                                "completed",
                                "paused",
                                "failed",
                                "killed"
                            ]
                        },
                        "mayRequireReapproval": { "type": "boolean" },
                        "identityRequirements": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Approved identity fields that must match for journal replay; the compact form summarizes them as sameApprovedWorkflowIdentity."
                        },
                        "observedRestoreIncompatibilities": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Identity fields named by restore errors already observed on this snapshot; absence does not guarantee a future resume will match. Cleared when an over-cap response had to shed it, so an empty list here does not by itself prove a clean resume."
                        }
                    },
                    "required": [
                        "recoveryEligible",
                        "reason",
                        "mayRequireReapproval",
                        "identityRequirements",
                        "observedRestoreIncompatibilities"
                    ],
                    "additionalProperties": false
                },
                "result": {},
                "resultAvailable": { "type": "boolean" },
                "resultInline": { "type": "boolean" },
                "resultTruncated": { "type": "boolean" },
                "resultPreview": { "type": ["string", "null"] },
                "resultBytes": { "type": ["integer", "null"], "minimum": 0 },
                "resultError": { "type": ["string", "null"] },
                "resultWritten": { "type": "boolean" },
                "resultWritePath": { "type": ["string", "null"] },
                "resultSha256": { "type": ["string", "null"] },
                "nextAction": { "type": ["string", "null"] }
            },
            "required": [
                "runId",
                "workflowName",
                "status",
                "summary",
                "error",
                "failureCount",
                "usage",
                "completedAt",
                "timedOut",
                "interruptedByUserInput",
                "timeoutMs",
                "recovery",
                "result",
                "resultAvailable",
                "resultInline",
                "resultTruncated",
                "resultPreview",
                "resultBytes",
                "resultError",
                "resultWritten",
                "resultWritePath",
                "resultSha256",
                "nextAction"
            ],
            "additionalProperties": false
        })),
    })
}

#[cfg(test)]
#[path = "wait_tool_tests.rs"]
mod tests;
