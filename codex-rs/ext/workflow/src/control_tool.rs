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

use crate::service::WorkflowRetryImpact;
use crate::service::WorkflowRetryScope;
use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value;
use crate::workflow_result_tool::truncate_model_text;

pub const STOP_WORKFLOW_TOOL_NAME: &str = "StopWorkflow";
pub const RETRY_WORKFLOW_AGENT_TOOL_NAME: &str = "RetryWorkflowAgent";
pub const SKIP_WORKFLOW_AGENT_TOOL_NAME: &str = "SkipWorkflowAgent";

const CONTROL_OUTPUT_TEXT_MAX_BYTES: usize = 32;
const MAX_CONTROL_CANDIDATES: usize = 4;
const MAX_CONTROL_CANDIDATE_SCAN: usize = 256;

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
            let (run_id, agent_index, retry_dry_run) = match self.kind {
                WorkflowControlToolKind::Stop => {
                    let args = parse_arguments::<StopWorkflowArgs>(self.kind, arguments)?;
                    (args.run_id, None, false)
                }
                WorkflowControlToolKind::RetryAgent => {
                    let args = parse_arguments::<RetryWorkflowAgentArgs>(self.kind, arguments)?;
                    (args.run_id, Some(args.agent_index), args.dry_run)
                }
                WorkflowControlToolKind::SkipAgent => {
                    let args = parse_arguments::<ControlWorkflowAgentArgs>(self.kind, arguments)?;
                    (args.run_id, Some(args.agent_index), false)
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
            let target_before = match agent_index {
                Some(agent_index) => self
                    .service
                    .agent_progress(self.thread_id, &run_id, agent_index)
                    .await
                    .map_err(model_bounded_error)?,
                None => None,
            };
            let mut retry_impact = None;
            let accepted = match (self.kind, agent_index) {
                (WorkflowControlToolKind::Stop, None) => {
                    self.service.stop(self.thread_id, &run_id).await
                }
                (WorkflowControlToolKind::RetryAgent, Some(_)) if terminal => Ok(false),
                (WorkflowControlToolKind::RetryAgent, Some(agent_index)) => {
                    let outcome = if retry_dry_run {
                        self.service
                            .retry_agent_impact(self.thread_id, &run_id, agent_index)
                            .await
                    } else {
                        self.service
                            .retry_agent_with_impact(self.thread_id, &run_id, agent_index)
                            .await
                    }
                    .map_err(model_bounded_error)?;
                    retry_impact = outcome;
                    Ok(retry_impact.is_some())
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
            let final_terminal = workflow_status_is_terminal(snapshot.status);
            let control_open = self
                .service
                .control_is_open(self.thread_id, &run_id)
                .await
                .map_err(model_bounded_error)?;
            let candidates = if accepted || final_terminal {
                Vec::new()
            } else {
                self.control_candidates(&run_id).await?
            };
            let reason = (!accepted).then(|| {
                control_rejection_reason(
                    self.kind,
                    final_terminal,
                    control_open,
                    target_before.is_some(),
                )
            });
            let next_action = if final_terminal {
                None
            } else {
                rejection_next_action(self.kind, &candidates)
            };
            let output = WorkflowControlOutput::new(
                snapshot,
                self.kind.action(),
                agent,
                accepted,
                reason,
                candidates,
                next_action,
                retry_impact,
                retry_dry_run,
            );
            let value = model_bounded_json_value(self.kind.tool_name(), &output)?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

impl WorkflowControlToolExecutor {
    async fn control_candidates(
        &self,
        run_id: &str,
    ) -> Result<Vec<WorkflowAgentControlCandidate>, FunctionCallError> {
        let mut candidates = Vec::new();
        let mut offset = 0;
        let mut scanned = 0;
        while scanned < MAX_CONTROL_CANDIDATE_SCAN {
            let page = self
                .service
                .progress_page(
                    self.thread_id,
                    run_id,
                    offset,
                    MAX_CONTROL_CANDIDATE_SCAN - scanned,
                )
                .await
                .map_err(model_bounded_error)?;
            let next_index = page.next_index;
            for agent in page.agents {
                scanned += 1;
                let candidate = WorkflowAgentControlCandidate::new(agent);
                let actionable = match self.kind {
                    WorkflowControlToolKind::Stop => false,
                    WorkflowControlToolKind::RetryAgent => candidate.can_retry,
                    WorkflowControlToolKind::SkipAgent => candidate.can_skip,
                };
                if actionable
                    && candidates.len() < MAX_CONTROL_CANDIDATES
                    && !candidates
                        .iter()
                        .any(|existing: &WorkflowAgentControlCandidate| {
                            existing.index == candidate.index
                        })
                {
                    candidates.push(candidate);
                }
            }
            let Some(next_index) = next_index else {
                break;
            };
            offset = next_index;
        }
        Ok(candidates)
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetryWorkflowAgentArgs {
    run_id: String,
    agent_index: usize,
    #[serde(default)]
    dry_run: bool,
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

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum WorkflowControlRejectionReason {
    TerminalWorkflow,
    UnknownAgent,
    AgentNotControllable,
    AgentNotActive,
    ControlClosed,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentControlCandidate {
    index: usize,
    label: String,
    state: WorkflowAgentState,
    awaiting_decision: bool,
    skipped: bool,
    attempt: u32,
    error: Option<String>,
    can_retry: bool,
    can_skip: bool,
}

impl WorkflowAgentControlCandidate {
    fn new(agent: codex_protocol::workflow::WorkflowAgentProgress) -> Self {
        let active = matches!(
            agent.state,
            WorkflowAgentState::Queued | WorkflowAgentState::Start
        ) && !agent.skipped;
        let settled_retryable = matches!(
            agent.state,
            WorkflowAgentState::Done | WorkflowAgentState::Error
        ) && !agent.awaiting_decision;
        let skippable = (active || agent.awaiting_decision) && !agent.skipped;
        Self {
            index: agent.index,
            label: bounded_output_text(&agent.label),
            state: agent.state,
            awaiting_decision: agent.awaiting_decision,
            skipped: agent.skipped,
            attempt: agent.attempt,
            error: agent.error.as_deref().map(bounded_output_text),
            can_retry: active || settled_retryable || agent.awaiting_decision,
            can_skip: skippable,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowControlOutput {
    run_id: String,
    action: WorkflowControlAction,
    accepted: bool,
    status: WorkflowTaskStatus,
    summary: String,
    reason: Option<WorkflowControlRejectionReason>,
    agent: Option<WorkflowAgentControlStatus>,
    candidates: Vec<WorkflowAgentControlCandidate>,
    next_action: Option<String>,
    retry_impact: Option<WorkflowRetryImpactStatus>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum WorkflowRetryScopeStatus {
    ActiveAttempt,
    SettledPrefix,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowRetryImpactStatus {
    dry_run: bool,
    scope: WorkflowRetryScopeStatus,
    target_agent_index: usize,
    known_rerun_agent_count: usize,
    known_replay_agent_count: usize,
}

impl WorkflowRetryImpactStatus {
    fn new(impact: WorkflowRetryImpact, dry_run: bool) -> Self {
        let scope = match impact.scope {
            WorkflowRetryScope::ActiveAttempt => WorkflowRetryScopeStatus::ActiveAttempt,
            WorkflowRetryScope::SettledPrefix => WorkflowRetryScopeStatus::SettledPrefix,
        };
        Self {
            dry_run,
            scope,
            target_agent_index: impact.target_agent_index,
            known_rerun_agent_count: impact.known_rerun_agent_count,
            known_replay_agent_count: impact.known_replay_agent_count,
        }
    }
}

impl WorkflowControlOutput {
    fn new(
        snapshot: WorkflowTaskSnapshot,
        action: WorkflowControlAction,
        agent: Option<codex_protocol::workflow::WorkflowAgentProgress>,
        accepted: bool,
        reason: Option<WorkflowControlRejectionReason>,
        candidates: Vec<WorkflowAgentControlCandidate>,
        next_action: Option<String>,
        retry_impact: Option<WorkflowRetryImpact>,
        retry_dry_run: bool,
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
            reason,
            agent,
            candidates,
            next_action,
            retry_impact: retry_impact
                .map(|impact| WorkflowRetryImpactStatus::new(impact, retry_dry_run)),
        }
    }
}

fn control_rejection_reason(
    kind: WorkflowControlToolKind,
    terminal: bool,
    control_open: bool,
    target_recorded: bool,
) -> WorkflowControlRejectionReason {
    if terminal {
        return WorkflowControlRejectionReason::TerminalWorkflow;
    }
    if !control_open {
        return WorkflowControlRejectionReason::ControlClosed;
    }
    match kind {
        WorkflowControlToolKind::Stop => WorkflowControlRejectionReason::ControlClosed,
        WorkflowControlToolKind::RetryAgent => {
            if target_recorded {
                WorkflowControlRejectionReason::AgentNotControllable
            } else {
                WorkflowControlRejectionReason::UnknownAgent
            }
        }
        WorkflowControlToolKind::SkipAgent => {
            if target_recorded {
                WorkflowControlRejectionReason::AgentNotActive
            } else {
                WorkflowControlRejectionReason::UnknownAgent
            }
        }
    }
}

fn rejection_next_action(
    kind: WorkflowControlToolKind,
    candidates: &[WorkflowAgentControlCandidate],
) -> Option<String> {
    let candidate = candidates.iter().find(|candidate| match kind {
        WorkflowControlToolKind::Stop => false,
        WorkflowControlToolKind::RetryAgent => candidate.can_retry,
        WorkflowControlToolKind::SkipAgent => candidate.can_skip,
    })?;
    match kind {
        WorkflowControlToolKind::Stop => Some(
            "The workflow control lane is closed; wait for status before trying again.".to_string(),
        ),
        WorkflowControlToolKind::RetryAgent => Some(format!(
            "Use agentIndex={} with RetryWorkflowAgent; dryRun=true reports the blast radius first.",
            candidate.index
        )),
        WorkflowControlToolKind::SkipAgent => Some(format!(
            "Use agentIndex={} with SkipWorkflowAgent while that agent remains active.",
            candidate.index
        )),
    }
}

fn workflow_status_is_terminal(status: WorkflowTaskStatus) -> bool {
    matches!(
        status,
        WorkflowTaskStatus::Completed
            | WorkflowTaskStatus::Failed
            | WorkflowTaskStatus::Paused
            | WorkflowTaskStatus::Killed
    )
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
                    "Zero-based workflow agent index to retry. An active attempt retries alone; a settled agent reruns itself and every later recorded invocation."
                        .to_string(),
                )),
            );
            properties.insert(
                "dryRun".to_string(),
                JsonSchema::boolean(Some(
                    "Return the retry blast radius without changing the workflow. Omit it to execute the retry."
                        .to_string(),
                )),
            );
            (
                "Retry one workflow agent and report the known blast radius. The action returns accepted=false when the run has no controllable or recorded agent at that index.",
                vec!["runId".to_string(), "agentIndex".to_string()],
            )
        }
        WorkflowControlToolKind::SkipAgent => {
            properties.insert(
                "agentIndex".to_string(),
                JsonSchema::integer(Some(
                    "Zero-based index of an active or awaiting-decision workflow agent to skip."
                        .to_string(),
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
            "reason": {
                "type": ["string", "null"],
                "enum": [
                    "terminalWorkflow",
                    "unknownAgent",
                    "agentNotControllable",
                    "agentNotActive",
                    "controlClosed",
                    null
                ]
            },
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
            },
            "candidates": {
                "type": "array",
                "maxItems": 4,
                "items": {
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer", "minimum": 0 },
                        "label": { "type": "string" },
                        "state": { "enum": ["queued", "start", "done", "error"] },
                        "awaitingDecision": { "type": "boolean" },
                        "skipped": { "type": "boolean" },
                        "attempt": { "type": "integer", "minimum": 0 },
                        "error": { "type": ["string", "null"] },
                        "canRetry": { "type": "boolean" },
                        "canSkip": { "type": "boolean" }
                    },
                    "required": [
                        "index",
                        "label",
                        "state",
                        "awaitingDecision",
                        "skipped",
                        "attempt",
                        "error",
                        "canRetry",
                        "canSkip"
                    ],
                    "additionalProperties": false
                }
            },
            "nextAction": { "type": ["string", "null"] },
            "retryImpact": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "dryRun": { "type": "boolean" },
                            "scope": { "enum": ["activeAttempt", "settledPrefix"] },
                            "targetAgentIndex": { "type": "integer", "minimum": 0 },
                            "knownRerunAgentCount": { "type": "integer", "minimum": 1 },
                            "knownReplayAgentCount": { "type": "integer", "minimum": 0 }
                        },
                        "required": [
                            "dryRun",
                            "scope",
                            "targetAgentIndex",
                            "knownRerunAgentCount",
                            "knownReplayAgentCount"
                        ],
                        "additionalProperties": false
                    },
                    { "type": "null" }
                ]
            }
        },
        "required": [
            "runId",
            "action",
            "accepted",
            "status",
            "summary",
            "reason",
            "agent",
            "candidates",
            "nextAction",
            "retryImpact"
        ],
        "additionalProperties": false
    })
}

#[cfg(test)]
#[path = "control_tool_tests.rs"]
mod tests;
