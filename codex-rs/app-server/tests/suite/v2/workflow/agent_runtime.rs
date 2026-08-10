use super::*;
use core_test_support::TestTargetOs;
use core_test_support::is_remote_test_environment;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_transcript_is_live_interactive_and_replayable() -> Result<()> {
    const PARENT_PROMPT: &str = "Run an interactive workflow agent";
    const STEER_TEXT: &str = "Focus the final answer on transcript interoperability.";
    let script = r#"export const meta = {
  name: "interactive-agent-transcript",
  description: "Verify live and completed workflow agent transcripts",
};
return await agent("Inspect transcript interoperability", { label: "interactive-agent" });
"#
    .to_string();
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PARENT_PROMPT)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-interactive-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-interactive-parent-1"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| body_contains(request, "You are a workflow subagent."))
        .respond_with(InteractiveWorkflowAgentResponder::default())
        .mount(&server)
        .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-interactive-parent-2"),
            responses::ev_assistant_message(
                "workflow-interactive-parent-message",
                "Workflow launched",
            ),
            responses::ev_completed("workflow-interactive-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let parent_thread_id = thread.id;
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: parent_thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let (run_id, child_thread_id) = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_started_workflow_agent(&mut mcp, "interactive-agent"),
    )
    .await??;
    let running_read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: child_thread_id.clone(),
            include_turns: true,
        })
        .await?;
    let ThreadReadResponse {
        thread: running_thread,
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(running_read_id)).await??;
    assert_eq!(running_thread.can_accept_direct_input, Some(true));
    let running_turn = running_thread
        .turns
        .last()
        .context("running workflow agent has no transcript turn")?;
    assert_eq!(running_turn.status, TurnStatus::InProgress);
    let child_turn_id = running_turn.id.clone();

    let steer_id = mcp
        .send_turn_steer_request(TurnSteerParams {
            thread_id: child_thread_id.clone(),
            client_user_message_id: Some("workflow-agent-steer".to_string()),
            input: vec![UserInput::Text {
                text: STEER_TEXT.to_string(),
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: None,
            additional_context: None,
            expected_turn_id: child_turn_id.clone(),
        })
        .await?;
    let steer: TurnSteerResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(steer_id)).await??;
    assert_eq!(steer.turn_id, child_turn_id);

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_workflow_completion(&mut mcp, &run_id),
    )
    .await??;
    assert_eq!(completed.status, WorkflowStatus::Completed);

    let completed_read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: child_thread_id.clone(),
            include_turns: true,
        })
        .await?;
    let completed_thread: ThreadReadResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(completed_read_id)).await??;
    let completed_turn = completed_thread
        .thread
        .turns
        .last()
        .context("completed workflow agent has no transcript turn")?;
    assert_eq!(completed_turn.status, TurnStatus::Completed);
    assert_eq!(
        completed_thread.thread.parent_thread_id.as_deref(),
        Some(parent_thread_id.as_str())
    );
    assert!(serde_json::to_string(&completed_thread.thread)?.contains(STEER_TEXT));

    timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;
    let mut restarted = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let list_id = restarted
        .send_thread_list_request(ThreadListParams {
            cursor: None,
            limit: Some(10),
            sort_key: None,
            sort_direction: None,
            model_providers: Some(Vec::new()),
            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
            archived: None,
            section_id: None,
            project_id: None,
            cwd: None,
            use_state_db_only: true,
            search_term: None,
            parent_thread_id: Some(parent_thread_id.clone()),
            ancestor_thread_id: None,
        })
        .await?;
    let listed: ThreadListResponse =
        timeout(DEFAULT_READ_TIMEOUT, restarted.read_response(list_id)).await??;
    assert_eq!(
        listed
            .data
            .iter()
            .map(|thread| (thread.id.clone(), thread.parent_thread_id.clone()))
            .collect::<Vec<_>>(),
        vec![(child_thread_id.clone(), Some(parent_thread_id))]
    );

    let replay_id = restarted
        .send_thread_read_request(ThreadReadParams {
            thread_id: child_thread_id,
            include_turns: true,
        })
        .await?;
    let replayed: ThreadReadResponse =
        timeout(DEFAULT_READ_TIMEOUT, restarted.read_response(replay_id)).await??;
    assert_eq!(
        replayed
            .thread
            .turns
            .last()
            .context("restarted app-server did not replay the workflow agent turn")?
            .status,
        TurnStatus::Completed
    );
    assert!(serde_json::to_string(&replayed.thread)?.contains(STEER_TEXT));

    let child_requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| body_contains(request, "You are a workflow subagent."))
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 2);
    assert!(body_contains(&child_requests[1], STEER_TEXT));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_structured_agent_uses_prompt_fallback_for_non_openai_provider() -> Result<()> {
    let script = r#"export const meta = {
  name: "structured-output-fallback",
  description: "Verify structured output on providers without native schema support",
};
return await agent("Return the compatibility result", {
  label: "structured-agent",
  schema: {
    type: "object",
    properties: { answer: { type: "string" } },
    required: ["answer"],
    additionalProperties: false,
  },
});
"#;
    let parent_prompt = "Run a structured workflow agent";
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, parent_prompt)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-structured-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-structured-parent-1"),
        ]),
    )
    .await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request) && body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-structured-child"),
            responses::ev_assistant_message(
                "workflow-structured-child-message",
                r#"{"answer":"compatible"}"#,
            ),
            responses::ev_completed("workflow-structured-child"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-structured-parent-2"),
            responses::ev_assistant_message(
                "workflow-structured-parent-message",
                "Workflow launched",
            ),
            responses::ev_completed("workflow-structured-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_id = thread.id;
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: parent_prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    let child_requests = child_turn.requests();
    assert_eq!(child_requests.len(), 1);
    assert!(
        child_requests
            .iter()
            .all(|request| request.body_json().pointer("/text/format").is_none())
    );
    assert_workflow_child_context(
        &child_requests[0],
        "Return the compatibility result",
        None,
        Some(&[
            "Return only a JSON value matching this schema",
            r#""answer":{"type":"string"}"#,
        ]),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_workflow_agent_inherits_the_auto_env_remote_executor() -> Result<()> {
    const PARENT_PROMPT: &str = "Run an inline Workflow child in the selected executor";
    const CHILD_PROMPT: &str = "Verify the selected executor cwd";
    const EXEC_CALL_ID: &str = "workflow-remote-exec";
    const MARKER: &str = "WORKFLOW_REMOTE_EXECUTOR_MARKER";
    let script = r#"export const meta = {
  name: "remote-executor-inheritance",
  description: "Verify Workflow child executor inheritance",
};
return agent("Verify the selected executor cwd", { label: "remote-executor" });
"#;
    let remote = is_remote_test_environment();
    let (shell, command) = match (remote, test_target_os()) {
        (true, TestTargetOs::Linux) => (
            "bash",
            r#"case "$PWD" in /tmp/codex-core-test-cwd-*) printf 'WORKFLOW_REMOTE_EXECUTOR_MARKER\n' ;; *) echo "unexpected cwd: $PWD" >&2; exit 1 ;; esac"#,
        ),
        (true, TestTargetOs::Windows) => (
            "powershell",
            r#"$cwd = (Get-Location).Path; if ($cwd -notlike 'C:\codex-core-test-cwd-*') { Write-Error "unexpected cwd: $cwd"; exit 1 }; Write-Output 'WORKFLOW_REMOTE_EXECUTOR_MARKER'"#,
        ),
        (true, TestTargetOs::MacOs) => unreachable!("remote tests do not target macOS"),
        (false, TestTargetOs::Windows) => (
            "powershell",
            "Write-Output 'WORKFLOW_REMOTE_EXECUTOR_MARKER'",
        ),
        (false, TestTargetOs::Linux | TestTargetOs::MacOs) => {
            ("bash", "printf 'WORKFLOW_REMOTE_EXECUTOR_MARKER\\n'")
        }
    };
    let exec_arguments = json!({
        "cmd": command,
        "shell": shell,
        "login": false,
        "yield_time_ms": 10_000,
    })
    .to_string();

    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PARENT_PROMPT)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-remote-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-remote-parent-1"),
        ]),
    )
    .await;
    let child_first = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_PROMPT)
                && !body_contains(request, EXEC_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-remote-child-1"),
            responses::ev_function_call(EXEC_CALL_ID, "exec_command", &exec_arguments),
            responses::ev_completed("workflow-remote-child-1"),
        ]),
    )
    .await;
    let child_second = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request) && body_contains(request, EXEC_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-remote-child-2"),
            responses::ev_assistant_message("workflow-remote-child-message", "executor verified"),
            responses::ev_completed("workflow-remote-child-2"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-remote-parent-2"),
            responses::ev_assistant_message("workflow-remote-parent-message", "Workflow launched"),
            responses::ev_completed("workflow-remote-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .enable_feature(Feature::UnifiedExec)
        .with_sandbox_mode("danger-full-access")
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let thread_start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            environments: None,
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(thread_start_id)).await??;
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id,
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.agent_count, 1);
    let child_request = child_first.single_request();
    assert_workflow_child_context(&child_request, CHILD_PROMPT, None, None);
    let child_tool_names = response_tool_names(&child_request.body_json());
    for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
        assert!(
            !child_tool_names.iter().any(|name| name == tool_name),
            "workflow child exposed owning tool {tool_name}: {child_tool_names:?}"
        );
    }
    let child_follow_up = child_second
        .requests()
        .into_iter()
        .find(|request| {
            request.input().iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(serde_json::Value::as_str) == Some(EXEC_CALL_ID)
            })
        })
        .context("Workflow child should receive its exec_command result")?;
    let (output, success) = child_follow_up
        .function_call_output_content_and_success(EXEC_CALL_ID)
        .context("Workflow child exec_command output should be readable")?;
    if remote {
        assert_ne!(success, Some(false));
        let output = output.context("remote exec_command output")?;
        assert!(output.contains("Process exited with code 0"));
        assert!(output.contains(MARKER));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agents_are_not_limited_by_the_shared_rollout_budget() -> Result<()> {
    const EXEC_CALL_ID: &str = "workflow-budget-exec";
    let script = r#"export const meta = {
  name: "budget-independent",
  description: "Verify workflow agents continue while reporting usage",
};
const first = await parallel([() => agent("Consume the available workflow budget")]);
const second = await parallel([
  () => agent("Consume workflow budget A"),
  () => agent("Consume workflow budget B"),
]);
const final = await parallel([() => agent("Continue after the owning budget is spent")]);
return [first[0], second[0], second[1], final[0]];
"#;
    let parent_prompt = "Run a budget-limited workflow";
    let (shell, command) = match test_target_os() {
        TestTargetOs::Windows => ("powershell", "Write-Output 'workflow budget tool use'"),
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            ("bash", "printf 'workflow budget tool use\\n'")
        }
    };
    let exec_arguments = json!({
        "cmd": command,
        "shell": shell,
        "login": false,
        "yield_time_ms": 10_000,
    })
    .to_string();
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, parent_prompt)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed_with_tokens("workflow-budget-parent-1", 5),
        ]),
    )
    .await;
    let first_child = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume the available workflow budget")
                && !body_contains(request, EXEC_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-first"),
            responses::ev_function_call(EXEC_CALL_ID, "exec_command", &exec_arguments),
            responses::ev_completed_with_tokens("workflow-budget-child-first", 26),
        ]),
    )
    .await;
    let first_child_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request) && body_contains(request, EXEC_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-first-follow-up"),
            responses::ev_assistant_message("workflow-budget-child-first-message", "first"),
            responses::ev_completed_with_tokens("workflow-budget-child-first-follow-up", 4),
        ]),
    )
    .await;
    let child_a = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume workflow budget A")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-a"),
            responses::ev_assistant_message("workflow-budget-child-a-message", "A"),
            responses::ev_completed_with_tokens("workflow-budget-child-a", 30),
        ]),
    )
    .await;
    let child_b = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume workflow budget B")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-b"),
            responses::ev_assistant_message("workflow-budget-child-b-message", "B"),
            responses::ev_completed_with_tokens("workflow-budget-child-b", 30),
        ]),
    )
    .await;
    let final_child = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Continue after the owning budget is spent")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-final"),
            responses::ev_assistant_message("workflow-budget-child-final-message", "final"),
            responses::ev_completed_with_tokens("workflow-budget-child-final", 30),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-parent-2"),
            responses::ev_assistant_message("workflow-budget-parent-message", "Workflow launched"),
            responses::ev_completed_with_tokens("workflow-budget-parent-2", 5),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .enable_feature(Feature::UnifiedExec)
        .with_extra_config(
            r#"[features.rollout_budget]
enabled = true
limit_tokens = 25
reminder_at_remaining_tokens = [10]
sampling_token_weight = 1.0
prefill_token_weight = 1.0"#,
        )
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_id = thread.id;
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: parent_prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.total_tokens, 120);
    assert_eq!(events.completed.usage.tool_uses, 1);
    assert_eq!(events.completed.usage.agent_count, 4);
    assert!(events.completed.failures.is_empty());
    let first_child_request = first_child.single_request();
    assert_workflow_child_context(
        &first_child_request,
        "Consume the available workflow budget",
        None,
        None,
    );
    let first_child_follow_up_request = first_child_follow_up.single_request();
    assert!(
        first_child_follow_up_request
            .function_call_output_content_and_success(EXEC_CALL_ID)
            .is_some()
    );
    let child_a_request = child_a.single_request();
    assert_workflow_child_context(&child_a_request, "Consume workflow budget A", None, None);
    let child_b_request = child_b.single_request();
    assert_workflow_child_context(&child_b_request, "Consume workflow budget B", None, None);
    let final_child_request = final_child.single_request();
    assert_workflow_child_context(
        &final_child_request,
        "Continue after the owning budget is spent",
        None,
        None,
    );
    for label in ["agent-1", "agent-2", "agent-3", "agent-4"] {
        assert!(events.progress.iter().any(|notification| {
            notification.progress.iter().any(|item| {
                matches!(
                    item,
                    WorkflowProgressItem::WorkflowAgent(agent)
                        if agent.label == label && agent.state == WorkflowAgentState::Done
                )
            })
        }));
    }

    const FOLLOW_UP_PROMPT: &str = "Continue after the Workflow completes";
    let owning_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FOLLOW_UP_PROMPT)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-parent-follow-up"),
            responses::ev_assistant_message(
                "workflow-budget-parent-follow-up-message",
                "continued",
            ),
            responses::ev_completed_with_tokens("workflow-budget-parent-follow-up", 10),
        ]),
    )
    .await;
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: FOLLOW_UP_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let completed = wait_for_turn_completed_notification(&mut mcp, &thread_id, &turn.id).await?;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(completed.turn.error, None);
    assert!(completed.turn.items.iter().any(|item| {
        matches!(
            item,
            codex_app_server_protocol::ThreadItem::AgentMessage { text, .. }
                if text == "continued"
        )
    }));
    assert_eq!(owning_follow_up.requests().len(), 1);
    Ok(())
}
