use codex_agent_extension::AgentExecutionEnvironmentSnapshot;
use codex_agent_extension::AgentRunner;
use codex_core::ThreadManager;
use codex_core::WORKFLOW_NOTIFICATION_RESULT_CANDIDATE_MAX_BYTES;
use codex_core::WorkflowNotificationResult;
use codex_core::config::Config;
use codex_extension_api::ExtensionEventDelivery;
use codex_extension_api::ExtensionEventSink;
use codex_features::Feature;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::workflow::WorkflowAgentState;
use codex_protocol::workflow::WorkflowCompletedEvent;
use codex_protocol::workflow::WorkflowProgressEvent;
use codex_protocol::workflow::WorkflowProgressItem;
use codex_protocol::workflow::WorkflowStartedEvent;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_workflow::ValidatedWorkflowScript;
use codex_workflow::WorkflowControl;
use codex_workflow::WorkflowEvent;
use codex_workflow::WorkflowExecutionError;
use codex_workflow::WorkflowRunOutcome;
use codex_workflow::WorkflowRuntimeConfig;
use codex_workflow::WorkflowTokenUsage;
use codex_workflow::execute_workflow;
use codex_workflow::serialize_workflow_result;
use codex_workflow::validate_workflow_script;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::agent::CodexWorkflowAgentRuntime;
use crate::agent::WorkflowEnvironmentLocation;
use crate::agent::WorktreeCleanupMode;
use crate::composition::FrozenWorkflowComposition;
use crate::composition::PersistedWorkflowComposition;
use crate::composition::persist_workflow_composition;
use crate::composition::restore_workflow_composition;
use crate::discovery::ResolvedWorkflow;
use crate::input_artifacts::FileWorkflowInputArtifactStore;
use crate::journal::FileWorkflowJournal;
use crate::persistence::journal_path;
use crate::persistence::load_restore_snapshots;
use crate::persistence::load_snapshot;
use crate::persistence::load_snapshot_page;
use crate::persistence::parse_validated_snapshot;
use crate::persistence::snapshot_path;
use crate::persistence::workflow_session_dir;
use crate::persistence::write_indexed_snapshot;
use crate::result_artifact::VerifiedWorkflowResult;
use crate::result_artifact::WorkflowResultArtifact;
use crate::result_artifact::WorkflowResultChunk;
use crate::result_artifact::cleanup_result_artifacts;
use crate::result_artifact::load_verified_result_artifact;
use crate::result_artifact::persist_result_artifact;
use crate::result_artifact::read_verified_result_chunk;

mod replay_identity;
mod support;
use replay_identity::workspace_fingerprint;
use support::*;

pub(crate) const MAX_RETAINED_TERMINAL_TASKS: usize = 256;
const MAX_TRACKED_THREAD_HOMES: usize = 256;
const SNAPSHOT_PERSIST_INTERVAL: Duration = Duration::from_secs(2);
const LIFECYCLE_DELIVERY_ATTEMPTS: usize = 4;
const LIFECYCLE_DELIVERY_INITIAL_BACKOFF: Duration = Duration::from_millis(/*millis*/ 25);
const OWNING_MODEL_DELIVERY_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LIFECYCLE_BACKOFF_SHIFT: u32 = 10;
const WORKFLOW_PROGRESS_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLaunch {
    pub status: String,
    pub task_id: String,
    pub task_type: String,
    pub workflow_name: String,
    pub run_id: String,
    pub summary: String,
    pub transcript_dir: String,
    pub script_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskSnapshot {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub title: Option<String>,
    pub status: WorkflowTaskStatus,
    pub summary: String,
    pub transcript_dir: AbsolutePathBuf,
    pub script_path: AbsolutePathBuf,
    #[serde(default)]
    pub args: JsonValue,
    pub result_artifact: Option<WorkflowResultArtifact>,
    /// Canonical path to this serialized snapshot.
    pub output_file: AbsolutePathBuf,
    pub progress: Vec<WorkflowProgressItem>,
    pub progress_version: u64,
    pub usage: WorkflowUsage,
    pub failures: Vec<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub script_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum PersistedWorkflowEnvironmentLocation {
    Local,
    Remote,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "camelCase")]
enum PersistedEnvironmentConfigState {
    FromThread,
    Pending,
    Ready { identity: JsonValue },
    Failed { error: String },
}

impl From<&EnvironmentConfigState> for PersistedEnvironmentConfigState {
    fn from(config: &EnvironmentConfigState) -> Self {
        match config {
            EnvironmentConfigState::FromThread => Self::FromThread,
            EnvironmentConfigState::Pending => Self::Pending,
            EnvironmentConfigState::Ready(config) => Self::Ready {
                identity: json!({
                    "allowLoginShell": config.allow_login_shell,
                    "permissionProfile": config.permission_profile.permission_profile(),
                    "activePermissionProfile": config.permission_profile.active_permission_profile(),
                    "profileWorkspaceRoots": config.permission_profile.profile_workspace_roots(),
                    "selectedCapabilityRoots": config.selected_capability_roots,
                }),
            },
            EnvironmentConfigState::Failed(error) => Self::Failed {
                error: error.clone(),
            },
        }
    }
}

impl PersistedEnvironmentConfigState {
    fn name(&self) -> &'static str {
        match self {
            Self::FromThread => "fromThread",
            Self::Pending => "pending",
            Self::Ready { .. } => "ready",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PersistedTurnEnvironmentSelection {
    environment_id: String,
    cwd: PathUri,
    workspace_roots: Vec<PathUri>,
    config: PersistedEnvironmentConfigState,
}

impl From<&TurnEnvironmentSelection> for PersistedTurnEnvironmentSelection {
    fn from(selection: &TurnEnvironmentSelection) -> Self {
        Self {
            environment_id: selection.environment_id.clone(),
            cwd: selection.cwd.clone(),
            workspace_roots: selection.workspace_roots.clone(),
            config: PersistedEnvironmentConfigState::from(&selection.config),
        }
    }
}

impl PersistedTurnEnvironmentSelection {
    fn restore(&self) -> Result<TurnEnvironmentSelection, String> {
        if !matches!(&self.config, PersistedEnvironmentConfigState::FromThread) {
            return Err(format!(
                "captured workflow environment `{}` uses {} configuration that cannot be reconstructed after restoration",
                self.environment_id,
                self.config.name()
            ));
        }
        Ok(TurnEnvironmentSelection {
            environment_id: self.environment_id.clone(),
            cwd: self.cwd.clone(),
            workspace_roots: self.workspace_roots.clone(),
            config: EnvironmentConfigState::FromThread,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistedWorkflowExecutionContext {
    location: PersistedWorkflowEnvironmentLocation,
    selections: Vec<PersistedTurnEnvironmentSelection>,
    cwd: PathUri,
    workspace_roots: Vec<PathUri>,
    permission_workspace_roots: Vec<PathUri>,
    permission_identity: JsonValue,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    model_provider_id: String,
    model_provider_fingerprint: String,
    default_subagent_model: Option<String>,
    default_subagent_reasoning_effort: Option<ReasoningEffort>,
    agent_roles_fingerprint: Option<String>,
    approval_policy: AskForApproval,
    approvals_reviewer: ApprovalsReviewer,
    effective_config_fingerprint: String,
    workspace_fingerprint: Option<String>,
    execution_environment_fingerprint: Option<String>,
}

impl PersistedWorkflowExecutionContext {
    async fn capture(
        config: &Config,
        thread_id: ThreadId,
        location: WorkflowEnvironmentLocation,
        selections: &[TurnEnvironmentSelection],
    ) -> Self {
        let selections = if selections.is_empty() && location == WorkflowEnvironmentLocation::Local
        {
            vec![PersistedTurnEnvironmentSelection {
                environment_id: "local".to_string(),
                cwd: PathUri::from_abs_path(&config.cwd),
                workspace_roots: config
                    .permissions
                    .workspace_roots()
                    .iter()
                    .map(PathUri::from_abs_path)
                    .collect(),
                config: PersistedEnvironmentConfigState::FromThread,
            }]
        } else {
            selections.iter().map(Into::into).collect()
        };
        let location = PersistedWorkflowEnvironmentLocation::from(location);
        let workspace_fingerprint = workspace_fingerprint(
            location,
            &selections,
            workflow_session_dir(&config.codex_home, thread_id).to_path_buf(),
        )
        .await;
        let execution_environment_fingerprint = Some(json_fingerprint(json!({
            "location": location,
            "selections": selections,
        })));
        Self {
            location,
            selections,
            cwd: PathUri::from_abs_path(&config.cwd),
            workspace_roots: config
                .workspace_roots
                .iter()
                .map(PathUri::from_abs_path)
                .collect(),
            permission_workspace_roots: config
                .permissions
                .workspace_roots()
                .iter()
                .map(PathUri::from_abs_path)
                .collect(),
            permission_identity: json!({
                "permissionProfile": config.permissions.permission_profile(),
                "activePermissionProfile": config.permissions.active_permission_profile(),
                "profileWorkspaceRoots": config.permissions.profile_workspace_roots(),
                "allowLoginShell": config.permissions.allow_login_shell,
            }),
            model: config.model.clone(),
            reasoning_effort: config.model_reasoning_effort.clone(),
            service_tier: config.service_tier.clone(),
            model_provider_id: config.model_provider_id.clone(),
            model_provider_fingerprint: model_provider_fingerprint(config),
            default_subagent_model: config.agent_default_subagent_model.clone(),
            default_subagent_reasoning_effort: config
                .agent_default_subagent_reasoning_effort
                .clone(),
            agent_roles_fingerprint: agent_roles_fingerprint(config).await,
            approval_policy: config.permissions.approval_policy.value(),
            approvals_reviewer: config.approvals_reviewer,
            effective_config_fingerprint: effective_config_fingerprint(config),
            workspace_fingerprint,
            execution_environment_fingerprint,
        }
    }

    async fn restore_local_selections(
        &self,
        config: &Config,
        thread_id: ThreadId,
    ) -> Result<Vec<TurnEnvironmentSelection>, String> {
        if self.location == PersistedWorkflowEnvironmentLocation::Remote {
            return Err(
                "remote workflow execution environment is unavailable after restoration; resume explicitly with the Workflow tool to recapture the current environment"
                    .to_string(),
            );
        }
        if self.selections.is_empty() {
            return Err(
                "workflow execution environment metadata contains no captured selections; resume explicitly with the Workflow tool"
                    .to_string(),
            );
        }
        let selections = self
            .selections
            .iter()
            .map(PersistedTurnEnvironmentSelection::restore)
            .collect::<Result<Vec<_>, _>>()?;
        let primary = &selections[0];
        let current = Self::capture(
            config,
            thread_id,
            WorkflowEnvironmentLocation::Local,
            &selections,
        )
        .await;
        if self.cwd != current.cwd
            || self.workspace_roots != current.workspace_roots
            || self.permission_workspace_roots != current.permission_workspace_roots
            || self.permission_identity != current.permission_identity
            || primary.cwd != self.cwd
            || primary.workspace_roots != self.permission_workspace_roots
        {
            return Err(format!(
                "captured workflow execution context is incompatible with the restored thread: captured cwd {} with workspace roots {:?}, restored cwd {} with workspace roots {:?}",
                self.cwd,
                self.permission_workspace_roots,
                current.cwd,
                current.permission_workspace_roots
            ));
        }
        if !self.replay_identity_matches(&current) {
            return Err(
                "captured workflow execution identity changed; resume explicitly with the Workflow tool to use the current workspace and configuration"
                    .to_string(),
            );
        }
        Ok(selections)
    }

    fn replay_identity_matches(&self, current: &Self) -> bool {
        self.location == PersistedWorkflowEnvironmentLocation::Local
            && self.workspace_fingerprint.is_some()
            && self.agent_roles_fingerprint.is_some()
            && self == current
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoadedWorkflowMetadata {
    pub(crate) execution_context: PersistedWorkflowExecutionContext,
    pub(crate) composition: PersistedWorkflowComposition,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentWorkflowTaskSnapshot<'a> {
    #[serde(flatten)]
    snapshot: &'a WorkflowTaskSnapshot,
    execution_context: &'a PersistedWorkflowExecutionContext,
    composition: &'a PersistedWorkflowComposition,
}

impl From<WorkflowEnvironmentLocation> for PersistedWorkflowEnvironmentLocation {
    fn from(location: WorkflowEnvironmentLocation) -> Self {
        match location {
            WorkflowEnvironmentLocation::Local => Self::Local,
            WorkflowEnvironmentLocation::Remote => Self::Remote,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowWaitOutcome {
    pub(crate) snapshot: WorkflowTaskSnapshot,
    pub(crate) timed_out: bool,
}

pub(crate) struct WorkflowAgentProgressPage {
    pub(crate) agents: Vec<codex_protocol::workflow::WorkflowAgentProgress>,
    pub(crate) total_agents: usize,
    pub(crate) next_index: Option<usize>,
}

pub(crate) struct WorkflowListPage {
    pub(crate) snapshots: Vec<WorkflowTaskSnapshot>,
    pub(crate) snapshot_sequences: Vec<u64>,
    pub(crate) total_matched: usize,
    pub(crate) next_sequence: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowServiceError {
    #[error("workflow task was not found")]
    NotFound,
    #[error("workflow run belongs to a different thread")]
    WrongThread,
    #[error("workflow run is still running; stop it before resuming")]
    StillRunning,
    #[error("failed to persist workflow state: {0}")]
    Persistence(String),
}

struct WorkflowTask {
    snapshot: Mutex<WorkflowTaskSnapshot>,
    progress_state: Mutex<WorkflowProgressState>,
    usage_tracker: Mutex<WorkflowUsageTracker>,
    execution_context: PersistedWorkflowExecutionContext,
    composition: PersistedWorkflowComposition,
    verified_result: tokio::sync::Mutex<Option<VerifiedWorkflowResult>>,
    persist_lock: Semaphore,
    persist_state: Mutex<PersistState>,
    execution_transition: Mutex<()>,
    execution_generation: AtomicU64,
    keep_thread_resident: AtomicBool,
    control: WorkflowControl,
    status_tx: tokio::sync::watch::Sender<WorkflowTaskStatus>,
}

impl WorkflowTask {
    fn new(
        mut snapshot: WorkflowTaskSnapshot,
        execution_context: PersistedWorkflowExecutionContext,
        composition: PersistedWorkflowComposition,
    ) -> Self {
        let (status_tx, _) = tokio::sync::watch::channel(snapshot.status);
        let control = WorkflowControl::new();
        let usage_tracker = WorkflowUsageTracker::new(&snapshot.usage);
        let progress_state = WorkflowProgressState::from_snapshot(&snapshot);
        let execution_generation = progress_state.execution_generation();
        snapshot.progress = progress_state.latest_window();
        snapshot.usage.agent_count = progress_state.agent_count();
        let active = !workflow_status_is_terminal(snapshot.status);
        if !active {
            control.close();
        }
        Self {
            snapshot: Mutex::new(snapshot),
            progress_state: Mutex::new(progress_state),
            usage_tracker: Mutex::new(usage_tracker),
            execution_context,
            composition,
            verified_result: tokio::sync::Mutex::new(None),
            persist_lock: Semaphore::new(1),
            persist_state: Mutex::new(PersistState::default()),
            execution_transition: Mutex::new(()),
            execution_generation: AtomicU64::new(execution_generation),
            keep_thread_resident: AtomicBool::new(active),
            control,
            status_tx,
        }
    }

    async fn persist_snapshot(&self) -> Result<(), String> {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.persist_snapshot_value(&snapshot).await
    }

    async fn persist_snapshot_value(&self, snapshot: &WorkflowTaskSnapshot) -> Result<(), String> {
        write_current_snapshot(
            &snapshot.output_file,
            snapshot,
            &self.execution_context,
            &self.composition,
        )
        .await
    }

    async fn ensure_result_verified(
        &self,
        requested_snapshot: &WorkflowTaskSnapshot,
    ) -> Result<VerifiedWorkflowResult, String> {
        let artifact = requested_snapshot
            .result_artifact
            .as_ref()
            .ok_or_else(|| "terminal workflow snapshot has no result artifact".to_string())?;
        let mut verified = self.verified_result.lock().await;
        if verified.as_ref().map(VerifiedWorkflowResult::artifact) != Some(artifact) {
            *verified = Some(
                load_verified_result_artifact(&requested_snapshot.output_file, artifact).await?,
            );
        }
        verified
            .clone()
            .ok_or_else(|| "terminal workflow result artifact was not loaded".to_string())
    }
}

struct WorkflowUsageTracker {
    observed: WorkflowTokenUsage,
    invocations: HashMap<(u64, String), WorkflowTokenUsage>,
}

impl WorkflowUsageTracker {
    fn new(usage: &WorkflowUsage) -> Self {
        Self {
            observed: WorkflowTokenUsage {
                total_tokens: usage.total_tokens,
                tool_uses: usage.tool_uses,
            },
            invocations: HashMap::new(),
        }
    }

    fn record(
        &mut self,
        execution_generation: u64,
        event: &WorkflowEvent,
        previous_agent: Option<&codex_protocol::workflow::WorkflowAgentProgress>,
        usage: &mut WorkflowUsage,
    ) {
        if let WorkflowEvent::WorkflowAgent(agent) = event
            && !agent.cached
        {
            let previous = self
                .invocations
                .entry((execution_generation, agent.invocation_id.clone()))
                .or_insert_with(|| WorkflowTokenUsage {
                    total_tokens: previous_agent
                        .and_then(|previous| previous.tokens)
                        .unwrap_or_default(),
                    tool_uses: previous_agent
                        .and_then(|previous| previous.tool_calls)
                        .unwrap_or_default(),
                });
            let current_tokens = agent.tokens.unwrap_or_default();
            let current_tool_uses = agent.tool_calls.unwrap_or_default();
            self.observed.total_tokens = self
                .observed
                .total_tokens
                .saturating_add(current_tokens.saturating_sub(previous.total_tokens));
            self.observed.tool_uses = self
                .observed
                .tool_uses
                .saturating_add(current_tool_uses.saturating_sub(previous.tool_uses));
            previous.total_tokens = previous.total_tokens.max(current_tokens);
            previous.tool_uses = previous.tool_uses.max(current_tool_uses);
        }
        usage.total_tokens = self.observed.total_tokens;
        usage.tool_uses = self.observed.tool_uses;
    }
}

#[derive(Default)]
struct PersistState {
    running: bool,
    dirty: bool,
    terminal: bool,
}

struct WorkflowTaskStart {
    task: Arc<WorkflowTask>,
    thread_id: ThreadId,
    config: Config,
    script: ValidatedWorkflowScript,
    args: JsonValue,
    agent_runner: AgentRunner,
    journal: Arc<FileWorkflowJournal>,
    input_artifact_store: Arc<FileWorkflowInputArtifactStore>,
    composition: FrozenWorkflowComposition,
    environments: Option<Vec<TurnEnvironmentSelection>>,
    captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
    environment_location: WorkflowEnvironmentLocation,
}

struct QueuedWorkflowEventSink {
    sender: mpsc::Sender<(u64, WorkflowEvent)>,
}

impl codex_workflow::WorkflowEventSink for QueuedWorkflowEventSink {
    fn emit(
        &self,
        execution_generation: u64,
        event: WorkflowEvent,
    ) -> codex_workflow::WorkflowEventSinkFuture<'_> {
        let sender = self.sender.clone();
        Box::pin(async move {
            let _ = sender.send((execution_generation, event)).await;
        })
    }
}

fn start_workflow_progress_worker(
    service: WorkflowService,
    task: Arc<WorkflowTask>,
    thread_id: ThreadId,
    execution_generation_base: u64,
) -> (
    mpsc::Sender<(u64, WorkflowEvent)>,
    Arc<dyn codex_workflow::WorkflowEventSink>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, mut receiver) =
        mpsc::channel::<(u64, WorkflowEvent)>(WORKFLOW_PROGRESS_QUEUE_CAPACITY);
    let worker = tokio::spawn(async move {
        while let Some((execution_generation, event)) = receiver.recv().await {
            let task = Arc::clone(&task);
            let service = service.clone();
            if tokio::task::spawn_blocking(move || {
                service.record_progress(
                    &task,
                    thread_id,
                    execution_generation_base.saturating_add(execution_generation),
                    event,
                );
            })
            .await
            .is_err()
            {
                tracing::warn!("workflow progress worker stopped unexpectedly");
                break;
            }
        }
    });
    let sink: Arc<dyn codex_workflow::WorkflowEventSink> = Arc::new(QueuedWorkflowEventSink {
        sender: sender.clone(),
    });
    (sender, sink, worker)
}

pub(crate) struct WorkflowLaunchRequest {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: String,
    pub(crate) config: Config,
    pub(crate) resolved: ResolvedWorkflow,
    pub(crate) agent_runner: AgentRunner,
    pub(crate) environments: Vec<TurnEnvironmentSelection>,
    pub(crate) captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
    pub(crate) environment_location: WorkflowEnvironmentLocation,
}

struct WorkflowResumeState {
    snapshot: WorkflowTaskSnapshot,
    execution_context: PersistedWorkflowExecutionContext,
    composition: PersistedWorkflowComposition,
}

struct WorkflowResumeReservation {
    cache: Arc<Mutex<WorkflowTaskCache>>,
    run_id: String,
    lock_file: File,
    snapshot_path: AbsolutePathBuf,
    expected_snapshot_sha256: String,
    state: WorkflowResumeState,
    committed: bool,
}

impl WorkflowResumeReservation {
    async fn commit(
        mut self,
        task: Arc<WorkflowTask>,
        snapshot: &WorkflowTaskSnapshot,
    ) -> Result<(), WorkflowServiceError> {
        let snapshot_path = self.snapshot_path.clone();
        let expected_snapshot_sha256 = self.expected_snapshot_sha256.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&snapshot_path).map_err(persistence_error)?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != expected_snapshot_sha256 {
                return Err(WorkflowServiceError::StillRunning);
            }
            Ok(())
        })
        .await
        .map_err(|error| WorkflowServiceError::Persistence(error.to_string()))??;
        task.persist_snapshot_value(snapshot)
            .await
            .map_err(WorkflowServiceError::Persistence)?;
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(self.run_id.clone(), task);
        self.committed = true;
        Ok(())
    }
}

impl Drop for WorkflowResumeReservation {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

#[derive(Clone)]
pub struct WorkflowService {
    cache: Arc<Mutex<WorkflowTaskCache>>,
    thread_codex_homes: Arc<Mutex<HashMap<String, AbsolutePathBuf>>>,
    thread_codex_home_order: Arc<Mutex<VecDeque<String>>>,
    delivery_retry_worker_running: Arc<Mutex<bool>>,
    event_sink: Arc<dyn ExtensionEventSink>,
    thread_manager: Weak<ThreadManager>,
}

#[derive(Default)]
struct WorkflowTaskCache {
    tasks: HashMap<WorkflowTaskKey, Arc<WorkflowTask>>,
    terminal_lru: VecDeque<WorkflowTaskKey>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct WorkflowTaskKey {
    thread_id: String,
    run_id: String,
}

impl WorkflowTaskKey {
    fn new(thread_id: ThreadId, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            run_id: run_id.into(),
        }
    }

    fn from_task(run_id: String, task: &WorkflowTask) -> Self {
        let thread_id = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_id
            .clone();
        Self { thread_id, run_id }
    }
}

impl WorkflowTaskCache {
    fn insert(&mut self, run_id: String, task: Arc<WorkflowTask>) {
        let key = WorkflowTaskKey::from_task(run_id, &task);
        self.tasks.insert(key.clone(), task);
        self.touch_terminal(&key);
        self.prune_terminal_tasks();
    }

    fn get(&mut self, key: &WorkflowTaskKey) -> Option<Arc<WorkflowTask>> {
        let task = self.tasks.get(key).cloned()?;
        self.touch_terminal(key);
        Some(task)
    }

    fn touch_terminal(&mut self, key: &WorkflowTaskKey) {
        self.terminal_lru.retain(|cached| cached != key);
        let terminal = self.tasks.get(key).is_some_and(|task| {
            workflow_status_is_terminal(
                task.snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .status,
            )
        });
        if terminal {
            self.terminal_lru.push_back(key.clone());
        }
    }

    fn prune_terminal_tasks(&mut self) {
        while self.terminal_lru.len() > MAX_RETAINED_TERMINAL_TASKS {
            let Some(key) = self.terminal_lru.pop_front() else {
                break;
            };
            self.tasks.remove(&key);
        }
    }

    fn order_restored_terminals_by_recency(&mut self) {
        let mut terminals = self
            .tasks
            .iter()
            .filter_map(|(key, task)| {
                let snapshot = task
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                workflow_status_is_terminal(snapshot.status)
                    .then(|| (snapshot.started_at, key.clone()))
            })
            .collect::<Vec<_>>();
        terminals.sort();
        self.terminal_lru = terminals.into_iter().map(|(_, key)| key).collect();
        self.prune_terminal_tasks();
    }
}

impl WorkflowService {
    pub fn new(
        event_sink: Arc<dyn ExtensionEventSink>,
        thread_manager: Weak<ThreadManager>,
    ) -> Self {
        Self {
            cache: Arc::new(Mutex::new(WorkflowTaskCache::default())),
            thread_codex_homes: Arc::new(Mutex::new(HashMap::new())),
            thread_codex_home_order: Arc::new(Mutex::new(VecDeque::new())),
            delivery_retry_worker_running: Arc::new(Mutex::new(false)),
            event_sink,
            thread_manager,
        }
    }

    pub(crate) async fn restore_thread(
        &self,
        thread_id: ThreadId,
        config: Config,
        agent_runner: AgentRunner,
    ) -> Result<(), WorkflowServiceError> {
        self.register_thread_codex_home(thread_id, &config.codex_home);
        let restored =
            load_restore_snapshots(&config.codex_home, thread_id, MAX_RETAINED_TERMINAL_TASKS)
                .await
                .map_err(WorkflowServiceError::Persistence)?;
        if let Err(error) = cleanup_result_artifacts(
            &workflow_session_dir(&config.codex_home, thread_id).join("workflows"),
            restored.referenced_results,
        )
        .await
        {
            tracing::warn!(%error, "failed to clean stale workflow result artifacts");
        }
        for loaded in restored.loaded {
            let mut snapshot = loaded.snapshot;
            if snapshot.thread_id != thread_id.to_string()
                || self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .tasks
                    .contains_key(&WorkflowTaskKey::new(thread_id, snapshot.run_id.clone()))
            {
                continue;
            }
            let active = matches!(
                snapshot.status,
                WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
            );
            let loaded_metadata = loaded.metadata;
            let task_execution_context = loaded_metadata.execution_context.clone();
            let task_composition = loaded_metadata.composition.clone();
            let mut restored_environments = None;
            let execution_context_available = if active {
                match loaded_metadata
                    .execution_context
                    .restore_local_selections(&config, thread_id)
                    .await
                {
                    Ok(environments) => {
                        restored_environments = Some(environments);
                        true
                    }
                    Err(error) => {
                        pause_unadoptable(&mut snapshot, error);
                        false
                    }
                }
            } else {
                false
            };
            let mut script = if execution_context_available {
                match tokio::fs::read_to_string(&snapshot.script_path).await {
                    Ok(source) if sha256(&source) == snapshot.script_sha256 => {
                        match validate_workflow_script(source) {
                            Ok(script) => Some(script),
                            Err(error) => {
                                pause_unadoptable(&mut snapshot, error.to_string());
                                None
                            }
                        }
                    }
                    Ok(_) => {
                        pause_unadoptable(
                            &mut snapshot,
                            "script content changed since it was approved; resume via the Workflow tool to re-approve"
                                .to_string(),
                        );
                        None
                    }
                    Err(error) => {
                        pause_unadoptable(
                            &mut snapshot,
                            format!("failed to read approved workflow script: {error}"),
                        );
                        None
                    }
                }
            } else {
                None
            };
            let composition = if let Some(validated_script) = script.as_ref() {
                match workflow_children_dir(&snapshot)
                    .map(|children_dir| (&loaded_metadata.composition, children_dir))
                {
                    Ok((persisted, children_dir)) => {
                        match restore_workflow_composition(
                            validated_script,
                            persisted,
                            &children_dir,
                        )
                        .await
                        {
                            Ok(composition) => Some(composition),
                            Err(error) => {
                                pause_unadoptable(
                                    &mut snapshot,
                                    format!(
                                        "failed to restore frozen child workflow composition: {error}"
                                    ),
                                );
                                script = None;
                                None
                            }
                        }
                    }
                    Err(error) => {
                        pause_unadoptable(&mut snapshot, error);
                        script = None;
                        None
                    }
                }
            } else {
                None
            };
            let journal = if script.is_some() && composition.is_some() {
                let current_journal_path =
                    journal_path(&snapshot.transcript_dir, &snapshot.task_id);
                match FileWorkflowJournal::open(current_journal_path, None).await {
                    Ok(journal) => Some(Arc::new(journal)),
                    Err(error) => {
                        pause_unadoptable(
                            &mut snapshot,
                            format!("failed to open workflow journal during restoration: {error}"),
                        );
                        None
                    }
                }
            } else {
                None
            };
            let input_artifact_store = journal.as_ref().map(|_| {
                Arc::new(FileWorkflowInputArtifactStore::new(
                    snapshot
                        .transcript_dir
                        .join("input-artifacts")
                        .to_path_buf(),
                    None,
                ))
            });
            let task = Arc::new(WorkflowTask::new(
                snapshot.clone(),
                task_execution_context,
                task_composition,
            ));
            self.cache_task(snapshot.run_id.clone(), Arc::clone(&task));

            if !active {
                continue;
            }

            let Some(script) = script else {
                if active && let Err(error) = persist_terminal_task(&task, &snapshot).await {
                    tracing::warn!(%error, "failed to persist paused workflow restoration");
                }
                continue;
            };
            let Some(journal) = journal else {
                if let Err(error) = persist_terminal_task(&task, &snapshot).await {
                    tracing::warn!(%error, "failed to persist paused workflow restoration");
                }
                continue;
            };
            let Some(input_artifact_store) = input_artifact_store else {
                continue;
            };
            let Some(composition) = composition else {
                if let Err(error) = persist_terminal_task(&task, &snapshot).await {
                    tracing::warn!(%error, "failed to persist paused workflow restoration");
                }
                continue;
            };
            while !self.emit_started(&snapshot, thread_id).await {
                tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
            }
            self.emit_progress_snapshot(&snapshot, thread_id);
            self.start_task(WorkflowTaskStart {
                task,
                thread_id,
                config: config.clone(),
                script,
                args: snapshot.args,
                agent_runner: agent_runner.clone(),
                journal,
                input_artifact_store,
                composition,
                environments: restored_environments,
                captured_environments: None,
                environment_location: WorkflowEnvironmentLocation::Local,
            });
        }
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .order_restored_terminals_by_recency();
        Ok(())
    }

    pub(crate) async fn launch(
        &self,
        request: WorkflowLaunchRequest,
    ) -> Result<WorkflowLaunch, WorkflowServiceError> {
        let WorkflowLaunchRequest {
            thread_id,
            turn_id,
            config,
            resolved,
            agent_runner,
            environments,
            captured_environments,
            environment_location,
        } = request;
        self.register_thread_codex_home(thread_id, &config.codex_home);
        let resume_run_id = resolved.resume_from_run_id.clone();
        let mut resume_reservation = match resume_run_id.as_deref() {
            Some(run_id) => Some(self.reserve_resume(thread_id, run_id).await?),
            None => None,
        };
        let script_sha256 = sha256(&resolved.script.source);
        let execution_location = if captured_environments
            .as_ref()
            .is_some_and(AgentExecutionEnvironmentSnapshot::has_remote_environment)
        {
            WorkflowEnvironmentLocation::Remote
        } else {
            environment_location
        };
        let execution_context = PersistedWorkflowExecutionContext::capture(
            &config,
            thread_id,
            execution_location,
            &environments,
        )
        .await;
        let resume_state = resume_reservation
            .as_ref()
            .map(|reservation| &reservation.state);
        let replay_identity_matches = resume_state.as_ref().is_some_and(|resume| {
            resume
                .execution_context
                .replay_identity_matches(&execution_context)
                && resume.snapshot.script_sha256 == script_sha256
                && resume.snapshot.args == resolved.args
                && resume.composition.definition_sha256 == resolved.composition.definition_sha256()
        });

        let run_id =
            resume_run_id.unwrap_or_else(|| format!("wf_{}", uuid::Uuid::new_v4().simple()));
        let task_id = format!("w{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let session_dir = workflow_session_dir(&config.codex_home, thread_id);
        let transcript_root = session_dir.join("subagents/workflows");
        let transcript_dir = match &resume_state {
            Some(resume) if replay_identity_matches => resume.snapshot.transcript_dir.clone(),
            Some(_) => transcript_root.join(format!("{run_id}-{task_id}")),
            None => transcript_root.join(run_id.as_str()),
        };
        let scripts_dir = session_dir.join("workflows/scripts");
        let snapshots_dir = session_dir.join("workflows");
        tokio::fs::create_dir_all(&transcript_dir)
            .await
            .map_err(persistence_error)?;
        tokio::fs::create_dir_all(&scripts_dir)
            .await
            .map_err(persistence_error)?;
        tokio::fs::create_dir_all(&snapshots_dir)
            .await
            .map_err(persistence_error)?;
        let current_journal_path = journal_path(&transcript_dir, &task_id);
        let slug = slugify(&resolved.script.meta.name);
        let script_path = scripts_dir.join(format!("{slug}-{run_id}-{task_id}.js"));
        tokio::fs::write(&script_path, resolved.script.source.as_bytes())
            .await
            .map_err(persistence_error)?;
        let output_file = snapshots_dir.join(format!("{run_id}.json"));
        let summary = format!("Running workflow {}", resolved.script.meta.name);
        let started_at = unix_seconds();
        let snapshot = WorkflowTaskSnapshot {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.clone(),
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            workflow_name: resolved.script.meta.name.clone(),
            title: resolved.script.meta.title.clone(),
            status: WorkflowTaskStatus::Running,
            summary: summary.clone(),
            transcript_dir: transcript_dir.clone(),
            script_path: script_path.clone(),
            args: resolved.args.clone(),
            result_artifact: None,
            output_file: output_file.clone(),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at,
            completed_at: None,
            script_sha256,
        };
        let snapshot_file = snapshot_path(&snapshot).map_err(WorkflowServiceError::Persistence)?;
        let children_dir =
            workflow_children_dir(&snapshot).map_err(WorkflowServiceError::Persistence)?;
        let persisted_composition =
            persist_workflow_composition(&resolved.composition, &children_dir)
                .await
                .map_err(WorkflowServiceError::Persistence)?;
        let replay_path = resume_state
            .as_ref()
            .filter(|_| replay_identity_matches)
            .map(|resume| journal_path(&resume.snapshot.transcript_dir, &resume.snapshot.task_id));
        let journal = Arc::new(
            FileWorkflowJournal::open(current_journal_path, replay_path.as_deref())
                .await
                .map_err(WorkflowServiceError::Persistence)?,
        );
        let replay_artifact_directory = resume_state
            .as_ref()
            .filter(|_| replay_identity_matches)
            .map(|resume| {
                resume
                    .snapshot
                    .transcript_dir
                    .join("input-artifacts")
                    .to_path_buf()
            });
        let input_artifact_store = Arc::new(FileWorkflowInputArtifactStore::new(
            transcript_dir.join("input-artifacts").to_path_buf(),
            replay_artifact_directory,
        ));
        let task = Arc::new(WorkflowTask::new(
            snapshot.clone(),
            execution_context,
            persisted_composition,
        ));
        if let Some(reservation) = resume_reservation.take() {
            reservation.commit(Arc::clone(&task), &snapshot).await?;
        } else {
            write_current_snapshot(
                snapshot_file,
                &snapshot,
                &task.execution_context,
                &task.composition,
            )
            .await
            .map_err(WorkflowServiceError::Persistence)?;
            self.cache_task(run_id.clone(), Arc::clone(&task));
        }
        while !self.emit_started(&snapshot, thread_id).await {
            tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
        }
        let workflow_name = resolved.script.meta.name.clone();
        let environments = (!environments.is_empty()).then_some(environments);
        self.start_task(WorkflowTaskStart {
            task,
            thread_id,
            config,
            script: resolved.script,
            args: resolved.args,
            agent_runner,
            journal,
            input_artifact_store,
            composition: resolved.composition,
            environments,
            captured_environments,
            environment_location,
        });

        Ok(WorkflowLaunch {
            status: "async_launched".to_string(),
            task_id,
            task_type: "local_workflow".to_string(),
            workflow_name,
            run_id,
            summary,
            transcript_dir: transcript_dir.display().to_string(),
            script_path: script_path.display().to_string(),
        })
    }

    fn start_task(&self, start: WorkflowTaskStart) {
        let WorkflowTaskStart {
            task,
            thread_id,
            config,
            script,
            args,
            agent_runner,
            journal,
            input_artifact_store,
            composition,
            environments,
            captured_environments,
            environment_location,
        } = start;
        let service = self.clone();
        tokio::spawn(async move {
            let (run_id, initial_usage) = {
                let snapshot = task
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    snapshot.run_id.clone(),
                    WorkflowTokenUsage {
                        total_tokens: snapshot.usage.total_tokens,
                        tool_uses: snapshot.usage.tool_uses,
                    },
                )
            };
            let child_resolver = composition.resolver();
            let definition_sha256 = (composition.child_count() > 0)
                .then(|| composition.definition_sha256().to_string());
            let agent_runtime = Arc::new(CodexWorkflowAgentRuntime::new_with_environments(
                agent_runner,
                thread_id,
                config,
                run_id,
                environments,
                captured_environments,
                environment_location,
            ));
            let execution_generation_base = task.execution_generation.load(Ordering::Acquire);
            let (progress_tx, event_sink, progress_worker) = start_workflow_progress_worker(
                service.clone(),
                Arc::clone(&task),
                thread_id,
                execution_generation_base,
            );
            let result = execute_workflow(
                &script,
                args,
                Arc::clone(&agent_runtime) as Arc<dyn codex_workflow::WorkflowAgentRuntime>,
                Arc::clone(&event_sink),
                WorkflowRuntimeConfig {
                    initial_usage,
                    definition_sha256,
                    child_resolver,
                    journal: Some(journal),
                    input_artifact_store,
                    ..WorkflowRuntimeConfig::default()
                },
                task.control.clone(),
            )
            .await;
            drop(event_sink);
            drop(progress_tx);
            if progress_worker.await.is_err() {
                tracing::warn!("workflow progress worker join failed");
            }
            let cleanup_mode = if result.is_ok() {
                WorktreeCleanupMode::Completed
            } else {
                WorktreeCleanupMode::Interrupted
            };
            for message in agent_runtime.cleanup_worktrees(cleanup_mode).await {
                let execution_generation = task.execution_generation.load(Ordering::Acquire);
                service.record_progress(
                    &task,
                    thread_id,
                    execution_generation,
                    WorkflowEvent::WorkflowLog { message },
                );
            }
            service.finish_task(task, thread_id, result).await;
        });
    }

    pub async fn list(
        &self,
        thread_id: ThreadId,
    ) -> Result<Vec<WorkflowTaskSnapshot>, WorkflowServiceError> {
        let owner = thread_id.to_string();
        let mut snapshots = HashMap::new();
        if let Some(codex_home) = self.thread_codex_home(thread_id) {
            let mut cursor = None;
            loop {
                let page = load_snapshot_page(
                    &codex_home,
                    thread_id,
                    &[],
                    cursor,
                    MAX_RETAINED_TERMINAL_TASKS,
                )
                .await
                .map_err(WorkflowServiceError::Persistence)?;
                snapshots.extend(
                    page.snapshots
                        .into_iter()
                        .filter(|snapshot| snapshot.thread_id == owner)
                        .map(|snapshot| (snapshot.run_id.clone(), snapshot)),
                );
                let Some(next_sequence) = page.next_sequence else {
                    break;
                };
                cursor = Some(next_sequence);
            }
        }
        for snapshot in self.cached_snapshots(thread_id) {
            snapshots.insert(snapshot.run_id.clone(), snapshot);
        }
        let mut snapshots = snapshots.into_values().collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        Ok(snapshots)
    }

    pub(crate) async fn list_page(
        &self,
        thread_id: ThreadId,
        statuses: &[WorkflowTaskStatus],
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<WorkflowListPage, WorkflowServiceError> {
        let Some(codex_home) = self.thread_codex_home(thread_id) else {
            return Ok(WorkflowListPage {
                snapshots: Vec::new(),
                snapshot_sequences: Vec::new(),
                total_matched: 0,
                next_sequence: None,
            });
        };
        let page = load_snapshot_page(&codex_home, thread_id, statuses, cursor, limit)
            .await
            .map_err(WorkflowServiceError::Persistence)?;
        Ok(WorkflowListPage {
            snapshots: page.snapshots,
            snapshot_sequences: page.snapshot_sequences,
            total_matched: page.total_matched,
            next_sequence: page.next_sequence,
        })
    }

    /// Returns whether workflow finalization still requires the owning thread.
    pub fn keeps_thread_resident(&self, thread_id: ThreadId) -> bool {
        let thread_id = thread_id.to_string();
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .values()
            .any(|task| {
                task.keep_thread_resident.load(Ordering::Acquire)
                    && task
                        .snapshot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .thread_id
                        == thread_id
            })
    }

    pub(crate) async fn wait_for_terminal(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        timeout_duration: Duration,
    ) -> Result<WorkflowWaitOutcome, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        let mut status_rx = task.status_tx.subscribe();
        let timed_out = if workflow_status_is_terminal(*status_rx.borrow_and_update()) {
            false
        } else {
            tokio::time::timeout(timeout_duration, async {
                loop {
                    status_rx.changed().await.map_err(|_| ())?;
                    if workflow_status_is_terminal(*status_rx.borrow_and_update()) {
                        return Ok::<(), ()>(());
                    }
                }
            })
            .await
            .is_err()
        };
        let snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let timed_out = timed_out && !workflow_status_is_terminal(snapshot.status);
        Ok(WorkflowWaitOutcome {
            snapshot,
            timed_out,
        })
    }

    pub async fn stop(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        Ok(task.control.try_stop())
    }

    pub async fn skip_agent(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        agent_index: usize,
    ) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        Ok(task.control.skip_agent(agent_index))
    }

    pub async fn retry_agent(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        agent_index: usize,
    ) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        // Active attempts and suspended agents are controlled directly; settled
        // (done/skipped/failed) agents restart the run from their index so
        // downstream stages that already ran are recomputed.
        if task.control.retry_agent(agent_index) {
            return Ok(true);
        }
        let execution_generation = task.execution_generation.load(Ordering::Acquire);
        let settled = task
            .progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent(agent_index)
            .is_some_and(|agent| {
                matches!(
                    agent.state,
                    WorkflowAgentState::Done | WorkflowAgentState::Error
                ) && !agent.awaiting_decision
            });
        if !settled {
            return Ok(false);
        }
        let _transition = task
            .execution_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.execution_generation.load(Ordering::Acquire) != execution_generation {
            return Ok(false);
        }
        let previous_generation = task.execution_generation.fetch_add(1, Ordering::AcqRel);
        let accepted = task.control.rerun_from(agent_index);
        if accepted {
            self.reset_progress_for_rerun(&task, thread_id, previous_generation.saturating_add(1));
        } else {
            let _ = task.execution_generation.compare_exchange(
                previous_generation.saturating_add(1),
                previous_generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        Ok(accepted)
    }

    pub(crate) async fn agent_progress(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        agent_index: usize,
    ) -> Result<Option<codex_protocol::workflow::WorkflowAgentProgress>, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        if let Some(agent) = task.control.agent_progress(agent_index) {
            return Ok(Some(agent));
        }
        let agent = task
            .progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent(agent_index);
        Ok(agent)
    }

    pub(crate) async fn progress_page(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<WorkflowAgentProgressPage, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        let progress_state = task
            .progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agents = progress_state.page(offset, limit);
        let next_offset = offset.saturating_add(limit);
        Ok(WorkflowAgentProgressPage {
            agents,
            total_agents: progress_state.agent_count(),
            next_index: (next_offset < progress_state.agent_high_water()).then_some(next_offset),
        })
    }

    #[cfg(test)]
    async fn validate_resume(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<WorkflowResumeState, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id).await?;
        if task.keep_thread_resident.load(Ordering::Acquire) {
            return Err(WorkflowServiceError::StillRunning);
        }
        let snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            snapshot.status,
            WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
        ) {
            return Err(WorkflowServiceError::StillRunning);
        }
        Ok(WorkflowResumeState {
            snapshot: snapshot.clone(),
            execution_context: task.execution_context.clone(),
            composition: task.composition.clone(),
        })
    }

    async fn reserve_resume(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<WorkflowResumeReservation, WorkflowServiceError> {
        let codex_home = self
            .thread_codex_home(thread_id)
            .ok_or(WorkflowServiceError::NotFound)?;
        let snapshot_path = workflow_session_dir(&codex_home, thread_id)
            .join("workflows")
            .join(format!("{run_id}.json"));
        let reservation_path = canonical_resume_lock_path(&snapshot_path)?;
        let blocking_snapshot_path = snapshot_path.clone();
        let owner = thread_id.to_string();
        let run_id_owned = run_id.to_string();
        let (lock_file, snapshot, metadata, expected_snapshot_sha256) =
            tokio::task::spawn_blocking(move || {
                let mut lock_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&reservation_path)
                    .map_err(persistence_error)?;
                match lock_file.try_lock() {
                    Ok(()) => {}
                    Err(std::fs::TryLockError::WouldBlock) => {
                        return Err(WorkflowServiceError::StillRunning);
                    }
                    Err(std::fs::TryLockError::Error(error)) => {
                        return Err(WorkflowServiceError::Persistence(error.to_string()));
                    }
                }
                let bytes = std::fs::read(&blocking_snapshot_path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        WorkflowServiceError::NotFound
                    } else {
                        persistence_error(error)
                    }
                })?;
                let loaded = parse_validated_snapshot(
                    &blocking_snapshot_path,
                    &run_id_owned,
                    &owner,
                    &bytes,
                )
                .map_err(WorkflowServiceError::Persistence)?;
                let snapshot = loaded.snapshot;
                if matches!(
                    snapshot.status,
                    WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
                ) {
                    return Err(WorkflowServiceError::StillRunning);
                }
                let expected_snapshot_sha256 = format!("{:x}", Sha256::digest(&bytes));
                lock_file.set_len(0).map_err(persistence_error)?;
                lock_file
                    .seek(std::io::SeekFrom::Start(0))
                    .map_err(persistence_error)?;
                lock_file
                    .write_all(expected_snapshot_sha256.as_bytes())
                    .map_err(persistence_error)?;
                lock_file.sync_data().map_err(persistence_error)?;
                Ok((
                    lock_file,
                    snapshot,
                    loaded.metadata,
                    expected_snapshot_sha256,
                ))
            })
            .await
            .map_err(|error| WorkflowServiceError::Persistence(error.to_string()))??;
        let state = WorkflowResumeState {
            snapshot,
            execution_context: metadata.execution_context,
            composition: metadata.composition,
        };
        Ok(WorkflowResumeReservation {
            cache: Arc::clone(&self.cache),
            run_id: run_id.to_string(),
            lock_file,
            snapshot_path,
            expected_snapshot_sha256,
            state,
            committed: false,
        })
    }

    pub(crate) async fn read_result_chunk(
        &self,
        thread_id: ThreadId,
        snapshot: &WorkflowTaskSnapshot,
        offset: u64,
        max_bytes: usize,
    ) -> Result<WorkflowResultChunk, String> {
        let task = self
            .task_for_thread(thread_id, &snapshot.run_id)
            .await
            .map_err(|error| error.to_string())?;
        let verified = task.ensure_result_verified(snapshot).await?;
        read_verified_result_chunk(&verified, offset, max_bytes)
    }

    async fn task_for_thread(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<Arc<WorkflowTask>, WorkflowServiceError> {
        if let Some(task) = self.cached_task(thread_id, run_id) {
            return validate_task_owner(task, thread_id);
        }
        let codex_home = self
            .thread_codex_home(thread_id)
            .ok_or(WorkflowServiceError::NotFound)?;
        let loaded = load_snapshot(&codex_home, thread_id, run_id)
            .await
            .map_err(WorkflowServiceError::Persistence)?
            .ok_or(WorkflowServiceError::NotFound)?;
        let snapshot = loaded.snapshot;
        if snapshot.thread_id != thread_id.to_string() {
            return Err(WorkflowServiceError::WrongThread);
        }
        let loaded = Arc::new(WorkflowTask::new(
            snapshot,
            loaded.metadata.execution_context,
            loaded.metadata.composition,
        ));
        let task = {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = WorkflowTaskKey::new(thread_id, run_id);
            if let Some(existing) = cache.get(&key) {
                existing
            } else {
                cache.insert(run_id.to_string(), Arc::clone(&loaded));
                loaded
            }
        };
        validate_task_owner(task, thread_id)
    }

    fn register_thread_codex_home(&self, thread_id: ThreadId, codex_home: &AbsolutePathBuf) {
        let thread_id = thread_id.to_string();
        let mut homes = self
            .thread_codex_homes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        homes.insert(thread_id.clone(), codex_home.clone());
        let mut order = self
            .thread_codex_home_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order.retain(|tracked| tracked != &thread_id);
        order.push_back(thread_id);
        while homes.len() > MAX_TRACKED_THREAD_HOMES {
            let Some(candidate) = order.pop_front() else {
                break;
            };
            homes.remove(&candidate);
        }
    }

    fn thread_codex_home(&self, thread_id: ThreadId) -> Option<AbsolutePathBuf> {
        self.thread_codex_homes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id.to_string())
            .cloned()
    }

    fn cached_task(&self, thread_id: ThreadId, run_id: &str) -> Option<Arc<WorkflowTask>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&WorkflowTaskKey::new(thread_id, run_id))
    }

    fn cache_task(&self, run_id: String, task: Arc<WorkflowTask>) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, task);
    }

    fn cached_snapshots(&self, thread_id: ThreadId) -> Vec<WorkflowTaskSnapshot> {
        let owner = thread_id.to_string();
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .values()
            .filter_map(|task| {
                let snapshot = task
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (snapshot.thread_id == owner).then(|| snapshot.clone())
            })
            .collect()
    }

    fn record_progress(
        &self,
        task: &Arc<WorkflowTask>,
        thread_id: ThreadId,
        execution_generation: u64,
        event: WorkflowEvent,
    ) {
        let _transition = task
            .execution_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_generation = task.execution_generation.load(Ordering::Acquire);
        if current_generation != execution_generation {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            task.usage_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record(execution_generation, &event, None, &mut snapshot.usage);
            drop(snapshot);
            persist_task_background(Arc::clone(task));
            return;
        }
        let progress_event = {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut progress_state = task
                .progress_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if execution_generation > progress_state.execution_generation() {
                progress_state.begin_execution(execution_generation);
            }
            let previous_agent = match &event {
                WorkflowEvent::WorkflowAgent(agent) => progress_state.agent(agent.index),
                WorkflowEvent::WorkflowPhase { .. } | WorkflowEvent::WorkflowLog { .. } => None,
            };
            task.usage_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record(
                    execution_generation,
                    &event,
                    previous_agent.as_ref(),
                    &mut snapshot.usage,
                );
            progress_state.record(execution_generation, event);
            snapshot.progress = progress_state.latest_window();
            snapshot.progress_version = snapshot.progress_version.saturating_add(1);
            snapshot.usage.agent_count = progress_state.agent_count();

            WorkflowProgressEvent {
                thread_id,
                turn_id: snapshot.turn_id.clone(),
                task_id: snapshot.task_id.clone(),
                run_id: snapshot.run_id.clone(),
                progress: snapshot.progress.clone(),
                usage: snapshot.usage.clone(),
            }
        };
        persist_task_background(Arc::clone(task));
        self.event_sink.emit(Event {
            id: progress_event.turn_id.clone(),
            msg: EventMsg::WorkflowProgress(progress_event),
        });
    }

    fn reset_progress_for_rerun(
        &self,
        task: &Arc<WorkflowTask>,
        thread_id: ThreadId,
        execution_generation: u64,
    ) {
        let progress_event = {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut progress_state = task
                .progress_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            progress_state.begin_execution(execution_generation);
            snapshot.progress = progress_state.latest_window();
            snapshot.progress_version = snapshot.progress_version.saturating_add(1);
            snapshot.usage.agent_count = progress_state.agent_count();
            WorkflowProgressEvent {
                thread_id,
                turn_id: snapshot.turn_id.clone(),
                task_id: snapshot.task_id.clone(),
                run_id: snapshot.run_id.clone(),
                progress: snapshot.progress.clone(),
                usage: snapshot.usage.clone(),
            }
        };
        persist_task_background(Arc::clone(task));
        self.event_sink.emit(Event {
            id: progress_event.turn_id.clone(),
            msg: EventMsg::WorkflowProgress(progress_event),
        });
    }

    async fn finish_task(
        &self,
        task: Arc<WorkflowTask>,
        thread_id: ThreadId,
        result: Result<WorkflowRunOutcome, WorkflowExecutionError>,
    ) {
        let execution_generation = task.execution_generation.load(Ordering::Acquire);
        let (mut terminal_snapshot, result_value, terminal_logs) = {
            let snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut snapshot = snapshot.clone();
            let completed_at = unix_seconds();
            snapshot.completed_at = Some(completed_at);
            let (result_value, terminal_logs) = match result {
                Ok(outcome) => {
                    let WorkflowRunOutcome {
                        result,
                        agent_count,
                        logs,
                        failures,
                        total_tokens,
                        total_tool_calls,
                        duration_ms,
                    } = outcome;
                    snapshot.status = WorkflowTaskStatus::Completed;
                    snapshot.summary = format!("Workflow {} completed", snapshot.workflow_name);
                    snapshot.failures = failures;
                    snapshot.usage = WorkflowUsage {
                        total_tokens: snapshot.usage.total_tokens.max(total_tokens),
                        tool_uses: snapshot.usage.tool_uses.max(total_tool_calls),
                        duration_ms: snapshot.usage.duration_ms.saturating_add(duration_ms),
                        agent_count,
                    };
                    (result, Some(logs))
                }
                Err(WorkflowExecutionError::Cancelled) => {
                    snapshot.status = WorkflowTaskStatus::Killed;
                    snapshot.summary = format!("Workflow {} stopped", snapshot.workflow_name);
                    snapshot.failures = task
                        .progress_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .failures();
                    (JsonValue::Null, None)
                }
                Err(error) => {
                    snapshot.status = WorkflowTaskStatus::Failed;
                    snapshot.summary = format!("Workflow {} failed", snapshot.workflow_name);
                    snapshot.error = Some(error.to_string());
                    snapshot.failures = task
                        .progress_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .failures();
                    (JsonValue::Null, None)
                }
            };
            if let Some(logs) = terminal_logs.as_ref() {
                let mut progress_state = task
                    .progress_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                progress_state.replace_logs(execution_generation, logs.clone());
                snapshot.progress = progress_state.latest_window();
                snapshot.progress_version = snapshot.progress_version.saturating_add(1);
            }
            if snapshot.usage.duration_ms == 0 {
                snapshot.usage.duration_ms =
                    u64::try_from(completed_at.saturating_sub(snapshot.started_at))
                        .unwrap_or(0)
                        .saturating_mul(1_000);
            }
            (snapshot, result_value, terminal_logs)
        };
        drop(terminal_logs);
        let serialized_result = match serialize_workflow_result(&result_value) {
            Ok(serialized) => Arc::<str>::from(serialized),
            Err(error) => {
                terminal_snapshot.status = WorkflowTaskStatus::Failed;
                terminal_snapshot.summary = format!(
                    "Workflow {} failed while preparing its result",
                    terminal_snapshot.workflow_name
                );
                terminal_snapshot.error = Some(error.to_string());
                Arc::<str>::from("null")
            }
        };
        let result_artifact = loop {
            match persist_result_artifact(
                &terminal_snapshot.output_file,
                Arc::clone(&serialized_result),
            )
            .await
            {
                Ok(artifact) => break artifact,
                Err(error) => {
                    tracing::warn!(%error, "failed to write terminal workflow result; retrying");
                    tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
                }
            }
        };
        terminal_snapshot.result_artifact = Some(result_artifact.clone());
        *task.verified_result.lock().await = None;
        let completed_event = workflow_completed_event(&terminal_snapshot, thread_id);
        loop {
            match persist_terminal_task(&task, &terminal_snapshot).await {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(%error, "failed to write terminal workflow snapshot; retrying");
                    tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
                }
            }
        }
        task.status_tx.send_replace(completed_event.status);
        while !self
            .deliver_completion(&terminal_snapshot, completed_event.clone())
            .await
        {
            tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
        }
        task.keep_thread_resident.store(false, Ordering::Release);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.touch_terminal(&WorkflowTaskKey::new(
            completed_event.thread_id,
            completed_event.run_id.clone(),
        ));
        cache.prune_terminal_tasks();
    }

    async fn notify_owning_thread(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        event: &WorkflowCompletedEvent,
        idempotency_key: &str,
    ) -> bool {
        let Some(thread_manager) = self.thread_manager.upgrade() else {
            return false;
        };
        let Ok(thread) = thread_manager.get_thread(event.thread_id).await else {
            return false;
        };
        let message = self.owning_model_completion_message(snapshot, event).await;
        thread
            .inject_user_message_without_turn_once(
                ResponseItemId::with_suffix("msg", format!("workflow_{}", sha256(idempotency_key))),
                message,
            )
            .await
    }

    async fn owning_model_completion_message(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        event: &WorkflowCompletedEvent,
    ) -> String {
        let result = if snapshot.result_artifact.is_some() {
            match self
                .read_result_chunk(
                    event.thread_id,
                    snapshot,
                    /*offset*/ 0,
                    WORKFLOW_NOTIFICATION_RESULT_CANDIDATE_MAX_BYTES,
                )
                .await
            {
                Ok(chunk) => Some(
                    WorkflowNotificationResult::from_chunk(
                        &chunk.text,
                        chunk.next_offset,
                        chunk.total_bytes,
                    )
                    .unwrap_or_else(|error| {
                        WorkflowNotificationResult::read_error(&error.to_string())
                    }),
                ),
                Err(error) => Some(WorkflowNotificationResult::read_error(&error)),
            }
        } else {
            None
        };
        codex_core::format_workflow_notification_message(event, result)
    }

    async fn replay_completion(&self, snapshot: &WorkflowTaskSnapshot, thread_id: ThreadId) {
        let event = workflow_completed_event(snapshot, thread_id);
        let _ = self.deliver_completion(snapshot, event).await;
    }

    pub(crate) async fn replay_pending_owning_model_completions(&self, thread_id: ThreadId) {
        let snapshots = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .values()
            .filter_map(|task| {
                let snapshot = task
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (snapshot.thread_id == thread_id.to_string()
                    && workflow_status_is_terminal(snapshot.status)
                    && !load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
                        .is_some_and(|state| state.owning_model_acknowledged))
                .then(|| snapshot.clone())
            })
            .collect::<Vec<_>>();

        for snapshot in snapshots {
            let event = workflow_completed_event(&snapshot, thread_id);
            let _ = self
                .deliver_owning_model_completion(&snapshot, &event)
                .await;
        }
    }

    async fn deliver_owning_model_completion(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        event: &WorkflowCompletedEvent,
    ) -> bool {
        let Ok(_delivery_lock) =
            lock_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Completed).await
        else {
            tracing::warn!(
                run_id = %snapshot.run_id,
                "failed to lock workflow completion delivery"
            );
            return false;
        };
        let (_, mut state) = completion_delivery_state(snapshot, event);
        if !persist_lifecycle_delivery_with_retry(
            snapshot,
            WorkflowLifecycleDelivery::Completed,
            &state,
        )
        .await
        {
            return false;
        }
        self.acknowledge_owning_model_completion(snapshot, event, &mut state)
            .await
    }

    async fn deliver_completion(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        event: WorkflowCompletedEvent,
    ) -> bool {
        let Ok(_delivery_lock) =
            lock_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Completed).await
        else {
            tracing::warn!(
                run_id = %snapshot.run_id,
                "failed to lock workflow completion delivery"
            );
            return false;
        };
        let (expected_key, mut state) = completion_delivery_state(snapshot, &event);
        if !persist_lifecycle_delivery_with_retry(
            snapshot,
            WorkflowLifecycleDelivery::Completed,
            &state,
        )
        .await
        {
            return false;
        }
        self.acknowledge_owning_model_completion(snapshot, &event, &mut state)
            .await;
        let delivered = self
            .deliver_lifecycle(
                snapshot,
                WorkflowLifecycleDelivery::Completed,
                expected_key,
                Event {
                    id: event.turn_id.clone(),
                    msg: EventMsg::WorkflowCompleted(event),
                },
            )
            .await
            .is_some();
        if !state.owning_model_acknowledged {
            self.schedule_delivery_retry_worker();
        }
        delivered
    }

    async fn acknowledge_owning_model_completion(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        event: &WorkflowCompletedEvent,
        state: &mut LifecycleDeliveryState,
    ) -> bool {
        if !state.owning_model_acknowledged && self.thread_manager.strong_count() > 0 {
            for attempt in 0..LIFECYCLE_DELIVERY_ATTEMPTS {
                if self
                    .notify_owning_thread(snapshot, event, &state.idempotency_key)
                    .await
                {
                    state.owning_model_acknowledged = true;
                    if persist_lifecycle_delivery_with_retry(
                        snapshot,
                        WorkflowLifecycleDelivery::Completed,
                        state,
                    )
                    .await
                    {
                        break;
                    } else {
                        state.owning_model_acknowledged = false;
                        tracing::warn!(
                            run_id = %snapshot.run_id,
                            "failed to persist owning-model workflow completion acknowledgment"
                        );
                    }
                }
                lifecycle_delivery_backoff(attempt).await;
            }
        }
        state.owning_model_acknowledged
    }

    async fn emit_started(&self, snapshot: &WorkflowTaskSnapshot, thread_id: ThreadId) -> bool {
        let Ok(_delivery_lock) =
            lock_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Started).await
        else {
            tracing::warn!(
                run_id = %snapshot.run_id,
                "failed to lock workflow started delivery"
            );
            return false;
        };
        let event = WorkflowStartedEvent {
            thread_id,
            turn_id: snapshot.turn_id.clone(),
            task_id: snapshot.task_id.clone(),
            run_id: snapshot.run_id.clone(),
            workflow_name: snapshot.workflow_name.clone(),
            title: snapshot.title.clone(),
            summary: snapshot.summary.clone(),
            transcript_dir: snapshot.transcript_dir.clone(),
            script_path: snapshot.script_path.clone(),
            started_at: snapshot.started_at,
        };
        let expected_key = format!(
            "workflow/started/{}/{}/{}",
            event.thread_id, event.run_id, event.task_id
        );
        self.deliver_lifecycle(
            snapshot,
            WorkflowLifecycleDelivery::Started,
            expected_key,
            Event {
                id: event.turn_id.clone(),
                msg: EventMsg::WorkflowStarted(event),
            },
        )
        .await
        .is_some()
    }

    async fn deliver_lifecycle(
        &self,
        snapshot: &WorkflowTaskSnapshot,
        lifecycle: WorkflowLifecycleDelivery,
        expected_key: String,
        event: Event,
    ) -> Option<LifecycleDeliveryState> {
        let mut state = load_lifecycle_delivery(snapshot, lifecycle)
            .filter(|state| state.idempotency_key == expected_key)
            .unwrap_or(LifecycleDeliveryState {
                idempotency_key: expected_key.clone(),
                transport_acknowledged: false,
                owning_model_acknowledged: false,
            });
        if state.transport_acknowledged {
            return Some(state);
        }
        if !persist_lifecycle_delivery_with_retry(snapshot, lifecycle, &state).await {
            tracing::warn!(
                run_id = %snapshot.run_id,
                ?lifecycle,
                "failed to persist pending workflow lifecycle delivery"
            );
            return None;
        }
        for attempt in 0..LIFECYCLE_DELIVERY_ATTEMPTS {
            let delivery = self.event_sink.emit_and_wait(event.clone()).await;
            let (acknowledged, idempotency_key) = match delivery {
                ExtensionEventDelivery::Acknowledged { idempotency_key } => (true, idempotency_key),
                ExtensionEventDelivery::Retryable { idempotency_key } => (false, idempotency_key),
            };
            if idempotency_key != expected_key {
                tracing::warn!(
                    %idempotency_key,
                    %expected_key,
                    "workflow lifecycle delivery returned an unexpected idempotency key"
                );
            } else if acknowledged {
                state.transport_acknowledged = true;
                if persist_lifecycle_delivery_with_retry(snapshot, lifecycle, &state).await {
                    return Some(state);
                }
                tracing::warn!(
                    run_id = %snapshot.run_id,
                    ?lifecycle,
                    "failed to persist workflow lifecycle delivery acknowledgment"
                );
                self.schedule_delivery_retry_worker();
                return Some(LifecycleDeliveryState {
                    transport_acknowledged: false,
                    ..state
                });
            }
            lifecycle_delivery_backoff(attempt).await;
        }
        self.schedule_delivery_retry_worker();
        Some(state)
    }

    fn schedule_delivery_retry_worker(&self) {
        let mut running = self
            .delivery_retry_worker_running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *running {
            return;
        }
        *running = true;
        drop(running);
        let service = self.clone();
        tokio::spawn(async move {
            service.run_delivery_retry_worker().await;
        });
    }

    async fn run_delivery_retry_worker(self) {
        loop {
            let pending = self.pending_delivery_snapshots().await;
            if pending.is_empty() {
                *self
                    .delivery_retry_worker_running
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
                return;
            }
            let mut availability = tokio::task::JoinSet::new();
            let mut threads = std::collections::HashSet::new();
            for snapshot in &pending {
                let Ok(thread_id) = ThreadId::from_string(&snapshot.thread_id) else {
                    continue;
                };
                if !threads.insert(thread_id) {
                    continue;
                }
                let event_sink = Arc::clone(&self.event_sink);
                availability.spawn(async move {
                    let Some(wait) = event_sink.wait_for_delivery_availability(thread_id) else {
                        return false;
                    };
                    wait.await;
                    true
                });
            }
            let owning_model_pending = self.thread_manager.strong_count() > 0
                && pending.iter().any(|snapshot| {
                    load_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Completed)
                        .is_some_and(|state| !state.owning_model_acknowledged)
                });
            let available = if owning_model_pending {
                tokio::select! {
                    result = availability.join_next(), if !availability.is_empty() => {
                        result.is_some_and(|result| result.unwrap_or(false))
                    }
                    () = tokio::time::sleep(OWNING_MODEL_DELIVERY_RETRY_INTERVAL) => true,
                }
            } else {
                availability
                    .join_next()
                    .await
                    .is_some_and(|result| result.unwrap_or(false))
            };
            availability.abort_all();
            if !available {
                *self
                    .delivery_retry_worker_running
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
                return;
            }
            for snapshot in pending {
                let Ok(thread_id) = ThreadId::from_string(&snapshot.thread_id) else {
                    continue;
                };
                let started_pending =
                    load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Started)
                        .is_some_and(|state| !state.transport_acknowledged);
                if started_pending {
                    let _ = self.emit_started(&snapshot, thread_id).await;
                }
                if workflow_status_is_terminal(snapshot.status) {
                    let completion_pending =
                        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
                            .is_some_and(|state| {
                                !state.transport_acknowledged || !state.owning_model_acknowledged
                            });
                    if completion_pending {
                        self.replay_completion(&snapshot, thread_id).await;
                    }
                }
            }
        }
    }

    async fn pending_delivery_snapshots(&self) -> Vec<WorkflowTaskSnapshot> {
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .values()
            .map(|task| {
                task.snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .collect::<Vec<_>>();
        let mut snapshots: Vec<WorkflowTaskSnapshot> = tokio::task::spawn_blocking(move || {
            cached
                .into_iter()
                .filter(|snapshot| {
                    load_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Started)
                        .is_some_and(|state| !state.transport_acknowledged)
                        || load_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Completed)
                            .is_some_and(|state| {
                                !state.transport_acknowledged || !state.owning_model_acknowledged
                            })
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        let tracked_homes = self
            .thread_codex_homes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut seen = snapshots
            .iter()
            .map(|snapshot| (snapshot.thread_id.clone(), snapshot.run_id.clone()))
            .collect::<std::collections::HashSet<_>>();
        for (owner, codex_home) in tracked_homes {
            let Ok(thread_id) = ThreadId::from_string(&owner) else {
                continue;
            };
            let directory = workflow_session_dir(&codex_home, thread_id).join("workflows");
            let Ok(pending) = load_pending_lifecycle_deliveries(directory).await else {
                continue;
            };
            for delivery in pending {
                if let Ok(Some(loaded)) =
                    load_snapshot(&codex_home, thread_id, &delivery.run_id).await
                {
                    let state = load_lifecycle_delivery(&loaded.snapshot, delivery.lifecycle);
                    let still_pending =
                        state.as_ref().is_none_or(|state| match delivery.lifecycle {
                            WorkflowLifecycleDelivery::Started => !state.transport_acknowledged,
                            WorkflowLifecycleDelivery::Completed => {
                                !state.transport_acknowledged || !state.owning_model_acknowledged
                            }
                        });
                    if !still_pending {
                        if let Some(state) = state
                            && let Err(error) = persist_lifecycle_delivery(
                                &loaded.snapshot,
                                delivery.lifecycle,
                                &state,
                            )
                            .await
                        {
                            tracing::warn!(
                                run_id = %loaded.snapshot.run_id,
                                %error,
                                "failed to clean acknowledged workflow delivery marker"
                            );
                        }
                        continue;
                    }
                    if !seen.insert((owner.clone(), delivery.run_id.clone())) {
                        continue;
                    }
                    snapshots.push(loaded.snapshot);
                }
            }
        }
        snapshots
    }

    fn emit_progress_snapshot(&self, snapshot: &WorkflowTaskSnapshot, thread_id: ThreadId) {
        let event = WorkflowProgressEvent {
            thread_id,
            turn_id: snapshot.turn_id.clone(),
            task_id: snapshot.task_id.clone(),
            run_id: snapshot.run_id.clone(),
            progress: snapshot.progress.clone(),
            usage: snapshot.usage.clone(),
        };
        self.event_sink.emit(Event {
            id: event.turn_id.clone(),
            msg: EventMsg::WorkflowProgress(event),
        });
    }
}

fn workflow_status_is_terminal(status: WorkflowTaskStatus) -> bool {
    match status {
        WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running => false,
        WorkflowTaskStatus::Completed
        | WorkflowTaskStatus::Failed
        | WorkflowTaskStatus::Paused
        | WorkflowTaskStatus::Killed => true,
    }
}

fn workflow_completed_event(
    snapshot: &WorkflowTaskSnapshot,
    thread_id: ThreadId,
) -> WorkflowCompletedEvent {
    WorkflowCompletedEvent {
        thread_id,
        turn_id: snapshot.turn_id.clone(),
        task_id: snapshot.task_id.clone(),
        run_id: snapshot.run_id.clone(),
        workflow_name: snapshot.workflow_name.clone(),
        status: snapshot.status,
        summary: snapshot.summary.clone(),
        output_file: snapshot.output_file.clone(),
        error: snapshot.error.clone(),
        failures: snapshot.failures.clone(),
        usage: snapshot.usage.clone(),
        completed_at: snapshot.completed_at.unwrap_or_default(),
    }
}

fn completion_delivery_state(
    snapshot: &WorkflowTaskSnapshot,
    event: &WorkflowCompletedEvent,
) -> (String, LifecycleDeliveryState) {
    let expected_key = format!(
        "workflow/completed/{}/{}/{}",
        event.thread_id, event.run_id, event.task_id
    );
    let state = load_lifecycle_delivery(snapshot, WorkflowLifecycleDelivery::Completed)
        .filter(|state| state.idempotency_key == expected_key)
        .unwrap_or(LifecycleDeliveryState {
            idempotency_key: expected_key.clone(),
            transport_acknowledged: false,
            owning_model_acknowledged: false,
        });
    (expected_key, state)
}

fn canonical_resume_lock_path(
    snapshot_path: &AbsolutePathBuf,
) -> Result<PathBuf, WorkflowServiceError> {
    let parent = snapshot_path.parent().ok_or_else(|| {
        WorkflowServiceError::Persistence(
            "workflow snapshot path has no parent for resume coordination".to_string(),
        )
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| WorkflowServiceError::Persistence(error.to_string()))?;
    let file_name = snapshot_path.file_name().ok_or_else(|| {
        WorkflowServiceError::Persistence(
            "workflow snapshot path has no file name for resume coordination".to_string(),
        )
    })?;
    Ok(canonical_parent
        .join(format!(".{}.resume.lock", file_name.to_string_lossy()))
        .to_path_buf())
}

fn workflow_children_dir(snapshot: &WorkflowTaskSnapshot) -> Result<AbsolutePathBuf, String> {
    let snapshot_stem = snapshot
        .output_file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "persisted workflow snapshot path has no valid file stem".to_string())?;
    let parent = snapshot
        .output_file
        .parent()
        .ok_or_else(|| "persisted workflow snapshot path has no parent".to_string())?;
    Ok(parent.join(format!("{snapshot_stem}.children")))
}

fn model_provider_fingerprint(config: &Config) -> String {
    json_fingerprint(serde_json::to_value(&config.model_provider).unwrap_or(JsonValue::Null))
}

fn effective_config_fingerprint(config: &Config) -> String {
    json_fingerprint(json!({
        "baseInstructionsSha256": optional_sha256(config.base_instructions.as_deref()),
        "developerInstructionsSha256": optional_sha256(config.developer_instructions.as_deref()),
        "compactPromptSha256": optional_sha256(config.compact_prompt.as_deref()),
        "guardianPolicyConfigSha256": optional_sha256(config.guardian_policy_config.as_deref()),
        "includePermissionsInstructions": config.include_permissions_instructions,
        "includeAppsInstructions": config.include_apps_instructions,
        "includeCollaborationModeInstructions": config.include_collaboration_mode_instructions,
        "includeSkillInstructions": config.include_skill_instructions,
        "orchestratorSkillsEnabled": config.orchestrator_skills_enabled,
        "orchestratorMcpEnabled": config.orchestrator_mcp_enabled,
        "includeEnvironmentContext": config.include_environment_context,
        "effectiveConfigLayersSha256": json_fingerprint(
            serde_json::to_value(config.config_layer_stack.effective_config())
                .unwrap_or(JsonValue::Null),
        ),
        "features": format!("{:?}", config.features.get()),
        "mcpServersSha256": json_fingerprint(
            serde_json::to_value(config.mcp_servers.get()).unwrap_or(JsonValue::Null),
        ),
        "mcpRuntimeState": config
            .mcp_servers
            .get()
            .iter()
            .map(|(name, server)| (
                name.clone(),
                format!("{:?}", server.disabled_reason),
            ))
            .collect::<BTreeMap<_, _>>(),
        "nonPrefixedMcpToolServers": config.non_prefixed_mcp_tool_servers,
        "projectDocMaxBytes": config.project_doc_max_bytes,
        "projectDocFallbackFilenames": config.project_doc_fallback_filenames,
        "toolOutputTokenLimit": config.tool_output_token_limit,
        "agentsEnabled": config.agents_enabled,
        "agentMaxThreads": config.agent_max_threads,
        "experimentalRequestUserInputEnabled": config.experimental_request_user_input_enabled,
        "updatePlanEnabled": config.update_plan_enabled,
        "useExperimentalUnifiedExecTool": config.features.enabled(Feature::UnifiedExec),
        "backgroundTerminalMaxTimeout": config.background_terminal_max_timeout,
        "webSearchMode": config.web_search_mode.value(),
        "webSearchConfig": config.web_search_config,
        "toolRegistry": config.tool_registry,
        "codeMode": config.code_mode,
        "multiAgentV2": config.multi_agent_v2,
        "currentTimeReminder": config.current_time_reminder,
        "appsMcpProductSku": config.apps_mcp_product_sku,
        "modelCatalogSha256": json_fingerprint(
            serde_json::to_value(&config.model_catalog).unwrap_or(JsonValue::Null),
        ),
        "personality": config.personality,
        "modelReasoningSummary": config.model_reasoning_summary,
        "modelVerbosity": config.model_verbosity,
        "responsesApiMetadata": config.responses_api_metadata,
    }))
}

fn optional_sha256(value: Option<&str>) -> Option<String> {
    value.map(sha256)
}

async fn agent_roles_fingerprint(config: &Config) -> Option<String> {
    let mut roles = Vec::with_capacity(config.agent_roles.len());
    for (name, role) in &config.agent_roles {
        let config_file_sha256 = match role.config_file.as_ref() {
            Some(path) => Some(format!(
                "{:x}",
                Sha256::digest(tokio::fs::read(path).await.ok()?)
            )),
            None => None,
        };
        roles.push(json!({
            "name": name,
            "description": role.description,
            "configFile": role.config_file.as_ref().map(|path| path.display().to_string()),
            "configFileSha256": config_file_sha256,
            "nicknameCandidates": role.nickname_candidates,
        }));
    }
    Some(json_fingerprint(JsonValue::Array(roles)))
}

fn json_fingerprint(value: JsonValue) -> String {
    fn canonicalize(value: JsonValue) -> JsonValue {
        match value {
            JsonValue::Array(values) => {
                JsonValue::Array(values.into_iter().map(canonicalize).collect())
            }
            JsonValue::Object(values) => JsonValue::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            value => value,
        }
    }

    let canonical = serde_json::to_string(&canonicalize(value)).unwrap_or_default();
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

async fn persist_lifecycle_delivery_with_retry(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
    state: &LifecycleDeliveryState,
) -> bool {
    for attempt in 0..LIFECYCLE_DELIVERY_ATTEMPTS {
        match persist_lifecycle_delivery(snapshot, lifecycle, state).await {
            Ok(()) => return true,
            Err(error) => {
                tracing::warn!(
                    run_id = %snapshot.run_id,
                    ?lifecycle,
                    %error,
                    "failed to persist workflow lifecycle delivery state"
                );
                lifecycle_delivery_backoff(attempt).await;
            }
        }
    }
    false
}

async fn lifecycle_delivery_backoff(attempt: usize) {
    if attempt.saturating_add(1) >= LIFECYCLE_DELIVERY_ATTEMPTS {
        return;
    }
    let shift = u32::try_from(attempt)
        .unwrap_or(MAX_LIFECYCLE_BACKOFF_SHIFT)
        .min(MAX_LIFECYCLE_BACKOFF_SHIFT);
    let multiplier = 1_u32 << shift;
    tokio::time::sleep(LIFECYCLE_DELIVERY_INITIAL_BACKOFF.saturating_mul(multiplier)).await;
}

async fn write_current_snapshot(
    path: impl AsRef<Path>,
    snapshot: &WorkflowTaskSnapshot,
    execution_context: &PersistedWorkflowExecutionContext,
    composition: &PersistedWorkflowComposition,
) -> Result<(), String> {
    let path = AbsolutePathBuf::try_from(path.as_ref().to_path_buf())
        .map_err(|error| error.to_string())?;
    let mut canonical_snapshot = snapshot.clone();
    canonical_snapshot.output_file = path.clone();
    let mut value = serde_json::to_value(CurrentWorkflowTaskSnapshot {
        snapshot: &canonical_snapshot,
        execution_context,
        composition,
    })
    .map_err(|error| error.to_string())?;
    if let Some(children) = value
        .get_mut("composition")
        .and_then(|composition| composition.get_mut("children"))
        .and_then(JsonValue::as_array_mut)
    {
        for child in children {
            if let Some(reference) = child
                .get_mut("reference")
                .and_then(JsonValue::as_object_mut)
                && let Some(script_path) = reference.remove("script_path")
            {
                reference.insert("scriptPath".to_string(), script_path);
            }
        }
    }
    let contents = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    write_indexed_snapshot(path, contents, &canonical_snapshot).await
}

#[cfg(test)]
async fn load_workflow_metadata(
    snapshot: &WorkflowTaskSnapshot,
) -> Result<LoadedWorkflowMetadata, String> {
    let bytes = tokio::fs::read(&snapshot.output_file)
        .await
        .map_err(|error| format!("failed to read workflow snapshot execution context: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid current workflow snapshot metadata: {error}"))
}

fn validate_task_owner(
    task: Arc<WorkflowTask>,
    thread_id: ThreadId,
) -> Result<Arc<WorkflowTask>, WorkflowServiceError> {
    let owner = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .thread_id
        .clone();
    if owner != thread_id.to_string() {
        return Err(WorkflowServiceError::WrongThread);
    }
    Ok(task)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
