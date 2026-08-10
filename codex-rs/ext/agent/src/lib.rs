use codex_core::CodexThread;
use codex_core::CodexThreadEventSubscription;
use codex_core::NewThread;
use codex_core::StartIfIdleSubmission;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::apply_agent_role_to_config;
use codex_core::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::user_input::UserInput;
use serde_json::Value as JsonValue;
use std::fmt;
use std::future::pending;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const LIVE_PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(2);
/// A fully resolved agent invocation.
///
/// Agent discovery owns rendering `prompt`, including any selected skill
/// references. The runtime starts that prompt using the caller-selected spawn mode.
pub struct AgentInvocation {
    pub config: Config,
    pub prompt: String,
    pub parent_trace: Option<W3cTraceContext>,
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
}
/// Live token and tool-use accounting for an in-progress agent turn.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentRunProgress {
    pub tokens: u64,
    pub tool_uses: u64,
}

/// Runtime controls for an agent turn that is owned by a host-side orchestrator.
pub struct AgentCompletionOptions {
    pub output_schema: Option<JsonValue>,
    /// Maximum time between events for this turn. `None` disables progress timeout handling.
    pub progress_timeout: Option<Duration>,
    pub spawn_mode: AgentSpawnMode,
}

/// A follow-up turn submitted to an existing host-orchestrated agent.
///
/// The target thread retains its prior conversation, so callers should send only the new
/// instruction instead of copying earlier prompts or model output into this value.
pub struct AgentFollowup {
    pub thread_id: ThreadId,
    pub prompt: String,
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
        rollout_budget: AgentRolloutBudget,
    },
}

/// Controls whether a fresh subagent aborts the completed turn that exhausts a shared budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentRolloutBudget {
    #[default]
    Enforce,
    Observe,
}

/// Failure from a host-orchestrated agent turn.
#[derive(Debug)]
pub enum AgentRunError {
    Codex(CodexErr),
    Stalled { timeout: Duration },
}

impl fmt::Display for AgentRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex(error) => fmt::Display::fmt(error, formatter),
            Self::Stalled { timeout } => {
                write!(formatter, "agent made no progress for {timeout:?}")
            }
        }
    }
}

impl std::error::Error for AgentRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codex(error) => Some(error),
            Self::Stalled { .. } => None,
        }
    }
}

impl From<CodexErr> for AgentRunError {
    fn from(error: CodexErr) -> Self {
        Self::Codex(error)
    }
}

/// Runs resolved agents in threads owned by the supplied [`ThreadManager`].
#[derive(Clone)]
pub struct AgentRunner {
    thread_manager: Weak<ThreadManager>,
}

impl AgentRunner {
    pub fn new(thread_manager: Weak<ThreadManager>) -> Self {
        Self { thread_manager }
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
        apply_agent_role_to_config(config, Some(agent_type))
            .await
            .map_err(CodexErr::InvalidRequest)
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
                },
                cancellation,
            )
            .await
        {
            Ok(completion) => Ok(completion),
            Err(AgentRunError::Codex(error)) => Err(error),
            Err(AgentRunError::Stalled { .. }) => Err(CodexErr::RequestTimeout),
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
            |_| {},
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
        on_progress: impl Fn(AgentRunProgress) + Send + Sync + 'a,
    ) -> Result<AgentCompletion, AgentRunError> {
        let AgentCompletionOptions {
            output_schema,
            progress_timeout,
            spawn_mode,
        } = options;
        let run = self
            .start_with_output_schema(parent_thread_id, invocation, output_schema, spawn_mode)
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
        let turn_id = submit_prompt(&thread, prompt, output_schema, parent_trace).await?;
        self.wait_for_completion(
            AgentRun {
                thread_id,
                turn_id,
                thread,
                event_subscription,
            },
            progress_timeout,
            cancellation,
            /*on_progress*/ None,
        )
        .await
    }

    async fn wait_for_completion(
        &self,
        run: AgentRun,
        progress_timeout: Option<Duration>,
        cancellation: CancellationToken,
        on_progress: Option<&(dyn Fn(AgentRunProgress) + Send + Sync)>,
    ) -> Result<AgentCompletion, AgentRunError> {
        let mut tool_uses = 0_u64;
        let mut progress_deadline = progress_timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let deadline = progress_deadline;
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
                if on_progress.is_some() {
                    tokio::time::sleep(LIVE_PROGRESS_REPORT_INTERVAL).await;
                } else {
                    pending::<()>().await;
                }
            };
            tokio::pin!(report);
            let event = tokio::select! {
                event = run.event_subscription.next_event() => event?,
                _ = cancellation.cancelled() => {
                    let _ = run.thread.submit(Op::Interrupt).await;
                    return Err(CodexErr::Interrupted.into());
                }
                timeout = &mut stall => {
                    let _ = run.thread.submit(Op::Interrupt).await;
                    return Err(AgentRunError::Stalled { timeout });
                }
                _ = &mut report => {
                    let usage = run.thread.token_usage_info().await;
                    let tokens = usage
                        .map(|usage| {
                            u64::try_from(usage.total_token_usage.total_tokens).unwrap_or(0)
                        })
                        .unwrap_or(0);
                    let on_progress = on_progress.expect("reporting requires a progress callback");
                    on_progress(AgentRunProgress {
                        tokens,
                        tool_uses,
                    });
                    continue;
                }
            };
            if event.id != run.turn_id {
                continue;
            }
            progress_deadline = progress_timeout.map(|timeout| Instant::now() + timeout);
            match event.msg {
                EventMsg::ItemStarted(item_started) if is_tool_item(&item_started.item) => {
                    tool_uses = tool_uses.saturating_add(1);
                }
                EventMsg::TurnComplete(completed) if completed.turn_id == run.turn_id => {
                    if let Some(error) = completed.error {
                        return Err(CodexErr::Fatal(error.message).into());
                    }
                    return Ok(AgentCompletion {
                        thread_id: run.thread_id,
                        output: completed.last_agent_message.unwrap_or_default(),
                        token_usage: run.thread.token_usage_info().await,
                        tool_uses,
                    });
                }
                EventMsg::TurnAborted(_) => {
                    return Err(CodexErr::Interrupted.into());
                }
                EventMsg::ShutdownComplete => {
                    let error = match run.thread.agent_status().await {
                        AgentStatus::Interrupted => CodexErr::Interrupted,
                        AgentStatus::Errored(message) => {
                            CodexErr::Fatal(format!("agent failed: {message}"))
                        }
                        AgentStatus::NotFound => CodexErr::ThreadNotFound(run.thread_id),
                        AgentStatus::PendingInit
                        | AgentStatus::Running
                        | AgentStatus::Completed(_)
                        | AgentStatus::Shutdown => {
                            CodexErr::Fatal("agent shut down before completing".to_string())
                        }
                    };
                    return Err(error.into());
                }
                EventMsg::Error(error) => return Err(CodexErr::Fatal(error.message).into()),
                _ => {}
            }
        }
    }

    async fn start_with_output_schema(
        &self,
        parent_thread_id: ThreadId,
        invocation: AgentInvocation,
        output_schema: Option<JsonValue>,
        spawn_mode: AgentSpawnMode,
    ) -> CodexResult<AgentRun> {
        let AgentInvocation {
            config,
            prompt,
            parent_trace,
        } = invocation;
        validate_prompt(&prompt)?;

        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))?;
        let mut start_options = StartThreadOptions {
            parent_trace: parent_trace.clone(),
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
                rollout_budget,
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
                match rollout_budget {
                    AgentRolloutBudget::Enforce => {
                        thread_manager
                            .start_fresh_subagent(parent_thread_id, start_options)
                            .await?
                    }
                    AgentRolloutBudget::Observe => {
                        thread_manager
                            .start_fresh_subagent_observing_rollout_budget(
                                parent_thread_id,
                                start_options,
                            )
                            .await?
                    }
                }
            }
        };
        let event_subscription = thread.subscribe_events();
        let turn_id = submit_prompt(&thread, prompt, output_schema, parent_trace).await?;

        Ok(AgentRun {
            thread_id,
            turn_id,
            thread,
            event_subscription,
        })
    }
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
    output_schema: Option<JsonValue>,
    parent_trace: Option<W3cTraceContext>,
) -> CodexResult<String> {
    let request = TurnInputRequest::user_input(vec![UserInput::Text {
        text: prompt,
        text_elements: Vec::new(),
    }])
    .on_start(TurnStartOptions {
        final_output_json_schema: output_schema,
        parent_turn_id: None,
        root_turn_id: None,
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
