use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owning_workflow_tools_reject_runs_from_another_thread() -> Result<()> {
    const FOREIGN_PROMPT: &str = "Try every workflow tool against another thread";
    const FOREIGN_WAIT_CALL_ID: &str = "foreign-wait";
    const FOREIGN_WAIT_MANY_CALL_ID: &str = "foreign-wait-many";
    const FOREIGN_READ_CALL_ID: &str = "foreign-read";
    const FOREIGN_STOP_CALL_ID: &str = "foreign-stop";
    const FOREIGN_RETRY_CALL_ID: &str = "foreign-retry";
    const FOREIGN_SKIP_CALL_ID: &str = "foreign-skip";
    const FOREIGN_LIST_CALL_ID: &str = "foreign-list";
    let script = r#"export const meta = {
  name: "cross-thread-owner",
  description: "Remain active for cross-thread ownership checks",
};
return new Promise(() => {});
"#;
    let mut fixture = start_workflow(script, "Launch a thread-owned workflow", "Workflow").await?;
    let started: WorkflowStartedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        fixture.mcp.read_notification("workflow/started"),
    )
    .await??;
    wait_for_turn_completed(&mut fixture.mcp, &fixture.thread_id, &fixture.turn_id).await?;

    responses::mount_sse_sequence(
        &fixture._server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-1"),
                responses::ev_function_call(
                    FOREIGN_WAIT_CALL_ID,
                    "WaitWorkflow",
                    &json!({ "runId": &started.run_id, "timeoutMs": 1_000 }).to_string(),
                ),
                responses::ev_completed("foreign-owner-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-2"),
                responses::ev_function_call(
                    FOREIGN_WAIT_MANY_CALL_ID,
                    "WaitWorkflows",
                    &json!({ "runIds": [&started.run_id], "timeoutMs": 1_000 }).to_string(),
                ),
                responses::ev_completed("foreign-owner-2"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-3"),
                responses::ev_function_call(
                    FOREIGN_READ_CALL_ID,
                    "ReadWorkflowResult",
                    &json!({ "runId": &started.run_id }).to_string(),
                ),
                responses::ev_completed("foreign-owner-3"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-4"),
                responses::ev_function_call(
                    FOREIGN_STOP_CALL_ID,
                    "StopWorkflow",
                    &json!({ "runId": &started.run_id }).to_string(),
                ),
                responses::ev_completed("foreign-owner-4"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-5"),
                responses::ev_function_call(
                    FOREIGN_RETRY_CALL_ID,
                    "RetryWorkflowAgent",
                    &json!({ "runId": &started.run_id, "agentIndex": 0 }).to_string(),
                ),
                responses::ev_completed("foreign-owner-5"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-6"),
                responses::ev_function_call(
                    FOREIGN_SKIP_CALL_ID,
                    "SkipWorkflowAgent",
                    &json!({ "runId": &started.run_id, "agentIndex": 0 }).to_string(),
                ),
                responses::ev_completed("foreign-owner-6"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-7"),
                responses::ev_function_call(FOREIGN_LIST_CALL_ID, "ListWorkflows", "{}"),
                responses::ev_completed("foreign-owner-7"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("foreign-owner-final"),
                responses::ev_assistant_message(
                    "foreign-owner-message",
                    "cross-thread checks complete",
                ),
                responses::ev_completed("foreign-owner-final"),
            ]),
        ],
    )
    .await;

    let ThreadStartResponse {
        thread: foreign_thread,
        ..
    } = fixture
        .mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = fixture
        .mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: foreign_thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: FOREIGN_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut fixture.mcp, &foreign_thread.id, &turn.id).await?;

    let requests = fixture
        ._server
        .received_requests()
        .await
        .context("failed to read cross-thread model requests")?;
    for call_id in [
        FOREIGN_WAIT_CALL_ID,
        FOREIGN_WAIT_MANY_CALL_ID,
        FOREIGN_READ_CALL_ID,
        FOREIGN_STOP_CALL_ID,
        FOREIGN_RETRY_CALL_ID,
        FOREIGN_SKIP_CALL_ID,
    ] {
        let output = captured_tool_output_text(&requests, call_id)
            .with_context(|| format!("missing cross-thread output for {call_id}"))?;
        assert!(
            output.contains("workflow run belongs to a different thread"),
            "unexpected cross-thread output for {call_id}: {output}"
        );
    }
    let list = captured_tool_output(&requests, FOREIGN_LIST_CALL_ID)
        .context("missing cross-thread ListWorkflows output")?;
    assert_eq!(list["totalMatched"], 0);
    assert_eq!(list["workflows"], json!([]));

    let stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id,
                run_id: started.run_id,
            },
        })
        .await?;
    assert!(stop.accepted);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owning_model_retries_and_skips_workflow_agents_without_affecting_siblings() -> Result<()> {
    const RETRY_LAUNCH_PROMPT: &str = "Launch the retry-controlled workflow";
    const SKIP_LAUNCH_PROMPT: &str = "Launch the skip-controlled workflow";
    const RETRY_AGENT_PROMPT: &str = "workflow agent controlled by retry";
    const SKIP_AGENT_PROMPT: &str = "workflow agent controlled by skip";
    const RETRY_LAUNCH_CALL_ID: &str = "controlled-retry-launch";
    const SKIP_LAUNCH_CALL_ID: &str = "controlled-skip-launch";
    const RETRY_CONTROL_CALL_ID: &str = "controlled-retry-action";
    const SKIP_CONTROL_CALL_ID: &str = "controlled-skip-action";
    const RETRY_CONTROL_PROMPT: &str = "Retry the active workflow agent";
    const SKIP_CONTROL_PROMPT: &str = "Skip the other active workflow agent";
    const RETRY_SCRIPT: &str = r#"export const meta = {
  name: "controlled-retry",
  description: "Retry one active workflow agent",
};
return await agent("workflow agent controlled by retry", { label: "retry-agent" });
"#;
    const SKIP_SCRIPT: &str = r#"export const meta = {
  name: "controlled-skip",
  description: "Skip one active workflow agent",
};
return await agent("workflow agent controlled by skip", { label: "skip-agent" });
"#;

    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, RETRY_AGENT_PROMPT)
        })
        .respond_with(RetryControlledWorkflowAgentResponder::default())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, SKIP_AGENT_PROMPT)
        })
        .respond_with(
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("controlled-skip-child"),
                responses::ev_assistant_message("controlled-skip-child-message", "too late"),
                responses::ev_completed("controlled-skip-child"),
            ]))
            .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
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

    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, RETRY_LAUNCH_PROMPT)
                && !body_contains(request, RETRY_LAUNCH_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("controlled-retry-parent-1"),
            responses::ev_function_call(
                RETRY_LAUNCH_CALL_ID,
                "Workflow",
                &json!({ "script": RETRY_SCRIPT }).to_string(),
            ),
            responses::ev_completed("controlled-retry-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, RETRY_LAUNCH_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("controlled-retry-parent-2"),
            responses::ev_assistant_message("controlled-retry-parent-message", "launched"),
            responses::ev_completed("controlled-retry-parent-2"),
        ]),
    )
    .await;
    let TurnStartResponse { turn: retry_turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: RETRY_LAUNCH_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &retry_turn.id).await?;
    let retry_run_id =
        wait_for_captured_workflow_run_id(&server, RETRY_LAUNCH_CALL_ID, RETRY_AGENT_PROMPT)
            .await?;

    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SKIP_LAUNCH_PROMPT)
                && !body_contains(request, SKIP_LAUNCH_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("controlled-skip-parent-1"),
            responses::ev_function_call(
                SKIP_LAUNCH_CALL_ID,
                "Workflow",
                &json!({ "script": SKIP_SCRIPT }).to_string(),
            ),
            responses::ev_completed("controlled-skip-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SKIP_LAUNCH_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("controlled-skip-parent-2"),
            responses::ev_assistant_message("controlled-skip-parent-message", "launched"),
            responses::ev_completed("controlled-skip-parent-2"),
        ]),
    )
    .await;
    let TurnStartResponse { turn: skip_turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: SKIP_LAUNCH_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &skip_turn.id).await?;
    let skip_run_id =
        wait_for_captured_workflow_run_id(&server, SKIP_LAUNCH_CALL_ID, SKIP_AGENT_PROMPT).await?;

    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, RETRY_CONTROL_PROMPT)
                && !body_contains(request, RETRY_CONTROL_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("controlled-retry-action-parent-1"),
            responses::ev_function_call(
                RETRY_CONTROL_CALL_ID,
                "RetryWorkflowAgent",
                &json!({ "runId": &retry_run_id, "agentIndex": 0 }).to_string(),
            ),
            responses::ev_completed("controlled-retry-action-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, RETRY_CONTROL_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("controlled-retry-action-parent-2"),
            responses::ev_assistant_message("controlled-retry-action-message", "retried"),
            responses::ev_completed("controlled-retry-action-parent-2"),
        ]),
    )
    .await;
    let TurnStartResponse {
        turn: retry_control_turn,
    } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: RETRY_CONTROL_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &retry_control_turn.id).await?;
    wait_for_listed_workflow_status(
        &mut mcp,
        &thread.id,
        &retry_run_id,
        WorkflowStatus::Completed,
    )
    .await?;
    let listed: WorkflowListResponse = mcp
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams {
                thread_id: thread.id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(
        listed
            .data
            .iter()
            .find(|workflow| workflow.run_id == skip_run_id)
            .context("skip workflow disappeared after retrying its sibling")?
            .status,
        WorkflowStatus::Running
    );

    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SKIP_CONTROL_PROMPT)
                && !body_contains(request, SKIP_CONTROL_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("controlled-skip-action-parent-1"),
            responses::ev_function_call(
                SKIP_CONTROL_CALL_ID,
                "SkipWorkflowAgent",
                &json!({ "runId": &skip_run_id, "agentIndex": 0 }).to_string(),
            ),
            responses::ev_completed("controlled-skip-action-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SKIP_CONTROL_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("controlled-skip-action-parent-2"),
            responses::ev_assistant_message("controlled-skip-action-message", "skipped"),
            responses::ev_completed("controlled-skip-action-parent-2"),
        ]),
    )
    .await;
    let TurnStartResponse {
        turn: skip_control_turn,
    } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: SKIP_CONTROL_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &skip_control_turn.id).await?;
    wait_for_listed_workflow_status(
        &mut mcp,
        &thread.id,
        &skip_run_id,
        WorkflowStatus::Completed,
    )
    .await?;

    let requests = server
        .received_requests()
        .await
        .context("failed to read controlled workflow requests")?;
    let retry_output = captured_tool_output(&requests, RETRY_CONTROL_CALL_ID)
        .context("missing RetryWorkflowAgent output")?;
    assert_eq!(retry_output["runId"], retry_run_id);
    assert_eq!(retry_output["action"], "retryAgent");
    assert_eq!(retry_output["accepted"], true);
    let skip_output = captured_tool_output(&requests, SKIP_CONTROL_CALL_ID)
        .context("missing SkipWorkflowAgent output")?;
    assert_eq!(skip_output["runId"], skip_run_id);
    assert_eq!(skip_output["action"], "skipAgent");
    assert_eq!(skip_output["accepted"], true);
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                body_contains(request, "You are a workflow subagent.")
                    && body_contains(request, RETRY_AGENT_PROMPT)
            })
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                body_contains(request, "You are a workflow subagent.")
                    && body_contains(request, SKIP_AGENT_PROMPT)
            })
            .count(),
        1
    );
    Ok(())
}
