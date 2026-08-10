use super::*;
use codex_app_server_protocol::RawResponseCompletedNotification;

pub(super) async fn wait_for_captured_workflow_run_id(
    server: &wiremock::MockServer,
    launch_call_id: &str,
    child_prompt: &str,
) -> Result<String> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let requests = server
                .received_requests()
                .await
                .context("failed to read workflow launch requests")?;
            let child_started = requests.iter().any(|request| {
                body_contains(request, "You are a workflow subagent.")
                    && body_contains(request, child_prompt)
            });
            if child_started
                && let Some(run_id) = captured_tool_output(&requests, launch_call_id)
                    .and_then(|output| output["runId"].as_str().map(str::to_string))
            {
                return Ok::<_, anyhow::Error>(run_id);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

pub(super) async fn wait_for_listed_workflow_status(
    mcp: &mut TestAppServer,
    thread_id: &str,
    run_id: &str,
    expected_status: WorkflowStatus,
) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let listed: WorkflowListResponse = mcp
                .request(|request_id| ClientRequest::WorkflowList {
                    request_id,
                    params: WorkflowListParams {
                        thread_id: thread_id.to_string(),
                        cursor: None,
                        limit: None,
                    },
                })
                .await?;
            if listed
                .data
                .iter()
                .any(|workflow| workflow.run_id == run_id && workflow.status == expected_status)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

pub(super) async fn wait_for_started_workflow_agent(
    mcp: &mut TestAppServer,
    expected_label: &str,
) -> Result<(String, String)> {
    loop {
        let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
            continue;
        };
        if notification.method != "workflow/progress" {
            continue;
        }
        let Some(params) = notification.params else {
            continue;
        };
        let progress: WorkflowProgressNotification = serde_json::from_value(params)?;
        let agent_id = progress.progress.iter().find_map(|item| match item {
            WorkflowProgressItem::WorkflowAgent(agent)
                if agent.label == expected_label && agent.state == WorkflowAgentState::Start =>
            {
                agent.agent_id.clone()
            }
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowAgent(_)
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        });
        if let Some(agent_id) = agent_id {
            return Ok((progress.run_id, agent_id));
        }
    }
}

pub(super) async fn wait_for_workflow_completion(
    mcp: &mut TestAppServer,
    expected_run_id: &str,
) -> Result<WorkflowCompletedNotification> {
    loop {
        let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
            continue;
        };
        if notification.method != "workflow/completed" {
            continue;
        }
        let Some(params) = notification.params else {
            continue;
        };
        let completed: WorkflowCompletedNotification = serde_json::from_value(params)?;
        if completed.run_id == expected_run_id {
            return Ok(completed);
        }
    }
}

pub(super) async fn wait_for_turn_completed(
    mcp: &mut TestAppServer,
    thread_id: &str,
    expected_turn_id: &str,
) -> Result<()> {
    wait_for_turn_completed_notification(mcp, thread_id, expected_turn_id).await?;
    Ok(())
}

pub(super) async fn wait_for_turn_completed_notification(
    mcp: &mut TestAppServer,
    thread_id: &str,
    expected_turn_id: &str,
) -> Result<TurnCompletedNotification> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            if notification.method != "turn/completed" {
                continue;
            }
            let Some(params) = notification.params else {
                continue;
            };
            let completed: TurnCompletedNotification = serde_json::from_value(params)?;
            if completed.thread_id == thread_id && completed.turn.id == expected_turn_id {
                return Ok::<_, anyhow::Error>(completed);
            }
        }
    })
    .await?
}

pub(super) async fn wait_for_raw_response_completed(
    mcp: &mut TestAppServer,
    expected_response_id: &str,
) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed: RawResponseCompletedNotification =
                mcp.read_notification("rawResponse/completed").await?;
            if completed.response_id == expected_response_id {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    Ok(())
}

pub(super) async fn collect_workflow_events(mcp: &mut TestAppServer) -> Result<WorkflowEvents> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        let mut methods = Vec::new();
        let mut started = None;
        let mut progress = Vec::new();
        let mut completed = None;
        let mut turn_completed = false;
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            let Some(params) = notification.params else {
                continue;
            };
            match notification.method.as_str() {
                "workflow/started" => {
                    methods.push(notification.method);
                    started = Some(serde_json::from_value(params)?);
                }
                "workflow/progress" => {
                    methods.push(notification.method);
                    progress.push(serde_json::from_value(params)?);
                }
                "workflow/completed" => {
                    methods.push(notification.method);
                    completed = Some(serde_json::from_value(params)?);
                }
                "turn/completed" => {
                    let _: TurnCompletedNotification = serde_json::from_value(params)?;
                    turn_completed = true;
                }
                _ => {}
            }
            if turn_completed && completed.is_some() {
                return Ok(WorkflowEvents {
                    methods,
                    started: started.context("missing workflow/started notification")?,
                    progress,
                    completed: completed.context("missing workflow/completed notification")?,
                });
            }
        }
    })
    .await?
}

pub(super) fn response_tool_names(body: &serde_json::Value) -> Vec<String> {
    body.get("tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.get("name")
                .or_else(|| tool.get("type"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

pub(super) fn tool_output_from_body(body: &serde_json::Value, call_id: &str) -> serde_json::Value {
    body["input"]
        .as_array()
        .expect("request input array")
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| item["output"].as_str())
        .and_then(|output| serde_json::from_str(output).ok())
        .unwrap_or_else(|| panic!("missing JSON function output for {call_id}"))
}

pub(super) fn captured_tool_output(
    requests: &[wiremock::Request],
    call_id: &str,
) -> Option<serde_json::Value> {
    requests.iter().find_map(|request| {
        let body = request.body_json::<serde_json::Value>().ok()?;
        body["input"].as_array()?.iter().find_map(|item| {
            if item["type"] != "function_call_output" || item["call_id"] != call_id {
                return None;
            }
            serde_json::from_str(item["output"].as_str()?).ok()
        })
    })
}

pub(super) fn captured_tool_output_text(
    requests: &[wiremock::Request],
    call_id: &str,
) -> Option<String> {
    requests.iter().find_map(|request| {
        let body = request.body_json::<serde_json::Value>().ok()?;
        body["input"].as_array()?.iter().find_map(|item| {
            if item["type"] != "function_call_output" || item["call_id"] != call_id {
                return None;
            }
            item["output"].as_str().map(str::to_string)
        })
    })
}

pub(super) fn captured_tool_arguments(
    requests: &[wiremock::Request],
    call_id: &str,
) -> Option<serde_json::Value> {
    requests.iter().find_map(|request| {
        let body = request.body_json::<serde_json::Value>().ok()?;
        body["input"].as_array()?.iter().find_map(|item| {
            if item["type"] != "function_call" || item["call_id"] != call_id {
                return None;
            }
            serde_json::from_str(item["arguments"].as_str()?).ok()
        })
    })
}

pub(super) fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(request.body.clone())
        .ok()
        .is_some_and(|body| body.contains(text))
}

pub(super) fn is_subagent_request(request: &wiremock::Request) -> bool {
    let Ok(body) = request.body_json::<serde_json::Value>() else {
        return false;
    };
    body.pointer("/client_metadata/x-openai-subagent")
        .and_then(serde_json::Value::as_str)
        .is_some()
}

pub(super) async fn initialize_websocket(
    client: &mut super::super::connection_handling_websocket::WsClient,
    id: i64,
    name: &str,
) -> Result<()> {
    send_request(
        client,
        "initialize",
        id,
        Some(serde_json::to_value(InitializeParams {
            client_info: ClientInfo {
                name: name.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        })?),
    )
    .await?;
    read_response_for_id(client, id).await?;
    Ok(())
}

pub(super) async fn assert_no_workflow_notification(
    client: &mut super::super::connection_handling_websocket::WsClient,
    wait_for: Duration,
) -> Result<()> {
    match timeout(wait_for, async {
        loop {
            if let JSONRPCMessage::Notification(notification) = read_jsonrpc_message(client).await?
                && notification.method.starts_with("workflow/")
            {
                return Ok::<_, anyhow::Error>(notification.method);
            }
        }
    })
    .await
    {
        Ok(Ok(method)) => {
            anyhow::bail!("workflow notification leaked to another connection: {method}")
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(()),
    }
}
