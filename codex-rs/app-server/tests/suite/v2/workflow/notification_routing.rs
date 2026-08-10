use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_notifications_only_reach_connections_subscribed_to_the_thread() -> Result<()> {
    let script = r#"export const meta = {
  name: "thread-scope-test",
  description: "Verify workflow notifications are thread scoped",
  phases: [{ title: "Check" }],
};
phase("Check");
return "done";
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("workflow-websocket-1"),
                responses::ev_function_call(
                    WORKFLOW_CALL_ID,
                    "Workflow",
                    &json!({ "script": script }).to_string(),
                ),
                responses::ev_completed("workflow-websocket-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("workflow-websocket-2"),
                responses::ev_assistant_message("workflow-websocket-message", "Launched"),
                responses::ev_completed("workflow-websocket-2"),
            ]),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .write(codex_home.path())?;
    let (mut process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;

    let result = async {
        let mut subscribed = connect_websocket(bind_addr).await?;
        let mut unrelated = connect_websocket(bind_addr).await?;
        initialize_websocket(&mut subscribed, /*id*/ 1, "workflow-subscribed").await?;
        initialize_websocket(&mut unrelated, /*id*/ 2, "workflow-unrelated").await?;

        send_request(
            &mut subscribed,
            "thread/start",
            /*id*/ 3,
            Some(serde_json::to_value(ThreadStartParams {
                cwd: Some(codex_home.path().display().to_string()),
                model: Some("mock-model".to_string()),
                ..Default::default()
            })?),
        )
        .await?;
        let ThreadStartResponse { thread, .. } =
            to_response(read_response_for_id(&mut subscribed, /*id*/ 3).await?)?;
        send_request(
            &mut subscribed,
            "turn/start",
            /*id*/ 4,
            Some(serde_json::to_value(TurnStartParams {
                thread_id: thread.id,
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: "Run the scoped workflow".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })?),
        )
        .await?;
        let _: TurnStartResponse =
            to_response(read_response_for_id(&mut subscribed, /*id*/ 4).await?)?;

        timeout(DEFAULT_READ_TIMEOUT, async {
            let mut saw_workflow_started = false;
            let mut saw_workflow_progress = false;
            let mut saw_workflow_completed = false;
            let mut saw_turn_completed = false;
            loop {
                let JSONRPCMessage::Notification(notification) =
                    read_jsonrpc_message(&mut subscribed).await?
                else {
                    continue;
                };
                match notification.method.as_str() {
                    "workflow/started" => saw_workflow_started = true,
                    "workflow/progress" => saw_workflow_progress = true,
                    "workflow/completed" => saw_workflow_completed = true,
                    "turn/completed" => saw_turn_completed = true,
                    _ => {}
                }
                if saw_workflow_started
                    && saw_workflow_progress
                    && saw_workflow_completed
                    && saw_turn_completed
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        assert_no_workflow_notification(&mut unrelated, Duration::from_millis(300)).await?;
        Ok(())
    }
    .await;

    process.kill().await.context("stop websocket app-server")?;
    result
}
