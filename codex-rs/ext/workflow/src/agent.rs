use codex_agent_extension::AgentCompletionOptions;
use codex_agent_extension::AgentFollowup;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentRolloutBudget;
use codex_agent_extension::AgentRunError;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::AgentSpawnMode;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_workflow::MAX_WORKFLOW_AGENT_STALL_MS;
use codex_workflow::WorkflowAgentFailure;
use codex_workflow::WorkflowAgentFailureKind;
use codex_workflow::WorkflowAgentFuture;
use codex_workflow::WorkflowAgentRequest;
use codex_workflow::WorkflowAgentResult;
use codex_workflow::WorkflowAgentRuntime;
use codex_workflow::WorkflowEffort;
use codex_workflow::WorkflowIsolation;
use codex_workflow::WorkflowTokenUsage;
use serde_json::Value as JsonValue;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod worktree;
use self::worktree::Worktree;
pub(crate) use self::worktree::WorktreeCleanupMode;

const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_STRUCTURED_OUTPUT_RETRIES: usize = 5;
const MAX_OUTPUT_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_PROMPT_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_STRUCTURED_RETRY_ERROR_BYTES: usize = 1_024;
const WORKFLOW_SUBAGENT_PREAMBLE: &str = r#"You are a workflow subagent. Complete only the bounded task below and return its result as your final response. Your final response is a value returned to the owning workflow, not a message to the user. Do not spawn or message other agents, invoke Workflow, or ask the user questions. Use available tools directly and preserve exact evidence such as paths, line numbers, URLs, commands, and errors."#;

pub(crate) struct CodexWorkflowAgentRuntime {
    runner: AgentRunner,
    parent_thread_id: ThreadId,
    config: Config,
    run_id: String,
    retained_worktrees: Mutex<Vec<Worktree>>,
}

impl CodexWorkflowAgentRuntime {
    pub(crate) fn new(
        runner: AgentRunner,
        parent_thread_id: ThreadId,
        config: Config,
        run_id: String,
    ) -> Self {
        Self {
            runner,
            parent_thread_id,
            config,
            run_id,
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
}

impl CodexWorkflowAgentRuntime {
    async fn run(
        &self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkflowAgentResult, WorkflowAgentFailure> {
        if matches!(request.options.isolation, Some(WorkflowIsolation::Remote)) {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                "remote workflow agent isolation is not available in this build",
            ));
        }

        let mut config = self.config.clone();
        if let Some(default_model) = config.agent_default_subagent_model.clone() {
            config.model = Some(default_model);
        }
        if let Some(default_effort) = config.agent_default_subagent_reasoning_effort.clone() {
            config.model_reasoning_effort = Some(default_effort);
        }
        if let Some(agent_type) = request.options.agent_type.as_deref() {
            self.runner
                .apply_role_to_config(&mut config, agent_type)
                .await
                .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        }
        for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::Workflows] {
            config.features.disable(feature).map_err(|error| {
                failure(
                    WorkflowAgentFailureKind::Blocked,
                    format!("managed policy prevents workflow subagent isolation: {error}"),
                )
            })?;
        }
        config.agents_enabled = false;
        // Keep the depth guard as a second host-side boundary if this config is
        // ever used with a legacy forked spawn mode.
        config.agent_max_depth = 0;
        if let Some(model) = request.options.model.as_ref() {
            config.model = Some(model.clone());
        }
        if let Some(effort) = request.options.effort {
            config.model_reasoning_effort = Some(reasoning_effort(effort));
        }
        let worktree = if matches!(request.options.isolation, Some(WorkflowIsolation::Worktree)) {
            let worktree = Worktree::create(
                &config.cwd,
                &config.codex_home,
                &self.run_id,
                request.index,
                request.attempt,
            )
            .await?;
            config.cwd = worktree.path.clone();
            Some(worktree)
        } else {
            None
        };

        let result = async {
            let isolation = worktree
                .as_ref()
                .map(|worktree| {
                    format!(
                        "\nYou are working in an isolated git worktree at {}. Keep all edits there. The worktree is temporary and will be deleted when this workflow finishes, so return every needed result or patch in your final response.",
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
            let base_prompt = format!(
                "{WORKFLOW_SUBAGENT_PREAMBLE}{isolation}\n\n{}{output_contract}",
                request.prompt
            );
            let stall_timeout = request
                .options
                .stall_ms
                .map(|stall_ms| {
                    if stall_ms > MAX_WORKFLOW_AGENT_STALL_MS {
                        Err(failure(
                            WorkflowAgentFailureKind::Failed,
                            format!(
                                "workflow agent stallMs exceeds the {MAX_WORKFLOW_AGENT_STALL_MS}ms limit"
                            ),
                        ))
                    } else {
                        Ok(Duration::from_millis(stall_ms))
                    }
                })
                .transpose()?
                .unwrap_or(DEFAULT_STALL_TIMEOUT);

            let mut total_tool_uses = 0_u64;
            let output_schema = if use_native_output_schema {
                request
                    .options
                    .schema
                    .as_ref()
                    .map(|schema| {
                        let normalized = strict_output_schema(schema);
                        let serialized = serde_json::to_vec(&normalized).map_err(|error| {
                            failure(
                                WorkflowAgentFailureKind::Failed,
                                format!("failed to serialize workflow agent schema: {error}"),
                            )
                        })?;
                        if serialized.len() > MAX_OUTPUT_SCHEMA_BYTES {
                            return Err(failure(
                                WorkflowAgentFailureKind::Failed,
                                format!(
                                    "normalized workflow agent schema exceeds the {MAX_OUTPUT_SCHEMA_BYTES}-byte limit"
                                ),
                            ));
                        }
                        Ok(normalized)
                    })
                    .transpose()?
            } else {
                None
            };
            // Forward the script prompt verbatim and keep structured-output corrections in the
            // same subagent conversation.
            let mut completion = self
                .runner
                .run_to_completion_with_options(
                    self.parent_thread_id,
                    AgentInvocation {
                        config: config.clone(),
                        prompt: base_prompt,
                        parent_trace: None,
                    },
                    AgentCompletionOptions {
                        output_schema: output_schema.clone(),
                        progress_timeout: Some(stall_timeout),
                        spawn_mode: AgentSpawnMode::FreshSubagent {
                            source: "workflow".to_string(),
                            rollout_budget: AgentRolloutBudget::Observe,
                        },
                    },
                    cancellation.clone(),
                )
                .await
                .map_err(map_agent_error)?;
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
                    ));
                }
                structured_attempt += 1;
                completion = self
                    .runner
                    .run_followup_to_completion(
                        AgentFollowup {
                            thread_id: completion.thread_id,
                            prompt: structured_retry_prompt(&validation_error),
                            output_schema: output_schema.clone(),
                            progress_timeout: Some(stall_timeout),
                            parent_trace: None,
                        },
                        cancellation.clone(),
                    )
                    .await
                    .map_err(map_agent_error)?;
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
        if let Some(worktree) = worktree
            && let Some(worktree) = worktree.cleanup_if_unchanged().await
        {
            self.retained_worktrees
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(worktree);
        }
        result
    }
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

fn map_agent_error(error: AgentRunError) -> WorkflowAgentFailure {
    let error = match error {
        AgentRunError::Stalled { timeout } => {
            return failure(
                WorkflowAgentFailureKind::Stalled,
                AgentRunError::Stalled { timeout }.to_string(),
            );
        }
        AgentRunError::Codex(error) => error,
    };
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("rate limit") || lower.contains("throttl") {
        WorkflowAgentFailureKind::Throttled
    } else if matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::Interrupted
    ) {
        WorkflowAgentFailureKind::Skipped
    } else {
        WorkflowAgentFailureKind::TerminalApi
    };
    failure(kind, message)
}

fn failure(kind: WorkflowAgentFailureKind, message: impl Into<String>) -> WorkflowAgentFailure {
    WorkflowAgentFailure {
        kind,
        message: message.into(),
    }
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
    jsonschema::validator_for(schema).map_err(|error| {
        failure(
            WorkflowAgentFailureKind::Failed,
            format!("invalid workflow agent JSON schema: {error}"),
        )
    })?;
    let schema = serde_json::to_string(schema).map_err(|error| {
        failure(
            WorkflowAgentFailureKind::Failed,
            format!("failed to serialize workflow agent schema: {error}"),
        )
    })?;
    if schema.len() > MAX_OUTPUT_SCHEMA_BYTES {
        return Err(failure(
            WorkflowAgentFailureKind::Failed,
            format!("workflow agent schema exceeds the {MAX_OUTPUT_SCHEMA_BYTES}-byte limit"),
        ));
    }
    if schema.len() > MAX_PROMPT_SCHEMA_BYTES {
        if use_native_output_schema {
            return Ok(
                "\n\nReturn only JSON matching the host-provided schema. Do not use Markdown fences or add prose."
                    .to_string(),
            );
        }
        return Err(failure(
            WorkflowAgentFailureKind::Failed,
            format!(
                "workflow agent schema exceeds the {MAX_PROMPT_SCHEMA_BYTES}-byte prompt fallback limit for this model provider"
            ),
        ));
    }
    Ok(format!(
        "\n\nReturn only a JSON value matching this schema. Do not use Markdown fences or add prose.\nJSON Schema:\n{schema}"
    ))
}

fn validate_schema(value: &JsonValue, schema: &JsonValue) -> Result<(), String> {
    let mut schema = schema.clone();
    make_optional_properties_nullable(&mut schema);
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| format!("invalid JSON schema: {error}"))?;
    validator.validate(value).map_err(|error| error.to_string())
}

fn make_optional_properties_nullable(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for alternatives in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = object
            .get_mut(alternatives)
            .and_then(JsonValue::as_array_mut)
        {
            for branch in branches {
                make_optional_properties_nullable(branch);
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        make_optional_properties_nullable(items);
    }

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
        make_optional_properties_nullable(property);
        if !required
            .iter()
            .any(|required| required.as_str() == Some(name))
        {
            make_schema_nullable(property);
        }
    }
}

fn strict_output_schema(schema: &JsonValue) -> JsonValue {
    let mut normalized = schema.clone();
    normalize_schema_node(&mut normalized);
    normalized
}

fn normalize_schema_node(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for alternatives in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = object
            .get_mut(alternatives)
            .and_then(JsonValue::as_array_mut)
        {
            for branch in branches {
                normalize_schema_node(branch);
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        normalize_schema_node(items);
    }

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
        normalize_schema_node(property);
        if !originally_required
            .iter()
            .any(|required| required.as_str() == Some(name))
        {
            make_schema_nullable(property);
        }
    }
    let required = JsonValue::Array(properties.keys().cloned().map(JsonValue::String).collect());
    object.insert("required".to_string(), required);
    object.insert("additionalProperties".to_string(), JsonValue::Bool(false));
}

fn make_schema_nullable(schema: &mut JsonValue) {
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
