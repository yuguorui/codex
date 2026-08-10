use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::ToolDefinition;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_runtime::InProcessCodeModeSession;
use codex_code_mode_runtime::StringCodeGeneration;
use codex_protocol::ToolName;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::MAX_WORKFLOW_AGENT_STALL_MS;
use crate::MAX_WORKFLOW_PROGRESS_TEXT_BYTES;
use crate::ValidatedWorkflowScript;
use crate::WorkflowAgentFailure;
use crate::WorkflowAgentFailureKind;
use crate::WorkflowAgentOptions;
use crate::WorkflowAgentProgress;
use crate::WorkflowAgentRequest;
use crate::WorkflowAgentResult;
use crate::WorkflowAgentState;
use crate::WorkflowEvent;
use crate::WorkflowExecutionError;
use crate::WorkflowProgressKind;
use crate::WorkflowRunOutcome;
use crate::script::WorkflowScriptContext;
use crate::script::compile_workflow_source_with_context;

mod delegate;

const AGENT_TOOL_NAME: &str = "workflow_agent";
const CHILD_TOOL_NAME: &str = "workflow_child";
const RESULT_TOOL_NAME: &str = "workflow_result";
const MAX_LOG_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_WORKFLOW_LOGS: usize = 4096;
const PROMPT_PREVIEW_BYTES: usize = 400;

pub type WorkflowAgentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkflowAgentResult, WorkflowAgentFailure>> + Send + 'a>>;
pub type WorkflowAgentStartedCallback<'a> = Box<dyn FnOnce(String) + Send + 'a>;
pub type WorkflowChildFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedWorkflowChild, String>> + Send + 'a>>;
pub type WorkflowJournalFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Executes one workflow agent call using host-owned agent infrastructure.
///
/// Implementations must observe `cancellation` and return promptly after it is
/// cancelled. Structured output validation belongs in the implementation when
/// `request.options.schema` is present.
pub trait WorkflowAgentRuntime: Send + Sync {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a>;

    fn run_agent_with_started<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        _on_started: WorkflowAgentStartedCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        self.run_agent(request, cancellation)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowChildRequest {
    pub name_or_ref: JsonValue,
    pub args: JsonValue,
}

#[derive(Clone, Debug)]
pub struct ResolvedWorkflowChild {
    pub script: ValidatedWorkflowScript,
    pub args: JsonValue,
}

/// Resolves saved workflows without coupling the runtime to host filesystem policy.
pub trait WorkflowChildResolver: Send + Sync {
    fn resolve_child<'a>(&'a self, request: WorkflowChildRequest) -> WorkflowChildFuture<'a>;
}

/// Stores deterministic agent results for replay when a workflow is resumed.
pub trait WorkflowJournal: Send + Sync {
    fn replay(&self, key: &str) -> Option<WorkflowAgentResult>;

    fn append_started(&self, key: String) -> WorkflowJournalFuture<'_>;

    fn append_result(&self, key: String, result: WorkflowAgentResult) -> WorkflowJournalFuture<'_>;
}

/// Receives ordered workflow progress snapshots as the runtime advances.
pub trait WorkflowEventSink: Send + Sync {
    fn emit(&self, event: WorkflowEvent);
}

impl<F> WorkflowEventSink for F
where
    F: Fn(WorkflowEvent) + Send + Sync,
{
    fn emit(&self, event: WorkflowEvent) {
        self(event);
    }
}

/// Live token accounting shared with the owning turn and sibling background work.
pub trait WorkflowBudget: Send + Sync {
    fn total(&self) -> u64;

    fn spent(&self) -> u64;
}

/// Selects either a workflow-local ceiling or a live budget shared with its owner.
#[derive(Clone)]
pub enum WorkflowBudgetSource {
    Fixed(u64),
    Shared(Arc<dyn WorkflowBudget>),
}

impl WorkflowBudgetSource {
    fn total(&self) -> u64 {
        match self {
            Self::Fixed(total) => *total,
            Self::Shared(budget) => budget.total(),
        }
    }

    fn spent(&self, workflow_spent: u64) -> u64 {
        match self {
            Self::Fixed(_) => workflow_spent,
            Self::Shared(budget) => budget.spent(),
        }
    }
}

#[derive(Clone)]
pub struct WorkflowRuntimeConfig {
    pub concurrency: usize,
    pub max_agents: usize,
    pub max_child_sessions: usize,
    pub max_agent_retries: u32,
    pub throttle_retry_delay: Duration,
    pub synchronous_timeout: Duration,
    pub budget: Option<WorkflowBudgetSource>,
    pub child_resolver: Option<Arc<dyn WorkflowChildResolver>>,
    pub journal: Option<Arc<dyn WorkflowJournal>>,
}

impl Default for WorkflowRuntimeConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        Self {
            concurrency: cores.saturating_sub(2).clamp(2, 16),
            max_agents: 1000,
            max_child_sessions: 16,
            max_agent_retries: 5,
            throttle_retry_delay: Duration::from_secs(45),
            synchronous_timeout: Duration::from_secs(30),
            budget: None,
            child_resolver: None,
            journal: None,
        }
    }
}

#[derive(Clone)]
pub struct WorkflowControl {
    state: Arc<ControlState>,
}

impl WorkflowControl {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ControlState::new()),
        }
    }

    pub fn stop(&self) {
        self.state.cancellation.cancel();
    }

    pub fn skip_agent(&self, index: usize) -> bool {
        self.state.control_agent(index, AgentAction::Skip)
    }

    pub fn retry_agent(&self, index: usize) -> bool {
        self.state.control_agent(index, AgentAction::Retry)
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancellation.is_cancelled()
    }
}

impl Default for WorkflowControl {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentAction {
    None = 0,
    Skip = 1,
    Retry = 2,
}

struct ActiveAgentControl {
    action: Arc<AtomicUsize>,
    cancellation: CancellationToken,
}

struct ControlState {
    cancellation: CancellationToken,
    agents: Mutex<HashMap<usize, ActiveAgentControl>>,
}

impl ControlState {
    fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            agents: Mutex::new(HashMap::new()),
        }
    }

    fn control_agent(&self, index: usize, action: AgentAction) -> bool {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(agent) = agents.get(&index) else {
            return false;
        };
        agent.action.store(action as usize, Ordering::Release);
        agent.cancellation.cancel();
        true
    }
}

pub async fn execute_workflow(
    script: &ValidatedWorkflowScript,
    args: JsonValue,
    agent_runtime: Arc<dyn WorkflowAgentRuntime>,
    event_sink: Arc<dyn WorkflowEventSink>,
    config: WorkflowRuntimeConfig,
    control: WorkflowControl,
) -> Result<WorkflowRunOutcome, WorkflowExecutionError> {
    let started = Instant::now();
    let result_tool_name = RESULT_TOOL_NAME.to_string();
    let token_budget = config.budget.as_ref().map(WorkflowBudgetSource::total);
    let initial_spent_tokens = config
        .budget
        .as_ref()
        .map_or(0, |budget| budget.spent(/*workflow_spent*/ 0));
    let source = compile_workflow_source_with_context(
        script,
        &args,
        token_budget,
        WorkflowScriptContext {
            result_tool_name: Some(result_tool_name.clone()),
            initial_spent_tokens,
            ..WorkflowScriptContext::default()
        },
    )
    .map_err(|error| WorkflowExecutionError::Runtime(error.to_string()))?;
    let delegate = Arc::new(WorkflowDelegate::new(
        agent_runtime,
        event_sink,
        config.clone(),
        Arc::clone(&control.state),
        workflow_cache_root(script),
    ));
    for (index, phase) in script.meta.phases.iter().enumerate() {
        delegate.emit(WorkflowEvent::WorkflowPhase {
            index,
            title: phase.title.clone(),
            kind: WorkflowProgressKind::Declared,
        });
    }
    let result = run_workflow_source(
        source,
        delegate.clone(),
        Arc::clone(&control.state),
        CancellationToken::new(),
        config.synchronous_timeout,
        result_tool_name,
        /*allow_child*/ true,
    )
    .await?;
    Ok(delegate.outcome(result.value, started.elapsed()))
}

struct WorkflowSourceResult {
    value: JsonValue,
    tokens: u64,
}

async fn run_workflow_source(
    source: String,
    delegate: Arc<WorkflowDelegate>,
    control: Arc<ControlState>,
    invocation_cancellation: CancellationToken,
    synchronous_timeout: Duration,
    result_tool_name: String,
    allow_child: bool,
) -> Result<WorkflowSourceResult, WorkflowExecutionError> {
    let session_delegate: Arc<dyn CodeModeSessionDelegate> =
        Arc::new(WorkflowDelegateHandle(delegate.clone()));
    let session = InProcessCodeModeSession::with_delegate_and_string_code_generation(
        session_delegate,
        StringCodeGeneration::Deny,
    );
    let started_cell = session
        .execute(ExecuteRequest {
            tool_call_id: "workflow".to_string(),
            enabled_tools: workflow_tools(&result_tool_name, allow_child),
            source,
            yield_time_ms: Some(50),
            max_output_tokens: Some(1),
        })
        .await
        .map_err(WorkflowExecutionError::Runtime)?;
    let cell_id = started_cell.cell_id.clone();
    let mut response = tokio::select! {
        response = tokio::time::timeout(synchronous_timeout, started_cell.initial_response()) => {
            match response {
                Ok(response) => response.map_err(WorkflowExecutionError::Runtime)?,
                Err(_) => {
                    let _ = session.terminate(cell_id.clone()).await;
                    let _ = session.shutdown().await;
                    return Err(WorkflowExecutionError::Runtime(format!(
                        "workflow synchronous execution exceeded {}ms",
                        synchronous_timeout.as_millis()
                    )));
                }
            }
        }
        _ = control.cancellation.cancelled() => {
            let _ = session.terminate(cell_id.clone()).await;
            let _ = session.shutdown().await;
            return Err(WorkflowExecutionError::Cancelled);
        }
        _ = invocation_cancellation.cancelled() => {
            let _ = session.terminate(cell_id.clone()).await;
            let _ = session.shutdown().await;
            return Err(WorkflowExecutionError::Cancelled);
        }
    };
    loop {
        match response {
            RuntimeResponse::Result { error_text, .. } => {
                session
                    .shutdown()
                    .await
                    .map_err(WorkflowExecutionError::Runtime)?;
                if let Some(error) = error_text {
                    return Err(WorkflowExecutionError::Runtime(error));
                }
                return delegate.take_result(&result_tool_name).ok_or_else(|| {
                    WorkflowExecutionError::Runtime(
                        "workflow completed without returning a result".to_string(),
                    )
                });
            }
            RuntimeResponse::Terminated { .. } => {
                let _ = session.shutdown().await;
                return Err(WorkflowExecutionError::Cancelled);
            }
            RuntimeResponse::Yielded { .. } => {
                response = tokio::select! {
                    _ = control.cancellation.cancelled() => {
                        session
                            .terminate(cell_id.clone())
                            .await
                            .map_err(WorkflowExecutionError::Runtime)?
                            .into()
                    }
                    _ = invocation_cancellation.cancelled() => {
                        session
                            .terminate(cell_id.clone())
                            .await
                            .map_err(WorkflowExecutionError::Runtime)?
                            .into()
                    }
                    waited = session.wait(WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: 100,
                    }) => waited.map_err(WorkflowExecutionError::Runtime)?.into(),
                };
            }
        }
    }
}

struct WorkflowDelegate {
    agent_runtime: Arc<dyn WorkflowAgentRuntime>,
    event_sink: Arc<dyn WorkflowEventSink>,
    config: WorkflowRuntimeConfig,
    control: Arc<ControlState>,
    semaphore: Arc<Semaphore>,
    invocation_state: Mutex<InvocationState>,
    total_tokens: AtomicU64,
    total_tool_calls: AtomicU64,
    final_results: Mutex<HashMap<String, WorkflowSourceResult>>,
    child_session_count: AtomicUsize,
    logs: Mutex<Vec<String>>,
    failures: Mutex<Vec<String>>,
}

#[derive(Default)]
struct InvocationState {
    agent_count: usize,
    previous_cache_key: String,
}

impl WorkflowDelegate {
    fn new(
        agent_runtime: Arc<dyn WorkflowAgentRuntime>,
        event_sink: Arc<dyn WorkflowEventSink>,
        config: WorkflowRuntimeConfig,
        control: Arc<ControlState>,
        cache_root: String,
    ) -> Self {
        Self {
            agent_runtime,
            event_sink,
            semaphore: Arc::new(Semaphore::new(config.concurrency)),
            config,
            control,
            invocation_state: Mutex::new(InvocationState {
                agent_count: 0,
                previous_cache_key: cache_root,
            }),
            total_tokens: AtomicU64::new(0),
            total_tool_calls: AtomicU64::new(0),
            final_results: Mutex::new(HashMap::new()),
            child_session_count: AtomicUsize::new(0),
            logs: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
        }
    }

    fn emit(&self, event: WorkflowEvent) {
        self.event_sink.emit(event);
    }

    fn outcome(&self, result: JsonValue, elapsed: Duration) -> WorkflowRunOutcome {
        WorkflowRunOutcome {
            result,
            agent_count: self
                .invocation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .agent_count,
            logs: self
                .logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            failures: self
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            total_tokens: self.total_tokens.load(Ordering::Acquire),
            total_tool_calls: self.total_tool_calls.load(Ordering::Acquire),
            duration_ms: duration_millis(elapsed),
        }
    }

    fn budget_total(&self) -> Option<u64> {
        self.config.budget.as_ref().map(WorkflowBudgetSource::total)
    }

    fn budget_spent(&self) -> u64 {
        let workflow_spent = self.total_tokens.load(Ordering::Acquire);
        self.config
            .budget
            .as_ref()
            .map_or(workflow_spent, |budget| budget.spent(workflow_spent))
    }

    fn remaining_budget(&self) -> Option<u64> {
        self.budget_total()
            .map(|total| total.saturating_sub(self.budget_spent()))
    }

    fn take_result(&self, result_tool_name: &str) -> Option<WorkflowSourceResult> {
        self.final_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(result_tool_name)
    }
}

struct WorkflowDelegateHandle(Arc<WorkflowDelegate>);

impl CodeModeSessionDelegate for WorkflowDelegateHandle {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        let delegate = Arc::clone(&self.0);
        Box::pin(async move {
            let tool_name = invocation.tool_name.name;
            match tool_name.as_str() {
                AGENT_TOOL_NAME => {
                    let input = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "workflow agent input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid workflow agent input: {error}"))?;
                    delegate.invoke_agent(input, cancellation_token).await
                }
                name if name == RESULT_TOOL_NAME || name.starts_with("workflow_result_") => {
                    let input: FinalResultInput = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "workflow result input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid workflow result: {error}"))?;
                    delegate
                        .final_results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            tool_name,
                            WorkflowSourceResult {
                                value: input.result,
                                tokens: input.tokens,
                            },
                        );
                    Ok(JsonValue::Null)
                }
                CHILD_TOOL_NAME => {
                    let input = serde_json::from_value(
                        invocation
                            .input
                            .ok_or_else(|| "child workflow input is missing".to_string())?,
                    )
                    .map_err(|error| format!("invalid child workflow input: {error}"))?;
                    delegate.invoke_child(input, cancellation_token).await
                }
                name => Err(format!("unknown workflow runtime tool `{name}`")),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        let delegate = Arc::clone(&self.0);
        Box::pin(async move {
            let event: WorkflowEvent = serde_json::from_str(&text)
                .map_err(|error| format!("invalid workflow notification: {error}"))?;
            match &event {
                WorkflowEvent::WorkflowPhase { title, .. } => {
                    ensure_progress_text_bound("workflow phase title", title)?;
                }
                WorkflowEvent::WorkflowAgent(agent) => {
                    ensure_progress_text_bound("workflow agent label", &agent.label)?;
                    if let Some(phase_title) = &agent.phase_title {
                        ensure_progress_text_bound("workflow phase title", phase_title)?;
                    }
                }
                WorkflowEvent::WorkflowLog { .. } => {}
            }
            if let WorkflowEvent::WorkflowLog { message } = event {
                delegate.record_log(message);
            } else {
                delegate.emit(event);
            }
            Ok(())
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentToolInput {
    index: usize,
    prompt: String,
    options: WorkflowAgentOptions,
    #[serde(rename = "phaseIndex")]
    phase_index: Option<usize>,
    #[serde(rename = "phaseTitle")]
    phase_title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalResultInput {
    result: JsonValue,
    tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ChildToolInput {
    name_or_ref: JsonValue,
    args: JsonValue,
    phase_index: Option<usize>,
    phase_title: Option<String>,
}

#[derive(Default)]
struct AgentEventDetails {
    queued_at: u64,
    started_at: Option<u64>,
    attempt: u32,
    agent_id: Option<String>,
    model: Option<String>,
    fallback_model: Option<String>,
    cached: bool,
    blocked: bool,
    skipped: bool,
    error: Option<String>,
    tokens: Option<u64>,
    tool_calls: Option<u64>,
    duration_ms: Option<u64>,
    result_preview: Option<String>,
    prompt_preview: String,
}

fn workflow_tools(result_tool_name: &str, allow_child: bool) -> Vec<ToolDefinition> {
    let mut names = vec![AGENT_TOOL_NAME.to_string(), result_tool_name.to_string()];
    if allow_child {
        names.push(CHILD_TOOL_NAME.to_string());
    }
    names
        .into_iter()
        .map(|name| ToolDefinition {
            tool_name: ToolName::plain(&name),
            name,
            description: String::new(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            output_schema: None,
        })
        .collect()
}

fn workflow_cache_key(previous_key: &str, prompt: &str, options: &WorkflowAgentOptions) -> String {
    let mut selected = BTreeMap::new();
    if let Some(schema) = options.schema.as_ref() {
        selected.insert("schema", canonical_json(schema));
    }
    if let Some(model) = options.model.as_ref() {
        selected.insert("model", JsonValue::String(model.clone()));
    }
    if let Some(effort) = options.effort {
        selected.insert(
            "effort",
            serde_json::to_value(effort).unwrap_or(JsonValue::Null),
        );
    }
    if let Some(isolation) = options.isolation {
        selected.insert(
            "isolation",
            serde_json::to_value(isolation).unwrap_or(JsonValue::Null),
        );
    }
    if let Some(agent_type) = options.agent_type.as_ref() {
        selected.insert("agentType", JsonValue::String(agent_type.clone()));
    }
    let canonical_options = serde_json::to_string(&selected).unwrap_or_else(|_| "{}".to_string());
    let mut digest = Sha256::new();
    digest.update(previous_key.as_bytes());
    digest.update([0]);
    digest.update(prompt.as_bytes());
    digest.update([0]);
    digest.update(canonical_options.as_bytes());
    format!("v3:{:x}", digest.finalize())
}

fn workflow_cache_root(script: &ValidatedWorkflowScript) -> String {
    script.body.clone()
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical_json).collect()),
        JsonValue::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_value(sorted).unwrap_or(JsonValue::Null)
        }
        value => value.clone(),
    }
}

fn agent_tool_result(value: JsonValue, tokens: u64, spent: u64) -> JsonValue {
    serde_json::json!({ "value": value, "tokens": tokens, "spent": spent })
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn preview_json(value: &JsonValue) -> String {
    truncate_utf8(&value.to_string(), PROMPT_PREVIEW_BYTES)
}

fn ensure_progress_text_bound(field: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_WORKFLOW_PROGRESS_TEXT_BYTES {
        Err(format!(
            "{field} exceeds the {MAX_WORKFLOW_PROGRESS_TEXT_BYTES}-byte limit"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
