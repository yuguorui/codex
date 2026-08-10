use codex_core::config::Config;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolAvailability;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_protocol::ThreadId;
use codex_protocol::workflow::WorkflowAgentProgress;
use codex_protocol::workflow::WorkflowAgentState;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::task::JoinSet;

use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;
use crate::service::WorkflowWaitOutcome;
use crate::wait_args::MAX_WAIT_WORKFLOW_ID_BYTES;
use crate::wait_args::MAX_WAIT_WORKFLOW_ITEMS;
use crate::wait_args::resolve_timeout_ms;
use crate::wait_args::validate_run_ids;
use crate::wait_text::WaitRunText;
use crate::wait_text::read_wait_result_data;
use crate::wait_tool::InterruptibleWait;
use crate::wait_tool::race_with_turn_activity;
use crate::workflow_recovery::WorkflowRecoverySummary;
use crate::workflow_recovery::workflow_recovery_status;
use crate::workflow_result_tool::MODEL_TOOL_OUTPUT_MAX_BYTES;
use crate::workflow_result_tool::WorkflowResultData;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value;
use crate::workflow_result_tool::serialized_output_len;
use crate::workflow_result_tool::truncate_model_text;
use crate::workflow_result_tool::workflow_result_is_available;

pub const LIST_WORKFLOWS_TOOL_NAME: &str = "ListWorkflows";
pub const LIST_WORKFLOW_AGENTS_TOOL_NAME: &str = "ListWorkflowAgents";
pub const WAIT_WORKFLOWS_TOOL_NAME: &str = "WaitWorkflows";
const DEFAULT_LIST_LIMIT: usize = 20;
const DEFAULT_AGENT_LIST_LIMIT: usize = 8;
const MAX_AGENT_LIST_LIMIT: usize = 16;
const MAX_WORKFLOW_COLLECTION_ITEMS: usize = 32;
const MAX_STATUS_FILTER_ITEMS: usize = 6;
const WORKFLOW_STATUSES: [WorkflowTaskStatus; 6] = [
    WorkflowTaskStatus::Pending,
    WorkflowTaskStatus::Running,
    WorkflowTaskStatus::Completed,
    WorkflowTaskStatus::Failed,
    WorkflowTaskStatus::Paused,
    WorkflowTaskStatus::Killed,
];
const STATUS_NAME_MAX_BYTES: usize = 96;
const STATUS_TITLE_MAX_BYTES: usize = 128;
const STATUS_TEXT_MAX_BYTES: usize = 160;

#[derive(Clone)]
pub(crate) struct ListWorkflowsToolExecutor {
    thread_id: ThreadId,
    service: WorkflowService,
}

impl ListWorkflowsToolExecutor {
    pub(crate) fn new(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self { thread_id, service }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ListWorkflowsToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LIST_WORKFLOWS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        list_workflows_tool_spec()
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
            let args: ListWorkflowsArgs =
                parse_arguments(LIST_WORKFLOWS_TOOL_NAME, invocation.function_arguments()?)?;
            let limit = args.limit.unwrap_or(DEFAULT_LIST_LIMIT);
            if limit == 0 || limit > MAX_WORKFLOW_COLLECTION_ITEMS {
                return Err(model_bounded_error(
                    "provide a positive limit or omit it to use the server-sized list page",
                ));
            }
            let statuses = canonical_statuses(&args.statuses.clone().unwrap_or_default());
            if statuses.len() > MAX_STATUS_FILTER_ITEMS {
                return Err(model_bounded_error(
                    "use a focused status filter or omit it to include every workflow status",
                ));
            }
            let cursor = args
                .cursor
                .as_deref()
                .map(decode_list_cursor)
                .transpose()
                .map_err(model_bounded_error)?;
            if let Some(cursor) = cursor.as_ref()
                && let Some(cursor_statuses) = &cursor.statuses
                && cursor_statuses != &statuses
            {
                return Err(model_bounded_error(
                    "list cursor belongs to a different statuses filter",
                ));
            }
            let page = self
                .service
                .list_page(
                    self.thread_id,
                    &statuses,
                    cursor.map(|cursor| cursor.sequence),
                    limit,
                )
                .await
                .map_err(model_bounded_error)?;
            let output =
                list_workflows_page_output(page, &statuses).map_err(model_bounded_error)?;
            let value = model_bounded_json_value(LIST_WORKFLOWS_TOOL_NAME, &output)?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Clone)]
pub(crate) struct ListWorkflowAgentsToolExecutor {
    thread_id: ThreadId,
    service: WorkflowService,
}

impl ListWorkflowAgentsToolExecutor {
    pub(crate) fn new(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self { thread_id, service }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ListWorkflowAgentsToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LIST_WORKFLOW_AGENTS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        list_workflow_agents_tool_spec()
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
            let args: ListWorkflowAgentsArgs = parse_arguments(
                LIST_WORKFLOW_AGENTS_TOOL_NAME,
                invocation.function_arguments()?,
            )?;
            let limit = args.limit.unwrap_or(DEFAULT_AGENT_LIST_LIMIT);
            if limit == 0 || limit > MAX_AGENT_LIST_LIMIT {
                return Err(model_bounded_error(
                    "provide a positive agent page size or omit it to use the server-sized page",
                ));
            }
            let page = self
                .service
                .progress_page(
                    self.thread_id,
                    &args.run_id,
                    args.start_index.unwrap_or(0),
                    limit,
                )
                .await
                .map_err(model_bounded_error)?;
            let output = ListWorkflowAgentsOutput {
                run_id: args.run_id,
                agents: page
                    .agents
                    .into_iter()
                    .map(WorkflowAgentStatus::from)
                    .collect(),
                total_agents: page.total_agents,
                next_index: page.next_index,
            };
            let value = model_bounded_json_value(LIST_WORKFLOW_AGENTS_TOOL_NAME, &output)?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Clone)]
pub(crate) struct WaitWorkflowsToolExecutor {
    thread_id: ThreadId,
    config: Config,
    service: WorkflowService,
}

impl WaitWorkflowsToolExecutor {
    pub(crate) fn new(thread_id: ThreadId, config: Config, service: WorkflowService) -> Self {
        Self {
            thread_id,
            config,
            service,
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for WaitWorkflowsToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_WORKFLOWS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        wait_workflows_tool_spec()
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
            let args: WaitWorkflowsArgs =
                parse_arguments(WAIT_WORKFLOWS_TOOL_NAME, invocation.function_arguments()?)?;
            validate_run_ids(&args.run_ids).map_err(model_bounded_error)?;
            let timeout_ms = resolve_timeout_ms(&self.config, args.timeout_ms)?;
            let timeout_duration =
                Duration::from_millis(u64::try_from(timeout_ms).map_err(|error| {
                    model_bounded_error(format_args!("invalid WaitWorkflows timeout: {error}"))
                })?);
            let mode = args.mode.unwrap_or_default();
            let interrupted_run_ids = args.run_ids.clone();
            let wait = wait_for_workflows(
                self.service.clone(),
                self.thread_id,
                args.run_ids,
                mode,
                timeout_duration,
                timeout_ms,
            );
            let mut output = match race_with_turn_activity(wait, invocation.turn_activity()).await {
                InterruptibleWait::Completed(output) => output?,
                InterruptibleWait::InterruptedByUserInput => {
                    let mut outcomes = Vec::with_capacity(interrupted_run_ids.len());
                    for run_id in &interrupted_run_ids {
                        outcomes.push(
                            self.service
                                .wait_for_terminal(self.thread_id, run_id, Duration::ZERO)
                                .await
                                .map_err(model_bounded_error)?,
                        );
                    }
                    wait_workflows_response(
                        &self.service,
                        self.thread_id,
                        mode,
                        &outcomes,
                        satisfied_index(mode, &outcomes),
                        timeout_ms,
                        /*interrupted_by_user_input*/ true,
                    )
                    .await
                }
            };
            bound_wait_workflows_output(&mut output);
            let value = model_bounded_json_value(WAIT_WORKFLOWS_TOOL_NAME, &output)?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListWorkflowsArgs {
    limit: Option<usize>,
    statuses: Option<Vec<WorkflowTaskStatus>>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListWorkflowAgentsArgs {
    run_id: String,
    start_index: Option<usize>,
    limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ListWorkflowAgentsOutput {
    run_id: String,
    agents: Vec<WorkflowAgentStatus>,
    total_agents: usize,
    next_index: Option<usize>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentStatus {
    invocation_id: String,
    index: usize,
    agent_id: Option<String>,
    label: String,
    phase_index: Option<usize>,
    phase_title: Option<String>,
    state: WorkflowAgentState,
    blocked: bool,
    skipped: bool,
    awaiting_decision: bool,
    cached: bool,
    attempt: u32,
    error: Option<String>,
    tokens: Option<u64>,
    tool_calls: Option<u64>,
    duration_ms: Option<u64>,
}

impl From<WorkflowAgentProgress> for WorkflowAgentStatus {
    fn from(agent: WorkflowAgentProgress) -> Self {
        Self {
            invocation_id: bounded_status_text(&agent.invocation_id),
            index: agent.index,
            agent_id: agent.agent_id,
            label: bounded_status_text(&agent.label),
            phase_index: agent.phase_index,
            phase_title: agent.phase_title.as_deref().map(bounded_status_text),
            state: agent.state,
            blocked: agent.blocked,
            skipped: agent.skipped,
            awaiting_decision: agent.awaiting_decision,
            cached: agent.cached,
            attempt: agent.attempt,
            error: agent.error.as_deref().map(bounded_status_text),
            tokens: agent.tokens,
            tool_calls: agent.tool_calls,
            duration_ms: agent.duration_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ListWorkflowsOutput {
    workflows: Vec<WorkflowStatusItem>,
    total_matched: usize,
    truncated: bool,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowListCursor {
    sequence: u64,
    #[serde(default)]
    statuses: Option<Vec<WorkflowTaskStatus>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitWorkflowsArgs {
    run_ids: Vec<String>,
    mode: Option<WaitMode>,
    timeout_ms: Option<i64>,
}

fn parse_arguments<T>(tool_name: &str, arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|error| model_bounded_error(format_args!("invalid {tool_name} input: {error}")))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum WaitMode {
    Any,
    #[default]
    All,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WaitWorkflowsOutput {
    mode: WaitMode,
    condition_met: bool,
    timed_out: bool,
    interrupted_by_user_input: bool,
    timeout_ms: i64,
    /// Status for every requested run, in request order, regardless of `mode`.
    workflows: Vec<WaitedWorkflowStatus>,
    /// The single run that ended a `mode: any` wait, with an inline result head.
    ///
    /// Null for `mode: all`, which has no single winner, and null when the response
    /// had to give it up to fit its size cap; read those results with `WaitWorkflow`
    /// or `ReadWorkflowResult`.
    winner: Option<WaitWinnerResult>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WaitedWorkflowStatus {
    run_id: String,
    status: WorkflowTaskStatus,
    timed_out: bool,
    result_available: bool,
    result_bytes: Option<u64>,
    result_sha256: Option<String>,
    recovery: Option<WorkflowRecoverySummary>,
}

/// Wait-level detail for the run that satisfied `mode: any`.
///
/// This is what keeps `WaitWorkflows` from forcing a second round trip on the
/// common fan-out/race pattern: the winner arrives with the same bounded result
/// head `WaitWorkflow` would have returned, while its siblings stay status-only.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WaitWinnerResult {
    run_id: String,
    workflow_name: String,
    status: WorkflowTaskStatus,
    summary: String,
    error: Option<String>,
    usage: WorkflowUsage,
    #[serde(flatten)]
    result_data: WorkflowResultData,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkflowStatusItem {
    run_id: String,
    workflow_name: String,
    title: Option<String>,
    status: WorkflowTaskStatus,
    summary: String,
    error: Option<String>,
    failure_count: usize,
    usage: WorkflowUsage,
    started_at: i64,
    completed_at: Option<i64>,
    result_available: bool,
    result_bytes: Option<u64>,
    result_sha256: Option<String>,
}

impl WorkflowStatusItem {
    fn from_snapshot(snapshot: &WorkflowTaskSnapshot) -> Self {
        Self {
            run_id: snapshot.run_id.clone(),
            workflow_name: truncate_model_text(&snapshot.workflow_name, STATUS_NAME_MAX_BYTES),
            title: snapshot
                .title
                .as_deref()
                .map(|title| truncate_model_text(title, STATUS_TITLE_MAX_BYTES)),
            status: snapshot.status,
            summary: bounded_status_text(&snapshot.summary),
            error: snapshot.error.as_deref().map(bounded_status_text),
            failure_count: snapshot.failures.len(),
            usage: snapshot.usage.clone(),
            started_at: snapshot.started_at,
            completed_at: snapshot.completed_at,
            result_available: workflow_result_is_available(snapshot.status)
                && snapshot.result_artifact.is_some(),
            result_bytes: snapshot
                .result_artifact
                .as_ref()
                .map(|artifact| artifact.bytes),
            result_sha256: snapshot
                .result_artifact
                .as_ref()
                .map(|artifact| artifact.sha256.clone()),
        }
    }
}

#[cfg(test)]
fn list_workflows_output(
    snapshots: Vec<WorkflowTaskSnapshot>,
    args: ListWorkflowsArgs,
) -> Result<ListWorkflowsOutput, String> {
    let limit = args.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_WORKFLOW_COLLECTION_ITEMS {
        return Err(
            "provide a positive limit or omit it to use the server-sized list page".to_string(),
        );
    }
    let statuses = canonical_statuses(&args.statuses.unwrap_or_default());
    if statuses.len() > MAX_STATUS_FILTER_ITEMS {
        return Err(
            "use a focused status filter or omit it to include every workflow status".to_string(),
        );
    }
    let cursor = args.cursor.as_deref().map(decode_list_cursor).transpose()?;
    if let Some(cursor) = cursor.as_ref()
        && let Some(cursor_statuses) = &cursor.statuses
        && cursor_statuses != &statuses
    {
        return Err("list cursor belongs to a different statuses filter".to_string());
    }
    let mut matching = snapshots
        .into_iter()
        .filter(|snapshot| statuses.is_empty() || statuses.contains(&snapshot.status))
        .collect::<Vec<_>>();
    matching.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then_with(|| right.run_id.cmp(&left.run_id))
    });
    let total_matched = matching.len();
    let start = cursor
        .map_or(matching.len(), |cursor| {
            usize::try_from(cursor.sequence).unwrap_or(matching.len())
        })
        .min(matching.len());
    let eligible = matching
        .iter()
        .skip(matching.len().saturating_sub(start))
        .take(limit)
        .collect::<Vec<_>>();
    let mut workflows = Vec::new();
    for snapshot in &eligible {
        workflows.push(WorkflowStatusItem::from_snapshot(snapshot));
        let next_cursor = encode_list_cursor(
            u64::try_from(start.saturating_sub(workflows.len()))
                .map_err(|error| error.to_string())?,
            &statuses,
        )?;
        let candidate = ListWorkflowsOutput {
            workflows: workflows.clone(),
            total_matched,
            truncated: true,
            next_cursor: Some(next_cursor),
        };
        let candidate_len = serde_json::to_vec(&candidate)
            .map_err(|error| format!("failed to measure ListWorkflows output: {error}"))?
            .len();
        if candidate_len > MODEL_TOOL_OUTPUT_MAX_BYTES {
            workflows.pop();
            break;
        }
    }
    let remaining = start.saturating_sub(workflows.len());
    let next_cursor = (remaining > 0)
        .then(|| u64::try_from(remaining).ok())
        .flatten()
        .and_then(|sequence| encode_list_cursor(sequence, &statuses).ok());
    Ok(ListWorkflowsOutput {
        truncated: next_cursor.is_some(),
        workflows,
        total_matched,
        next_cursor,
    })
}

fn list_workflows_page_output(
    page: crate::service::WorkflowListPage,
    statuses: &[WorkflowTaskStatus],
) -> Result<ListWorkflowsOutput, String> {
    let mut workflows = Vec::new();
    let mut size_truncated = false;
    let mut last_sequence = None;
    for (snapshot, sequence) in page.snapshots.iter().zip(&page.snapshot_sequences) {
        workflows.push(WorkflowStatusItem::from_snapshot(snapshot));
        last_sequence = Some(*sequence);
        let candidate = ListWorkflowsOutput {
            workflows: workflows.clone(),
            total_matched: page.total_matched,
            truncated: true,
            next_cursor: encode_list_cursor(*sequence, statuses).ok(),
        };
        if serde_json::to_vec(&candidate)
            .map_err(|error| format!("failed to measure ListWorkflows output: {error}"))?
            .len()
            > MODEL_TOOL_OUTPUT_MAX_BYTES
        {
            workflows.pop();
            last_sequence = workflows
                .len()
                .checked_sub(1)
                .and_then(|index| page.snapshot_sequences.get(index))
                .copied();
            size_truncated = true;
            break;
        }
    }
    let next_cursor = if size_truncated {
        last_sequence.and_then(|sequence| encode_list_cursor(sequence, statuses).ok())
    } else {
        page.next_sequence
            .and_then(|sequence| encode_list_cursor(sequence, statuses).ok())
    };
    Ok(ListWorkflowsOutput {
        workflows,
        total_matched: page.total_matched,
        truncated: next_cursor.is_some(),
        next_cursor,
    })
}

fn encode_list_cursor(sequence: u64, statuses: &[WorkflowTaskStatus]) -> Result<String, String> {
    serde_json::to_string(&WorkflowListCursor {
        sequence,
        statuses: Some(statuses.to_vec()),
    })
    .map_err(|error| format!("failed to encode workflow list cursor: {error}"))
}

fn decode_list_cursor(cursor: &str) -> Result<WorkflowListCursor, String> {
    serde_json::from_str(cursor).map_err(|_| "invalid workflow list cursor".to_string())
}

/// True when the requested wait condition holds for the observed outcomes.
fn condition_satisfied(mode: WaitMode, outcomes: &[WorkflowWaitOutcome]) -> bool {
    match mode {
        WaitMode::Any => outcomes.iter().any(|outcome| !outcome.timed_out),
        WaitMode::All => outcomes.iter().all(|outcome| !outcome.timed_out),
    }
}

/// Index of the run to report as the `mode: any` winner.
///
/// `mode: all` has no single winner, so this is `None` there. For `mode: any` the
/// caller tracks the run that actually ended the wait; this fallback picks the first
/// terminal run in request order, which is what an interrupted wait can still
/// determine without that history.
fn satisfied_index(mode: WaitMode, outcomes: &[WorkflowWaitOutcome]) -> Option<usize> {
    match mode {
        WaitMode::Any => outcomes.iter().position(|outcome| !outcome.timed_out),
        WaitMode::All => None,
    }
}

async fn wait_for_workflows(
    service: WorkflowService,
    thread_id: ThreadId,
    run_ids: Vec<String>,
    mode: WaitMode,
    timeout_duration: Duration,
    timeout_ms: i64,
) -> Result<WaitWorkflowsOutput, FunctionCallError> {
    let mut outcomes = Vec::with_capacity(run_ids.len());
    for run_id in &run_ids {
        outcomes.push(
            service
                .wait_for_terminal(thread_id, run_id, Duration::ZERO)
                .await
                .map_err(model_bounded_error)?,
        );
    }
    let mut winner_index = satisfied_index(mode, &outcomes);

    if !condition_satisfied(mode, &outcomes) {
        let mut waits = JoinSet::new();
        for (index, run_id) in run_ids.into_iter().enumerate() {
            if outcomes[index].timed_out {
                let service = service.clone();
                waits.spawn(async move {
                    (
                        index,
                        service
                            .wait_for_terminal(thread_id, &run_id, timeout_duration)
                            .await,
                    )
                });
            }
        }

        while let Some(joined) = waits.join_next().await {
            let (index, outcome) = joined.map_err(|error| {
                model_bounded_error(format_args!("WaitWorkflows task failed: {error}"))
            })?;
            outcomes[index] = outcome.map_err(model_bounded_error)?;
            if condition_satisfied(mode, &outcomes) {
                if mode == WaitMode::Any {
                    winner_index = Some(index);
                }
                waits.abort_all();
                break;
            }
        }

        // `mode: any` stops as soon as one run is terminal, so sibling statuses can
        // be stale by the time the wait returns. Refresh them cheaply instead of
        // reporting a run as still waiting when it already finished. A refresh
        // failure keeps the stale entry: the wait already succeeded, and losing it
        // over one unreadable sibling would be worse than a slightly dated status.
        if mode == WaitMode::Any && winner_index.is_some() {
            for outcome in &mut outcomes {
                if !outcome.timed_out {
                    continue;
                }
                let run_id = outcome.snapshot.run_id.clone();
                if let Ok(refreshed) = service
                    .wait_for_terminal(thread_id, &run_id, Duration::ZERO)
                    .await
                {
                    *outcome = refreshed;
                }
            }
        }
    }

    Ok(wait_workflows_response(
        &service,
        thread_id,
        mode,
        &outcomes,
        winner_index,
        timeout_ms,
        /*interrupted_by_user_input*/ false,
    )
    .await)
}

/// Assembles the wait response, attaching the `mode: any` winner detail.
///
/// Shared by the normal and the interrupted-by-user-input paths so both produce the
/// same shape from the same outcome set.
async fn wait_workflows_response(
    service: &WorkflowService,
    thread_id: ThreadId,
    mode: WaitMode,
    outcomes: &[WorkflowWaitOutcome],
    winner_index: Option<usize>,
    timeout_ms: i64,
    interrupted_by_user_input: bool,
) -> WaitWorkflowsOutput {
    let mut output = wait_workflows_output(mode, outcomes, timeout_ms, interrupted_by_user_input);
    output.winner = winner_result(service, thread_id, outcomes, winner_index).await;
    output
}

/// Builds the wait-level detail for the run that satisfied `mode: any`.
///
/// Siblings stay status-only so a full batch cannot multiply the inline result head;
/// the winner gets the same bounded identity text and result head `WaitWorkflow`
/// would return.
async fn winner_result(
    service: &WorkflowService,
    thread_id: ThreadId,
    outcomes: &[WorkflowWaitOutcome],
    winner_index: Option<usize>,
) -> Option<WaitWinnerResult> {
    let snapshot = &outcomes.get(winner_index?)?.snapshot;
    let text = WaitRunText::from_snapshot(snapshot);
    Some(WaitWinnerResult {
        run_id: snapshot.run_id.clone(),
        workflow_name: text.workflow_name,
        status: snapshot.status,
        summary: text.summary,
        error: text.error,
        usage: snapshot.usage.clone(),
        result_data: read_wait_result_data(service, thread_id, snapshot).await,
    })
}

fn wait_workflows_output(
    mode: WaitMode,
    outcomes: &[WorkflowWaitOutcome],
    timeout_ms: i64,
    interrupted_by_user_input: bool,
) -> WaitWorkflowsOutput {
    let condition_met = condition_satisfied(mode, outcomes);
    WaitWorkflowsOutput {
        mode,
        condition_met,
        timed_out: !condition_met && !interrupted_by_user_input,
        interrupted_by_user_input,
        timeout_ms,
        workflows: outcomes
            .iter()
            .map(|outcome| {
                let can_resume = matches!(
                    outcome.snapshot.status,
                    WorkflowTaskStatus::Paused
                        | WorkflowTaskStatus::Failed
                        | WorkflowTaskStatus::Killed
                );
                let recovery =
                    can_resume.then(|| workflow_recovery_status(&outcome.snapshot).into_summary());
                WaitedWorkflowStatus {
                    run_id: outcome.snapshot.run_id.clone(),
                    status: outcome.snapshot.status,
                    timed_out: outcome.timed_out && !interrupted_by_user_input,
                    result_available: workflow_result_is_available(outcome.snapshot.status)
                        && outcome.snapshot.result_artifact.is_some(),
                    result_bytes: outcome
                        .snapshot
                        .result_artifact
                        .as_ref()
                        .map(|artifact| artifact.bytes),
                    result_sha256: outcome
                        .snapshot
                        .result_artifact
                        .as_ref()
                        .map(|artifact| artifact.sha256.clone()),
                    recovery,
                }
            })
            .collect(),
        winner: None,
    }
}

/// Degrades a multi-run wait response until it fits the tool output cap.
///
/// Run statuses are the contract, so they are never dropped — but a batch of eight
/// failed runs can carry five observed restore incompatibilities each, which alone
/// is enough to push a real-`runId` batch past `MODEL_TOOL_OUTPUT_MAX_BYTES`. The
/// ladder therefore gives up, in order: the winner's inline result head, the advisory
/// per-entry restore detail, and finally the winner itself. Any `workflows` entry with
/// `timedOut: false` is a valid run to read next, so losing the winner costs a round
/// trip rather than the wait.
fn bound_wait_workflows_output(output: &mut WaitWorkflowsOutput) {
    if wait_workflows_output_fits(output) {
        return;
    }
    compact_winner_result_data(output);
    if wait_workflows_output_fits(output) {
        return;
    }
    for workflow in &mut output.workflows {
        if let Some(recovery) = workflow.recovery.as_mut() {
            recovery.drop_observed_restore_incompatibilities();
        }
    }
    if wait_workflows_output_fits(output) {
        return;
    }
    output.winner = None;
}

/// Compacts the winner's inline result head, but never at the cost of a larger item.
///
/// Dropping a small inline result while adding a longer `nextAction` would make the
/// response both bigger and less useful, so the uncompacted head is kept whenever
/// compaction does not actually reclaim bytes.
fn compact_winner_result_data(output: &mut WaitWorkflowsOutput) {
    let Some(winner) = output.winner.as_mut() else {
        return;
    };
    let uncompacted = winner.result_data.clone();
    winner.result_data.compact_for_wait();
    let grew = match (
        serialized_output_len(&winner.result_data),
        serialized_output_len(&uncompacted),
    ) {
        (Ok(compacted), Ok(original)) => compacted >= original,
        // Unmeasurable either way, so keep the compacted form and let the shared
        // output bound report the serialization failure.
        _ => false,
    };
    if grew {
        winner.result_data = uncompacted;
    }
}

fn wait_workflows_output_fits(output: &WaitWorkflowsOutput) -> bool {
    serialized_output_len(output).is_ok_and(|bytes| bytes <= MODEL_TOOL_OUTPUT_MAX_BYTES)
}

fn bounded_status_text(value: &str) -> String {
    truncate_model_text(value, STATUS_TEXT_MAX_BYTES)
}

fn list_workflows_tool_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "Number of newest matching runs to return. Omit it to use the server-sized list page."
                    .to_string(),
            )),
        ),
        (
            "cursor".to_string(),
            JsonSchema::string(Some(
                "Continuation token returned by an earlier ListWorkflows call using the same statuses filter; new cursors embed that filter and reject a mismatch."
                    .to_string(),
            )),
        ),
        (
            "statuses".to_string(),
            JsonSchema::array(
                workflow_status_schema(),
                Some("Optional focused status filter. Order and duplicates are ignored; explicitly listing every status is equivalent to omitting the filter.".to_string()),
            ),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: LIST_WORKFLOWS_TOOL_NAME.to_string(),
        description:
            "List focused status summaries for workflow runs owned by this thread, newest first."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: Some(list_workflows_output_schema()),
    })
}

fn list_workflows_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "workflows": {
                "type": "array",
                "maxItems": MAX_WORKFLOW_COLLECTION_ITEMS,
                "items": {
                    "type": "object",
                    "properties": {
                        "runId": { "type": "string" },
                        "workflowName": { "type": "string" },
                        "title": { "type": ["string", "null"] },
                        "status": {
                            "enum": [
                                "pending",
                                "running",
                                "completed",
                                "failed",
                                "paused",
                                "killed"
                            ]
                        },
                        "summary": { "type": "string" },
                        "error": { "type": ["string", "null"] },
                        "failureCount": { "type": "integer", "minimum": 0 },
                        "usage": usage_schema(),
                        "startedAt": { "type": "integer" },
                        "completedAt": { "type": ["integer", "null"] },
                        "resultAvailable": {
                            "type": "boolean",
                            "description": "True when a terminal snapshot carries a persisted result artifact descriptor verified at write time."
                        },
                        "resultBytes": {
                            "type": ["integer", "null"],
                            "minimum": 0,
                            "description": "Serialized result size from the persisted artifact descriptor."
                        },
                        "resultSha256": {
                            "type": ["string", "null"],
                            "description": "SHA-256 from the persisted artifact descriptor verified at write time."
                        }
                    },
                    "required": [
                        "runId",
                        "workflowName",
                        "title",
                        "status",
                        "summary",
                        "error",
                        "failureCount",
                        "usage",
                        "startedAt",
                        "completedAt",
                        "resultAvailable",
                        "resultBytes",
                        "resultSha256"
                    ],
                    "additionalProperties": false
                }
            },
            "totalMatched": { "type": "integer", "minimum": 0 },
            "truncated": { "type": "boolean" },
            "nextCursor": { "type": ["string", "null"] }
        },
        "required": ["workflows", "totalMatched", "truncated", "nextCursor"],
        "additionalProperties": false
    })
}

fn usage_schema() -> serde_json::Value {
    json!({
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
    })
}

fn list_workflow_agents_tool_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "runId".to_string(),
            JsonSchema::string(Some("Workflow run id owned by this thread.".to_string())),
        ),
        (
            "startIndex".to_string(),
            JsonSchema::integer(Some(
                "First stable agent index in this page. Defaults to zero.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "Number of consecutive agent indexes to inspect.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: LIST_WORKFLOW_AGENTS_TOOL_NAME.to_string(),
        description: "Page through the latest persisted agent states for one Workflow run. Continue from nextIndex. Use a completed entry's agentId with wait_agent when its intermediate detail is useful; stable indexes select Workflow agent control actions."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["runId".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn wait_workflows_tool_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "runIds".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(format!(
                    "A focused set of at most {MAX_WAIT_WORKFLOW_ITEMS} unique workflow run ids owned by this thread; each id is at most {MAX_WAIT_WORKFLOW_ID_BYTES} UTF-8 bytes."
                )),
            ),
        ),
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("any"), json!("all")],
                Some(
                    "Wait for any one run or for all runs. Defaults to all. Only mode any returns a winner with an inline result head."
                        .to_string(),
                ),
            ),
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
        name: WAIT_WORKFLOWS_TOOL_NAME.to_string(),
        description: "Wait concurrently for any one or all of a focused set of workflow runs owned by this thread. The wait also returns on new owning-turn user input, and repeated waits are safe. Every requested run is reported in workflows, in request order, with status only. When mode is any and a run ended the wait, winner names that run and carries its bounded result head; otherwise no result content is returned, so read an individual terminal result with WaitWorkflow or ReadWorkflowResult. winner is null for mode all and when it had to be dropped to fit the response size cap; any workflows entry with timedOut false is then a valid run to read next."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["runIds".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(wait_workflows_output_schema()),
    })
}

/// Schema for the flattened `WorkflowResultData` fields carried by a winner.
///
/// Mirrors the `WaitWorkflow` result fields so one artifact is described the same
/// way by both wait tools.
fn wait_winner_result_schema() -> serde_json::Value {
    json!({
        "type": ["object", "null"],
        "description": "The run that ended a mode any wait, with the same bounded result head WaitWorkflow returns. Null for mode all, when no run satisfied the condition, and when it had to be dropped to fit the response size cap.",
        "properties": {
            "runId": { "type": "string" },
            "workflowName": { "type": "string" },
            "status": {
                "enum": ["pending", "running", "completed", "failed", "paused", "killed"]
            },
            "summary": { "type": "string" },
            "error": {
                "type": ["string", "null"],
                "description": "Terminal run error. Never compacted, and bounded the same way as ListWorkflows."
            },
            "usage": usage_schema(),
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
            "usage",
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
    })
}

fn wait_workflows_output_schema() -> serde_json::Value {
    let mut schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["any", "all"] },
                "conditionMet": { "type": "boolean" },
                "timedOut": { "type": "boolean" },
                "interruptedByUserInput": { "type": "boolean" },
                "timeoutMs": { "type": "integer", "minimum": 0 },
                "workflows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "runId": { "type": "string" },
                            "status": {
                                "enum": [
                                    "pending",
                                    "running",
                                    "completed",
                                    "failed",
                                    "paused",
                                    "killed"
                                ]
                            },
                            "timedOut": { "type": "boolean" },
                            "resultAvailable": {
                                "type": "boolean",
                                "description": "True when a terminal snapshot carries a persisted result artifact descriptor verified at write time."
                            },
                            "resultBytes": {
                                "type": ["integer", "null"],
                                "minimum": 0,
                                "description": "Serialized result size from the persisted artifact descriptor."
                            },
                            "resultSha256": {
                                "type": ["string", "null"],
                                "description": "SHA-256 from the persisted artifact descriptor verified at write time."
                            },
                            "recovery": {
                                "type": ["object", "null"],
                                "description": "Present only for paused, failed, or killed unfinished-run recovery candidates. The resume target is this entry's runId.",
                                "properties": {
                                    "recoveryEligible": { "type": "boolean" },
                                    "reason": { "enum": ["paused", "failed", "killed"] },
                                    "mayRequireReapproval": { "type": "boolean" },
                                    "observedRestoreIncompatibilities": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "description": "Identity fields named by restore errors already observed on this snapshot; absence does not guarantee a future resume will match."
                                    }
                                },
                                "required": [
                                    "recoveryEligible",
                                    "reason",
                                    "mayRequireReapproval",
                                    "observedRestoreIncompatibilities"
                                ],
                                "additionalProperties": false
                            }
                        },
                        "required": [
                            "runId",
                            "status",
                            "timedOut",
                            "resultAvailable",
                            "resultBytes",
                            "resultSha256",
                            "recovery"
                        ],
                        "additionalProperties": false
                    },
                    "maxItems": MAX_WAIT_WORKFLOW_ITEMS
                }
            },
            "required": [
                "mode",
                "conditionMet",
                "timedOut",
                "interruptedByUserInput",
                "timeoutMs",
                "workflows",
                "winner"
            ],
            "additionalProperties": false
    });
    schema["properties"]["winner"] = wait_winner_result_schema();
    schema
}

fn canonical_statuses(statuses: &[WorkflowTaskStatus]) -> Vec<WorkflowTaskStatus> {
    let mut canonical = statuses.to_vec();
    canonical.sort_by_key(|status| {
        WORKFLOW_STATUSES
            .iter()
            .position(|candidate| candidate == status)
            .unwrap_or_else(|| WORKFLOW_STATUSES.len())
    });
    canonical.dedup();
    if canonical.len() == WORKFLOW_STATUSES.len() {
        canonical.clear();
    }
    canonical
}

fn workflow_status_schema() -> JsonSchema {
    JsonSchema::string_enum(
        WORKFLOW_STATUSES
            .iter()
            .map(|status| json!(status))
            .collect(),
        None,
    )
}

#[cfg(test)]
#[path = "workflow_status_tool_tests.rs"]
mod tests;
