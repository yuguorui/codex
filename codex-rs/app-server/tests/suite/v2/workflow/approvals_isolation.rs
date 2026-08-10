use super::*;
use codex_app_server_protocol::PermissionGrantScope;
use core_test_support::skip_if_no_remote_env;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Default)]
struct GuardianArtifactPagingResponder {
    page_reads: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Respond for GuardianArtifactPagingResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("Guardian request body");
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body.clone());
        let page = find_artifact_page(&body);
        let response_number = self.page_reads.load(Ordering::Relaxed);
        let events = match page {
            Some(page) if page["nextOffset"].is_null() => vec![
                responses::ev_response_created(&format!(
                    "workflow-strict-guardian-allow-{response_number}"
                )),
                responses::ev_assistant_message(
                    &format!("workflow-strict-guardian-message-{response_number}"),
                    &json!({
                        "risk_level": "low",
                        "user_authorization": "high",
                        "outcome": "allow",
                        "rationale": "The complete Workflow artifact is safe to run under strict review."
                    })
                    .to_string(),
                ),
                responses::ev_completed(&format!(
                    "workflow-strict-guardian-allow-{response_number}"
                )),
            ],
            page => {
                let sha256 = page
                    .as_ref()
                    .and_then(|page| page["sha256"].as_str().map(str::to_string))
                    .or_else(|| find_artifact_sha256(&body))
                    .expect("Guardian prompt should contain artifact SHA-256");
                let offset = page
                    .as_ref()
                    .and_then(|page| page["nextOffset"].as_u64())
                    .unwrap_or(0);
                self.page_reads.fetch_add(1, Ordering::Relaxed);
                let call_id = format!("workflow-strict-read-{offset}");
                vec![
                    responses::ev_response_created(&format!(
                        "workflow-strict-guardian-read-{offset}"
                    )),
                    responses::ev_function_call(
                        &call_id,
                        "read_guardian_approval_artifact",
                        &json!({"sha256": sha256, "offset": offset}).to_string(),
                    ),
                    responses::ev_completed(&format!(
                        "workflow-strict-guardian-read-{offset}"
                    )),
                ]
            }
        };
        responses::sse_response(responses::sse(events))
    }
}

fn find_artifact_sha256(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => text
            .find("\"sha256\":\"")
            .and_then(|start| text.get(start + 10..start + 74))
            .filter(|hash| hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_string),
        serde_json::Value::Array(values) => values.iter().find_map(find_artifact_sha256),
        serde_json::Value::Object(values) => values.values().find_map(find_artifact_sha256),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => None,
    }
}

fn find_artifact_page(value: &serde_json::Value) -> Option<serde_json::Value> {
    let mut pages = Vec::new();
    collect_artifact_pages(value, &mut pages);
    pages.pop()
}

fn collect_artifact_pages(value: &serde_json::Value, pages: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::String(text) => {
            if let Ok(page) = serde_json::from_str::<serde_json::Value>(text)
                && page.get("sha256").is_some()
                && page.get("offset").is_some()
                && page.get("contents").is_some()
                && page.get("nextOffset").is_some()
            {
                pages.push(page);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_artifact_pages(value, pages);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_artifact_pages(value, pages);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_user_rejection_preserves_other_reason_and_prevents_launch() -> Result<()> {
    const REJECTION_REASON: &str = "The requested repository is outside the approved scope.";
    let script = r#"export const meta = {
  name: "rejected-workflow",
  description: "Must not launch after user rejection",
};
return "must not run";
"#;
    assert_workflow_user_rejection(
        "Run a workflow that requires user approval",
        script,
        REJECTION_REASON,
    )
    .await
}

async fn assert_workflow_user_rejection(
    prompt: &str,
    script: &str,
    rejection_reason: &str,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let workflow_arguments = json!({ "script": script }).to_string();
    let prompt_matcher = prompt.to_string();
    responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, &prompt_matcher) && !body_contains(request, WORKFLOW_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-rejection-parent-1"),
            responses::ev_function_call(WORKFLOW_CALL_ID, "Workflow", &workflow_arguments),
            responses::ev_completed("workflow-rejection-parent-1"),
        ]),
    )
    .await;
    let rejection_follow_up = responses::mount_sse_once_match(
        &server,
        {
            let rejection_reason = rejection_reason.to_string();
            move |request: &wiremock::Request| body_contains(request, &rejection_reason)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-rejection-parent-2"),
            responses::ev_assistant_message(
                "workflow-rejection-parent-message",
                "Workflow rejected",
            ),
            responses::ev_completed("workflow-rejection-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
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

    let request = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::ToolRequestUserInput { request_id, params } = request else {
        anyhow::bail!("expected Workflow approval request, got {request:?}");
    };
    assert_eq!(params.thread_id, thread.id);
    assert_eq!(params.turn_id, turn.id);
    assert_eq!(params.item_id, WORKFLOW_CALL_ID);
    assert_eq!(params.questions.len(), 1);
    let question = &params.questions[0];
    assert_eq!(question.header, "Workflow");
    assert!(question.is_other);
    let artifact_reference = question
        .question
        .split_whitespace()
        .find(|value| value.starts_with("codex://workflow-approval/"))
        .context("Workflow approval artifact reference")?;
    let artifact_id = artifact_reference
        .rsplit('/')
        .next()
        .context("Workflow approval artifact id")?;
    let wrong_thread: Result<WorkflowApprovalArtifactReadResponse, _> = mcp
        .request(|request_id| ClientRequest::WorkflowApprovalArtifactRead {
            request_id,
            params: WorkflowApprovalArtifactReadParams {
                thread_id: "11111111-1111-4111-8111-111111111111".to_string(),
                artifact_id: artifact_id.to_string(),
                offset: None,
            },
        })
        .await;
    assert!(wrong_thread.is_err());
    let mut offset = None;
    let mut artifact_contents = String::new();
    loop {
        let artifact: WorkflowApprovalArtifactReadResponse = mcp
            .request(|request_id| ClientRequest::WorkflowApprovalArtifactRead {
                request_id,
                params: WorkflowApprovalArtifactReadParams {
                    thread_id: thread.id.clone(),
                    artifact_id: artifact_id.to_string(),
                    offset,
                },
            })
            .await?;
        assert_eq!(artifact.sha256, artifact_id);
        assert_eq!(artifact.offset, offset.unwrap_or(0));
        assert!(artifact.contents.len() <= 512);
        artifact_contents.push_str(&artifact.contents);
        let Some(next_offset) = artifact.next_offset else {
            break;
        };
        offset = Some(next_offset);
    }
    let action = serde_json::from_str::<serde_json::Value>(&artifact_contents)?;
    assert_eq!(action["tool"], "Workflow");
    let environment = &action["execution"]["environments"][0];
    assert_eq!(environment["environmentId"], "local");
    assert_eq!(environment["location"], "local");
    assert!(environment["cwd"].is_string());
    assert!(environment["workspaceRoots"].is_array());
    assert!(environment["environmentConfig"].is_object());
    assert!(environment["sandboxContext"].is_object());
    assert!(environment["executorId"].is_string());
    let permissions = &action["execution"]["effectivePermissions"];
    assert!(permissions["approvalPolicy"].is_string());
    assert!(permissions["approvalReviewMode"].is_string());
    assert!(permissions["permissionProfile"].is_object());
    assert_eq!(
        question
            .options
            .as_ref()
            .context("Workflow approval options")?
            .iter()
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Run workflow", "Cancel"]
    );
    mcp.send_response(
        request_id,
        json!({
            "answers": {
                (question.id.clone()): { "answers": [rejection_reason] }
            }
        }),
    )
    .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    let follow_up = rejection_follow_up.single_request();
    let tool_output = follow_up
        .function_call_output_text(WORKFLOW_CALL_ID)
        .context("missing rejected Workflow tool output")?;
    assert!(tool_output.contains("the user denied the dynamic workflow"));
    assert!(tool_output.contains(rejection_reason));

    let listed: WorkflowListResponse = mcp
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams {
                thread_id: thread.id,
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(listed.data.is_empty());
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
async fn strict_turn_auto_review_overrides_never_for_workflow() -> Result<()> {
    const PERMISSIONS_CALL_ID: &str = "workflow-strict-permissions";
    const PROMPT: &str = "Grant strict review and launch the workflow";
    let script = r#"export const meta = {
  name: "strict-never-review",
  description: "Verify strict review overrides approval policy never",
};
return "reviewed";
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT)
                && !body_contains(request, PERMISSIONS_CALL_ID)
                && !is_subagent_request(request)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-strict-parent-1"),
            responses::ev_function_call(
                PERMISSIONS_CALL_ID,
                "request_permissions",
                &json!({
                    "reason": "Keep later actions under strict automatic review",
                    "permissions": { "network": { "enabled": true } }
                })
                .to_string(),
            ),
            responses::ev_completed("workflow-strict-parent-1"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PERMISSIONS_CALL_ID)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !is_subagent_request(request)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-strict-parent-2"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-strict-parent-2"),
        ]),
    )
    .await;
    let guardian_response = GuardianArtifactPagingResponder::default();
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(is_subagent_request)
        .respond_with(guardian_response.clone())
        .mount(&server)
        .await;
    let parent_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID) && !is_subagent_request(request)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-strict-parent-3"),
            responses::ev_assistant_message("workflow-strict-parent-message", "Workflow reviewed"),
            responses::ev_completed("workflow-strict-parent-3"),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .with_root_config("approvals_reviewer = \"user\"")
        .enable_feature(Feature::Workflows)
        .enable_feature(Feature::RequestPermissionsTool)
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
                input: vec![UserInput::Text {
                    text: PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let request = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::PermissionsRequestApproval { request_id, params } = request else {
        anyhow::bail!("expected strict permission grant request, got {request:?}");
    };
    assert_eq!(params.item_id, PERMISSIONS_CALL_ID);
    let settings_request_id = mcp
        .send_thread_settings_update_request(
            codex_app_server_protocol::ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                approval_policy: Some(codex_app_server_protocol::AskForApproval::Never),
                ..Default::default()
            },
        )
        .await?;
    let _: codex_app_server_protocol::ThreadSettingsUpdateResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(settings_request_id)).await??;
    let updated: codex_app_server_protocol::ThreadSettingsUpdatedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("thread/settings/updated"),
    )
    .await??;
    assert_eq!(
        updated.thread_settings.approval_policy,
        codex_app_server_protocol::AskForApproval::Never
    );
    mcp.send_response(
        request_id,
        json!({
            "permissions": params.permissions,
            "scope": PermissionGrantScope::Turn,
            "strictAutoReview": true
        }),
    )
    .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    assert!(guardian_response.page_reads.load(Ordering::Relaxed) > 0);
    let guardian_requests = guardian_response
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(guardian_requests.len() > 1);
    assert!(guardian_requests.iter().any(|request| {
        find_artifact_page(request).is_some_and(|page| page["nextOffset"].is_null())
    }));
    assert!(
        !guardian_requests[0]
            .to_string()
            .contains("strict-never-review")
    );
    assert!(
        guardian_requests[0]
            .to_string()
            .contains("read_guardian_approval_artifact")
    );
    let parent_follow_up = parent_follow_up.single_request();
    assert!(
        parent_follow_up
            .function_call_output_text(WORKFLOW_CALL_ID)
            .is_some()
    );
    let listed: WorkflowListResponse = mcp
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams {
                thread_id: thread.id,
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(listed.data.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_workflow_rejects_mixed_inline_and_host_path_sources() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));
    const PROMPT: &str = "Reject mixed Workflow sources in the remote environment";
    let server = responses::start_mock_server().await;
    let final_response = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("workflow-remote-mixed-parent-1"),
                responses::ev_function_call(
                    WORKFLOW_CALL_ID,
                    "Workflow",
                    &json!({
                        "script": "export const meta = { name: 'inline', description: 'inline' }; return null;",
                        "scriptPath": "host-only.js"
                    })
                    .to_string(),
                ),
                responses::ev_completed("workflow-remote-mixed-parent-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("workflow-remote-mixed-parent-2"),
                responses::ev_assistant_message(
                    "workflow-remote-mixed-parent-message",
                    "Mixed sources rejected",
                ),
                responses::ev_completed("workflow-remote-mixed-parent-2"),
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
                input: vec![UserInput::Text {
                    text: PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    let output = final_response
        .requests()
        .into_iter()
        .find_map(|request| request.function_call_output_text(WORKFLOW_CALL_ID))
        .context("missing mixed-source Workflow output")?;
    assert!(output.contains("accepts only the `script` source"));
    let listed: WorkflowListResponse = mcp
        .request(|request_id| ClientRequest::WorkflowList {
            request_id,
            params: WorkflowListParams {
                thread_id: thread.id,
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(listed.data.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_agent_v1_child_cannot_use_owning_workflow_tools() -> Result<()> {
    run_ordinary_agent_workflow_tool_isolation(ParentAgentProtocol::V1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_agent_v2_child_cannot_use_owning_workflow_tools() -> Result<()> {
    run_ordinary_agent_workflow_tool_isolation(ParentAgentProtocol::V2).await
}
