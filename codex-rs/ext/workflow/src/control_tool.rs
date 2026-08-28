use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolAvailability;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_protocol::ThreadId;
use codex_protocol::workflow::WorkflowAgentState;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;

use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value;
use crate::workflow_result_tool::truncate_model_text;

pub const STOP_WORKFLOW_TOOL_NAME: &str = "StopWorkflow";
pub const RETRY_WORKFLOW_AGENT_TOOL_NAME: &str = "RetryWorkflowAgent";
pub const SKIP_WORKFLOW_AGENT_TOOL_NAME: &str = "SkipWorkflowAgent";

const CONTROL_OUTPUT_TEXT_MAX_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkflowControlToolKind {
    Stop,
    RetryAgent,
    SkipAgent,
}

impl WorkflowControlToolKind {
    fn tool_name(self) -> &'static str {
        match self {
            Self::Stop => STOP_WORKFLOW_TOOL_NAME,
            Self::RetryAgent => RETRY_WORKFLOW_AGENT_TOOL_NAME,
            Self::SkipAgent => SKIP_WORKFLOW_AGENT_TOOL_NAME,
        }
    }

    fn action(self) -> WorkflowControlAction {
        match self {
            Self::Stop => WorkflowControlAction::Stop,
            Self::RetryAgent => WorkflowControlAction::RetryAgent,
            Self::SkipAgent => WorkflowControlAction::SkipAgent,
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkflowControlToolExecutor {
    thread_id: ThreadId,
    service: WorkflowService,
    kind: WorkflowControlToolKind,
}

impl WorkflowControlToolExecutor {
    pub(crate) fn stop(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self {
            thread_id,
            service,
            kind: WorkflowControlToolKind::Stop,
        }
    }

    pub(crate) fn retry_agent(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self {
            thread_id,
            service,
            kind: WorkflowControlToolKind::RetryAgent,
        }
    }

    pub(crate) fn skip_agent(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self {
            thread_id,
            service,
            kind: WorkflowControlToolKind::SkipAgent,
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for WorkflowControlToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.kind.tool_name())
    }

    fn spec(&self) -> ToolSpec {
        workflow_control_tool_spec(self.kind)
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
            let arguments = invocation.function_arguments()?;
            let (run_id, agent_index) = match self.kind {
                WorkflowControlToolKind::Stop => {
                    let args = parse_arguments::<StopWorkflowArgs>(self.kind, arguments)?;
                    (args.run_id, None)
                }
                WorkflowControlToolKind::RetryAgent => {
                    let args = parse_arguments::<ControlWorkflowAgentArgs>(self.kind, arguments)?;
                    (args.run_id, Some(args.agent_index))
                }
                WorkflowControlToolKind::SkipAgent => {
                    let args = parse_arguments::<ControlWorkflowAgentArgs>(self.kind, arguments)?;
                    (args.run_id, Some(args.agent_index))
                }
            };
            let fallback_snapshot = self
                .service
                .wait_for_terminal(self.thread_id, &run_id, std::time::Duration::ZERO)
                .await
                .map_err(model_bounded_error)?
                .snapshot;
            let terminal = matches!(
                fallback_snapshot.status,
                WorkflowTaskStatus::Completed
                    | WorkflowTaskStatus::Failed
                    | WorkflowTaskStatus::Paused
                    | WorkflowTaskStatus::Killed
            );
            let accepted = match (self.kind, agent_index) {
                (WorkflowControlToolKind::Stop, None) => {
                    self.service.stop(self.thread_id, &run_id).await
                }
                (WorkflowControlToolKind::RetryAgent, Some(_)) if terminal => Ok(false),
                (WorkflowControlToolKind::RetryAgent, Some(agent_index)) => {
                    self.service
                        .retry_agent(self.thread_id, &run_id, agent_index)
                        .await
                }
                (WorkflowControlToolKind::SkipAgent, Some(agent_index)) => {
                    self.service
                        .skip_agent(self.thread_id, &run_id, agent_index)
                        .await
                }
                (WorkflowControlToolKind::Stop, Some(_))
                | (WorkflowControlToolKind::RetryAgent, None)
                | (WorkflowControlToolKind::SkipAgent, None) => {
                    unreachable!("agent index presence is fixed by the tool kind")
                }
            }
            .map_err(model_bounded_error)?;
            let snapshot = self
                .service
                .wait_for_terminal(self.thread_id, &run_id, std::time::Duration::ZERO)
                .await
                .map_err(model_bounded_error)?
                .snapshot;
            let agent = match agent_index {
                Some(agent_index) => self
                    .service
                    .agent_progress(self.thread_id, &run_id, agent_index)
                    .await
                    .map_err(model_bounded_error)?,
                None => None,
            };
            let output = WorkflowControlOutput::new(snapshot, self.kind.action(), agent, accepted);
            let value = model_bounded_json_value(self.kind.tool_name(), &output)?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

fn parse_arguments<T>(
    kind: WorkflowControlToolKind,
    arguments: &str,
) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|error| {
        model_bounded_error(format_args!("invalid {} input: {error}", kind.tool_name()))
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StopWorkflowArgs {
    run_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlWorkflowAgentArgs {
    run_id: String,
    agent_index: usize,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum WorkflowControlAction {
    Stop,
    RetryAgent,
    SkipAgent,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentControlStatus {
    index: usize,
    state: WorkflowAgentState,
    awaiting_decision: bool,
    skipped: bool,
    attempt: u32,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowControlOutput {
    run_id: String,
    action: WorkflowControlAction,
    accepted: bool,
    status: WorkflowTaskStatus,
    summary: String,
    agent: Option<WorkflowAgentControlStatus>,
}

impl WorkflowControlOutput {
    fn new(
        snapshot: WorkflowTaskSnapshot,
        action: WorkflowControlAction,
        agent: Option<codex_protocol::workflow::WorkflowAgentProgress>,
        accepted: bool,
    ) -> Self {
        let agent = agent.map(|agent| WorkflowAgentControlStatus {
            index: agent.index,
            state: agent.state,
            awaiting_decision: agent.awaiting_decision,
            skipped: agent.skipped,
            attempt: agent.attempt,
            error: agent.error.as_deref().map(bounded_output_text),
        });
        Self {
            run_id: snapshot.run_id,
            action,
            accepted,
            status: snapshot.status,
            summary: bounded_output_text(&snapshot.summary),
            agent,
        }
    }
}

fn bounded_output_text(value: &str) -> String {
    truncate_model_text(value, CONTROL_OUTPUT_TEXT_MAX_BYTES)
}

fn workflow_control_tool_spec(kind: WorkflowControlToolKind) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "runId".to_string(),
        JsonSchema::string(Some(
            "Workflow run id returned by the Workflow tool.".to_string(),
        )),
    )]);
    let (description, required) = match kind {
        WorkflowControlToolKind::Stop => (
            "Request that one running workflow stop. A terminal workflow is left unchanged and returns accepted=false.",
            vec!["runId".to_string()],
        ),
        WorkflowControlToolKind::RetryAgent => {
            properties.insert(
                "agentIndex".to_string(),
                JsonSchema::integer(Some(
                    "Zero-based workflow agent index to retry. Settled agents rerun with their downstream work."
                        .to_string(),
                )),
            );
            (
                "Retry one workflow agent. The action returns accepted=false when the run has no controllable or recorded agent at that index.",
                vec!["runId".to_string(), "agentIndex".to_string()],
            )
        }
        WorkflowControlToolKind::SkipAgent => {
            properties.insert(
                "agentIndex".to_string(),
                JsonSchema::integer(Some(
                    "Zero-based index of the active workflow agent to skip.".to_string(),
                )),
            );
            (
                "Skip one active workflow agent. The action returns accepted=false when no active agent exists at that index.",
                vec!["runId".to_string(), "agentIndex".to_string()],
            )
        }
    };
    ToolSpec::Function(ResponsesApiTool {
        name: kind.tool_name().to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(required), Some(false.into())),
        output_schema: Some(control_output_schema()),
    })
}

fn control_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "runId": { "type": "string" },
            "action": { "enum": ["stop", "retryAgent", "skipAgent"] },
            "accepted": { "type": "boolean" },
            "status": {
                "enum": ["pending", "running", "completed", "failed", "paused", "killed"]
            },
            "summary": { "type": "string" },
            "agent": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "index": { "type": "integer", "minimum": 0 },
                            "state": { "enum": ["queued", "start", "done", "error"] },
                            "awaitingDecision": { "type": "boolean" },
                            "skipped": { "type": "boolean" },
                            "attempt": { "type": "integer", "minimum": 0 },
                            "error": { "type": ["string", "null"] }
                        },
                        "required": [
                            "index",
                            "state",
                            "awaitingDecision",
                            "skipped",
                            "attempt",
                            "error"
                        ],
                        "additionalProperties": false
                    },
                    { "type": "null" }
                ]
            }
        },
        "required": ["runId", "action", "accepted", "status", "summary", "agent"],
        "additionalProperties": false
    })
}

#[cfg(test)]
#[path = "control_tool_tests.rs"]
mod tests;
