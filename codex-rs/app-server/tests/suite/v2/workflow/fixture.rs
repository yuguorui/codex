use super::*;
use pretty_assertions::assert_eq;

const WORKFLOW_AGENT_TASK_INSTRUCTION: &str = "Complete the ordered Workflow task provided in the runtime context and return the requested result in your final response.";

pub(super) struct WorkflowFixture {
    pub(super) mcp: TestAppServer,
    pub(super) _server: wiremock::MockServer,
    pub(super) _codex_home: TempDir,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
}

pub(super) async fn start_workflow(
    script: &str,
    prompt: &str,
    tool_name: &str,
) -> Result<WorkflowFixture> {
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

pub(super) async fn mount_completed_workflow_agent(
    server: &wiremock::MockServer,
    prompt: &'static str,
    response_id: &'static str,
) -> core_test_support::responses::ResponseMock {
    responses::mount_sse_once_match(
        server,
        move |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, "You are a workflow subagent.")
                && body_contains(request, prompt)
        },
        responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_assistant_message(&format!("{response_id}-message"), "completed"),
            responses::ev_completed_with_tokens(response_id, 5),
        ]),
    )
    .await
}

pub(super) fn assert_workflow_child_context(
    request: &core_test_support::responses::ResponsesRequest,
    task: &str,
    expected_isolation: Option<&str>,
    expected_output_contract: Option<&[&str]>,
) {
    let developer_groups = request.message_input_text_groups("developer");
    let user_groups = request.message_input_text_groups("user");
    assert!(
        developer_groups
            .iter()
            .flatten()
            .all(|text| !text.starts_with("<rollout_budget>"))
    );
    let preamble_groups = developer_groups
        .iter()
        .filter(|group| {
            group
                .first()
                .is_some_and(|text| text.starts_with("<workflow_child_0_preamble>"))
        })
        .collect::<Vec<_>>();
    assert_eq!(preamble_groups.len(), 1);
    assert_eq!(preamble_groups[0].len(), 1);
    assert!(preamble_groups[0][0].contains("You are a workflow subagent."));

    let isolation_groups = developer_groups
        .iter()
        .filter(|group| {
            group
                .first()
                .is_some_and(|text| text.starts_with("<workflow_child_1_isolation_part_"))
        })
        .collect::<Vec<_>>();
    assert!(isolation_groups.iter().all(|group| group.len() == 1));
    match expected_isolation {
        Some(expected) => assert!(
            isolation_groups
                .iter()
                .flat_map(|group| group.iter())
                .any(|text| text.contains(expected)),
            "missing Workflow child isolation context: {developer_groups:?}"
        ),
        None => assert!(isolation_groups.is_empty()),
    }

    let output_contract_groups = developer_groups
        .iter()
        .filter(|group| {
            group
                .first()
                .is_some_and(|text| text.starts_with("<workflow_child_2_output_contract_part_"))
        })
        .collect::<Vec<_>>();
    assert!(output_contract_groups.iter().all(|group| group.len() == 1));
    match expected_output_contract {
        Some(expected_fragments) => {
            assert!(!output_contract_groups.is_empty());
            let rendered_contract = output_contract_groups
                .iter()
                .flat_map(|group| group.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            for expected in expected_fragments {
                assert!(
                    rendered_contract.contains(expected),
                    "missing {expected:?} in Workflow child output contract: {rendered_contract}"
                );
            }
        }
        None => assert!(output_contract_groups.is_empty()),
    }

    let task_groups = user_groups
        .iter()
        .filter(|group| {
            group
                .first()
                .is_some_and(|text| workflow_task_fragment_body(text).is_some())
        })
        .collect::<Vec<_>>();
    assert!(!task_groups.is_empty());
    assert!(task_groups.iter().all(|group| group.len() == 1));
    let reconstructed_task = task_groups
        .iter()
        .filter_map(|group| workflow_task_fragment_body(&group[0]))
        .collect::<String>();
    assert_eq!(reconstructed_task, task);
    assert_eq!(
        user_groups
            .iter()
            .flatten()
            .filter(|text| text.as_str() == WORKFLOW_AGENT_TASK_INSTRUCTION)
            .count(),
        1
    );
    assert!(
        developer_groups
            .iter()
            .flatten()
            .all(|text| !text.starts_with("<workflow_child_3_task_part_"))
    );
    assert!(request.message_input_texts("user").iter().all(|text| {
        !text.contains("You are a workflow subagent.")
            && expected_isolation.is_none_or(|expected| !text.contains(expected))
            && expected_output_contract.is_none_or(|expected_fragments| {
                expected_fragments
                    .iter()
                    .all(|expected| !text.contains(expected))
            })
    }));
}

fn workflow_task_fragment_body(text: &str) -> Option<&str> {
    const PREFIX: &str = "<workflow_child_3_task_part_";
    let rest = text.strip_prefix(PREFIX)?;
    let (part, body_and_close) = rest.split_once('>')?;
    let suffix = format!("</workflow_child_3_task_part_{part}>");
    body_and_close.strip_suffix(suffix.as_str())
}

#[test]
fn workflow_task_fixture_reconstructs_multifragment_utf8() {
    use codex_core::context::ContextualUserFragment;
    use codex_core::context::WorkflowChildTask;

    let task = format!("start:{}:end", "界".repeat(300));
    let rendered = WorkflowChildTask::parts(task.clone())
        .into_iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();
    let reconstructed = rendered
        .iter()
        .filter_map(|fragment| workflow_task_fragment_body(fragment))
        .collect::<String>();

    assert!(rendered.len() > 1);
    assert_eq!(reconstructed, task);
}

pub(super) async fn run_ordinary_agent_workflow_tool_isolation(
    protocol: ParentAgentProtocol,
) -> Result<()> {
    const SPAWN_CALL_ID: &str = "ordinary-agent-spawn";
    const CHILD_PROMPT: &str = "Inspect ordinary subagent workflow tool isolation";
    let (parent_prompt, namespace, spawn_arguments) = match protocol {
        ParentAgentProtocol::V1 => (
            "Spawn an ordinary Agent v1 child",
            "multi_agent_v1",
            json!({ "message": CHILD_PROMPT }),
        ),
        ParentAgentProtocol::V2 => (
            "Spawn an ordinary Agent v2 child",
            "collaboration",
            json!({ "message": CHILD_PROMPT, "task_name": "workflow_isolation" }),
        ),
    };

    let server = responses::start_mock_server().await;
    let parent_turn = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, parent_prompt) && !body_contains(request, SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("ordinary-agent-parent-1"),
            responses::ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                namespace,
                "spawn_agent",
                &spawn_arguments.to_string(),
            ),
            responses::ev_completed("ordinary-agent-parent-1"),
        ]),
    )
    .await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && is_subagent_request(request)
        },
        responses::sse(vec![
            responses::ev_response_created("ordinary-agent-child"),
            responses::ev_assistant_message("ordinary-agent-child-message", "isolated"),
            responses::ev_completed("ordinary-agent-child"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("ordinary-agent-parent-2"),
            responses::ev_assistant_message("ordinary-agent-parent-message", "spawned"),
            responses::ev_completed("ordinary-agent-parent-2"),
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
    wait_for_turn_completed(&mut mcp, &thread.id, &turn.id).await?;

    let parent_request = parent_turn.single_request();
    let parent_tool_names = response_tool_names(&parent_request.body_json());
    for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
        assert!(
            parent_tool_names.iter().any(|name| name == tool_name),
            "owning thread did not expose {tool_name}: {parent_tool_names:?}"
        );
    }
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let requests = child_turn.requests();
            if let Some(request) = requests.into_iter().find(|request| {
                request.body_contains_text(CHILD_PROMPT)
                    && request.body_json()["client_metadata"]["x-openai-subagent"]
                        .as_str()
                        .is_some()
            }) {
                return Ok::<_, anyhow::Error>(request);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
    .and_then(|child_request| {
        let child_body = child_request.body_json();
        let child_tool_names = response_tool_names(&child_body);
        for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
            if child_tool_names.iter().any(|name| name == tool_name) {
                anyhow::bail!(
                    "ordinary child using {namespace} exposed owning workflow tool {tool_name}: {child_tool_names:?}; client_metadata={:?}",
                    child_body.get("client_metadata")
                );
            }
        }
        if child_tool_names
            .iter()
            .any(|name| name == ANALYZE_WORKFLOW_INPUTS_TOOL_NAME)
        {
            anyhow::bail!(
                "ordinary child using {namespace} exposed {ANALYZE_WORKFLOW_INPUTS_TOOL_NAME}: {child_tool_names:?}"
            );
        }
        Ok(())
    })?;
    Ok(())
}

pub(super) async fn run_workflow_agent_protocol_compatibility(
    protocol: ParentAgentProtocol,
) -> Result<()> {
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
        |request: &wiremock::Request| {
            is_subagent_request(request) && body_contains(request, "You are a workflow subagent.")
        },
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
    for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
        assert!(
            parent_tool_names.iter().any(|name| name == tool_name),
            "{tool_name} was not visible to the parent using {expected_parent_namespace}: {parent_tool_names:?}"
        );
    }
    let child_request = child_turn.single_request();
    assert_workflow_child_context(&child_request, WORKFLOW_AGENT_PROMPT, None, None);
    let child_tool_names = response_tool_names(&child_request.body_json());
    assert!(
        !child_tool_names.iter().any(|name| name == "multi_agent_v1"),
        "workflow child exposed Agent v1 tools: {child_tool_names:?}"
    );
    assert!(
        !child_tool_names.iter().any(|name| name == "collaboration"),
        "workflow child exposed Agent v2 tools: {child_tool_names:?}"
    );
    for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
        assert!(
            !child_tool_names.iter().any(|name| name == tool_name),
            "workflow child exposed owning-thread tool {tool_name}: {child_tool_names:?}"
        );
    }
    assert!(
        !child_tool_names
            .iter()
            .any(|name| name == ANALYZE_WORKFLOW_INPUTS_TOOL_NAME),
        "workflow child without inputs exposed {ANALYZE_WORKFLOW_INPUTS_TOOL_NAME}: {child_tool_names:?}"
    );
    assert!(parent_follow_up.requests().iter().any(|request| {
        let body = request.body_json().to_string();
        body.contains(WORKFLOW_CALL_ID) && !body.contains("You are a workflow subagent.")
    }));
    Ok(())
}
