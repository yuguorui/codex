use codex_config::types::McpServerTransportConfig;
use codex_core::CapturedThreadEnvironments;
use codex_core::CodexThread;
use codex_core::CodexThreadEventSubscription;
use codex_core::NewThread;
use codex_core::StartIfIdleSubmission;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::ThreadTeardownStatus;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::apply_agent_role_to_config;
use codex_core::apply_frozen_agent_role_to_config;
use codex_core::config::Config;
use codex_core::freeze_agent_roles;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ToolExecutionEnvironment;
use codex_extension_api::sha256_hex;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::user_input::UserInput;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::future::pending;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

mod completion_reconciliation;

pub use completion_reconciliation::AgentCompletionSignal;
use completion_reconciliation::ended_turn;
use completion_reconciliation::reconcile;

const LIVE_PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_PROGRESS_STATE_READ_TIMEOUT: Duration = Duration::from_secs(1);
const TERMINAL_STATUS_EVENT_GRACE_PERIOD: Duration = Duration::from_millis(250);
const SHUTDOWN_USAGE_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const TERMINAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const FORCE_CLOSE_TEARDOWN_RESERVE: Duration = Duration::from_millis(250);
/// A fully resolved agent invocation.
///
/// Agent discovery owns rendering `prompt`, including any selected skill
/// references. The runtime starts that prompt using the caller-selected spawn mode.
pub struct AgentInvocation {
    pub config: Config,
    pub prompt: String,
    /// Stable runtime-owned context inserted before the task prompt.
    pub additional_context: BTreeMap<String, AdditionalContextEntry>,
    pub parent_trace: Option<W3cTraceContext>,
}

/// Optional model settings requested for a host-orchestrated agent.
#[derive(Clone, Debug, Default)]
pub struct AgentModelOverrides {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Controls whether model validation may use fallback metadata for an unknown model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelMetadataPolicy {
    AllowFallback,
    RequireKnown,
}

/// A spawned agent whose initial turn has been submitted.
pub struct AgentRun {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub thread: Arc<CodexThread>,
    event_subscription: CodexThreadEventSubscription,
}

/// Terminal output and accounting captured from a completed agent turn.
pub struct AgentCompletion {
    pub thread_id: ThreadId,
    pub output: String,
    pub token_usage: Option<TokenUsageInfo>,
    pub tool_uses: u64,
    pub signal: AgentCompletionSignal,
}

/// Live token and tool-use accounting for an in-progress agent turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgentRunProgress {
    pub tokens: u64,
    pub tool_uses: u64,
    pub activity: Option<AgentRunActivity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRunActivity {
    AnalyzingWorkflowInputs,
}

/// A cancellable progress callback invocation.
pub type AgentProgressFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Runtime controls for an agent turn that is owned by a host-side orchestrator.
pub struct AgentCompletionOptions {
    pub output_schema: Option<JsonValue>,
    /// Maximum time between events for this turn. `None` disables progress timeout handling.
    pub progress_timeout: Option<Duration>,
    pub spawn_mode: AgentSpawnMode,
    /// Host-only capabilities attached to the fresh child thread.
    pub thread_extension_init: ExtensionDataInit,
}

/// A follow-up turn submitted to an existing host-orchestrated agent.
///
/// The target thread retains its prior conversation, so callers should send only the new
/// instruction instead of copying earlier prompts or model output into this value.
pub struct AgentFollowup {
    pub thread_id: ThreadId,
    pub prompt: String,
    /// The same stable runtime-owned context used for the initial turn.
    pub additional_context: BTreeMap<String, AdditionalContextEntry>,
    pub output_schema: Option<JsonValue>,
    /// Maximum time between events for this turn. `None` disables progress timeout handling.
    pub progress_timeout: Option<Duration>,
    pub parent_trace: Option<W3cTraceContext>,
}

/// Selects whether a host-owned agent inherits parent history or starts with resolved config only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AgentSpawnMode {
    #[default]
    ForkParent,
    FreshSubagent {
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    },
}

/// Failure from a host-orchestrated agent turn.
#[derive(Debug)]
pub enum AgentRunError {
    Codex {
        error: CodexErr,
        progress: AgentRunProgress,
    },
    Stalled {
        timeout: Duration,
        progress: AgentRunProgress,
    },
    TeardownTimedOut {
        progress: AgentRunProgress,
    },
}

impl fmt::Display for AgentRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex { error, .. } => fmt::Display::fmt(error, formatter),
            Self::Stalled { timeout, .. } => {
                write!(formatter, "agent made no progress for {timeout:?}")
            }
            Self::TeardownTimedOut { .. } => {
                formatter.write_str("agent teardown did not complete before the shutdown deadline")
            }
        }
    }
}

impl std::error::Error for AgentRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codex { error, .. } => Some(error),
            Self::Stalled { .. } | Self::TeardownTimedOut { .. } => None,
        }
    }
}

impl AgentRunError {
    pub fn progress(&self) -> AgentRunProgress {
        match self {
            Self::Codex { progress, .. }
            | Self::Stalled { progress, .. }
            | Self::TeardownTimedOut { progress } => *progress,
        }
    }
}

impl From<CodexErr> for AgentRunError {
    fn from(error: CodexErr) -> Self {
        Self::Codex {
            error,
            progress: AgentRunProgress::default(),
        }
    }
}

/// Runs resolved agents in threads owned by the supplied [`ThreadManager`].
#[derive(Clone)]
pub struct AgentRunner {
    thread_manager: Weak<ThreadManager>,
    frozen_workflow_agent_configs: Option<Arc<FrozenWorkflowAgentConfigs>>,
}

struct FrozenWorkflowAgentConfigs {
    configs: BTreeMap<String, Config>,
    project_instructions: Option<Arc<codex_core::LoadedAgentsMd>>,
}

fn instruction_summary(value: Option<&str>) -> JsonValue {
    match value {
        Some(value) => serde_json::json!({
            "sha256": sha256_hex(value),
            "byteLength": value.len(),
        }),
        None => JsonValue::Null,
    }
}

fn mcp_capability_summary(config: &Config) -> JsonValue {
    JsonValue::Array(
        config
            .mcp_servers
            .get()
            .iter()
            .map(|(name, server)| {
                let transport = match &server.transport {
                    McpServerTransportConfig::Stdio { .. } => "stdio",
                    McpServerTransportConfig::StreamableHttp { .. } => "streamableHttp",
                };
                serde_json::json!({
                    "name": name,
                    "transport": transport,
                    "enabled": server.enabled,
                    "required": server.required,
                    "supportsParallelToolCalls": server.supports_parallel_tool_calls,
                    "hasToolAllowlist": server.enabled_tools.is_some(),
                    "enabledToolCount": server.enabled_tools.as_ref().map_or(0, Vec::len),
                    "hasToolDenylist": server.disabled_tools.is_some(),
                    "disabledToolCount": server.disabled_tools.as_ref().map_or(0, Vec::len),
                })
            })
            .collect(),
    )
}

#[derive(Clone)]
pub struct AgentExecutionEnvironmentSnapshot {
    captured: CapturedThreadEnvironments,
}

impl AgentExecutionEnvironmentSnapshot {
    /// Returns the exact process-scoped executor bindings held by this snapshot.
    pub fn execution_identity(&self) -> JsonValue {
        JsonValue::Array(
            self.captured
                .descriptors()
                .into_iter()
                .map(|descriptor| {
                    let selection = descriptor.selection;
                    serde_json::json!({
                        "selection": {
                            "environmentId": selection.environment_id,
                            "cwd": selection.cwd,
                            "workspaceRoots": selection.workspace_roots,
                            "config": execution_environment_config(&selection.config),
                        },
                        "isRemote": descriptor.is_remote,
                        "executorId": descriptor.executor_id,
                    })
                })
                .collect(),
        )
    }

    /// Returns whether any captured executor is remote to this Codex process.
    pub fn has_remote_environment(&self) -> bool {
        self.captured
            .descriptors()
            .iter()
            .any(|descriptor| descriptor.is_remote)
    }
}

fn execution_environment_config(
    state: &codex_protocol::protocol::EnvironmentConfigState,
) -> JsonValue {
    match state {
        codex_protocol::protocol::EnvironmentConfigState::FromThread => {
            serde_json::json!({ "state": "fromThread" })
        }
        codex_protocol::protocol::EnvironmentConfigState::Pending => {
            serde_json::json!({ "state": "pending" })
        }
        codex_protocol::protocol::EnvironmentConfigState::Ready(config) => serde_json::json!({
            "state": "ready",
            "allowLoginShell": config.allow_login_shell,
            "permissionProfile": config.permission_profile.permission_profile(),
            "activePermissionProfile": config.permission_profile.active_permission_profile(),
            "profileWorkspaceRoots": config.permission_profile.profile_workspace_roots(),
            "selectedCapabilityRoots": config.selected_capability_roots,
        }),
        codex_protocol::protocol::EnvironmentConfigState::Failed(error) => {
            serde_json::json!({ "state": "failed", "error": error })
        }
    }
}

impl AgentRunner {
    pub fn new(thread_manager: Weak<ThreadManager>) -> Self {
        Self {
            thread_manager,
            frozen_workflow_agent_configs: None,
        }
    }

    /// Force-closes a host-orchestrated agent thread and reports whether teardown completed.
    pub async fn force_terminate(&self, thread_id: ThreadId) -> CodexResult<ThreadTeardownStatus> {
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        thread_manager
            .force_close_subagent(thread_id, TERMINAL_SHUTDOWN_TIMEOUT)
            .await
    }

    /// Resolves role files and their resulting child configurations before approval.
    pub async fn freeze_workflow_agent_configs(
        &self,
        parent_thread_id: ThreadId,
        config: &Config,
        default_model_overrides: AgentModelOverrides,
    ) -> CodexResult<(Self, JsonValue)> {
        let roles = freeze_agent_roles(config)
            .await
            .map_err(CodexErr::InvalidRequest)?;
        let project_instructions = match self.thread_manager.upgrade() {
            Some(thread_manager) => {
                let parent_thread = thread_manager.get_thread(parent_thread_id).await?;
                parent_thread.loaded_project_instructions().await
            }
            None => None,
        };
        let project_instructions_text = project_instructions
            .as_ref()
            .map(|instructions| instructions.text());
        let project_instructions_summary =
            instruction_summary(project_instructions_text.as_deref());
        let mut configs = BTreeMap::new();
        let mut summaries = Vec::new();
        for role_name in roles.names() {
            let mut child_config = config.clone();
            self.apply_model_overrides(&mut child_config, default_model_overrides.clone())
                .await?;
            let model_before_role = child_config.model.clone();
            let effort_before_role = child_config.model_reasoning_effort.clone();
            apply_frozen_agent_role_to_config(&mut child_config, &roles, Some(role_name))
                .await
                .map_err(CodexErr::InvalidRequest)?;
            if child_config.model != model_before_role
                || child_config.model_reasoning_effort != effort_before_role
            {
                self.validate_model_configuration(
                    &mut child_config,
                    ModelMetadataPolicy::AllowFallback,
                )
                .await?;
            }
            summaries.push(serde_json::json!({
                "role": role_name,
                "model": &child_config.model,
                "reasoningEffort": &child_config.model_reasoning_effort,
                "serviceTier": &child_config.service_tier,
                "instructions": {
                    "base": instruction_summary(child_config.base_instructions.as_deref()),
                    "developer": instruction_summary(child_config.developer_instructions.as_deref()),
                    "project": &project_instructions_summary,
                },
                "mcpCapabilities": mcp_capability_summary(&child_config),
            }));
            configs.insert(role_name.to_string(), child_config);
        }
        let mut frozen = self.clone();
        frozen.frozen_workflow_agent_configs = Some(Arc::new(FrozenWorkflowAgentConfigs {
            configs,
            project_instructions,
        }));
        Ok((
            frozen,
            serde_json::json!({
                "roles": roles.approval_value(),
                "resolvedChildConfigs": summaries,
            }),
        ))
    }

    /// Returns a child configuration resolved at the Workflow approval boundary.
    pub fn frozen_workflow_agent_config(
        &self,
        agent_type: Option<&str>,
    ) -> CodexResult<Option<Config>> {
        let Some(frozen) = &self.frozen_workflow_agent_configs else {
            return Ok(None);
        };
        let role_name = agent_type.unwrap_or(codex_core::DEFAULT_AGENT_ROLE_NAME);
        frozen
            .configs
            .get(role_name)
            .cloned()
            .map(Some)
            .ok_or_else(|| CodexErr::InvalidRequest(format!("unknown agent_type '{role_name}'")))
    }

    /// Captures and validates the concrete executor bindings visible to an extension tool call.
    pub async fn capture_execution_environments(
        &self,
        parent_thread_id: ThreadId,
        environments: &[ToolExecutionEnvironment],
    ) -> CodexResult<AgentExecutionEnvironmentSnapshot> {
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let captured = thread_manager
            .capture_thread_environments(parent_thread_id)
            .await?;
        let descriptors = captured.descriptors();
        if descriptors.len() != environments.len()
            || descriptors
                .iter()
                .zip(environments)
                .any(|(captured, expected)| {
                    captured.selection.environment_id != expected.environment_id
                        || captured.selection.cwd != expected.cwd
                        || expected.selection.as_ref() != Some(&captured.selection)
                        || captured.is_remote != expected.is_remote
                        || captured.executor_id != expected.executor_id
                })
            || !captured.has_same_executors(environments)
        {
            return Err(CodexErr::InvalidRequest(
                "workflow execution environments changed before they could be captured".to_string(),
            ));
        }
        Ok(AgentExecutionEnvironmentSnapshot { captured })
    }

    /// Applies an Agent-tool role to a host-orchestrated child configuration.
    ///
    /// Callers can apply their own isolation policy and explicit model overrides after this
    /// resolves, while sharing the same built-in and user-defined role registry as Agent v1/v2.
    pub async fn apply_role_to_config(
        &self,
        config: &mut Config,
        agent_type: &str,
    ) -> CodexResult<()> {
        self.apply_optional_role_to_config(config, Some(agent_type))
            .await
    }

    /// Applies an optional Agent-tool role, leaving the configuration unchanged when absent.
    pub async fn apply_optional_role_to_config(
        &self,
        config: &mut Config,
        agent_type: Option<&str>,
    ) -> CodexResult<()> {
        apply_agent_role_to_config(config, agent_type)
            .await
            .map_err(CodexErr::InvalidRequest)
    }

    /// Applies explicit/default subagent model settings using the same effort semantics as
    /// `spawn_agent`: selecting a model without an effort selects that model's default effort.
    pub async fn apply_model_overrides(
        &self,
        config: &mut Config,
        overrides: AgentModelOverrides,
    ) -> CodexResult<()> {
        let AgentModelOverrides {
            model: requested_model,
            reasoning_effort: requested_reasoning_effort,
        } = overrides;
        if requested_model.is_none() && requested_reasoning_effort.is_none() {
            return Ok(());
        }

        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let model = requested_model
            .clone()
            .or_else(|| config.model.clone())
            .ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "workflow agent could not resolve the child model".to_string(),
                )
            })?;
        let model_info = thread_manager
            .get_models_manager()
            .get_model_info(&model, &config.to_models_manager_config())
            .await;
        if requested_model.is_some() && model_info.used_fallback_model_metadata {
            return Err(CodexErr::InvalidRequest(format!(
                "Unknown model `{model}` for workflow agent"
            )));
        }
        if !model_info.used_fallback_model_metadata
            && let Some(reasoning_effort) = requested_reasoning_effort.as_ref()
        {
            validate_reasoning_effort(
                &model,
                &model_info.supported_reasoning_levels,
                reasoning_effort,
            )?;
        }

        if requested_model.is_some() {
            config.model = Some(model);
            config.model_reasoning_effort =
                requested_reasoning_effort.or_else(|| model_info.default_reasoning_level.clone());
        } else {
            config.model_reasoning_effort = requested_reasoning_effort;
        }
        if config
            .service_tier
            .as_deref()
            .is_some_and(|service_tier| !model_info.supports_service_tier(service_tier))
        {
            config.service_tier = None;
        }
        Ok(())
    }

    /// Validates workflow-requested model settings against the same model metadata used by
    /// `spawn_agent`, and drops an inherited service tier when the resolved model cannot use it.
    pub async fn validate_model_configuration(
        &self,
        config: &mut Config,
        metadata_policy: ModelMetadataPolicy,
    ) -> CodexResult<()> {
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let model = config.model.clone().ok_or_else(|| {
            CodexErr::InvalidRequest("workflow agent could not resolve the child model".to_string())
        })?;
        let model_info = thread_manager
            .get_models_manager()
            .get_model_info(&model, &config.to_models_manager_config())
            .await;
        if metadata_policy == ModelMetadataPolicy::RequireKnown
            && model_info.used_fallback_model_metadata
        {
            return Err(CodexErr::InvalidRequest(format!(
                "Unknown model `{model}` for workflow agent"
            )));
        }
        if !model_info.used_fallback_model_metadata
            && let Some(reasoning_effort) = config.model_reasoning_effort.as_ref()
        {
            validate_reasoning_effort(
                &model,
                &model_info.supported_reasoning_levels,
                reasoning_effort,
            )?;
        }
        if config
            .service_tier
            .as_deref()
            .is_some_and(|service_tier| !model_info.supports_service_tier(service_tier))
        {
            config.service_tier = None;
        }
        Ok(())
    }

    /// Starts a resolved agent in a fork of `parent_thread_id`.
    pub async fn start(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
    ) -> CodexResult<AgentRun> {
        self.start_with_output_schema(
            parent_thread_id,
            invocation,
            None,
            AgentSpawnMode::ForkParent,
            /*environments*/ None,
            /*captured_environments*/ None,
            ExtensionDataInit::default(),
        )
        .await
    }

    /// Runs an agent turn to completion without depending on model-visible
    /// multi-agent tool syntax.
    pub async fn run_to_completion(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        output_schema: Option<JsonValue>,
        cancellation: CancellationToken,
    ) -> CodexResult<AgentCompletion> {
        match self
            .run_to_completion_with_options(
                parent_thread_id,
                invocation,
                AgentCompletionOptions {
                    output_schema,
                    progress_timeout: None,
                    spawn_mode: AgentSpawnMode::ForkParent,
                    thread_extension_init: ExtensionDataInit::default(),
                },
                cancellation,
            )
            .await
        {
            Ok(completion) => Ok(completion),
            Err(AgentRunError::Codex { error, .. }) => Err(error),
            Err(AgentRunError::Stalled { .. } | AgentRunError::TeardownTimedOut { .. }) => {
                Err(CodexErr::RequestTimeout)
            }
        }
    }

    /// Runs an agent while tracking tool use and optionally enforcing a no-progress timeout.
    pub async fn run_to_completion_with_options(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        options: AgentCompletionOptions,
        cancellation: CancellationToken,
    ) -> Result<AgentCompletion, AgentRunError> {
        self.run_to_completion_with_options_and_started(
            parent_thread_id,
            invocation,
            options,
            cancellation,
            |_| {},
        )
        .await
    }

    /// Runs an agent to completion and reports its thread id immediately after startup.
    pub async fn run_to_completion_with_options_and_started(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        options: AgentCompletionOptions,
        cancellation: CancellationToken,
        on_started: impl FnOnce(ThreadId) + Send,
    ) -> Result<AgentCompletion, AgentRunError> {
        self.run_to_completion_with_progress(
            parent_thread_id,
            invocation,
            options,
            cancellation,
            on_started,
            |_| Box::pin(async {}),
        )
        .await
    }

    /// Runs an agent to completion while reporting live token and tool-use progress.
    pub async fn run_to_completion_with_progress<'a>(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        options: AgentCompletionOptions,
        cancellation: CancellationToken,
        on_started: impl FnOnce(ThreadId) + Send + 'a,
        on_progress: impl Fn(AgentRunProgress) -> AgentProgressFuture<'a> + Send + Sync + 'a,
    ) -> Result<AgentCompletion, AgentRunError> {
        self.run_to_completion_with_progress_in_environments(
            parent_thread_id,
            invocation,
            options,
            cancellation,
            /*environments*/ None,
            /*captured_environments*/ None,
            on_started,
            on_progress,
        )
        .await
    }

    /// Runs an agent with the environment selection captured by a host-side orchestrator.
    ///
    /// This is used when execution begins asynchronously after the parent turn, so a later
    /// environment selection cannot redirect the child to a different executor or filesystem.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_to_completion_with_progress_in_environments<'a>(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        options: AgentCompletionOptions,
        cancellation: CancellationToken,
        environments: Option<Vec<TurnEnvironmentSelection>>,
        captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
        on_started: impl FnOnce(ThreadId) + Send + 'a,
        on_progress: impl Fn(AgentRunProgress) -> AgentProgressFuture<'a> + Send + Sync + 'a,
    ) -> Result<AgentCompletion, AgentRunError> {
        let AgentCompletionOptions {
            output_schema,
            progress_timeout,
            spawn_mode,
            thread_extension_init,
        } = options;
        let run = self
            .start_with_output_schema(
                parent_thread_id,
                invocation,
                output_schema,
                spawn_mode,
                environments,
                captured_environments,
                thread_extension_init,
            )
            .await?;
        on_started(run.thread_id);
        self.wait_for_completion(
            run,
            progress_timeout,
            cancellation,
            /*on_progress*/ Some(&on_progress),
        )
        .await
    }

    /// Runs a follow-up turn in an existing host-orchestrated agent thread.
    ///
    /// This preserves the agent's conversation history and is intended for validation nudges and
    /// other continuations that should not create a fresh subagent.
    pub async fn run_followup_to_completion(
        &self,
        followup: AgentFollowup,
        cancellation: CancellationToken,
    ) -> Result<AgentCompletion, AgentRunError> {
        let AgentFollowup {
            thread_id,
            prompt,
            additional_context,
            output_schema,
            progress_timeout,
            parent_trace,
        } = followup;
        validate_prompt(&prompt)?;
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let thread = thread_manager.get_thread(thread_id).await?;
        let event_subscription = thread.subscribe_events();
        let turn_id = submit_prompt(
            &thread,
            prompt,
            additional_context,
            output_schema,
            parent_trace,
        )
        .await?;
        let on_progress = |_| Box::pin(std::future::ready(())) as AgentProgressFuture<'_>;
        self.wait_for_completion(
            AgentRun {
                thread_id,
                turn_id,
                thread,
                event_subscription,
            },
            progress_timeout,
            cancellation,
            Some(&on_progress),
        )
        .await
    }

    async fn wait_for_completion<'a>(
        &self,
        run: impl AgentCompletionSource,
        progress_timeout: Option<Duration>,
        cancellation: CancellationToken,
        on_progress: Option<
            &(dyn Fn(AgentRunProgress) -> AgentProgressFuture<'a> + Send + Sync + 'a),
        >,
    ) -> Result<AgentCompletion, AgentRunError> {
        let freshest_tokens = AtomicU64::new(0);
        let mut last_usage = None;
        let mut tool_uses = 0_u64;
        let mut active_tool_count = 0_usize;
        let mut current_activity = None;
        let mut saw_current_turn_event = false;
        let mut progress_deadline = progress_timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let outcome = {
                let deadline = progress_deadline;
                let status_reconciliation_enabled = saw_current_turn_event;
                let stall = async move {
                    match (deadline, progress_timeout) {
                        (Some(deadline), Some(timeout)) => {
                            tokio::time::sleep_until(deadline).await;
                            timeout
                        }
                        _ => pending().await,
                    }
                };
                tokio::pin!(stall);
                let report = async {
                    if on_progress.is_none() {
                        return pending::<AgentProgressSample>().await;
                    }
                    tokio::time::sleep(LIVE_PROGRESS_REPORT_INTERVAL).await;
                    let (usage, status) = tokio::join!(
                        tokio::time::timeout(LIVE_PROGRESS_STATE_READ_TIMEOUT, run.sample_usage(),),
                        tokio::time::timeout(LIVE_PROGRESS_STATE_READ_TIMEOUT, run.agent_status(),),
                    );
                    let status = status.ok().filter(|_| status_reconciliation_enabled);
                    if ended_turn(status.as_ref()) {
                        // Status is updated before the event is forwarded to passive subscribers.
                        // Give the authoritative turn event a short opportunity to win the race.
                        tokio::time::sleep(TERMINAL_STATUS_EVENT_GRACE_PERIOD).await;
                    }
                    AgentProgressSample {
                        usage: usage.ok(),
                        status,
                    }
                };
                tokio::pin!(report);
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => AgentWaitOutcome::Cancelled,
                    _timeout = &mut stall => AgentWaitOutcome::Stalled,
                    event = run.next_completion_event() => AgentWaitOutcome::Event(event),
                    sample = &mut report => AgentWaitOutcome::ProgressSample(sample),
                }
            };
            let event = match outcome {
                AgentWaitOutcome::Cancelled => {
                    return Err(interrupted_after_shutdown(
                        shutdown_terminal_progress(
                            &run,
                            AgentRunProgress {
                                tokens: freshest_tokens.load(Ordering::Relaxed),
                                tool_uses,
                                activity: current_activity,
                            },
                            TERMINAL_SHUTDOWN_TIMEOUT,
                        )
                        .await,
                    ));
                }
                AgentWaitOutcome::Stalled => {
                    match check_progress_deadline(
                        &run,
                        active_tool_count,
                        progress_timeout,
                        progress_deadline,
                    )
                    .await
                    {
                        ProgressDeadlineCheck::NotExpired | ProgressDeadlineCheck::ConcreteWork => {
                            progress_deadline =
                                progress_timeout.map(|timeout| Instant::now() + timeout);
                            continue;
                        }
                        ProgressDeadlineCheck::Stalled(timeout) => {
                            return Err(stalled_after_shutdown(
                                timeout,
                                shutdown_terminal_progress(
                                    &run,
                                    AgentRunProgress {
                                        tokens: freshest_tokens.load(Ordering::Relaxed),
                                        tool_uses,
                                        activity: current_activity,
                                    },
                                    TERMINAL_SHUTDOWN_TIMEOUT,
                                )
                                .await,
                            ));
                        }
                    }
                }
                AgentWaitOutcome::Event(Ok(event)) => event,
                AgentWaitOutcome::Event(Err(error)) => {
                    if cancellation.is_cancelled() {
                        return Err(interrupted_after_shutdown(
                            shutdown_terminal_progress(
                                &run,
                                AgentRunProgress {
                                    tokens: freshest_tokens.load(Ordering::Relaxed),
                                    tool_uses,
                                    activity: current_activity,
                                },
                                TERMINAL_SHUTDOWN_TIMEOUT,
                            )
                            .await,
                        ));
                    }
                    match check_progress_deadline(
                        &run,
                        active_tool_count,
                        progress_timeout,
                        progress_deadline,
                    )
                    .await
                    {
                        ProgressDeadlineCheck::NotExpired | ProgressDeadlineCheck::ConcreteWork => {
                        }
                        ProgressDeadlineCheck::Stalled(timeout) => {
                            return Err(stalled_after_shutdown(
                                timeout,
                                shutdown_terminal_progress(
                                    &run,
                                    AgentRunProgress {
                                        tokens: freshest_tokens.load(Ordering::Relaxed),
                                        tool_uses,
                                        activity: current_activity,
                                    },
                                    TERMINAL_SHUTDOWN_TIMEOUT,
                                )
                                .await,
                            ));
                        }
                    }
                    return Err(AgentRunError::Codex {
                        error,
                        progress: AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                    });
                }
                AgentWaitOutcome::ProgressSample(sample) => {
                    let usage_sampled = sample.usage.is_some();
                    if let Some(usage) = sample.usage {
                        merge_token_usage(&mut last_usage, usage.clone());
                        freshest_tokens.fetch_max(
                            usage.as_ref().and_then(token_count).unwrap_or_default(),
                            Ordering::Relaxed,
                        );
                    }
                    if cancellation.is_cancelled() {
                        return Err(interrupted_after_shutdown(
                            shutdown_terminal_progress(
                                &run,
                                AgentRunProgress {
                                    tokens: freshest_tokens.load(Ordering::Relaxed),
                                    tool_uses,
                                    activity: current_activity,
                                },
                                TERMINAL_SHUTDOWN_TIMEOUT,
                            )
                            .await,
                        ));
                    }
                    match check_progress_deadline(
                        &run,
                        active_tool_count,
                        progress_timeout,
                        progress_deadline,
                    )
                    .await
                    {
                        ProgressDeadlineCheck::NotExpired => {}
                        ProgressDeadlineCheck::ConcreteWork => {
                            progress_deadline =
                                progress_timeout.map(|timeout| Instant::now() + timeout);
                        }
                        ProgressDeadlineCheck::Stalled(timeout) => {
                            return Err(stalled_after_shutdown(
                                timeout,
                                shutdown_terminal_progress(
                                    &run,
                                    AgentRunProgress {
                                        tokens: freshest_tokens.load(Ordering::Relaxed),
                                        tool_uses,
                                        activity: current_activity,
                                    },
                                    TERMINAL_SHUTDOWN_TIMEOUT,
                                )
                                .await,
                            ));
                        }
                    }
                    if let Some(completion) = reconcile(
                        sample.status,
                        run.thread_id(),
                        last_usage.as_ref(),
                        AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                    )? {
                        return Ok(completion);
                    }
                    if !usage_sampled {
                        continue;
                    }
                    let Some(on_progress) = on_progress else {
                        unreachable!("reporting requires a progress callback");
                    };
                    let callback_outcome = {
                        let callback = on_progress(AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        });
                        tokio::pin!(callback);
                        let callback_stall = async {
                            match (progress_deadline, progress_timeout) {
                                (Some(deadline), Some(timeout)) => {
                                    tokio::time::sleep_until(deadline).await;
                                    timeout
                                }
                                _ => pending().await,
                            }
                        };
                        tokio::pin!(callback_stall);
                        tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => ProgressCallbackOutcome::Cancelled,
                            _timeout = &mut callback_stall => {
                                ProgressCallbackOutcome::Stalled
                            }
                            _ = &mut callback => ProgressCallbackOutcome::Completed,
                        }
                    };
                    match callback_outcome {
                        ProgressCallbackOutcome::Cancelled => {
                            return Err(interrupted_after_shutdown(
                                shutdown_terminal_progress(
                                    &run,
                                    AgentRunProgress {
                                        tokens: freshest_tokens.load(Ordering::Relaxed),
                                        tool_uses,
                                        activity: current_activity,
                                    },
                                    TERMINAL_SHUTDOWN_TIMEOUT,
                                )
                                .await,
                            ));
                        }
                        ProgressCallbackOutcome::Stalled => match check_progress_deadline(
                            &run,
                            active_tool_count,
                            progress_timeout,
                            progress_deadline,
                        )
                        .await
                        {
                            ProgressDeadlineCheck::NotExpired
                            | ProgressDeadlineCheck::ConcreteWork => {
                                progress_deadline =
                                    progress_timeout.map(|timeout| Instant::now() + timeout);
                                continue;
                            }
                            ProgressDeadlineCheck::Stalled(timeout) => {
                                return Err(stalled_after_shutdown(
                                    timeout,
                                    shutdown_terminal_progress(
                                        &run,
                                        AgentRunProgress {
                                            tokens: freshest_tokens.load(Ordering::Relaxed),
                                            tool_uses,
                                            activity: current_activity,
                                        },
                                        TERMINAL_SHUTDOWN_TIMEOUT,
                                    )
                                    .await,
                                ));
                            }
                        },
                        ProgressCallbackOutcome::Completed => continue,
                    }
                }
            };

            if !matches!(&event, AgentCompletionEvent::OtherTurn) {
                saw_current_turn_event = true;
            }
            match &event {
                AgentCompletionEvent::ToolStarted(_) => {
                    tool_uses = tool_uses.saturating_add(1);
                    active_tool_count = active_tool_count.saturating_add(1);
                }
                AgentCompletionEvent::ToolCompleted(_) => {
                    active_tool_count = active_tool_count.saturating_sub(1);
                }
                AgentCompletionEvent::Usage(usage) => {
                    merge_token_usage(&mut last_usage, usage.clone());
                    freshest_tokens.fetch_max(
                        usage.as_ref().and_then(token_count).unwrap_or_default(),
                        Ordering::Relaxed,
                    );
                }
                AgentCompletionEvent::Completed { .. }
                | AgentCompletionEvent::Aborted
                | AgentCompletionEvent::Shutdown
                | AgentCompletionEvent::Error(_)
                | AgentCompletionEvent::CurrentActivity
                | AgentCompletionEvent::OtherTurn => {}
            }
            if cancellation.is_cancelled() {
                return Err(interrupted_after_shutdown(
                    shutdown_terminal_progress(
                        &run,
                        AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                        TERMINAL_SHUTDOWN_TIMEOUT,
                    )
                    .await,
                ));
            }
            match check_progress_deadline(
                &run,
                active_tool_count,
                progress_timeout,
                progress_deadline,
            )
            .await
            {
                ProgressDeadlineCheck::NotExpired => {}
                ProgressDeadlineCheck::ConcreteWork => {
                    progress_deadline = progress_timeout.map(|timeout| Instant::now() + timeout);
                }
                ProgressDeadlineCheck::Stalled(timeout) => {
                    return Err(stalled_after_shutdown(
                        timeout,
                        shutdown_terminal_progress(
                            &run,
                            AgentRunProgress {
                                tokens: freshest_tokens.load(Ordering::Relaxed),
                                tool_uses,
                                activity: current_activity,
                            },
                            TERMINAL_SHUTDOWN_TIMEOUT,
                        )
                        .await,
                    ));
                }
            }
            if !matches!(&event, AgentCompletionEvent::OtherTurn) {
                progress_deadline = progress_timeout.map(|timeout| Instant::now() + timeout);
            }
            match event {
                AgentCompletionEvent::ToolStarted(activity) => {
                    if let Some(activity) = activity {
                        current_activity = Some(activity);
                        report_agent_activity(
                            on_progress,
                            AgentRunProgress {
                                tokens: freshest_tokens.load(Ordering::Relaxed),
                                tool_uses,
                                activity: current_activity,
                            },
                        )
                        .await;
                    }
                }
                AgentCompletionEvent::ToolCompleted(activity) => {
                    if let Some(activity) = activity
                        && current_activity == Some(activity)
                    {
                        current_activity = None;
                        report_agent_activity(
                            on_progress,
                            AgentRunProgress {
                                tokens: freshest_tokens.load(Ordering::Relaxed),
                                tool_uses,
                                activity: current_activity,
                            },
                        )
                        .await;
                    }
                }
                AgentCompletionEvent::Completed { output, error } => {
                    if let Some(error) = error {
                        return Err(AgentRunError::Codex {
                            error: CodexErr::Fatal(error),
                            progress: AgentRunProgress {
                                tokens: freshest_tokens.load(Ordering::Relaxed),
                                tool_uses,
                                activity: current_activity,
                            },
                        });
                    }
                    return Ok(AgentCompletion {
                        thread_id: run.thread_id(),
                        output,
                        token_usage: match tokio::time::timeout(
                            LIVE_PROGRESS_STATE_READ_TIMEOUT,
                            run.completion_token_usage(),
                        )
                        .await
                        {
                            Ok(usage) => usage.or(last_usage),
                            Err(_) => last_usage,
                        },
                        tool_uses,
                        signal: AgentCompletionSignal::Event,
                    });
                }
                AgentCompletionEvent::Aborted => {
                    return Err(AgentRunError::Codex {
                        error: CodexErr::Interrupted,
                        progress: AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                    });
                }
                AgentCompletionEvent::Shutdown => {
                    let error = match tokio::time::timeout(
                        LIVE_PROGRESS_STATE_READ_TIMEOUT,
                        run.agent_status(),
                    )
                    .await
                    {
                        Ok(AgentStatus::Interrupted) => CodexErr::Interrupted,
                        Ok(AgentStatus::Errored(message)) => {
                            CodexErr::Fatal(format!("agent failed: {message}"))
                        }
                        Ok(AgentStatus::NotFound) => CodexErr::ThreadNotFound(run.thread_id()),
                        Ok(
                            AgentStatus::PendingInit
                            | AgentStatus::Running
                            | AgentStatus::Completed(_)
                            | AgentStatus::Shutdown,
                        ) => CodexErr::Fatal("agent shut down before completing".to_string()),
                        Err(_) => CodexErr::Fatal(
                            "agent status read timed out after shutdown".to_string(),
                        ),
                    };
                    return Err(AgentRunError::Codex {
                        error,
                        progress: AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                    });
                }
                AgentCompletionEvent::Error(error) => {
                    return Err(AgentRunError::Codex {
                        error: CodexErr::Fatal(error),
                        progress: AgentRunProgress {
                            tokens: freshest_tokens.load(Ordering::Relaxed),
                            tool_uses,
                            activity: current_activity,
                        },
                    });
                }
                AgentCompletionEvent::Usage(_) => {}
                AgentCompletionEvent::CurrentActivity | AgentCompletionEvent::OtherTurn => {}
            }
        }
    }

    async fn start_with_output_schema(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        output_schema: Option<JsonValue>,
        spawn_mode: AgentSpawnMode,
        environments: Option<Vec<TurnEnvironmentSelection>>,
        captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
        thread_extension_init: ExtensionDataInit,
    ) -> CodexResult<AgentRun> {
        let AgentInvocation {
            config,
            prompt,
            additional_context,
            parent_trace,
        } = invocation;
        validate_prompt(&prompt)?;

        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let mut start_options = StartThreadOptions {
            parent_trace: parent_trace.clone(),
            environments,
            captured_environments: captured_environments.map(|snapshot| snapshot.captured),
            frozen_project_instructions: self
                .frozen_workflow_agent_configs
                .as_ref()
                .and_then(|frozen| frozen.project_instructions.clone()),
            thread_extension_init,
            ..StartThreadOptions::new(config)
        };
        let NewThread {
            thread_id, thread, ..
        } = match spawn_mode {
            AgentSpawnMode::ForkParent => {
                thread_manager
                    .spawn_subagent(parent_thread_id, start_options)
                    .await?
            }
            AgentSpawnMode::FreshSubagent {
                agent_nickname,
                agent_role,
            } => {
                start_options.session_source =
                    Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id,
                        depth: 1,
                        agent_path: None,
                        agent_nickname,
                        agent_role,
                    }));
                start_options.thread_source = Some(ThreadSource::Subagent);
                thread_manager
                    .start_fresh_subagent_without_rollout_budget(parent_thread_id, start_options)
                    .await?
            }
        };
        let event_subscription = thread.subscribe_events();
        let turn_id = submit_prompt(
            &thread,
            prompt,
            additional_context,
            output_schema,
            parent_trace,
        )
        .await?;

        Ok(AgentRun {
            thread_id,
            turn_id,
            thread,
            event_subscription,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgentCompletionEvent {
    ToolStarted(Option<AgentRunActivity>),
    ToolCompleted(Option<AgentRunActivity>),
    Completed {
        output: String,
        error: Option<String>,
    },
    Aborted,
    Shutdown,
    Error(String),
    Usage(Option<TokenUsageInfo>),
    CurrentActivity,
    OtherTurn,
}

enum ProgressCallbackOutcome {
    Completed,
    Cancelled,
    Stalled,
}

async fn report_agent_activity<'a>(
    on_progress: Option<&(dyn Fn(AgentRunProgress) -> AgentProgressFuture<'a> + Send + Sync + 'a)>,
    progress: AgentRunProgress,
) {
    if let Some(on_progress) = on_progress {
        on_progress(progress).await;
    }
}

enum AgentWaitOutcome {
    Cancelled,
    Stalled,
    Event(CodexResult<AgentCompletionEvent>),
    ProgressSample(AgentProgressSample),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentActualWork {
    None,
    ActiveTool,
    TrackedProcess,
    ModelStream,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressDeadlineCheck {
    NotExpired,
    ConcreteWork,
    Stalled(Duration),
}

struct AgentProgressSample {
    usage: Option<Option<TokenUsageInfo>>,
    status: Option<AgentStatus>,
}

/// Supplies the event and state boundaries used to await a host-orchestrated agent turn.
trait AgentCompletionSource: Send + Sync {
    fn thread_id(&self) -> ThreadId;

    fn submit_interrupt(&self) -> impl Future<Output = ()> + Send;

    fn force_terminate(
        &self,
        timeout: Duration,
    ) -> impl Future<Output = ThreadTeardownStatus> + Send;

    fn next_completion_event(
        &self,
    ) -> impl Future<Output = CodexResult<AgentCompletionEvent>> + Send;

    fn sample_usage(&self) -> impl Future<Output = Option<TokenUsageInfo>> + Send;

    fn final_progress(&self, tool_uses: u64) -> impl Future<Output = AgentRunProgress> + Send;

    fn completion_token_usage(&self) -> impl Future<Output = Option<TokenUsageInfo>> + Send;

    fn agent_status(&self) -> impl Future<Output = AgentStatus> + Send;

    fn actual_work(&self, active_tool_count: usize)
    -> impl Future<Output = AgentActualWork> + Send;
}

impl AgentCompletionSource for AgentRun {
    fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    async fn submit_interrupt(&self) {
        let _ = self.thread.submit(Op::Interrupt).await;
    }

    async fn force_terminate(&self, timeout: Duration) -> ThreadTeardownStatus {
        self.thread.force_close(timeout).await
    }

    async fn next_completion_event(&self) -> CodexResult<AgentCompletionEvent> {
        let event = self.event_subscription.next_event().await?;
        if event.id != self.turn_id {
            return Ok(AgentCompletionEvent::OtherTurn);
        }
        Ok(match event.msg {
            EventMsg::ItemStarted(item_started) if is_tool_item(&item_started.item) => {
                AgentCompletionEvent::ToolStarted(agent_activity(&item_started.item))
            }
            EventMsg::ItemCompleted(item_completed) if is_tool_item(&item_completed.item) => {
                AgentCompletionEvent::ToolCompleted(agent_activity(&item_completed.item))
            }
            EventMsg::TurnComplete(completed) if completed.turn_id == self.turn_id => {
                AgentCompletionEvent::Completed {
                    output: completed.last_agent_message.unwrap_or_default(),
                    error: completed.error.map(|error| error.message),
                }
            }
            EventMsg::TurnAborted(_) => AgentCompletionEvent::Aborted,
            EventMsg::ShutdownComplete => AgentCompletionEvent::Shutdown,
            EventMsg::Error(error) => AgentCompletionEvent::Error(error.message),
            EventMsg::TokenCount(event) => AgentCompletionEvent::Usage(event.info),
            _ => AgentCompletionEvent::CurrentActivity,
        })
    }

    async fn sample_usage(&self) -> Option<TokenUsageInfo> {
        self.thread.token_usage_info().await
    }

    async fn final_progress(&self, tool_uses: u64) -> AgentRunProgress {
        AgentRunProgress {
            tokens: self
                .sample_usage()
                .await
                .as_ref()
                .and_then(token_count)
                .unwrap_or_default(),
            tool_uses,
            activity: None,
        }
    }

    async fn completion_token_usage(&self) -> Option<TokenUsageInfo> {
        self.thread.token_usage_info().await
    }

    async fn agent_status(&self) -> AgentStatus {
        self.thread.agent_status().await
    }

    async fn actual_work(&self, active_tool_count: usize) -> AgentActualWork {
        if active_tool_count > 0 {
            return AgentActualWork::ActiveTool;
        }
        if self.thread.has_live_tracked_processes().await {
            return AgentActualWork::TrackedProcess;
        }
        if self.thread.model_stream_active().await {
            return AgentActualWork::ModelStream;
        }
        AgentActualWork::None
    }
}

fn token_count(usage: &TokenUsageInfo) -> Option<u64> {
    u64::try_from(usage.total_token_usage.total_tokens).ok()
}

fn merge_token_usage(current: &mut Option<TokenUsageInfo>, candidate: Option<TokenUsageInfo>) {
    let Some(candidate) = candidate else {
        return;
    };
    let candidate_tokens = token_count(&candidate).unwrap_or_default();
    let current_tokens = current.as_ref().and_then(token_count).unwrap_or_default();
    if candidate_tokens >= current_tokens {
        *current = Some(candidate);
    }
}

async fn check_progress_deadline(
    source: &impl AgentCompletionSource,
    active_tool_count: usize,
    progress_timeout: Option<Duration>,
    progress_deadline: Option<Instant>,
) -> ProgressDeadlineCheck {
    let (timeout, deadline) = match (progress_timeout, progress_deadline) {
        (Some(timeout), Some(deadline)) => (timeout, deadline),
        _ => return ProgressDeadlineCheck::NotExpired,
    };
    if Instant::now() < deadline {
        return ProgressDeadlineCheck::NotExpired;
    }

    let actual_work = tokio::time::timeout(
        LIVE_PROGRESS_STATE_READ_TIMEOUT,
        source.actual_work(active_tool_count),
    )
    .await
    .unwrap_or(AgentActualWork::Unknown);
    if actual_work == AgentActualWork::None {
        ProgressDeadlineCheck::Stalled(timeout)
    } else {
        ProgressDeadlineCheck::ConcreteWork
    }
}

async fn shutdown_terminal_progress(
    source: &impl AgentCompletionSource,
    progress: AgentRunProgress,
    timeout: Duration,
) -> Result<AgentRunProgress, AgentRunProgress> {
    let now = Instant::now();
    let deadline = now + timeout;
    let force_close_at = now + timeout.saturating_sub(FORCE_CLOSE_TEARDOWN_RESERVE);
    let freshest_tokens = AtomicU64::new(progress.tokens);
    let tool_uses = AtomicU64::new(progress.tool_uses);
    let shutdown = async {
        source.submit_interrupt().await;
        loop {
            match source.next_completion_event().await {
                Ok(AgentCompletionEvent::ToolStarted(_)) => {
                    let _ =
                        tool_uses.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tool_uses| {
                            Some(tool_uses.saturating_add(1))
                        });
                }
                Ok(AgentCompletionEvent::Usage(usage)) => {
                    freshest_tokens.fetch_max(
                        usage.as_ref().and_then(token_count).unwrap_or_default(),
                        Ordering::Relaxed,
                    );
                }
                Ok(
                    AgentCompletionEvent::Completed { .. }
                    | AgentCompletionEvent::Aborted
                    | AgentCompletionEvent::Shutdown
                    | AgentCompletionEvent::Error(_),
                )
                | Err(_) => break,
                Ok(
                    AgentCompletionEvent::ToolCompleted(_)
                    | AgentCompletionEvent::CurrentActivity
                    | AgentCompletionEvent::OtherTurn,
                ) => {}
            }
            tokio::task::yield_now().await;
        }
        source
            .final_progress(tool_uses.load(Ordering::Relaxed))
            .await
    };
    let sample_usage = async {
        let mut interval = tokio::time::interval(SHUTDOWN_USAGE_SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            freshest_tokens.fetch_max(
                source
                    .sample_usage()
                    .await
                    .as_ref()
                    .and_then(token_count)
                    .unwrap_or_default(),
                Ordering::Relaxed,
            );
        }
    };
    tokio::pin!(shutdown);
    tokio::pin!(sample_usage);
    let mut force_close = std::pin::pin!(tokio::time::sleep_until(force_close_at));
    let mut final_progress = tokio::select! {
        biased;
        final_progress = &mut shutdown => Some(final_progress),
        _ = &mut sample_usage => unreachable!("usage sampling runs until shutdown finishes"),
        _ = &mut force_close => None,
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    let force_terminate = source.force_terminate(remaining);
    tokio::pin!(force_terminate);
    let mut deadline = std::pin::pin!(tokio::time::sleep_until(deadline));
    let mut teardown_status = None;
    loop {
        if final_progress.is_some() && teardown_status.is_some() {
            break;
        }
        tokio::select! {
            biased;
            progress = &mut shutdown, if final_progress.is_none() => {
                final_progress = Some(progress);
            }
            _ = &mut sample_usage => {
                unreachable!("usage sampling runs until shutdown finishes");
            }
            status = &mut force_terminate, if teardown_status.is_none() => {
                teardown_status = Some(status);
            }
            _ = &mut deadline => break,
        }
    }
    let progress = AgentRunProgress {
        tokens: final_progress
            .as_ref()
            .map(|progress| progress.tokens)
            .unwrap_or_default()
            .max(freshest_tokens.load(Ordering::Relaxed)),
        tool_uses: final_progress
            .as_ref()
            .map(|progress| progress.tool_uses)
            .unwrap_or_default()
            .max(tool_uses.load(Ordering::Relaxed)),
        activity: None,
    };
    match teardown_status {
        Some(ThreadTeardownStatus::Confirmed) => Ok(progress),
        Some(ThreadTeardownStatus::TimedOut) | None => Err(progress),
    }
}

fn interrupted_after_shutdown(
    shutdown: Result<AgentRunProgress, AgentRunProgress>,
) -> AgentRunError {
    match shutdown {
        Ok(progress) => AgentRunError::Codex {
            error: CodexErr::Interrupted,
            progress,
        },
        Err(progress) => AgentRunError::TeardownTimedOut { progress },
    }
}

fn stalled_after_shutdown(
    timeout: Duration,
    shutdown: Result<AgentRunProgress, AgentRunProgress>,
) -> AgentRunError {
    match shutdown {
        Ok(progress) => AgentRunError::Stalled { timeout, progress },
        Err(progress) => AgentRunError::TeardownTimedOut { progress },
    }
}

fn validate_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[codex_protocol::openai_models::ReasoningEffortPreset],
    reasoning_effort: &ReasoningEffort,
) -> CodexResult<()> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == reasoning_effort)
    {
        return Ok(());
    }
    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(CodexErr::InvalidRequest(format!(
        "Reasoning effort `{reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    )))
}

fn validate_prompt(prompt: &str) -> CodexResult<()> {
    if prompt.trim().is_empty() {
        return Err(CodexErr::InvalidRequest(
            "agent prompt must not be empty".to_string(),
        ));
    }
    Ok(())
}

async fn submit_prompt(
    thread: &CodexThread,
    prompt: String,
    additional_context: BTreeMap<String, AdditionalContextEntry>,
    output_schema: Option<JsonValue>,
    parent_trace: Option<W3cTraceContext>,
) -> CodexResult<String> {
    let request = TurnInputRequest::user_input(vec![UserInput::Text {
        text: prompt,
        text_elements: Vec::new(),
    }])
    .with_additional_context(additional_context)
    .on_start(TurnStartOptions {
        turn_trigger: None,
        final_output_json_schema: output_schema,
        service_tier: None,
        parent_turn_id: None,
        root_turn_id: None,
        cyber_access_program: None,
        guardian_ticket: None,
    })
    .with_trace(parent_trace);
    match thread.start_turn_if_idle(request).await? {
        StartIfIdleSubmission::Started { turn_id } => Ok(turn_id),
        StartIfIdleSubmission::NotSubmitted { reason } => Err(CodexErr::InvalidRequest(format!(
            "agent prompt was not submitted: {reason:?}"
        ))),
    }
}

fn is_tool_item(item: &TurnItem) -> bool {
    matches!(
        item,
        TurnItem::CommandExecution(_)
            | TurnItem::DynamicToolCall(_)
            | TurnItem::CollabAgentToolCall(_)
            | TurnItem::WebSearch(_)
            | TurnItem::ImageView(_)
            | TurnItem::Extension(_)
            | TurnItem::ImageGeneration(_)
            | TurnItem::FileChange(_)
            | TurnItem::McpToolCall(_)
    )
}

fn agent_activity(item: &TurnItem) -> Option<AgentRunActivity> {
    match item {
        TurnItem::Extension(item) if item.is_workflow_input_analysis() => {
            Some(AgentRunActivity::AnalyzingWorkflowInputs)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
