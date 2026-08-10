#![recursion_limit = "256"]

use anyhow::Result;
use codex_agent_extension::AgentCompletionOptions;
use codex_agent_extension::AgentFollowup;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentRolloutBudget;
use codex_agent_extension::AgentRunError;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::AgentSpawnMode;
use codex_core::config::RolloutBudgetConfig;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

    let initial = agent_runner
        .run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: INITIAL_PROMPT.to_string(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: Some(schema.clone()),
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    source: "workflow-test".to_string(),
                    rollout_budget: AgentRolloutBudget::Observe,
                },
            },
            CancellationToken::new(),
        )
        .await?;
    let corrected = agent_runner
        .run_followup_to_completion(
            AgentFollowup {
                thread_id: initial.thread_id,
                prompt: FOLLOWUP_PROMPT.to_string(),
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
                responses::ev_function_call(
                    "agent-shell-call",
                    "shell_command",
                    &json!({
                        "command": "pwd",
                        "login": false,
                        "timeout_ms": 1_000,
                    })
                    .to_string(),
                ),
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
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: Some(progress_timeout),
                spawn_mode: Default::default(),
            },
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(
        result,
        Err(AgentRunError::Stalled { timeout }) if timeout == progress_timeout
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
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    source: "workflow-test".to_string(),
                    rollout_budget: AgentRolloutBudget::Enforce,
                },
            },
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(completion.output, "child done");
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
async fn fresh_subagents_share_the_parent_rollout_budget() -> Result<()> {
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
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    source: "workflow-test".to_string(),
                    rollout_budget: AgentRolloutBudget::Enforce,
                },
            },
            CancellationToken::new(),
        )
    };

    let first = run("consume the first budget share").await?;
    let second = run("exhaust the shared budget").await;

    assert_eq!(first.output, "first done");
    assert!(matches!(
        second,
        Err(AgentRunError::Codex(error))
            if error.to_string().contains("shared rollout token budget exhausted")
    ));
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observing_fresh_subagent_records_budget_and_preserves_the_crossing_result() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("observed-budget-first"),
                responses::ev_assistant_message("observed-budget-first-message", "first done"),
                responses::ev_completed_with_tokens("observed-budget-first", 30),
            ]),
            responses::sse(vec![
                responses::ev_response_created("observed-budget-crossing"),
                responses::ev_assistant_message(
                    "observed-budget-crossing-message",
                    "crossing result",
                ),
                responses::ev_completed_with_tokens("observed-budget-crossing", 30),
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

    let run = |prompt: &str, rollout_budget| {
        agent_runner.run_to_completion_with_options(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: prompt.to_string(),
                parent_trace: None,
            },
            AgentCompletionOptions {
                output_schema: None,
                progress_timeout: None,
                spawn_mode: AgentSpawnMode::FreshSubagent {
                    source: "workflow-test".to_string(),
                    rollout_budget,
                },
            },
            CancellationToken::new(),
        )
    };

    let first = run(
        "consume the first budget share",
        AgentRolloutBudget::Enforce,
    )
    .await?;
    let crossing = run(
        "finish the already-launched crossing request",
        AgentRolloutBudget::Observe,
    )
    .await?;

    assert_eq!(
        (first.output, crossing.output),
        ("first done".to_string(), "crossing result".to_string())
    );
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}
