use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_input_interrupts_owning_model_multi_workflow_wait() -> Result<()> {
    const PROMPT: &str = "Wait for two workflows that will remain active";
    const STEER_TEXT: &str = "Stop waiting and respond to this instruction now.";
    const FIRST_SCRIPT: &str = r#"export const meta = {
  name: "steer-interrupts-wait-first",
  description: "First active run while the owning turn waits",
};
return new Promise(() => {});
"#;
    const SECOND_SCRIPT: &str = r#"export const meta = {
  name: "steer-interrupts-wait-second",
  description: "Second active run while the owning turn waits",
};
return new Promise(() => {});
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, WORKFLOW_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-steer-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": FIRST_SCRIPT }).to_string(),
            ),
            responses::ev_completed("workflow-steer-parent-1"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(ToolOutputStageMatcher {
            output_call_id: WORKFLOW_CALL_ID,
            next_call_id: STEER_SECOND_WORKFLOW_CALL_ID,
        })
        .respond_with(LaunchSteerSecondWorkflowResponder {
            script: SECOND_SCRIPT.to_string(),
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(ToolOutputStageMatcher {
            output_call_id: STEER_SECOND_WORKFLOW_CALL_ID,
            next_call_id: STEER_WAIT_WORKFLOWS_CALL_ID,
        })
        .respond_with(LongWaitWorkflowsResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(ToolOutputStageMatcher {
            output_call_id: STEER_WAIT_WORKFLOWS_CALL_ID,
            next_call_id: STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID,
        })
        .respond_with(RepeatLongWaitWorkflowsResponder)
        .expect(1)
        .mount(&server)
        .await;
    let final_response = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID)
                && body_contains(request, STEER_TEXT)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-steer-parent-final"),
            responses::ev_assistant_message(
                "workflow-steer-parent-message",
                "Steered input received while the workflow remains active",
            ),
            responses::ev_completed("workflow-steer-parent-final"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let thread_start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            experimental_raw_events: true,
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(thread_start_id)).await??;
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_raw_response_completed(&mut mcp, "workflow-steer-parent-3").await?;

    let steer: TurnSteerResponse = mcp
        .request(|request_id| ClientRequest::TurnSteer {
            request_id,
            params: TurnSteerParams {
                thread_id: thread.id.clone(),
                client_user_message_id: Some("workflow-wait-steer".to_string()),
                input: vec![UserInput::Text {
                    text: STEER_TEXT.to_string(),
                    text_elements: Vec::new(),
                }],
                responsesapi_client_metadata: None,
                additional_context: None,
                expected_turn_id: turn.id.clone(),
            },
        })
        .await?;
    assert_eq!(steer.turn_id, turn.id);
    timeout(
        Duration::from_secs(5),
        wait_for_turn_completed(&mut mcp, &thread.id, &turn.id),
    )
    .await??;

    let final_request = final_response.single_request();
    assert!(final_request.body_json().to_string().contains(STEER_TEXT));
    assert!(
        final_request
            .function_call_output_text(STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID)
            .is_some(),
        "the repeated interrupted wait did not return control to the owning model"
    );

    let requests = server
        .received_requests()
        .await
        .context("failed to read steered multi-workflow requests")?;
    let first_run_id = captured_tool_output(&requests, WORKFLOW_CALL_ID)
        .context("missing first steered Workflow output")?["runId"]
        .as_str()
        .context("first steered Workflow runId")?
        .to_string();
    let second_run_id = captured_tool_output(&requests, STEER_SECOND_WORKFLOW_CALL_ID)
        .context("missing second steered Workflow output")?["runId"]
        .as_str()
        .context("second steered Workflow runId")?
        .to_string();
    assert_ne!(first_run_id, second_run_id);
    for call_id in [
        STEER_WAIT_WORKFLOWS_CALL_ID,
        STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID,
    ] {
        let wait_output = captured_tool_output(&requests, call_id)
            .with_context(|| format!("missing interrupted output for {call_id}"))?;
        assert_eq!(wait_output["conditionMet"], false);
        assert_eq!(wait_output["timedOut"], false);
        assert_eq!(wait_output["interruptedByUserInput"], true);
        assert_eq!(
            wait_output["workflows"]
                .as_array()
                .context("interrupted WaitWorkflows workflows")?
                .iter()
                .map(|workflow| (
                    workflow["runId"].as_str().unwrap_or_default(),
                    workflow["status"].as_str().unwrap_or_default(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (first_run_id.as_str(), "running"),
                (second_run_id.as_str(), "running"),
            ]
        );
    }
    for run_id in [first_run_id, second_run_id] {
        let stop: WorkflowStopResponse = mcp
            .request(|request_id| ClientRequest::WorkflowStop {
                request_id,
                params: WorkflowStopParams {
                    thread_id: thread.id.clone(),
                    run_id,
                },
            })
            .await?;
        assert!(stop.accepted);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_input_interrupts_owning_model_single_workflow_wait() -> Result<()> {
    const PROMPT: &str = "Wait for one workflow that remains active";
    const STEER_TEXT: &str = "Stop the single workflow wait and answer now.";
    const SCRIPT: &str = r#"export const meta = {
  name: "steer-interrupts-single-wait",
  description: "Remain active while the owning turn waits",
};
return new Promise(() => {});
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, WORKFLOW_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-single-steer-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": SCRIPT }).to_string(),
            ),
            responses::ev_completed("workflow-single-steer-parent-1"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(WorkflowLaunchOutputMatcher)
        .respond_with(WaitWorkflowResponder)
        .expect(1)
        .mount(&server)
        .await;
    let final_response = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WAIT_WORKFLOW_CALL_ID) && body_contains(request, STEER_TEXT)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-single-steer-parent-3"),
            responses::ev_assistant_message(
                "workflow-single-steer-parent-message",
                "single wait interrupted",
            ),
            responses::ev_completed("workflow-single-steer-parent-3"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            experimental_raw_events: true,
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_raw_response_completed(&mut mcp, "workflow-wait-parent-2").await?;

    let steer: TurnSteerResponse = mcp
        .request(|request_id| ClientRequest::TurnSteer {
            request_id,
            params: TurnSteerParams {
                thread_id: thread.id.clone(),
                client_user_message_id: Some("workflow-single-wait-steer".to_string()),
                input: vec![UserInput::Text {
                    text: STEER_TEXT.to_string(),
                    text_elements: Vec::new(),
                }],
                responsesapi_client_metadata: None,
                additional_context: None,
                expected_turn_id: turn.id.clone(),
            },
        })
        .await?;
    assert_eq!(steer.turn_id, turn.id);
    timeout(
        Duration::from_secs(5),
        wait_for_turn_completed(&mut mcp, &thread.id, &turn.id),
    )
    .await??;

    let request = final_response.single_request();
    let wait_output: serde_json::Value = serde_json::from_str(
        &request
            .function_call_output_text(WAIT_WORKFLOW_CALL_ID)
            .context("missing interrupted WaitWorkflow output")?,
    )?;
    assert_eq!(wait_output["status"], "running");
    assert_eq!(wait_output["timedOut"], false);
    assert_eq!(wait_output["interruptedByUserInput"], true);
    let run_id = wait_output["runId"]
        .as_str()
        .context("interrupted WaitWorkflow runId")?;
    let stop: WorkflowStopResponse = mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: thread.id,
                run_id: run_id.to_string(),
            },
        })
        .await?;
    assert!(stop.accepted);
    Ok(())
}
