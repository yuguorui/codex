use codex_agent_extension::AgentCompletionOptions;
use codex_agent_extension::AgentCompletionSignal;
use codex_agent_extension::AgentExecutionEnvironmentSnapshot;
use codex_agent_extension::AgentFollowup;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentModelOverrides;
use codex_agent_extension::AgentRunActivity;
use codex_agent_extension::AgentRunError;
use codex_agent_extension::AgentRunProgress;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::AgentSpawnMode;
use codex_core::ThreadTeardownStatus;
use codex_core::config::Config;
use codex_core::context::WorkflowChildIsolation;
use codex_core::context::WorkflowChildOutputContract;
use codex_core::context::WorkflowChildPreamble;
use codex_core::context::WorkflowChildTask;
use codex_extension_api::ExtensionDataInit;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_path_uri::PathUri;
use codex_workflow::MAX_WORKFLOW_AGENT_STALL_MS;
use codex_workflow::WORKFLOW_SUBAGENT_PREAMBLE;
use codex_workflow::WorkflowAgentActivity;
use codex_workflow::WorkflowAgentFailure;
use codex_workflow::WorkflowAgentFailureKind;
use codex_workflow::WorkflowAgentFuture;
use codex_workflow::WorkflowAgentInputs;
use codex_workflow::WorkflowAgentProgressCallback;
use codex_workflow::WorkflowAgentProgressUpdate;
use codex_workflow::WorkflowAgentRequest;
use codex_workflow::WorkflowAgentResult;
use codex_workflow::WorkflowAgentRuntime;
use codex_workflow::WorkflowAgentStartedCallback;
use codex_workflow::WorkflowEffort;
use codex_workflow::WorkflowIsolation;
use codex_workflow::WorkflowTokenUsage;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::analyze_inputs::WorkflowInputsCapability;

mod worktree;
use self::worktree::Worktree;
pub(crate) use self::worktree::WorktreeCleanupMode;

const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_STRUCTURED_OUTPUT_RETRIES: usize = 5;
const MAX_OUTPUT_SCHEMA_DEPTH: usize = 64;
const MAX_OUTPUT_SCHEMA_NODES: usize = 4 * 1024;
const MAX_STRUCTURED_RETRY_ERROR_BYTES: usize = 512;
const WORKFLOW_AGENT_NATIVE_SCHEMA_CONTRACT: &str =
    "\n\nReturn only JSON matching the host-provided schema.";
const WORKFLOW_AGENT_SCHEMA_CONTRACT_PREFIX: &str =
    "\n\nReturn only a JSON value matching this schema.\nJSON Schema:\n";
const WORKFLOW_AGENT_TASK_INSTRUCTION: &str = "Complete the ordered Workflow task provided in the runtime context and return the requested result in your final response.";
const WORKFLOW_AGENT_IDLE_CONTINUATION: &str = "Your previous turn became idle before the Workflow runtime received its completion event. Continue the assigned task from the current state. If the task is already complete, return the final requested result again.";

pub(crate) struct CodexWorkflowAgentRuntime {
    runner: AgentRunner,
    parent_thread_id: ThreadId,
    config: Config,
    run_id: String,
    environments: Option<Vec<TurnEnvironmentSelection>>,
    captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
    environment_location: WorkflowEnvironmentLocation,
    retained_worktrees: Mutex<Vec<Worktree>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowEnvironmentLocation {
    Local,
    Remote,
}

impl CodexWorkflowAgentRuntime {
    #[cfg(test)]
    pub(crate) fn new(
        runner: AgentRunner,
        parent_thread_id: ThreadId,
        config: Config,
        run_id: String,
    ) -> Self {
        Self::new_with_environments(
            runner,
            parent_thread_id,
            config,
            run_id,
            None,
            None,
            WorkflowEnvironmentLocation::Local,
        )
    }

    pub(crate) fn new_with_environments(
        runner: AgentRunner,
        parent_thread_id: ThreadId,
        config: Config,
        run_id: String,
        environments: Option<Vec<TurnEnvironmentSelection>>,
        captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
        environment_location: WorkflowEnvironmentLocation,
    ) -> Self {
        Self {
            runner,
            parent_thread_id,
            config,
            run_id,
            environments,
            captured_environments,
            environment_location,
            retained_worktrees: Mutex::new(Vec::new()),
        }
    }

    pub(crate) async fn cleanup_worktrees(&self, mode: WorktreeCleanupMode) -> Vec<String> {
        let worktrees = {
            let mut retained = self
                .retained_worktrees
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *retained)
        };
        let mut retained = Vec::new();
        for worktree in worktrees {
            match mode {
                WorktreeCleanupMode::Completed => worktree.cleanup().await,
                WorktreeCleanupMode::Interrupted => {
                    if let Some(worktree) = worktree.cleanup_if_unchanged().await {
                        retained.push(worktree.preserve_after_interruption());
                    }
                }
            }
        }
        retained
    }
}

impl WorkflowAgentRuntime for CodexWorkflowAgentRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move { self.run(request, cancellation).await })
    }

    fn run_agent_with_started<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.run_with_started(request, cancellation, on_started)
                .await
        })
    }

    fn run_agent_with_progress<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'a>,
        on_progress: WorkflowAgentProgressCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.run_with_progress(request, cancellation, on_started, on_progress)
                .await
        })
    }
}

impl CodexWorkflowAgentRuntime {
    async fn run(
        &self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkflowAgentResult, WorkflowAgentFailure> {
        self.run_with_started(request, cancellation, Box::new(|_| {}))
            .await
    }

    async fn run_with_started(
        &self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'_>,
    ) -> Result<WorkflowAgentResult, WorkflowAgentFailure> {
        self.run_with_progress(
            request,
            cancellation,
            on_started,
            Box::new(|_| Box::pin(async {})),
        )
        .await
    }

    async fn run_with_progress(
        &self,
        mut request: WorkflowAgentRequest,
        cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'_>,
        on_progress: WorkflowAgentProgressCallback<'_>,
    ) -> Result<WorkflowAgentResult, WorkflowAgentFailure> {
        if self.environments.as_ref().is_some_and(Vec::is_empty) {
            return Err(failure(
                WorkflowAgentFailureKind::Blocked,
                "capture the workflow agent execution environment before starting the agent",
            ));
        }
        if matches!(request.options.isolation, Some(WorkflowIsolation::Remote)) {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                "use the captured remote execution environment without an isolation override",
            ));
        }

        let analysis_inputs = request.inputs.take();
        let mut config = match self
            .runner
            .frozen_workflow_agent_config(request.options.agent_type.as_deref())
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?
        {
            Some(config) => config,
            None => {
                #[cfg(not(test))]
                return Err(failure(
                    WorkflowAgentFailureKind::Failed,
                    format!(
                        "workflow agent configuration for {} was not frozen before approval",
                        self.config.cwd.display()
                    ),
                ));
                #[cfg(test)]
                {
                    let mut config = self.config.clone();
                    let default_model_overrides = AgentModelOverrides {
                        model: config.agent_default_subagent_model.clone(),
                        reasoning_effort: config.agent_default_subagent_reasoning_effort.clone(),
                    };
                    self.runner
                        .apply_model_overrides(&mut config, default_model_overrides)
                        .await
                        .map_err(|error| {
                            failure(WorkflowAgentFailureKind::Failed, error.to_string())
                        })?;
                    self.runner
                        .apply_optional_role_to_config(
                            &mut config,
                            request.options.agent_type.as_deref(),
                        )
                        .await
                        .map_err(|error| {
                            failure(WorkflowAgentFailureKind::Failed, error.to_string())
                        })?;
                    config
                }
            }
        };
        let explicit_model_overrides = AgentModelOverrides {
            model: request.options.model.clone(),
            reasoning_effort: request.options.effort.map(reasoning_effort),
        };
        self.runner
            .apply_model_overrides(&mut config, explicit_model_overrides)
            .await
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::Workflows] {
            config.features.disable(feature).map_err(|error| {
                failure(
                    WorkflowAgentFailureKind::Blocked,
                    format!("managed policy prevents workflow subagent isolation: {error}"),
                )
            })?;
        }
        config.agents_enabled = false;
        config.agent_max_depth = 0;
        if matches!(request.options.isolation, Some(WorkflowIsolation::Worktree))
            && self.environment_location == WorkflowEnvironmentLocation::Remote
        {
            return Err(failure(
                WorkflowAgentFailureKind::Blocked,
                "use worktree isolation with a local workflow execution environment",
            ));
        }
        let mut worktree = if matches!(request.options.isolation, Some(WorkflowIsolation::Worktree))
        {
            let worktree = Worktree::create(
                &config.cwd,
                &config.codex_home,
                &self.run_id,
                request.index,
                request.attempt,
            )
            .await?;
            Some(worktree)
        } else {
            None
        };
        if let Some(worktree) = &mut worktree {
            worktree.disable_drop_cleanup();
        }
        let environments = match worktree.as_ref() {
            Some(worktree) => isolated_worktree_context(
                &mut config,
                self.environments.as_deref(),
                &worktree.path,
            )?,
            None => self.environments.clone(),
        };

        let started_thread_id = Arc::new(Mutex::new(None));
        let started_thread_id_for_callback = Arc::clone(&started_thread_id);
        let mut teardown_timed_out = false;
        let mut result = async {
            let isolation = worktree
                .as_ref()
                .map(|worktree| {
                    format!(
                        "You are working in an isolated git worktree at {}. Keep all edits there. The worktree is temporary and will be deleted when this workflow finishes, so return every needed result or patch in your final response.",
                        worktree.path.display()
                    )
                })
                .unwrap_or_default();
            let use_native_output_schema = config.model_provider.is_openai();
            let output_contract = request
                .options
                .schema
                .as_ref()
                .map(|schema| structured_output_contract(schema, use_native_output_schema))
                .transpose()?
                .unwrap_or_default();
            let additional_context = workflow_agent_context(
                &request.prompt,
                &isolation,
                &output_contract,
            )?;
            let stall_timeout = request
                .options
                .stall_ms
                .map(|stall_ms| {
                    if stall_ms > MAX_WORKFLOW_AGENT_STALL_MS {
                        Err(failure(
                            WorkflowAgentFailureKind::Failed,
                            "choose stallMs within the supported workflow agent timeout range",
                        ))
                    } else {
                        Ok(Duration::from_millis(stall_ms))
                    }
                })
                .transpose()?
                .unwrap_or(DEFAULT_STALL_TIMEOUT);

            let mut total_tool_uses = 0_u64;
            let thread_extension_init = workflow_agent_extension_init(analysis_inputs);
            let output_schema = if use_native_output_schema {
                request
                    .options
                    .schema
                    .as_ref()
                    .map(strict_output_schema)
                    .transpose()?
            } else {
                None
            };
            // Keep runtime-owned task data in bounded context fragments and structured-output
            // corrections in the same subagent conversation.
            let mut completion = self
                .runner
                .run_to_completion_with_progress_in_environments(
                    self.parent_thread_id,
                    AgentInvocation {
                        config: config.clone(),
                        prompt: WORKFLOW_AGENT_TASK_INSTRUCTION.to_string(),
                        additional_context: additional_context.clone(),
                        parent_trace: None,
                    },
                    AgentCompletionOptions {
                        output_schema: output_schema.clone(),
                        progress_timeout: Some(stall_timeout),
                        spawn_mode: AgentSpawnMode::FreshSubagent {
                            agent_nickname: request.options.label.clone(),
                            agent_role: request.options.agent_type.clone(),
                        },
                        thread_extension_init,
                    },
                    cancellation.clone(),
                    environments,
                    self.captured_environments.clone(),
                    move |thread_id| {
                        *started_thread_id_for_callback
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread_id);
                        on_started(thread_id.to_string());
                    },
                    move |progress: AgentRunProgress| {
                        on_progress(WorkflowAgentProgressUpdate {
                            usage: WorkflowTokenUsage {
                                total_tokens: progress.tokens,
                                tool_uses: progress.tool_uses,
                            },
                            activity: progress.activity.map(|activity| match activity {
                                AgentRunActivity::AnalyzingWorkflowInputs => {
                                    WorkflowAgentActivity::AnalyzingInputs
                                }
                            }),
                        })
                    },
                )
                .await
                .map_err(|error| {
                    teardown_timed_out =
                        matches!(&error, AgentRunError::TeardownTimedOut { .. });
                    map_agent_error(error, /*prior_tool_uses*/ 0)
                })?;
            if completion.signal == AgentCompletionSignal::TerminalStatus {
                total_tool_uses = total_tool_uses.saturating_add(completion.tool_uses);
                completion = self
                    .runner
                    .run_followup_to_completion(
                        AgentFollowup {
                            thread_id: completion.thread_id,
                            prompt: WORKFLOW_AGENT_IDLE_CONTINUATION.to_string(),
                            additional_context: additional_context.clone(),
                            output_schema: output_schema.clone(),
                            progress_timeout: Some(stall_timeout),
                            parent_trace: None,
                        },
                        cancellation.clone(),
                    )
                    .await
                    .map_err(|error| {
                        teardown_timed_out =
                            matches!(&error, AgentRunError::TeardownTimedOut { .. });
                        map_agent_error(error, total_tool_uses)
                    })?;
            }
            let mut structured_attempt = 0;
            let final_result = loop {
                total_tool_uses = total_tool_uses.saturating_add(completion.tool_uses);
                let validation_error = match request.options.schema.as_ref() {
                    None => break JsonValue::String(std::mem::take(&mut completion.output)),
                    Some(schema) => match serde_json::from_str::<JsonValue>(&completion.output) {
                        Ok(value) => match validate_schema(&value, schema) {
                            Ok(()) => break value,
                            Err(error) => error,
                        },
                        Err(error) => format!("invalid JSON: {error}"),
                    },
                };

                if structured_attempt == MAX_STRUCTURED_OUTPUT_RETRIES {
                    return Err(failure(
                        WorkflowAgentFailureKind::Failed,
                        "workflow agent exhausted structured output retries",
                    )
                    .with_usage(WorkflowTokenUsage {
                        total_tokens: completion
                            .token_usage
                            .as_ref()
                            .and_then(|usage| {
                                u64::try_from(usage.total_token_usage.total_tokens).ok()
                            })
                            .unwrap_or(0),
                        tool_uses: total_tool_uses,
                    }));
                }
                structured_attempt += 1;
                completion = self
                    .runner
                    .run_followup_to_completion(
                        AgentFollowup {
                            thread_id: completion.thread_id,
                            prompt: structured_retry_prompt(&validation_error),
                            additional_context: additional_context.clone(),
                            output_schema: output_schema.clone(),
                            progress_timeout: Some(stall_timeout),
                            parent_trace: None,
                        },
                        cancellation.clone(),
                    )
                    .await
                    .map_err(|error| {
                        teardown_timed_out =
                            matches!(&error, AgentRunError::TeardownTimedOut { .. });
                        map_agent_error(error, total_tool_uses)
                    })?;
            };
            let total_tokens = completion
                .token_usage
                .as_ref()
                .and_then(|usage| u64::try_from(usage.total_token_usage.total_tokens).ok())
                .unwrap_or(0);

            Ok(WorkflowAgentResult {
                value: final_result,
                usage: WorkflowTokenUsage {
                    total_tokens,
                    tool_uses: total_tool_uses,
                },
                agent_id: Some(completion.thread_id.to_string()),
                model: config.model.clone(),
                fallback_model: None,
            })
        }
        .await;
        let mut teardown_error = teardown_timed_out.then(|| {
            "workflow agent teardown did not complete before the shutdown deadline".to_string()
        });
        if teardown_error.is_none() {
            let thread_id = *started_thread_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(thread_id) = thread_id {
                teardown_error = match self.runner.force_terminate(thread_id).await {
                    Ok(ThreadTeardownStatus::Confirmed) => None,
                    Ok(ThreadTeardownStatus::TimedOut) => Some(
                        "workflow agent teardown did not complete before the shutdown deadline"
                            .to_string(),
                    ),
                    Err(error) => Some(format!(
                        "workflow agent teardown could not be confirmed: {error}"
                    )),
                };
            }
        }
        match (worktree, teardown_error) {
            (Some(worktree), None) => {
                if let Some(worktree) = worktree.cleanup_if_unchanged().await {
                    self.retained_worktrees
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(worktree);
                }
            }
            (Some(worktree), Some(error)) => {
                let retained = worktree.preserve_after_teardown_timeout();
                apply_teardown_failure(&mut result, format!("{error}; {retained}"));
            }
            (None, Some(error)) => apply_teardown_failure(&mut result, error),
            (None, None) => {}
        }
        result
    }
}

fn apply_teardown_failure(
    result: &mut Result<WorkflowAgentResult, WorkflowAgentFailure>,
    message: String,
) {
    match result {
        Ok(success) => {
            let usage = success.usage.clone();
            *result = Err(failure(WorkflowAgentFailureKind::Failed, message).with_usage(usage));
        }
        Err(existing) => {
            existing.kind = WorkflowAgentFailureKind::Failed;
            if message.starts_with(&existing.message) {
                existing.message = message;
            } else if !existing.message.contains(&message) {
                existing.message = format!("{}; {message}", existing.message);
            }
        }
    }
}

pub(crate) fn workflow_agent_extension_init(
    inputs: Option<WorkflowAgentInputs>,
) -> ExtensionDataInit {
    let mut init = ExtensionDataInit::new();
    if let Some(inputs) = inputs {
        init.insert(WorkflowInputsCapability::new(inputs));
    }
    init
}

fn isolated_worktree_context(
    config: &mut Config,
    environments: Option<&[TurnEnvironmentSelection]>,
    worktree_path: &codex_utils_absolute_path::AbsolutePathBuf,
) -> Result<Option<Vec<TurnEnvironmentSelection>>, WorkflowAgentFailure> {
    config.cwd = worktree_path.clone();
    config.workspace_roots = vec![worktree_path.clone()];
    config.workspace_roots_explicit = true;
    config
        .permissions
        .set_workspace_roots(vec![worktree_path.clone()]);

    let Some(environments) = environments else {
        return Ok(None);
    };
    let Some(primary) = environments.first() else {
        return Err(failure(
            WorkflowAgentFailureKind::Blocked,
            "capture the workflow agent execution environment before using worktree isolation",
        ));
    };
    if !matches!(primary.config, EnvironmentConfigState::FromThread) {
        return Err(failure(
            WorkflowAgentFailureKind::Blocked,
            format!(
                "use thread-derived configuration for worktree isolation in environment `{}`",
                primary.environment_id
            ),
        ));
    }

    let mut primary = primary.clone();
    let worktree_uri = PathUri::from_abs_path(worktree_path);
    primary.cwd = worktree_uri.clone();
    primary.workspace_roots = vec![worktree_uri];
    let mut isolated = Vec::with_capacity(environments.len());
    isolated.push(primary);
    isolated.extend(environments.iter().skip(1).cloned());
    Ok(Some(isolated))
}

fn reasoning_effort(effort: WorkflowEffort) -> ReasoningEffort {
    match effort {
        WorkflowEffort::Low => ReasoningEffort::Low,
        WorkflowEffort::Medium => ReasoningEffort::Medium,
        WorkflowEffort::High => ReasoningEffort::High,
        WorkflowEffort::Xhigh => ReasoningEffort::XHigh,
        WorkflowEffort::Max => ReasoningEffort::Max,
    }
}

fn map_agent_error(error: AgentRunError, prior_tool_uses: u64) -> WorkflowAgentFailure {
    let progress = error.progress();
    let usage = WorkflowTokenUsage {
        total_tokens: progress.tokens,
        tool_uses: prior_tool_uses.saturating_add(progress.tool_uses),
    };
    let error = match error {
        AgentRunError::Stalled { timeout, .. } => {
            return failure(
                WorkflowAgentFailureKind::Stalled,
                format!("agent made no progress for {timeout:?}"),
            )
            .with_usage(usage);
        }
        AgentRunError::TeardownTimedOut { .. } => {
            return failure(
                WorkflowAgentFailureKind::Failed,
                "workflow agent teardown did not complete before the shutdown deadline",
            )
            .with_usage(usage);
        }
        AgentRunError::Codex { error, .. } => error,
    };
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let kind = if matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::Interrupted
    ) {
        WorkflowAgentFailureKind::Cancelled
    } else if lower.contains("rate limit") || lower.contains("throttl") {
        WorkflowAgentFailureKind::Throttled
    } else {
        WorkflowAgentFailureKind::TerminalApi
    };
    failure(kind, message).with_usage(usage)
}

fn failure(kind: WorkflowAgentFailureKind, message: impl Into<String>) -> WorkflowAgentFailure {
    WorkflowAgentFailure {
        kind,
        message: message.into(),
        usage: WorkflowTokenUsage::default(),
    }
}

fn workflow_agent_context(
    prompt: &str,
    isolation: &str,
    output_contract: &str,
) -> Result<BTreeMap<String, AdditionalContextEntry>, WorkflowAgentFailure> {
    let mut context = BTreeMap::from([
        WorkflowChildPreamble::new(WORKFLOW_SUBAGENT_PREAMBLE).into_additional_context()
    ]);
    for fragment in WorkflowChildTask::parts(prompt) {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildIsolation::parts(isolation) {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildOutputContract::parts(output_contract.trim_start()) {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    Ok(context)
}

fn structured_retry_prompt(error: &str) -> String {
    let error = truncate_text(
        error,
        TruncationPolicy::Bytes(MAX_STRUCTURED_RETRY_ERROR_BYTES),
    );
    format!(
        "Your previous final output did not satisfy the required JSON schema ({error}). Return only corrected JSON."
    )
}

fn structured_output_contract(
    schema: &JsonValue,
    use_native_output_schema: bool,
) -> Result<String, WorkflowAgentFailure> {
    serialize_bounded_schema(schema, "workflow agent schema")?;
    jsonschema::validator_for(schema).map_err(|error| {
        failure(
            WorkflowAgentFailureKind::Failed,
            format!("invalid workflow agent JSON schema: {error}"),
        )
    })?;
    let normalized = strict_output_schema(schema)?;
    if use_native_output_schema {
        return Ok(WORKFLOW_AGENT_NATIVE_SCHEMA_CONTRACT.to_string());
    }
    let serialized = serialize_bounded_schema(&normalized, "normalized workflow agent schema")?;
    Ok(format!(
        "{WORKFLOW_AGENT_SCHEMA_CONTRACT_PREFIX}{serialized}"
    ))
}

fn validate_schema(value: &JsonValue, schema: &JsonValue) -> Result<(), String> {
    serialize_bounded_schema(schema, "workflow agent schema").map_err(|error| error.message)?;
    let mut schema = schema.clone();
    make_optional_properties_nullable(&mut schema);
    serialize_bounded_schema(&schema, "normalized workflow agent schema")
        .map_err(|error| error.message)?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| format!("invalid JSON schema: {error}"))?;
    validator.validate(value).map_err(|error| error.to_string())
}

fn make_optional_properties_nullable(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for_each_subschema_mut(object, make_optional_properties_nullable);

    let required = object
        .get("required")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(properties) = object
        .get_mut("properties")
        .and_then(JsonValue::as_object_mut)
    else {
        return;
    };
    for (name, property) in properties {
        if !required
            .iter()
            .any(|required| required.as_str() == Some(name))
        {
            make_schema_nullable(property);
        }
    }
    object
        .entry("additionalProperties".to_string())
        .or_insert(JsonValue::Bool(false));
}

fn strict_output_schema(schema: &JsonValue) -> Result<JsonValue, WorkflowAgentFailure> {
    serialize_bounded_schema(schema, "workflow agent schema")?;
    let mut normalized = schema.clone();
    normalize_schema_node(&mut normalized);
    serialize_bounded_schema(&normalized, "normalized workflow agent schema")?;
    Ok(normalized)
}

fn serialize_bounded_schema(
    schema: &JsonValue,
    label: &str,
) -> Result<String, WorkflowAgentFailure> {
    let mut stack = vec![(schema, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_OUTPUT_SCHEMA_DEPTH {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                format!("use a focused {label}"),
            ));
        }
        nodes = nodes.saturating_add(1);
        if nodes > MAX_OUTPUT_SCHEMA_NODES {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                format!("use a focused {label}"),
            ));
        }
        match value {
            JsonValue::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            JsonValue::Object(values) => {
                for value in values.values() {
                    stack.push((value, depth.saturating_add(1)));
                }
            }
            JsonValue::String(_) => {}
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
        }
    }

    serde_json::to_string(schema).map_err(|error| {
        failure(
            WorkflowAgentFailureKind::Failed,
            format!("failed to serialize {label}: {error}"),
        )
    })
}

fn normalize_schema_node(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for_each_subschema_mut(object, normalize_schema_node);

    let originally_required = object
        .get("required")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(properties) = object
        .get_mut("properties")
        .and_then(JsonValue::as_object_mut)
    else {
        return;
    };
    for (name, property) in properties.iter_mut() {
        if !originally_required
            .iter()
            .any(|required| required.as_str() == Some(name))
        {
            make_schema_nullable(property);
        }
    }
    let required = JsonValue::Array(properties.keys().cloned().map(JsonValue::String).collect());
    object.insert("required".to_string(), required);
    object
        .entry("additionalProperties".to_string())
        .or_insert(JsonValue::Bool(false));
}

fn for_each_subschema_mut(
    object: &mut serde_json::Map<String, JsonValue>,
    mut visit: impl FnMut(&mut JsonValue),
) {
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(values) = object.get_mut(key).and_then(JsonValue::as_array_mut) {
            for value in values {
                visit(value);
            }
        }
    }

    for key in [
        "$defs",
        "definitions",
        "dependentSchemas",
        "patternProperties",
        "properties",
    ] {
        if let Some(values) = object.get_mut(key).and_then(JsonValue::as_object_mut) {
            for value in values.values_mut() {
                visit(value);
            }
        }
    }

    for key in [
        "additionalItems",
        "additionalProperties",
        "contains",
        "contentSchema",
        "else",
        "if",
        "items",
        "not",
        "propertyNames",
        "then",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if let Some(value) = object.get_mut(key) {
            if key == "items"
                && let Some(values) = value.as_array_mut()
            {
                for value in values {
                    visit(value);
                }
            } else if value.is_object() || value.is_boolean() {
                visit(value);
            }
        }
    }

    if let Some(dependencies) = object
        .get_mut("dependencies")
        .and_then(JsonValue::as_object_mut)
    {
        for dependency in dependencies.values_mut() {
            if dependency.is_object() || dependency.is_boolean() {
                visit(dependency);
            }
        }
    }
}

fn make_schema_nullable(schema: &mut JsonValue) {
    if let JsonValue::Bool(allows_every_value) = schema {
        if !*allows_every_value {
            *schema = serde_json::json!({ "type": "null" });
        }
        return;
    }
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object
        .get("enum")
        .and_then(JsonValue::as_array)
        .is_some_and(|values| values.iter().any(JsonValue::is_null))
        || object.get("type").is_some_and(|value| match value {
            JsonValue::String(value) => value == "null",
            JsonValue::Array(values) => values.iter().any(|value| value == "null"),
            _ => false,
        })
    {
        return;
    }
    if let Some(values) = object.get_mut("enum").and_then(JsonValue::as_array_mut) {
        values.push(JsonValue::Null);
        return;
    }
    let existing_type = object.get("type").cloned();
    match existing_type {
        Some(JsonValue::String(value)) => {
            object.insert(
                "type".to_string(),
                JsonValue::Array(vec![
                    JsonValue::String(value),
                    JsonValue::String("null".to_string()),
                ]),
            );
        }
        Some(JsonValue::Array(mut values)) => {
            values.push(JsonValue::String("null".to_string()));
            object.insert("type".to_string(), JsonValue::Array(values));
        }
        _ => {
            let original = std::mem::take(schema);
            *schema = serde_json::json!({
                "anyOf": [original, { "type": "null" }]
            });
        }
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
