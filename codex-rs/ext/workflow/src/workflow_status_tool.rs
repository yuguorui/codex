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
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::task::JoinSet;

use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;
use crate::service::WorkflowWaitOutcome;
use crate::wait_tool::InterruptibleWait;
use crate::wait_tool::race_with_turn_activity;
use crate::workflow_result_tool::MODEL_TOOL_OUTPUT_MAX_BYTES;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value;
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
            let statuses = args.statuses.clone().unwrap_or_default();
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
            let output = list_workflows_page_output(page).map_err(model_bounded_error)?;
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
        wait_workflows_tool_spec(&self.config)
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
            let output = match race_with_turn_activity(wait, invocation.turn_activity()).await {
                InterruptibleWait::Completed(output) => output?,
                InterruptibleWait::InterruptedByUserInput => {
                    let mut outcomes = Vec::with_capacity(interrupted_run_ids.len());
                    for run_id in interrupted_run_ids {
                        outcomes.push(
                            self.service
                                .wait_for_terminal(self.thread_id, &run_id, Duration::ZERO)
                                .await
                                .map_err(model_bounded_error)?,
                        );
                    }
                    wait_workflows_output(mode, outcomes, timeout_ms, true)
                }
            };
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
    workflows: Vec<WaitedWorkflowStatus>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WaitedWorkflowStatus {
    run_id: String,
    status: WorkflowTaskStatus,
    timed_out: bool,
    result_available: bool,
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
            result_available: workflow_result_is_available(snapshot.status),
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
    let statuses = args.statuses.unwrap_or_default();
    if statuses.len() > MAX_STATUS_FILTER_ITEMS {
        return Err(
            "use a focused status filter or omit it to include every workflow status".to_string(),
        );
    }
    let cursor = args.cursor.as_deref().map(decode_list_cursor).transpose()?;
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
        .and_then(|sequence| encode_list_cursor(sequence).ok());
    Ok(ListWorkflowsOutput {
        truncated: next_cursor.is_some(),
        workflows,
        total_matched,
        next_cursor,
    })
}

fn list_workflows_page_output(
    page: crate::service::WorkflowListPage,
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
            next_cursor: encode_list_cursor(*sequence).ok(),
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
        last_sequence.and_then(|sequence| encode_list_cursor(sequence).ok())
    } else {
        page.next_sequence
            .and_then(|sequence| encode_list_cursor(sequence).ok())
    };
    Ok(ListWorkflowsOutput {
        workflows,
        total_matched: page.total_matched,
        truncated: next_cursor.is_some(),
        next_cursor,
    })
}

fn encode_list_cursor(sequence: u64) -> Result<String, String> {
    serde_json::to_string(&WorkflowListCursor { sequence })
        .map_err(|error| format!("failed to encode workflow list cursor: {error}"))
}

fn decode_list_cursor(cursor: &str) -> Result<WorkflowListCursor, String> {
    serde_json::from_str(cursor).map_err(|_| "invalid workflow list cursor".to_string())
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

    if mode == WaitMode::Any
        && let Some(outcome) = outcomes.iter().find(|outcome| !outcome.timed_out)
    {
        return Ok(wait_workflows_output(
            mode,
            vec![outcome.clone()],
            timeout_ms,
            false,
        ));
    }
    if mode == WaitMode::All && outcomes.iter().all(|outcome| !outcome.timed_out) {
        return Ok(wait_workflows_output(mode, outcomes, timeout_ms, false));
    }

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
        let outcome = outcome.map_err(model_bounded_error)?;
        outcomes[index] = outcome.clone();
        if mode == WaitMode::Any && !outcome.timed_out {
            waits.abort_all();
            return Ok(wait_workflows_output(
                mode,
                vec![outcome],
                timeout_ms,
                false,
            ));
        }
    }

    Ok(wait_workflows_output(mode, outcomes, timeout_ms, false))
}

fn wait_workflows_output(
    mode: WaitMode,
    outcomes: Vec<WorkflowWaitOutcome>,
    timeout_ms: i64,
    interrupted_by_user_input: bool,
) -> WaitWorkflowsOutput {
    let condition_met = match mode {
        WaitMode::Any => outcomes.iter().any(|outcome| !outcome.timed_out),
        WaitMode::All => outcomes.iter().all(|outcome| !outcome.timed_out),
    };
    WaitWorkflowsOutput {
        mode,
        condition_met,
        timed_out: !condition_met && !interrupted_by_user_input,
        interrupted_by_user_input,
        timeout_ms,
        workflows: outcomes
            .into_iter()
            .map(|outcome| WaitedWorkflowStatus {
                run_id: outcome.snapshot.run_id,
                status: outcome.snapshot.status,
                timed_out: outcome.timed_out && !interrupted_by_user_input,
                result_available: workflow_result_is_available(outcome.snapshot.status),
            })
            .collect(),
    }
}

fn validate_run_ids(run_ids: &[String]) -> Result<(), String> {
    if run_ids.is_empty() || run_ids.len() > MAX_WORKFLOW_COLLECTION_ITEMS {
        return Err(
            "provide a focused, non-empty set of runIds; split larger sets across additional WaitWorkflows calls"
                .to_string(),
        );
    }
    if run_ids.iter().any(String::is_empty) {
        return Err("provide a workflow run id for every runIds entry".to_string());
    }
    let unique = run_ids.iter().collect::<BTreeSet<_>>();
    if unique.len() != run_ids.len() {
        return Err("provide each workflow run id once in runIds".to_string());
    }
    Ok(())
}

fn resolve_timeout_ms(
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
                "Continuation token returned by an earlier ListWorkflows call using the same filters."
                    .to_string(),
            )),
        ),
        (
            "statuses".to_string(),
            JsonSchema::array(
                workflow_status_schema(),
                Some("Optional focused status filter.".to_string()),
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
        output_schema: None,
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

fn wait_workflows_tool_spec(_config: &Config) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "runIds".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("A focused set of unique workflow run ids owned by this thread.".to_string()),
            ),
        ),
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("any"), json!("all")],
                Some("Wait for any one run or for all runs. Defaults to all.".to_string()),
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
        description: "Wait concurrently for any one or all of a focused set of workflow runs owned by this thread. The wait also returns on new owning-turn user input, and repeated waits are safe. Read an individual terminal result with WaitWorkflow or ReadWorkflowResult."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["runIds".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn workflow_status_schema() -> JsonSchema {
    JsonSchema::string_enum(
        vec![
            json!("pending"),
            json!("running"),
            json!("completed"),
            json!("failed"),
            json!("paused"),
            json!("killed"),
        ],
        None,
    )
}

#[cfg(test)]
#[path = "workflow_status_tool_tests.rs"]
mod tests;
