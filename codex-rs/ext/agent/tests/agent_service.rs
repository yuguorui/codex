#![recursion_limit = "256"]

use anyhow::Result;
use codex_agent_extension::AgentCompletionOptions;
use codex_agent_extension::AgentFollowup;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentModelOverrides;
use codex_agent_extension::AgentRunError;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::AgentSpawnMode;
use codex_agent_extension::ModelMetadataPolicy;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerOAuthConfig;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::AgentRoleConfig;
use codex_core::config::RolloutBudgetConfig;
use codex_core::context::WorkflowChildIsolation;
use codex_core::context::WorkflowChildOutputContract;
use codex_core::context::WorkflowChildPreamble;
use codex_core::test_support::all_model_presets;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct BlockingThreadStop {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

struct BlockOnStopMarker;

impl codex_extension_api::ThreadLifecycleContributor<codex_core::config::Config>
    for BlockingThreadStop
{
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, codex_core::config::Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(input.session_source, SessionSource::SubAgent(_)) {
                input.thread_store.insert(BlockOnStopMarker);
            }
        })
    }

    fn on_thread_stop<'a>(
        &'a self,
        input: codex_extension_api::ThreadStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<BlockOnStopMarker>().is_some() {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

async fn wait_for_agent_running(thread: &codex_core::CodexThread) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if thread.agent_status().await == AgentStatus::Running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent should enter the running state");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_unknown_model_is_rejected() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();

    let error = runner
        .apply_model_overrides(
            &mut config,
            AgentModelOverrides {
                model: Some("workflow-model-that-does-not-exist".to_string()),
                reasoning_effort: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Unknown model `workflow-model-that-does-not-exist` for workflow agent"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_explicit_reasoning_effort_is_rejected() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let model = known_model_id();
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model(&model)
        .build_with_auto_env(&server)
        .await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    let unsupported_effort = ReasoningEffort::Custom("workflow-unsupported".to_string());

    let error = runner
        .apply_model_overrides(
            &mut config,
            AgentModelOverrides {
                model: Some(model.clone()),
                reasoning_effort: Some(unsupported_effort.clone()),
            },
        )
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with(&format!(
        "Reasoning effort `{unsupported_effort}` is not supported for model `{model}`."
    )));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_model_drops_an_inherited_unsupported_service_tier() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let model = known_model_id();
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model(&model)
        .build_with_auto_env(&server)
        .await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    config.service_tier = Some("workflow-unsupported-tier".to_string());

    runner
        .apply_model_overrides(
            &mut config,
            AgentModelOverrides {
                model: Some(model.clone()),
                reasoning_effort: None,
            },
        )
        .await?;

    assert_eq!(config.model.as_deref(), Some(model.as_str()));
    assert_eq!(config.service_tier, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_validation_allows_fallback_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    config.model = Some("workflow-role-model-with-fallback-metadata".to_string());
    config.model_reasoning_effort = Some(ReasoningEffort::Custom(
        "fallback-metadata-does-not-advertise-efforts".to_string(),
    ));

    runner
        .validate_model_configuration(&mut config, ModelMetadataPolicy::AllowFallback)
        .await?;

    assert_eq!(
        (config.model, config.model_reasoning_effort),
        (
            Some("workflow-role-model-with-fallback-metadata".to_string()),
            Some(ReasoningEffort::Custom(
                "fallback-metadata-does-not-advertise-efforts".to_string(),
            )),
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_validation_rejects_fallback_metadata_when_known_model_is_required() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    config.model = Some("workflow-model-that-does-not-exist".to_string());

    let error = runner
        .validate_model_configuration(&mut config, ModelMetadataPolicy::RequireKnown)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Unknown model `workflow-model-that-does-not-exist` for workflow agent"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_provided_unknown_model_uses_fallback_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const ROLE_NAME: &str = "workflow-fallback-role";
    const ROLE_MODEL: &str = "workflow-model-provided-by-role";
    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    let role_path = test.codex_home_path().join("workflow-fallback-role.toml");
    tokio::fs::write(&role_path, format!("model = \"{ROLE_MODEL}\"\n")).await?;
    config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            description: Some("Role with an intentionally unknown model".to_string()),
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    runner.apply_role_to_config(&mut config, ROLE_NAME).await?;
    runner
        .validate_model_configuration(&mut config, ModelMetadataPolicy::AllowFallback)
        .await?;

    assert_eq!(config.model.as_deref(), Some(ROLE_MODEL));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_validation_drops_an_inherited_unsupported_service_tier() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let model = known_model_id();
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model(&model)
        .build_with_auto_env(&server)
        .await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    config.service_tier = Some("workflow-unsupported-tier".to_string());

    runner
        .validate_model_configuration(&mut config, ModelMetadataPolicy::RequireKnown)
        .await?;

    assert_eq!((config.model, config.service_tier), (Some(model), None));
    Ok(())
}

fn known_model_id() -> String {
    let Some(preset) = all_model_presets()
        .iter()
        .find(|preset| preset.supported_in_api && !preset.supported_reasoning_efforts.is_empty())
    else {
        panic!("bundled models should include a known reasoning model");
    };
    preset.id.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_freeze_redacts_secrets_and_launches_with_approved_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const APPROVED_PROJECT: &str = "APPROVED_PROJECT_INSTRUCTIONS_SECRET";
    const CHANGED_PROJECT: &str = "CHANGED_PROJECT_INSTRUCTIONS_SECRET";
    const APPROVED_ROLE: &str = "APPROVED_ROLE_INSTRUCTIONS_SECRET";
    const CHANGED_ROLE: &str = "CHANGED_ROLE_INSTRUCTIONS_SECRET";
    const BASE_SECRET: &str = "BASE_INSTRUCTIONS_SECRET";
    const MCP_URL_SECRET: &str = "https://mcp-secret.example.invalid/hidden";
    const MCP_HEADER_SECRET: &str = "MCP_INLINE_HEADER_SECRET";
    const MCP_ENV_HEADER_SECRET: &str = "MCP_ENV_HEADER_SECRET_NAME";
    const MCP_BEARER_SECRET: &str = "MCP_BEARER_SECRET_NAME";
    const MCP_HELPER_SECRET: &str = "MCP_HEADER_HELPER_SECRET";
    const MCP_OAUTH_SECRET: &str = "MCP_OAUTH_CLIENT_SECRET";
    const MCP_SCOPE_SECRET: &str = "MCP_SCOPE_SECRET";

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("frozen-child-response"),
            responses::ev_assistant_message("frozen-child-message", "done"),
            responses::ev_completed("frozen-child-response"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            std::fs::write(config.cwd.join("AGENTS.md"), APPROVED_PROJECT)
                .expect("write approved project instructions");
        })
        .build_with_auto_env(&server)
        .await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut config = test.config.clone();
    config.base_instructions = Some(BASE_SECRET.to_string());
    let role_path = test.codex_home_path().join("frozen-workflow-role.toml");
    tokio::fs::write(
        &role_path,
        format!("developer_instructions = {APPROVED_ROLE:?}\n"),
    )
    .await?;
    config.agent_roles.insert(
        "frozen-workflow".to_string(),
        AgentRoleConfig {
            description: Some("Frozen Workflow role".to_string()),
            config_file: Some(role_path.clone()),
            nickname_candidates: None,
        },
    );
    let mut mcp_servers = config.mcp_servers.get().clone();
    mcp_servers.insert(
        "redacted-capability".to_string(),
        McpServerConfig {
            transport: McpServerTransportConfig::StreamableHttp {
                url: MCP_URL_SECRET.to_string(),
                bearer_token_env_var: Some(MCP_BEARER_SECRET.to_string()),
                http_headers: Some(HashMap::from([(
                    "Authorization".to_string(),
                    MCP_HEADER_SECRET.to_string(),
                )])),
                env_http_headers: Some(HashMap::from([(
                    "X-Secret".to_string(),
                    MCP_ENV_HEADER_SECRET.to_string(),
                )])),
                http_headers_helper: Some(MCP_HELPER_SECRET.to_string()),
            },
            auth: Default::default(),
            environment_id: "local".to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: true,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: Some(vec!["allowed-tool-secret".to_string()]),
            disabled_tools: Some(vec!["denied-tool-secret".to_string()]),
            scopes: Some(vec![MCP_SCOPE_SECRET.to_string()]),
            oauth: Some(McpServerOAuthConfig {
                client_id: Some(MCP_OAUTH_SECRET.to_string()),
                callback_url: None,
                callback_port: Some(32123),
            }),
            oauth_resource: Some("MCP_OAUTH_RESOURCE_SECRET".to_string()),
            tools: HashMap::new(),
        },
    );
    config.mcp_servers.set(mcp_servers)?;

    let (frozen_runner, approval) = runner
        .freeze_workflow_agent_configs(parent_thread_id, &config, AgentModelOverrides::default())
        .await?;
    let approval = approval.to_string();
    for secret in [
        APPROVED_PROJECT,
        APPROVED_ROLE,
        BASE_SECRET,
        MCP_URL_SECRET,
        MCP_HEADER_SECRET,
        MCP_ENV_HEADER_SECRET,
        MCP_BEARER_SECRET,
        MCP_HELPER_SECRET,
        MCP_OAUTH_SECRET,
        MCP_SCOPE_SECRET,
        "MCP_OAUTH_RESOURCE_SECRET",
        "allowed-tool-secret",
        "denied-tool-secret",
    ] {
        assert!(!approval.contains(secret), "approval leaked {secret}");
    }
    assert!(approval.contains("redacted-capability"));
    assert!(approval.contains("configLayerSha256"));

    tokio::fs::write(test.config.cwd.join("AGENTS.md"), CHANGED_PROJECT).await?;
    tokio::fs::write(
        &role_path,
        format!("developer_instructions = {CHANGED_ROLE:?}\n"),
    )
    .await?;
    let child_config = frozen_runner
        .frozen_workflow_agent_config(Some("frozen-workflow"))?
        .expect("Workflow freeze should retain child config");
    frozen_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: child_config,
                prompt: "Run with the approved frozen instructions.".to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    agent_nickname: Some("frozen-instructions-test".to_string()),
                    agent_role: Some("frozen-workflow".to_string()),
                },
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
        .await?;

    let request = response_mock.single_request();
    assert!(request.body_contains_text(APPROVED_PROJECT));
    assert!(request.body_contains_text(APPROVED_ROLE));
    assert!(!request.body_contains_text(CHANGED_PROJECT));
    assert!(!request.body_contains_text(CHANGED_ROLE));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starts_resolved_agent_prompt_in_forked_thread() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("agent-response"),
            responses::ev_assistant_message("message", "done"),
            responses::ev_completed("agent-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));

    let agent_run = agent_runner
        .start(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: "Use $example-agent to inspect the current changes.".to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
        )
        .await?;

    assert_ne!(agent_run.thread_id, parent_thread_id);
    assert_eq!(
        agent_run
            .thread
            .config_snapshot()
            .await
            .forked_from_thread_id,
        Some(parent_thread_id)
    );
    let started = wait_for_event(&agent_run.thread, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let EventMsg::TurnStarted(started) = started else {
        unreachable!("event predicate only matches turn started events");
    };
    assert_eq!(started.turn_id, agent_run.turn_id);
    wait_for_event_with_timeout(
        &agent_run.thread,
        |event| {
            matches!(event, EventMsg::TurnComplete(completed) if completed.turn_id == agent_run.turn_id)
        },
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        agent_run.thread.agent_status().await,
        AgentStatus::Completed(Some("done".to_string()))
    );

    let request = response_mock.single_request();
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text == "Use $example-agent to inspect the current changes.")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_context_precedes_and_stays_separate_from_the_agent_task() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const TASK: &str = "WORKFLOW_TASK_MARKER";
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("agent-response"),
            responses::ev_assistant_message("message", "done"),
            responses::ev_completed("agent-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut additional_context =
        BTreeMap::from([
            WorkflowChildPreamble::new("WORKFLOW_PREAMBLE_MARKER").into_additional_context()
        ]);
    for fragment in WorkflowChildIsolation::parts("WORKFLOW_ISOLATION_MARKER") {
        let (key, entry) = fragment.into_additional_context();
        additional_context.insert(key, entry);
    }
    for fragment in WorkflowChildOutputContract::parts("WORKFLOW_OUTPUT_CONTRACT_MARKER") {
        let (key, entry) = fragment.into_additional_context();
        additional_context.insert(key, entry);
    }

    agent_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: TASK.to_string(),
                additional_context,
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    agent_nickname: Some("workflow-context-test".to_string()),
                    agent_role: Some("workflow-test".to_string()),
                },
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
        .await?;

    let request = response_mock.single_request();
    let workflow_messages = request
        .inputs_of_type("message")
        .into_iter()
        .filter(|item| item.to_string().contains("WORKFLOW_"))
        .map(|item| {
            json!({
                "type": item["type"],
                "role": item["role"],
                "content": item["content"],
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        workflow_messages,
        vec![
            json!({
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<workflow_child_0_preamble>WORKFLOW_PREAMBLE_MARKER</workflow_child_0_preamble>"
                }]
            }),
            json!({
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<workflow_child_1_isolation_part_0001_of_0001>WORKFLOW_ISOLATION_MARKER</workflow_child_1_isolation_part_0001_of_0001>"
                }]
            }),
            json!({
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<workflow_child_2_output_contract_part_0001_of_0001>WORKFLOW_OUTPUT_CONTRACT_MARKER</workflow_child_2_output_contract_part_0001_of_0001>"
                }]
            }),
            json!({
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": TASK
                }]
            }),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_agent_to_completion_with_structured_output() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("agent-response"),
            responses::ev_assistant_message("message", r#"{"answer":"done"}"#),
            responses::ev_completed_with_tokens("agent-response", 42),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false,
    });

    let completion = agent_runner
        .run_to_completion(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: "Return a structured answer.".to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            Some(schema.clone()),
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(completion.output, r#"{"answer":"done"}"#);
    assert_eq!(completion.tool_uses, 0);
    assert_eq!(
        completion
            .token_usage
            .map(|usage| usage.total_token_usage.total_tokens),
        Some(42)
    );
    let request = response_mock.single_request();
    assert_eq!(request.body_json()["text"]["format"]["schema"], schema);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_close_cancels_a_real_child_session_task_and_waits_for_teardown() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let request_args = json!({
        "questions": [{
            "id": "confirm",
            "header": "Confirm",
            "question": "Continue?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the task."
            }, {
                "label": "No",
                "description": "Stop the task."
            }]
        }]
    });
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("force-close-response"),
            responses::ev_function_call(
                "force-close-input",
                "request_user_input",
                &request_args.to_string(),
            ),
            responses::ev_completed("force-close-response"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::DefaultModeRequestUserInput)
                .expect("test config should allow request_user_input");
        })
        .build_with_auto_env(&server)
        .await?;
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let run_runner = runner.clone();
    let parent_thread_id = test.session_configured.session_id.into();
    let config = test.config.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let run = tokio::spawn(async move {
        run_runner
            .run_to_completion_with_progress(
                parent_thread_id,
                AgentInvocation {
                    config,
                    prompt: "Wait for confirmation.".to_string(),
                    additional_context: Default::default(),
                    parent_trace: None,
                },
                AgentCompletionOptions {
                    output_schema: None,
                    progress_timeout: None,
                    spawn_mode: AgentSpawnMode::FreshSubagent {
                        agent_nickname: Some("force-close-test".to_string()),
                        agent_role: Some("workflow-test".to_string()),
                    },
                    thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
                },
                CancellationToken::new(),
                move |thread_id| {
                    started_tx
                        .send(thread_id)
                        .expect("test should still be waiting for child startup");
                },
                |_| Box::pin(async {}),
            )
            .await
    });
    let child_id = started_rx.await.expect("child should start");
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_agent_running(&child).await;

    assert_eq!(
        runner.force_terminate(child_id).await?,
        codex_core::ThreadTeardownStatus::Confirmed
    );
    tokio::time::timeout(Duration::from_millis(100), child.wait_until_terminated())
        .await
        .expect("force_close should wait for session teardown");
    let error = match run.await.expect("agent wait task should finish") {
        Ok(_) => panic!("forced child should not complete normally"),
        Err(error) => error,
    };

    assert!(matches!(
        &error,
        AgentRunError::Codex { error, .. }
            if matches!(
                error.details(),
                codex_protocol::error::CodexErrorDetails::Interrupted
            )
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_close_times_out_without_starting_pending_mailbox_work() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "gated SSE server is host-local");

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(BlockingThreadStop {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));
    let (response_gate_tx, response_gate_rx) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created(
                "force-close-timeout-response",
            )]),
        },
        StreamingSseChunk {
            gate: Some(response_gate_rx),
            body: responses::sse(vec![responses::ev_completed(
                "force-close-timeout-response",
            )]),
        },
    ]])
    .await;
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&server)
        .await?;
    let runner = AgentRunner::new(Arc::downgrade(&test.thread_manager));
    let parent_thread_id = test.session_configured.session_id.into();
    let config = test.config.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let run = tokio::spawn(async move {
        runner
            .run_to_completion_with_progress(
                parent_thread_id,
                AgentInvocation {
                    config,
                    prompt: "Wait for a delayed response.".to_string(),
                    additional_context: Default::default(),
                    parent_trace: None,
                },
                AgentCompletionOptions {
                    output_schema: None,
                    progress_timeout: None,
                    spawn_mode: AgentSpawnMode::FreshSubagent {
                        agent_nickname: Some("force-close-timeout-test".to_string()),
                        agent_role: Some("workflow-test".to_string()),
                    },
                    thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
                },
                CancellationToken::new(),
                move |thread_id| {
                    started_tx.send(thread_id).expect("child should start");
                },
                |_| Box::pin(async {}),
            )
            .await
    });
    let child_id = started_rx.await.expect("child should start");
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_agent_running(&child).await;
    server.wait_for_request_count(1).await;
    child
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path should parse"),
                AgentPath::root(),
                Vec::new(),
                "pending mailbox work".to_string(),
                /*trigger_turn*/ true,
            ),
            start_options: Default::default(),
        })
        .await?;
    child.submit(Op::RealtimeConversationListVoices).await?;
    wait_for_event(&child, |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;

    let closing = {
        let child = Arc::clone(&child);
        tokio::spawn(async move { child.force_close(Duration::from_millis(100)).await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("real session teardown should reach the blocking lifecycle hook");
    assert_eq!(
        closing.await.expect("force-close task should finish"),
        codex_core::ThreadTeardownStatus::TimedOut
    );
    assert_eq!(server.requests().await.len(), 1);

    let repeated_wait = {
        let child = Arc::clone(&child);
        tokio::spawn(async move { child.force_close(Duration::from_secs(5)).await })
    };
    tokio::task::yield_now().await;
    assert!(!repeated_wait.is_finished());
    release.notify_one();
    assert_eq!(
        repeated_wait
            .await
            .expect("repeated force-close wait should finish"),
        codex_core::ThreadTeardownStatus::Confirmed
    );
    assert_eq!(server.requests().await.len(), 1);
    let _ = run.await;
    drop(response_gate_tx);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn followup_preserves_the_existing_agent_conversation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const INITIAL_PROMPT: &str = "Return a structured answer.";
    const INVALID_OUTPUT: &str = "not valid JSON";
    const FOLLOWUP_PROMPT: &str = "Correct the previous output and return only valid JSON.";
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("initial-agent-response"),
                responses::ev_assistant_message("initial-agent-message", INVALID_OUTPUT),
                responses::ev_completed_with_tokens("initial-agent-response", 11),
            ]),
            responses::sse(vec![
                responses::ev_response_created("followup-agent-response"),
                responses::ev_assistant_message(
                    "followup-agent-message",
                    r#"{"answer":"corrected"}"#,
                ),
                responses::ev_completed_with_tokens("followup-agent-response", 13),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false,
    });
    let additional_context =
        BTreeMap::from([
            WorkflowChildPreamble::new("FOLLOWUP_CONTEXT_MARKER").into_additional_context()
        ]);

    let initial = agent_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: INITIAL_PROMPT.to_string(),
                additional_context: additional_context.clone(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: Some(schema.clone()),
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    agent_nickname: Some("structured-agent".to_string()),
                    agent_role: Some("workflow-test".to_string()),
                },
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
        .await?;
    let corrected = agent_runner
        .run_followup_to_completion(
            AgentFollowup {
                thread_id: initial.thread_id,
                prompt: FOLLOWUP_PROMPT.to_string(),
                additional_context,
                output_schema: Some(schema.clone()),
                progress_timeout: None,
                parent_trace: None,
            },
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(initial.output, INVALID_OUTPUT);
    assert_eq!(corrected.thread_id, initial.thread_id);
    assert_eq!(corrected.output, r#"{"answer":"corrected"}"#);
    assert_eq!(
        corrected
            .token_usage
            .map(|usage| usage.total_token_usage.total_tokens),
        Some(24)
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body_json()["text"]["format"]["schema"], schema);
    for expected in [INITIAL_PROMPT, INVALID_OUTPUT, FOLLOWUP_PROMPT] {
        assert!(requests[1].body_contains_text(expected));
    }
    assert_eq!(
        requests[1]
            .message_input_texts("developer")
            .into_iter()
            .filter(|text| text.contains("FOLLOWUP_CONTEXT_MARKER"))
            .collect::<Vec<_>>(),
        vec!["<workflow_child_0_preamble>FOLLOWUP_CONTEXT_MARKER</workflow_child_0_preamble>"]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counts_tool_items_while_running_to_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("agent-tool-response"),
                responses::ev_exec_command_call("agent-shell-call", "pwd"),
                responses::ev_completed("agent-tool-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("agent-final-response"),
                responses::ev_assistant_message("agent-final-message", "done"),
                responses::ev_completed("agent-final-response"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));

    let completion = agent_runner
        .run_to_completion(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: "Run one command, then finish.".to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            None,
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(completion.tool_uses, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_timeout_reports_a_stalled_agent() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    responses::mount_response_once(
        &server,
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("delayed-agent-response"),
            responses::ev_assistant_message("delayed-agent-message", "too late"),
            responses::ev_completed("delayed-agent-response"),
        ]))
        .set_delay(Duration::from_secs(1)),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let progress_timeout = Duration::from_millis(50);

    let result = agent_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: "Wait for a delayed response.".to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: Some(progress_timeout),
                spawn_mode: Default::default(),
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(
        result,
        Err(AgentRunError::Stalled { timeout, .. }) if timeout == progress_timeout
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_subagent_uses_resolved_config_without_parent_history() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const PARENT_PROMPT: &str = "PARENT_HISTORY_MARKER";
    const PARENT_OUTPUT: &str = "PARENT_OUTPUT_MARKER";
    const CHILD_PROMPT: &str = "CHILD_PROMPT_MARKER";
    const CHILD_INSTRUCTIONS: &str = "FRESH_CHILD_INSTRUCTIONS_MARKER";

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("parent-response"),
                responses::ev_assistant_message("parent-message", PARENT_OUTPUT),
                responses::ev_completed("parent-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("child-response"),
                responses::ev_assistant_message("child-message", "child done"),
                responses::ev_completed("child-response"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    test.submit_turn(PARENT_PROMPT).await?;

    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut child_config = test.config.clone();
    child_config.developer_instructions = Some(CHILD_INSTRUCTIONS.to_string());
    let completion = agent_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: child_config,
                prompt: CHILD_PROMPT.to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    agent_nickname: Some("fresh-agent".to_string()),
                    agent_role: Some("workflow-test".to_string()),
                },
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(completion.output, "child done");
    let child_config = test
        .thread_manager
        .get_thread(completion.thread_id)
        .await?
        .config_snapshot()
        .await;
    assert_eq!(
        (
            child_config.session_source,
            child_config.parent_thread_id,
            child_config.forked_from_thread_id,
            child_config.thread_source,
        ),
        (
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("fresh-agent".to_string()),
                agent_role: Some("workflow-test".to_string()),
            }),
            Some(parent_thread_id),
            None,
            Some(ThreadSource::Subagent),
        )
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let child_request = &requests[1];
    assert!(child_request.body_contains_text(CHILD_PROMPT));
    assert!(child_request.body_contains_text(CHILD_INSTRUCTIONS));
    assert!(!child_request.body_contains_text(PARENT_PROMPT));
    assert!(!child_request.body_contains_text(PARENT_OUTPUT));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_subagent_notifies_host_without_competing_for_completion_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("observed-agent-response"),
            responses::ev_assistant_message("observed-agent-message", "observed done"),
            responses::ev_completed("observed-agent-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));
    let mut thread_created = test.thread_manager.subscribe_thread_created();
    let config = test.config.clone();

    let completion_task = tokio::spawn(async move {
        runner
            .run_to_completion_with_options(
                parent_thread_id,
                AgentInvocation {
                    config,
                    prompt: "Complete while a host listener observes this turn.".to_string(),
                    additional_context: Default::default(),
                    parent_trace: None,
                },
                AgentCompletionOptions {
                    output_schema: None,
                    progress_timeout: None,
                    spawn_mode: AgentSpawnMode::FreshSubagent {
                        agent_nickname: Some("observed-agent".to_string()),
                        agent_role: Some("workflow-test".to_string()),
                    },
                    thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
                },
                CancellationToken::new(),
            )
            .await
    });

    let child_thread_id =
        tokio::time::timeout(Duration::from_secs(5), thread_created.recv()).await??;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let completed_event = wait_for_event_with_timeout(
        &child_thread,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(30),
    )
    .await;
    let EventMsg::TurnComplete(completed_event) = completed_event else {
        unreachable!("event predicate only matches turn complete events");
    };
    let completion = completion_task.await??;

    assert_eq!(completion.thread_id, child_thread_id);
    assert_eq!(completion.output, "observed done");
    assert_eq!(
        completed_event.last_agent_message.as_deref(),
        Some("observed done")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_subagents_do_not_share_the_parent_rollout_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("first-budget-agent"),
                responses::ev_assistant_message("first-budget-message", "first done"),
                responses::ev_completed_with_tokens("first-budget-agent", 30),
            ]),
            responses::sse(vec![
                responses::ev_response_created("second-budget-agent"),
                responses::ev_assistant_message("second-budget-message", "second done"),
                responses::ev_completed_with_tokens("second-budget-agent", 30),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 50,
                reminder_at_remaining_tokens: vec![25],
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
        })
        .build_with_auto_env(&server)
        .await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));

    let run = |prompt: &str| {
        agent_runner.run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: prompt.to_string(),
                additional_context: Default::default(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    agent_nickname: None,
                    agent_role: Some("workflow-test".to_string()),
                },
                thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
            },
            CancellationToken::new(),
        )
    };

    let first = run("complete the first fresh subagent").await?;
    let second = run("complete the second fresh subagent").await?;

    assert_eq!(
        (first.output, second.output),
        ("first done".to_string(), "second done".to_string())
    );
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}
