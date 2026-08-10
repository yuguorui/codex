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

use crate::service::WorkflowService;
use crate::service::WorkflowWaitOutcome;
use crate::workflow_result_tool;
use crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES;
use crate::workflow_result_tool::WorkflowResultData;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value_with_limit;

pub const WAIT_WORKFLOW_TOOL_NAME: &str = "WaitWorkflow";
const WAIT_WORKFLOW_NAME_MAX_BYTES: usize = 64;
const COMPACT_WAIT_TEXT_MAX_BYTES: usize = 32;

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

impl ToolExecutor<ToolCall> for WaitWorkflowToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        wait_workflow_tool_spec(&self.config)
    }

    fn availability(&self) -> ToolAvailability {
        ToolAvailability::RootSessionOnly
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args = parse_arguments(invocation.function_arguments()?)?;
            let min_timeout_ms = self.config.multi_agent_v2.min_wait_timeout_ms;
            let max_timeout_ms = self.config.multi_agent_v2.max_wait_timeout_ms;
            let default_timeout_ms = self.config.multi_agent_v2.default_wait_timeout_ms;
            let timeout_ms = match args.timeout_ms {
                Some(timeout_ms) if timeout_ms > max_timeout_ms => {
                    return Err(FunctionCallError::RespondToModel(
                        "choose timeoutMs within the configured wait window or omit it to use the server default"
                            .to_string(),
                    ));
                }
                Some(timeout_ms) => timeout_ms.max(min_timeout_ms),
                None => default_timeout_ms,
            };
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
            let (result_chunk, result_error) =
                if workflow_result_tool::workflow_result_is_available(outcome.snapshot.status) {
                    match self
                        .service
                        .read_result_chunk(
                            self.thread_id,
                            &outcome.snapshot,
                            /*offset*/ 0,
                            workflow_result_tool::RESULT_INLINE_MAX_BYTES,
                        )
                        .await
                    {
                        Ok(chunk) => (Some(chunk), None),
                        Err(error) => (None, Some(error)),
                    }
                } else {
                    (None, None)
                };
            let output = WaitWorkflowOutput::from_outcome_with_result_chunk(
                outcome,
                timeout_ms,
                interrupted_by_user_input,
                result_chunk.as_ref(),
                result_error.as_deref(),
            )
            .map_err(|error| {
                model_bounded_error(format_args!(
                    "failed to serialize WaitWorkflow result: {error}"
                ))
            })?;
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
        Self::from_outcome_with_result_chunk(
            outcome,
            timeout_ms,
            interrupted_by_user_input,
            /*result_chunk*/ None,
            /*result_error*/ None,
        )
    }

    pub(super) fn from_outcome_with_result_chunk(
        outcome: WorkflowWaitOutcome,
        timeout_ms: i64,
        interrupted_by_user_input: bool,
        result_chunk: Option<&crate::result_artifact::WorkflowResultChunk>,
        result_error: Option<&str>,
    ) -> serde_json::Result<Self> {
        let snapshot = outcome.snapshot;
        let result_data =
            WorkflowResultData::from_snapshot_with_result(&snapshot, result_chunk, result_error)?;
        let mut output = Self {
            run_id: snapshot.run_id,
            workflow_name: workflow_result_tool::truncate_model_text(
                &snapshot.workflow_name,
                WAIT_WORKFLOW_NAME_MAX_BYTES,
            ),
            status: snapshot.status,
            summary: workflow_result_tool::bounded_output_text(&snapshot.summary),
            error: snapshot
                .error
                .as_deref()
                .map(workflow_result_tool::bounded_output_text),
            failure_count: snapshot.failures.len(),
            usage: snapshot.usage,
            completed_at: snapshot.completed_at,
            timed_out: outcome.timed_out && !interrupted_by_user_input,
            interrupted_by_user_input,
            timeout_ms,
            result_data,
        };
        if serde_json::to_vec(&output)?.len() > WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES {
            output.result_data.compact_for_wait(&output.run_id);
            output.workflow_name = workflow_result_tool::truncate_model_text(
                &output.workflow_name,
                COMPACT_WAIT_TEXT_MAX_BYTES,
            );
            output.summary = workflow_result_tool::truncate_model_text(
                &output.summary,
                COMPACT_WAIT_TEXT_MAX_BYTES,
            );
            output.error = output.error.as_deref().map(|error| {
                workflow_result_tool::truncate_model_text(error, COMPACT_WAIT_TEXT_MAX_BYTES)
            });
        }
        Ok(output)
    }
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
            JsonSchema::string(Some(
                "Workflow run id returned by the Workflow tool.".to_string(),
            )),
        ),
        (
            "timeoutMs".to_string(),
            JsonSchema::integer(Some(
                "Wait duration in milliseconds. Omit it to use the configured default; shorter values use the configured minimum."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: WAIT_WORKFLOW_TOOL_NAME.to_string(),
        description: "Wait for one background workflow to reach a terminal status. The wait returns early for an already-terminal workflow and otherwise ends at completion, timeout, or new owning-turn user input. Repeated waits are safe. A focused terminal result is returned inline; use ReadWorkflowResult by runId when resultTruncated is true."
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
                        "agentCount": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["totalTokens", "toolUses", "durationMs", "agentCount"],
                    "additionalProperties": false
                },
                "completedAt": { "type": ["integer", "null"] },
                "timedOut": { "type": "boolean" },
                "interruptedByUserInput": { "type": "boolean" },
                "timeoutMs": { "type": "integer", "minimum": 0 },
                "result": {},
                "resultAvailable": { "type": "boolean" },
                "resultInline": { "type": "boolean" },
                "resultTruncated": { "type": "boolean" },
                "resultPreview": { "type": ["string", "null"] },
                "resultBytes": { "type": ["integer", "null"], "minimum": 0 },
                "resultError": { "type": ["string", "null"] },
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
                "result",
                "resultAvailable",
                "resultInline",
                "resultTruncated",
                "resultPreview",
                "resultBytes",
                "resultError",
                "nextAction"
            ],
            "additionalProperties": false
        })),
    })
}

#[cfg(test)]
#[path = "wait_tool_tests.rs"]
mod tests;
