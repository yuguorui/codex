use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_runs_with_parent_agent_v1() -> Result<()> {
    run_workflow_agent_protocol_compatibility(ParentAgentProtocol::V1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_runs_with_parent_agent_v2() -> Result<()> {
    run_workflow_agent_protocol_compatibility(ParentAgentProtocol::V2).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_model_precedence_reaches_actual_child_requests() -> Result<()> {
    const PARENT_PROMPT: &str = "Run workflow agents with every model precedence source";
    const DEFAULT_PROMPT: &str = "workflow model precedence default";
    const ROLE_PROMPT: &str = "workflow model precedence role";
    const EXPLICIT_PROMPT: &str = "workflow model precedence explicit";
    const MODEL_ONLY_PROMPT: &str = "workflow model precedence model only";

    let selectable_models = all_model_presets()
        .iter()
        .filter(|preset| preset.show_in_picker && preset.supported_in_api)
        .collect::<Vec<_>>();
    let default_model = *selectable_models
        .first()
        .context("model precedence test requires a default model")?;
    let role_model = *selectable_models
        .get(1)
        .context("model precedence test requires a role model")?;
    let explicit_model = *selectable_models
        .get(2)
        .context("model precedence test requires an explicit model")?;
    let model_only = *selectable_models
        .get(3)
        .context("model precedence test requires a model-only override")?;
    let explicit_effort = explicit_model
        .supported_reasoning_efforts
        .iter()
        .map(|preset| preset.effort.to_string())
        .find(|effort| matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max"))
        .context("explicit model has no Workflow-compatible reasoning effort")?;
    let default_effort = default_model.default_reasoning_effort.to_string();
    let role_effort = role_model.default_reasoning_effort.to_string();
    let model_only_default_effort = model_only.default_reasoning_effort.to_string();
    let script = format!(
        r#"export const meta = {{
  name: "model-precedence",
  description: "Exercise workflow child model precedence",
}};
const fromDefault = await agent("{DEFAULT_PROMPT}", {{ label: "default" }});
const fromRole = await agent("{ROLE_PROMPT}", {{
  label: "role",
  agentType: "workflow-precedence",
}});
const fromExplicit = await agent("{EXPLICIT_PROMPT}", {{
  label: "explicit",
  agentType: "workflow-precedence",
  model: "{}",
  effort: "{explicit_effort}",
}});
const fromModelOnly = await agent("{MODEL_ONLY_PROMPT}", {{
  label: "model-only",
  model: "{}",
}});
return [fromDefault, fromRole, fromExplicit, fromModelOnly];
"#,
        explicit_model.id, model_only.id,
    );

    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PARENT_PROMPT)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-model-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-model-parent-1"),
        ]),
    )
    .await;
    let default_child =
        mount_completed_workflow_agent(&server, DEFAULT_PROMPT, "workflow-model-default").await;
    let role_child =
        mount_completed_workflow_agent(&server, ROLE_PROMPT, "workflow-model-role").await;
    let explicit_child =
        mount_completed_workflow_agent(&server, EXPLICIT_PROMPT, "workflow-model-explicit").await;
    let model_only_child =
        mount_completed_workflow_agent(&server, MODEL_ONLY_PROMPT, "workflow-model-model-only")
            .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-model-parent-2"),
            responses::ev_assistant_message("workflow-model-parent-message", "launched"),
            responses::ev_completed("workflow-model-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Workflows)
        .with_extra_config(&format!(
            r#"
[agents]
default_subagent_model = "{}"
default_subagent_reasoning_effort = "{default_effort}"

[agents.workflow-precedence]
description = "Workflow model precedence role"
config_file = "./workflow-precedence-role.toml"
"#,
            default_model.id,
        ))
        .write(codex_home.path())?;
    std::fs::write(
        codex_home.path().join("workflow-precedence-role.toml"),
        format!(
            "model = \"{}\"\nmodel_reasoning_effort = \"{role_effort}\"\n",
            role_model.id,
        ),
    )?;
    write_models_cache(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some(default_model.id.clone()),
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
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.agent_count, 4);

    for (mock, prompt, expected_model, expected_effort) in [
        (
            default_child,
            DEFAULT_PROMPT,
            default_model.id.as_str(),
            default_effort.as_str(),
        ),
        (
            role_child,
            ROLE_PROMPT,
            role_model.id.as_str(),
            role_effort.as_str(),
        ),
        (
            explicit_child,
            EXPLICIT_PROMPT,
            explicit_model.id.as_str(),
            explicit_effort.as_str(),
        ),
        (
            model_only_child,
            MODEL_ONLY_PROMPT,
            model_only.id.as_str(),
            model_only_default_effort.as_str(),
        ),
    ] {
        let request = mock
            .requests()
            .into_iter()
            .find(|request| {
                request.body_contains_text(prompt)
                    && request.body_json()["client_metadata"]["x-openai-subagent"]
                        .as_str()
                        .is_some()
            })
            .with_context(|| format!("missing Workflow child request for {prompt}"))?
            .body_json();
        assert_eq!(
            (
                request["model"].as_str(),
                request["reasoning"]["effort"].as_str()
            ),
            (Some(expected_model), Some(expected_effort))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_settled_result_reaches_owning_model() -> Result<()> {
    const PROMPT: &str = "Run settled workflow agents and wait for their result";
    const SUCCESS_PROMPT: &str = "workflow settled success";
    const FAILURE_PROMPT: &str = "workflow settled terminal failure";
    let script = r#"export const meta = {
  name: "agent-settled-e2e",
  description: "Expose explicit workflow agent outcomes",
};
const success = await agentSettled("workflow settled success", { label: "success" });
const failure = await agentSettled("workflow settled terminal failure", { label: "failure" });
return { success, failure };
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, WORKFLOW_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-settled-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-settled-parent-1"),
        ]),
    )
    .await;
    mount_completed_workflow_agent(&server, SUCCESS_PROMPT, "workflow-settled-success").await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, FAILURE_PROMPT)
        },
        responses::sse_failed(
            "workflow-settled-failure",
            "invalid_request",
            "settled child failed",
        ),
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
            responses::ev_response_created("workflow-settled-parent-3"),
            responses::ev_assistant_message(
                "workflow-settled-parent-message",
                "settled outcomes received",
            ),
            responses::ev_completed("workflow-settled-parent-3"),
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
                    text: PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.agent_count, 2);

    let request = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if let Some(request) = final_response.requests().into_iter().find(|request| {
                request
                    .function_call_output_text(WAIT_WORKFLOW_CALL_ID)
                    .is_some()
            }) {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let output: serde_json::Value = serde_json::from_str(
        &request
            .function_call_output_text(WAIT_WORKFLOW_CALL_ID)
            .context("missing settled WaitWorkflow output")?,
    )?;
    assert_eq!(output["status"], "completed");
    assert_eq!(
        output["result"]["success"],
        json!({ "status": "fulfilled", "value": "completed" })
    );
    assert_eq!(output["result"]["failure"]["status"], "rejected");
    assert_eq!(output["result"]["failure"]["reason"]["kind"], "terminalApi");
    assert!(
        output["result"]["failure"]["reason"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("settled child failed"))
    );
    Ok(())
}
