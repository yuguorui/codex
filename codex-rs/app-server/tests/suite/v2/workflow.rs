use anyhow::Context;
use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentRetryResponse;
use codex_app_server_protocol::WorkflowAgentSkipResponse;
use codex_app_server_protocol::WorkflowAgentState;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowProgressItem;
use codex_app_server_protocol::WorkflowProgressNotification;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::read_jsonrpc_message;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

const WORKFLOW_CALL_ID: &str = "workflow-call-1";
const WORKFLOW_AGENT_PROMPT: &str = "Inspect Agent protocol compatibility";

#[derive(Clone, Copy)]
enum ParentAgentProtocol {
    V1,
    V2,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_launch_emits_ordered_notifications_and_is_listed() -> Result<()> {
    let script = r#"export const meta = {
  name: "app-server-test",
  title: "App server test",
  description: "Exercise the public workflow event stream",
  phases: [{ title: "Inspect" }],
};
phase("Inspect");
log("inspection complete");
// Keep the run alive past the parent turn so the completion notice is
// recorded while the owning thread is idle rather than mid-turn.
await new Promise((resolve) => setTimeout(resolve, 2000));
return { ok: true };
"#;
    let mut fixture = start_workflow(script, "Run an immediate workflow", "Workflow").await?;

    let events = collect_workflow_events(&mut fixture.mcp).await?;

    assert_eq!(
        events.methods.first().map(String::as_str),
        Some("workflow/started")
    );
    assert_eq!(
        events.methods.last().map(String::as_str),
        Some("workflow/completed")
    );
    assert!(
        events.methods[1..events.methods.len() - 1]
            .iter()
            .all(|method| method == "workflow/progress")
    );
    assert!(!events.progress.is_empty());
    assert_eq!(events.started.thread_id, fixture.thread_id);
    assert_eq!(events.started.turn_id, fixture.turn_id);
    assert_eq!(events.started.workflow_name, "app-server-test");
    assert_eq!(events.completed.run_id, events.started.run_id);
    assert_eq!(events.completed.status, WorkflowStatus::Completed);

    let list_id = fixture
        .mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: fixture.thread_id.clone(),
            cursor: None,
            limit: None,
        })
        .await?;
    let listed: WorkflowListResponse =
        timeout(DEFAULT_READ_TIMEOUT, fixture.mcp.read_response(list_id)).await??;
    assert_eq!(listed.data.len(), 1);
    assert_eq!(listed.data[0].run_id, events.started.run_id);
    assert_eq!(listed.data[0].status, WorkflowStatus::Completed);

    // The completion notice must reach the owning thread's model context: the
    // next user-triggered turn observes it as a user-role message.
    let notification_turn = responses::mount_sse_once_match(
        &fixture._server,
        move |request: &wiremock::Request| body_contains(request, "<workflow_notification>"),
        responses::sse(vec![
            responses::ev_response_created("workflow-notify-response"),
            responses::ev_assistant_message(
                "workflow-notify-message",
                "Workflow notification received",
            ),
            responses::ev_completed("workflow-notify-response"),
        ]),
    )
    .await;
    // Give the completion notice time to be recorded before starting the
    // follow-up turn.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let TurnStartResponse { turn: follow_up } = fixture
        .mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: fixture.thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: "Continue".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut fixture.mcp, &fixture.thread_id, &follow_up.id).await?;
    let notified_requests = notification_turn.requests();
    assert!(
        !notified_requests.is_empty(),
        "workflow completion notification was not delivered to the owning thread"
    );
    assert!(
        notified_requests[0]
            .body_json()
            .to_string()
            .contains(&events.completed.run_id)
    );

    let stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id,
                run_id: events.started.run_id,
            },
        })
        .await?;
    assert!(!stop.accepted);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_stop_accepts_only_an_active_run() -> Result<()> {
    let script = r#"export const meta = {
  name: "pending-test",
  description: "Remain active until stopped",
};
return new Promise(() => {});
"#;
    let mut fixture = start_workflow(script, "Run a pending workflow", "Workflow").await?;
    let started: WorkflowStartedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        fixture.mcp.read_notification("workflow/started"),
    )
    .await??;

    let skip: WorkflowAgentSkipResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowAgentSkip {
            request_id,
            params: WorkflowAgentControlParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
                agent_index: 0,
            },
        })
        .await?;
    let retry: WorkflowAgentRetryResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowAgentRetry {
            request_id,
            params: WorkflowAgentControlParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
                agent_index: 0,
            },
        })
        .await?;
    assert!(!skip.accepted);
    assert!(!retry.accepted);

    let first_stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
            },
        })
        .await?;
    assert!(first_stop.accepted);

    let completed: WorkflowCompletedNotification = timeout(DEFAULT_READ_TIMEOUT, async {
        let mut completed = None;
        let mut turn_completed = false;
        loop {
            let JSONRPCMessage::Notification(notification) =
                fixture.mcp.read_next_message().await?
            else {
                continue;
            };
            let Some(params) = notification.params else {
                continue;
            };
            match notification.method.as_str() {
                "workflow/completed" => {
                    let candidate: WorkflowCompletedNotification = serde_json::from_value(params)?;
                    if candidate.run_id == started.run_id {
                        completed = Some(candidate);
                    }
                }
                "turn/completed" => {
                    let candidate: TurnCompletedNotification = serde_json::from_value(params)?;
                    turn_completed = candidate.thread_id == fixture.thread_id
                        && candidate.turn.id == fixture.turn_id;
                }
                _ => {}
            }
            if turn_completed && completed.is_some() {
                return completed.context("missing workflow/completed notification");
            }
        }
    })
    .await??;
    assert_eq!(completed.run_id, started.run_id);
    assert_eq!(completed.status, WorkflowStatus::Killed);

    let second_stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id,
                run_id: started.run_id,
            },
        })
        .await?;
    assert!(!second_stop.accepted);
    Ok(())
}

#[tokio::test]
async fn workflow_list_requires_experimental_api_capability() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    let initialized = mcp
        .initialize_with_capabilities(
            ClientInfo {
                name: DEFAULT_CLIENT_NAME.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: false,
                ..Default::default()
            }),
        )
        .await?;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));

    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cursor: None,
            limit: None,
        })
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "workflow/list requires experimentalApi capability"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_run_workflow_alias_remains_dispatchable() -> Result<()> {
    let script = r#"export const meta = {
  name: "alias-test",
  description: "Exercise the hidden compatibility alias",
};
return "done";
"#;
    let mut fixture = start_workflow(script, "Run the compatibility alias", "RunWorkflow").await?;

    let events = collect_workflow_events(&mut fixture.mcp).await?;

    assert_eq!(events.started.workflow_name, "alias-test");
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_runs_with_parent_agent_v1() -> Result<()> {
    run_workflow_agent_protocol_compatibility(ParentAgentProtocol::V1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_runs_with_parent_agent_v2() -> Result<()> {
    run_workflow_agent_protocol_compatibility(ParentAgentProtocol::V2).await
}

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
        |request: &wiremock::Request| body_contains(request, "You are a workflow subagent."),
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
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id,
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
    assert!(!child_requests.is_empty());
    assert!(
        child_requests
            .iter()
            .all(|request| request.body_json().pointer("/text/format").is_none())
    );
    let child_prompt = child_requests
        .iter()
        .flat_map(|request| request.message_input_texts("user"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(child_prompt.contains("Return only a JSON value matching this schema"));
    assert!(child_prompt.contains(r#""answer":{"type":"string"}"#));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agents_enforce_the_shared_rollout_budget() -> Result<()> {
    let script = r#"export const meta = {
  name: "budget-enforcement",
  description: "Verify workflow agents consume the shared rollout budget",
};
const first = await parallel([() => agent("Consume the available workflow budget")]);
const second = await parallel([
  () => agent("Consume workflow budget A"),
  () => agent("Consume workflow budget B"),
]);
const blocked = await parallel([() => agent("This agent must not be launched")]);
return [first[0], second[0], second[1], blocked[0]];
"#;
    let parent_prompt = "Run a budget-limited workflow";
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
            responses::ev_completed("workflow-budget-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume the available workflow budget")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-first"),
            responses::ev_assistant_message("workflow-budget-child-first-message", "first"),
            responses::ev_completed_with_tokens("workflow-budget-child-first", 10),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume workflow budget A")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-a"),
            responses::ev_assistant_message("workflow-budget-child-a-message", "A"),
            responses::ev_completed_with_tokens("workflow-budget-child-a", 10),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, "Consume workflow budget B")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-budget-child-b"),
            responses::ev_assistant_message("workflow-budget-child-b-message", "B"),
            responses::ev_completed_with_tokens("workflow-budget-child-b", 10),
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
            responses::ev_completed("workflow-budget-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
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
    let _: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id,
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
    assert_eq!(events.completed.usage.agent_count, 3);
    assert!(events.completed.failures.is_empty());
    let requests = server.received_requests().await.unwrap_or_default();
    let child_requests = requests
        .iter()
        .filter(|request| body_contains(request, "You are a workflow subagent."))
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 3);
    for prompt in [
        "Consume the available workflow budget",
        "Consume workflow budget A",
        "Consume workflow budget B",
    ] {
        assert_eq!(
            child_requests
                .iter()
                .filter(|request| body_contains(request, prompt))
                .count(),
            1,
            "expected one workflow subagent request for {prompt}"
        );
    }
    assert!(
        child_requests
            .iter()
            .all(|request| !body_contains(request, "This agent must not be launched"))
    );
    for label in ["agent-1", "agent-2", "agent-3"] {
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
    Ok(())
}

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

struct WorkflowFixture {
    mcp: TestAppServer,
    _server: wiremock::MockServer,
    _codex_home: TempDir,
    thread_id: String,
    turn_id: String,
}

async fn start_workflow(script: &str, prompt: &str, tool_name: &str) -> Result<WorkflowFixture> {
    let server = responses::start_mock_server().await;
    responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("workflow-parent-1"),
                responses::ev_function_call(
                    WORKFLOW_CALL_ID,
                    tool_name,
                    &json!({ "script": script }).to_string(),
                ),
                responses::ev_completed("workflow-parent-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("workflow-parent-2"),
                responses::ev_assistant_message("workflow-message-1", "Workflow launched"),
                responses::ev_completed("workflow-parent-2"),
            ]),
        ],
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
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    Ok(WorkflowFixture {
        mcp,
        _server: server,
        _codex_home: codex_home,
        thread_id: thread.id,
        turn_id: turn.id,
    })
}

async fn run_workflow_agent_protocol_compatibility(protocol: ParentAgentProtocol) -> Result<()> {
    let script = format!(
        r#"export const meta = {{
  name: "agent-protocol-compatibility",
  description: "Verify host-orchestrated agents across parent protocols",
  phases: [{{ title: "Inspect" }}],
}};
phase("Inspect");
return await agent("{WORKFLOW_AGENT_PROMPT}", {{ label: "compatibility-agent" }});
"#
    );
    let parent_prompt = match protocol {
        ParentAgentProtocol::V1 => "Run a workflow while the parent uses Agent v1",
        ParentAgentProtocol::V2 => "Run a workflow while the parent uses Agent v2",
    };
    let expected_parent_namespace = match protocol {
        ParentAgentProtocol::V1 => "multi_agent_v1",
        ParentAgentProtocol::V2 => "collaboration",
    };
    let unexpected_parent_namespace = match protocol {
        ParentAgentProtocol::V1 => "collaboration",
        ParentAgentProtocol::V2 => "multi_agent_v1",
    };

    let server = responses::start_mock_server().await;
    let parent_turn = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, parent_prompt)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-agent-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-agent-parent-1"),
        ]),
    )
    .await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "You are a workflow subagent."),
        responses::sse(vec![
            responses::ev_response_created("workflow-agent-child"),
            responses::ev_assistant_message("workflow-agent-child-message", "compatible"),
            responses::ev_completed_with_tokens("workflow-agent-child", 21),
        ]),
    )
    .await;
    let parent_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-agent-parent-2"),
            responses::ev_assistant_message("workflow-agent-parent-message", "Workflow launched"),
            responses::ev_completed("workflow-agent-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .enable_feature(Feature::Collab);
    let config = match protocol {
        ParentAgentProtocol::V1 => config.disable_feature(Feature::MultiAgentV2),
        ParentAgentProtocol::V2 => config.enable_feature(Feature::MultiAgentV2),
    };
    config.write(codex_home.path())?;
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
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
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
    assert_eq!(events.started.thread_id, thread.id);
    assert_eq!(events.started.turn_id, turn.id);
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.agent_count, 1);
    assert!(events.progress.iter().any(|notification| {
        notification.progress.iter().any(|item| {
            matches!(
                item,
                WorkflowProgressItem::WorkflowAgent(agent)
                    if agent.label == "compatibility-agent"
                        && agent.agent_id.is_some()
                        && agent.state == WorkflowAgentState::Done
            )
        })
    }));

    let parent_requests = parent_turn
        .requests()
        .into_iter()
        .filter(|request| {
            let body = request.body_json().to_string();
            body.contains(parent_prompt) && !body.contains("You are a workflow subagent.")
        })
        .collect::<Vec<_>>();
    assert!(!parent_requests.is_empty());
    let parent_tool_names = parent_requests
        .iter()
        .flat_map(|request| response_tool_names(&request.body_json()))
        .collect::<Vec<_>>();
    assert!(
        parent_tool_names
            .iter()
            .any(|name| name == expected_parent_namespace)
    );
    assert!(
        !parent_tool_names
            .iter()
            .any(|name| name == unexpected_parent_namespace)
    );
    assert!(
        parent_tool_names.iter().any(|name| name == "Workflow"),
        "Workflow was not visible to the parent using {expected_parent_namespace}: {parent_tool_names:?}"
    );
    assert!(
        !parent_tool_names.iter().any(|name| name == "RunWorkflow"),
        "hidden Workflow alias leaked into the model-visible tool list"
    );

    let child_requests = child_turn
        .requests()
        .into_iter()
        .filter(|request| {
            request
                .message_input_texts("user")
                .iter()
                .any(|message| message.contains("You are a workflow subagent."))
        })
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 1);
    assert!(
        child_requests
            .iter()
            .flat_map(|request| request.message_input_texts("user"))
            .any(|message| message.contains(WORKFLOW_AGENT_PROMPT))
    );
    let child_tool_names = child_requests
        .iter()
        .flat_map(|request| response_tool_names(&request.body_json()))
        .collect::<Vec<_>>();
    assert!(
        !child_tool_names.iter().any(|name| name == "multi_agent_v1"),
        "workflow child exposed Agent v1 tools: {child_tool_names:?}"
    );
    assert!(
        !child_tool_names.iter().any(|name| name == "collaboration"),
        "workflow child exposed Agent v2 tools: {child_tool_names:?}"
    );
    assert!(
        !child_tool_names.iter().any(|name| name == "Workflow"),
        "workflow child exposed nested Workflow: {child_tool_names:?}"
    );
    assert!(parent_follow_up.requests().iter().any(|request| {
        let body = request.body_json().to_string();
        body.contains(WORKFLOW_CALL_ID) && !body.contains("You are a workflow subagent.")
    }));
    Ok(())
}

struct WorkflowEvents {
    methods: Vec<String>,
    started: WorkflowStartedNotification,
    progress: Vec<WorkflowProgressNotification>,
    completed: WorkflowCompletedNotification,
}

#[derive(Default)]
struct InteractiveWorkflowAgentResponder {
    attempts: AtomicUsize,
}

impl Respond for InteractiveWorkflowAgentResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return responses::sse_response(responses::sse(vec![
                responses::ev_response_created("workflow-interactive-child-1"),
                responses::ev_assistant_message(
                    "workflow-interactive-child-message-1",
                    "Initial transcript analysis",
                ),
                responses::ev_completed("workflow-interactive-child-1"),
            ]))
            .set_delay(Duration::from_millis(750));
        }
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-interactive-child-2"),
            responses::ev_assistant_message(
                "workflow-interactive-child-message-2",
                "Transcript interoperability confirmed",
            ),
            responses::ev_completed_with_tokens("workflow-interactive-child-2", 34),
        ]))
    }
}

async fn wait_for_started_workflow_agent(
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

async fn wait_for_workflow_completion(
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

async fn wait_for_turn_completed(
    mcp: &mut TestAppServer,
    thread_id: &str,
    expected_turn_id: &str,
) -> Result<()> {
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
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    Ok(())
}

async fn collect_workflow_events(mcp: &mut TestAppServer) -> Result<WorkflowEvents> {
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

fn response_tool_names(body: &serde_json::Value) -> Vec<String> {
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

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(request.body.clone())
        .ok()
        .is_some_and(|body| body.contains(text))
}

async fn initialize_websocket(
    client: &mut super::connection_handling_websocket::WsClient,
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

async fn assert_no_workflow_notification(
    client: &mut super::connection_handling_websocket::WsClient,
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
