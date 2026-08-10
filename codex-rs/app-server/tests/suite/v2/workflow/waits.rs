use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owning_model_waits_for_workflow_completion_in_the_same_turn() -> Result<()> {
    const PROMPT: &str = "Run a workflow and wait for its result";
    let script = r#"export const meta = {
  name: "wait-in-owning-turn",
  description: "Verify the owning model can await workflow completion",
};
await new Promise((resolve) => setTimeout(resolve, 500));
return { answer: 42 };
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, WORKFLOW_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-wait-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-wait-parent-1"),
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
        |request: &wiremock::Request| body_contains(request, WAIT_WORKFLOW_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("workflow-wait-parent-3"),
            responses::ev_assistant_message(
                "workflow-wait-parent-message",
                "Workflow result received",
            ),
            responses::ev_completed("workflow-wait-parent-3"),
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
    let thread_start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(thread_start_id)).await??;
    let TurnStartResponse { turn: _turn } = mcp
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

    let events = collect_workflow_events(&mut mcp).await?;

    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    let request = final_response.single_request();
    let output_text = request
        .function_call_output_text(WAIT_WORKFLOW_CALL_ID)
        .context("missing WaitWorkflow output")?;
    let output: serde_json::Value = serde_json::from_str(&output_text)?;
    assert_eq!(output["runId"], events.completed.run_id);
    assert_eq!(output["status"], "completed");
    assert_eq!(output["timedOut"], false);
    assert_eq!(output["result"], json!({ "answer": 42 }));
    assert_eq!(output["resultInline"], true);
    assert_eq!(output["resultTruncated"], false);
    assert!(output.get("outputFile").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owning_model_coordinates_multiple_workflows_without_cross_talk() -> Result<()> {
    const PROMPT: &str = "Coordinate two workflows and inspect their results independently";
    const FIRST_SCRIPT: &str = r#"export const meta = {
  name: "multi-workflow-first",
  description: "Complete with a result while another workflow remains active",
};
await new Promise((resolve) => setTimeout(resolve, 500));
return { run: "first" };
"#;
    const SECOND_SCRIPT: &str = r#"export const meta = {
  name: "multi-workflow-second",
  description: "Remain active until the owning model stops this run",
};
return new Promise(() => {});
"#;

    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, MULTI_WORKFLOW_FIRST_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("multi-workflow-parent-1"),
            responses::ev_function_call(
                MULTI_WORKFLOW_FIRST_CALL_ID,
                "Workflow",
                &json!({ "script": FIRST_SCRIPT }).to_string(),
            ),
            responses::ev_completed("multi-workflow-parent-1"),
        ]),
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_FIRST_CALL_ID,
        MULTI_WORKFLOW_SECOND_CALL_ID,
        MultiWorkflowModelStep::LaunchSecond {
            script: SECOND_SCRIPT.to_string(),
        },
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_SECOND_CALL_ID,
        MULTI_WORKFLOW_LIST_CALL_ID,
        MultiWorkflowModelStep::List,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_LIST_CALL_ID,
        MULTI_WORKFLOW_WAIT_ANY_CALL_ID,
        MultiWorkflowModelStep::WaitAny,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_WAIT_ANY_CALL_ID,
        MULTI_WORKFLOW_READ_FIRST_CALL_ID,
        MultiWorkflowModelStep::ReadFirst,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_READ_FIRST_CALL_ID,
        MULTI_WORKFLOW_STOP_SECOND_CALL_ID,
        MultiWorkflowModelStep::StopSecond,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_STOP_SECOND_CALL_ID,
        MULTI_WORKFLOW_WAIT_ALL_CALL_ID,
        MultiWorkflowModelStep::WaitAll,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_WAIT_ALL_CALL_ID,
        MULTI_WORKFLOW_WAIT_FIRST_CALL_ID,
        MultiWorkflowModelStep::WaitFirst,
    )
    .await;
    mount_multi_workflow_step(
        &server,
        MULTI_WORKFLOW_WAIT_FIRST_CALL_ID,
        MULTI_WORKFLOW_WAIT_FIRST_AGAIN_CALL_ID,
        MultiWorkflowModelStep::WaitFirstAgain,
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, MULTI_WORKFLOW_WAIT_FIRST_AGAIN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("multi-workflow-parent-final"),
            responses::ev_assistant_message(
                "multi-workflow-parent-message",
                "Both workflow runs were handled independently",
            ),
            responses::ev_completed("multi-workflow-parent-final"),
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
    let thread_start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
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
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    let requests = server
        .received_requests()
        .await
        .context("failed to read multi-workflow model requests")?;
    let first_launch = captured_tool_output(&requests, MULTI_WORKFLOW_FIRST_CALL_ID)
        .context("missing first Workflow output")?;
    let second_launch = captured_tool_output(&requests, MULTI_WORKFLOW_SECOND_CALL_ID)
        .context("missing second Workflow output")?;
    let first_run_id = first_launch["runId"]
        .as_str()
        .context("first Workflow runId")?;
    let second_run_id = second_launch["runId"]
        .as_str()
        .context("second Workflow runId")?;
    assert_ne!(first_run_id, second_run_id);

    let list = captured_tool_output(&requests, MULTI_WORKFLOW_LIST_CALL_ID)
        .context("missing model-visible ListWorkflows output")?;
    assert_eq!(list["totalMatched"], 2);
    let mut listed_run_ids = list["workflows"]
        .as_array()
        .context("ListWorkflows workflows")?
        .iter()
        .map(|workflow| workflow["runId"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    listed_run_ids.sort_unstable();
    let mut expected_run_ids = vec![first_run_id, second_run_id];
    expected_run_ids.sort_unstable();
    assert_eq!(listed_run_ids, expected_run_ids);

    let wait_any_args = captured_tool_arguments(&requests, MULTI_WORKFLOW_WAIT_ANY_CALL_ID)
        .context("missing WaitWorkflows(any) arguments")?;
    assert_eq!(
        wait_any_args["runIds"],
        json!([first_run_id, second_run_id])
    );
    assert_eq!(wait_any_args["mode"], "any");
    let wait_any = captured_tool_output(&requests, MULTI_WORKFLOW_WAIT_ANY_CALL_ID)
        .context("missing WaitWorkflows(any) output")?;
    assert_eq!(wait_any["conditionMet"], true);
    assert_eq!(wait_any["timedOut"], false);
    let waited_any = wait_any["workflows"]
        .as_array()
        .context("WaitWorkflows(any) workflows")?
        .iter()
        .map(|workflow| {
            (
                workflow["runId"].as_str().unwrap_or_default().to_string(),
                workflow["status"].as_str().unwrap_or_default().to_string(),
                workflow["timedOut"].as_bool().unwrap_or(true),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(waited_any.len(), 2);
    assert_eq!(
        waited_any[0],
        (first_run_id.to_string(), "completed".to_string(), false)
    );
    // The sibling the race stopped waiting for stays visible, still active and still
    // reported as not terminal for this wait.
    assert_eq!(waited_any[1].0, second_run_id);
    assert!(waited_any[1].2);
    assert!(
        matches!(waited_any[1].1.as_str(), "pending" | "running"),
        "the sibling should still be active, got {}",
        waited_any[1].1
    );
    // mode any names the satisfying run and inlines its result head, so the owning
    // model learns the outcome without spending another round trip.
    assert_eq!(wait_any["winner"]["runId"], first_run_id);
    assert_eq!(wait_any["winner"]["status"], "completed");
    assert_eq!(wait_any["winner"]["resultInline"], true);
    assert_eq!(wait_any["winner"]["result"], json!({ "run": "first" }));

    let read_first = captured_tool_output(&requests, MULTI_WORKFLOW_READ_FIRST_CALL_ID)
        .context("missing ReadWorkflowResult output")?;
    assert_eq!(read_first["runId"], first_run_id);
    assert_eq!(read_first["status"], "completed");
    assert_eq!(read_first["available"], true);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            read_first["chunk"]
                .as_str()
                .context("ReadWorkflowResult chunk")?
        )?,
        json!({ "run": "first" })
    );

    let stop_second = captured_tool_output(&requests, MULTI_WORKFLOW_STOP_SECOND_CALL_ID)
        .context("missing StopWorkflow output")?;
    assert_eq!(stop_second["runId"], second_run_id);
    assert_eq!(stop_second["action"], "stop");
    assert_eq!(stop_second["accepted"], true);

    let wait_all = captured_tool_output(&requests, MULTI_WORKFLOW_WAIT_ALL_CALL_ID)
        .context("missing WaitWorkflows(all) output")?;
    assert_eq!(wait_all["conditionMet"], true);
    assert_eq!(wait_all["timedOut"], false);
    assert_eq!(
        wait_all["workflows"]
            .as_array()
            .context("WaitWorkflows(all) workflows")?
            .iter()
            .map(|workflow| (
                workflow["runId"].as_str().unwrap_or_default().to_string(),
                workflow["status"].as_str().unwrap_or_default().to_string(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (first_run_id.to_string(), "completed".to_string()),
            (second_run_id.to_string(), "killed".to_string()),
        ]
    );
    // mode all has no single winner, so the inline head stays absent.
    assert_eq!(wait_all["winner"], json!(null));

    let first_wait = captured_tool_output(&requests, MULTI_WORKFLOW_WAIT_FIRST_CALL_ID)
        .context("missing first terminal WaitWorkflow output")?;
    let repeated_wait = captured_tool_output(&requests, MULTI_WORKFLOW_WAIT_FIRST_AGAIN_CALL_ID)
        .context("missing repeated terminal WaitWorkflow output")?;
    for wait_output in [&first_wait, &repeated_wait] {
        assert_eq!(wait_output["runId"], first_run_id);
        assert_eq!(wait_output["status"], "completed");
        assert_eq!(wait_output["timedOut"], false);
        assert_eq!(wait_output["result"], json!({ "run": "first" }));
    }

    let list_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: thread.id,
            cursor: None,
            limit: None,
        })
        .await?;
    let listed: WorkflowListResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(list_id)).await??;
    assert_eq!(listed.data.len(), 2);
    assert_eq!(
        listed
            .data
            .iter()
            .find(|workflow| workflow.run_id == first_run_id)
            .context("first workflow missing from workflow/list")?
            .status,
        WorkflowStatus::Completed
    );
    assert_eq!(
        listed
            .data
            .iter()
            .find(|workflow| workflow.run_id == second_run_id)
            .context("second workflow missing from workflow/list")?
            .status,
        WorkflowStatus::Killed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_all_returns_both_terminal_workflows_with_completion_notifications() -> Result<()> {
    const PROMPT: &str = "Wait for both controlled workflows to finish";
    const FIRST_SCRIPT: &str = r#"export const meta = {
  name: "notification-wait-all-first",
  description: "Remain active until externally completed first",
};
return new Promise(() => {});
"#;
    const SECOND_SCRIPT: &str = r#"export const meta = {
  name: "notification-wait-all-second",
  description: "Remain active until externally completed second",
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
            responses::ev_response_created("workflow-notification-wait-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": FIRST_SCRIPT }).to_string(),
            ),
            responses::ev_completed("workflow-notification-wait-parent-1"),
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
    let final_response = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, STEER_WAIT_WORKFLOWS_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("workflow-notification-wait-parent-final"),
            responses::ev_assistant_message(
                "workflow-notification-wait-parent-message",
                "Both controlled workflows finished",
            ),
            responses::ev_completed("workflow-notification-wait-parent-final"),
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
    // This only confirms that the model emitted WaitWorkflows. Focused extension and Core tests
    // cover handler-entry ordering.
    wait_for_raw_response_completed(&mut mcp, "workflow-steer-parent-3").await?;
    let (first_run_id, second_run_id) = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let requests = server
                .received_requests()
                .await
                .context("failed to read notification wait requests")?;
            let first_run_id = captured_tool_output(&requests, WORKFLOW_CALL_ID)
                .and_then(|output| output["runId"].as_str().map(str::to_string));
            let second_run_id = captured_tool_output(&requests, STEER_SECOND_WORKFLOW_CALL_ID)
                .and_then(|output| output["runId"].as_str().map(str::to_string));
            if let (Some(first_run_id), Some(second_run_id)) = (first_run_id, second_run_id) {
                return Ok::<_, anyhow::Error>((first_run_id, second_run_id));
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;

    for run_id in [&first_run_id, &second_run_id] {
        wait_for_listed_workflow_status(&mut mcp, &thread.id, run_id, WorkflowStatus::Running)
            .await?;
    }
    let _: WorkflowStopResponse = mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: thread.id.clone(),
                run_id: first_run_id.clone(),
            },
        })
        .await?;
    let first_completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_workflow_completion(&mut mcp, &first_run_id),
    )
    .await??;
    assert_eq!(first_completed.status, WorkflowStatus::Killed);
    wait_for_listed_workflow_status(
        &mut mcp,
        &thread.id,
        &second_run_id,
        WorkflowStatus::Running,
    )
    .await?;

    let _: WorkflowStopResponse = mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: thread.id.clone(),
                run_id: second_run_id.clone(),
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    let final_request = final_response.single_request();
    assert!(final_request.body_contains_text("<workflow_notification>"));
    let wait_output: serde_json::Value = serde_json::from_str(
        &final_request
            .function_call_output_text(STEER_WAIT_WORKFLOWS_CALL_ID)
            .context("missing WaitWorkflows(all) output")?,
    )?;
    assert_eq!(wait_output["conditionMet"], true);
    assert_eq!(wait_output["timedOut"], false);
    assert_eq!(wait_output["interruptedByUserInput"], false);
    assert_eq!(
        wait_output["workflows"]
            .as_array()
            .context("WaitWorkflows(all) workflows")?
            .iter()
            .map(|workflow| (
                workflow["runId"].as_str().unwrap_or_default(),
                workflow["status"].as_str().unwrap_or_default(),
                workflow["timedOut"].as_bool().unwrap_or(true),
            ))
            .collect::<Vec<_>>(),
        vec![
            (first_run_id.as_str(), "killed", false),
            (second_run_id.as_str(), "killed", false),
        ]
    );
    Ok(())
}
