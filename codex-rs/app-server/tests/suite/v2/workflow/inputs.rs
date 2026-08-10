use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_workflow_reads_declared_inputs_with_auto_env() -> Result<()> {
    const PARENT_PROMPT: &str = "Run an inline Workflow that reads its declared input";
    const INPUT_CONTENT: &str = "declared input from selected executor";
    let script = r#"export const meta = {
  name: "declared-input-e2e",
  description: "Read a frozen workspace input through the Workflow isolate",
  inputs: ["inputs/*.txt"],
};
const files = await listInputs();
const content = await readInput("inputs/input.txt");
if (files.length !== 1 || files[0].path !== "inputs/input.txt") {
  throw new Error("declared input manifest mismatch");
}
if (content !== "declared input from selected executor") {
  throw new Error("declared input content mismatch");
}
return { files, content };
"#;
    let server = responses::start_mock_server().await;
    responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("workflow-declared-input-parent-1"),
                responses::ev_function_call(
                    WORKFLOW_CALL_ID,
                    "Workflow",
                    &json!({ "script": script }).to_string(),
                ),
                responses::ev_completed("workflow-declared-input-parent-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("workflow-declared-input-parent-2"),
                responses::ev_assistant_message(
                    "workflow-declared-input-parent-message",
                    "Workflow launched",
                ),
                responses::ev_completed("workflow-declared-input-parent-2"),
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
    let environment = mcp.auto_env()?;
    let file_system = environment.environment().get_filesystem();
    let input_dir = environment.selection().cwd.join("inputs")?;
    file_system
        .create_directory(
            &input_dir,
            codex_exec_server::CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;
    file_system
        .write_file(
            &input_dir.join("input.txt")?,
            INPUT_CONTENT.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
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
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.error, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_agent_analyzes_multiple_large_artifacts_in_v8_with_auto_env() -> Result<()> {
    const PARENT_PROMPT: &str = "Run a Workflow that analyzes structured upstream reports";
    const UPSTREAM_PROMPT: &str = "Produce the structured upstream reports";
    const SECOND_UPSTREAM_PROMPT: &str = "Produce the structured upstream findings";
    const DOWNSTREAM_PROMPT: &str = "Aggregate the named workflow inputs";
    const ANALYZE_CALL_ID: &str = "workflow-inputs-analyze";
    const PRIVATE_NOTE: &str = "PRIVATE-UPSTREAM-界😀-VALUE";
    let script = r#"export const meta = {
  name: "structured-input-analysis",
  description: "Analyze complete upstream values without prompt concatenation",
};
const reports = await agent("Produce the structured upstream reports", {
  label: "produce",
  schema: {
    type: "object",
    properties: {
      items: {
        type: "array",
        items: {
          type: "object",
          properties: {
            area: { type: "string" },
            score: { type: "number" },
            note: { type: "string" },
          },
          required: ["area", "score", "note"],
          additionalProperties: false,
        },
      },
      padding: { type: "string" },
    },
    required: ["items", "padding"],
    additionalProperties: false,
  },
});
const findings = await agent("Produce the structured upstream findings", {
  label: "findings",
  schema: {
    type: "object",
    properties: {
      findings: { type: "array", items: { type: "string" } },
      padding: { type: "string" },
    },
    required: ["findings", "padding"],
    additionalProperties: false,
  },
});
return agent("Aggregate the named workflow inputs", {
  label: "aggregate",
  inputs: { reports, findings },
});
"#;
    let upstream_result = json!({
        "items": [
            {"area": "核心", "score": 7, "note": PRIVATE_NOTE},
            {"area": "TUI 😀", "score": 3, "note": "not selected"},
            {"area": "protocol", "score": 9, "note": "wire evidence"},
        ],
        "padding": "A".repeat(6 * 1024),
    });
    let second_upstream_result = json!({
        "findings": ["runtime", "protocol"],
        "padding": "B".repeat(6 * 1024),
    });
    let analysis_program = r#"
const selected = inputs.reports.items.filter(report => report.score >= 7);
console.log("selected", selected.length);
return {
  areas: selected.map(report => report.area),
  total: selected.reduce((sum, report) => sum + report.score, 0),
  findingCount: inputs.findings.findings.length,
  materializedBytes: inputs.reports.padding.length + inputs.findings.padding.length,
};
"#;

    let server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PARENT_PROMPT)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-inputs-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({"script": script}).to_string(),
            ),
            responses::ev_completed("workflow-inputs-parent-1"),
        ]),
    )
    .await;
    let upstream = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, UPSTREAM_PROMPT)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-inputs-upstream"),
            responses::ev_assistant_message(
                "workflow-inputs-upstream-message",
                &upstream_result.to_string(),
            ),
            responses::ev_completed("workflow-inputs-upstream"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, SECOND_UPSTREAM_PROMPT)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-inputs-upstream-second"),
            responses::ev_assistant_message(
                "workflow-inputs-upstream-second-message",
                &second_upstream_result.to_string(),
            ),
            responses::ev_completed("workflow-inputs-upstream-second"),
        ]),
    )
    .await;
    let downstream_initial = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, DOWNSTREAM_PROMPT)
                && !body_contains(request, ANALYZE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-inputs-downstream-1"),
            responses::ev_function_call(
                ANALYZE_CALL_ID,
                ANALYZE_WORKFLOW_INPUTS_TOOL_NAME,
                &json!({"program": analysis_program}).to_string(),
            ),
            responses::ev_completed("workflow-inputs-downstream-1"),
        ]),
    )
    .await;
    let downstream_followup = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, ANALYZE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-inputs-downstream-2"),
            responses::ev_assistant_message(
                "workflow-inputs-downstream-message",
                "analysis complete",
            ),
            responses::ev_completed("workflow-inputs-downstream-2"),
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
            responses::ev_response_created("workflow-inputs-parent-2"),
            responses::ev_assistant_message("workflow-inputs-parent-message", "Workflow launched"),
            responses::ev_completed("workflow-inputs-parent-2"),
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
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let events = collect_workflow_events(&mut mcp).await?;
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(events.completed.usage.agent_count, 3);

    let upstream_request = upstream
        .requests()
        .into_iter()
        .find(|request| {
            request
                .message_input_texts("user")
                .iter()
                .any(|text| text.contains(UPSTREAM_PROMPT))
        })
        .context("missing upstream Workflow child request")?
        .body_json();
    let upstream_tools = response_tool_names(&upstream_request);
    assert!(
        !upstream_tools
            .iter()
            .any(|name| name == ANALYZE_WORKFLOW_INPUTS_TOOL_NAME)
    );

    let initial_request = downstream_initial
        .requests()
        .into_iter()
        .find(|request| {
            request
                .message_input_texts("user")
                .iter()
                .any(|text| text.contains(DOWNSTREAM_PROMPT))
                && !request.body_json()["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == ANALYZE_CALL_ID
                    })
        })
        .context("missing initial downstream Workflow child request")?
        .body_json();
    let initial_request_text = initial_request.to_string();
    assert!(!initial_request_text.contains(PRIVATE_NOTE));
    assert!(!initial_request_text.contains("核心"));
    assert!(!initial_request_text.contains("TUI 😀"));
    assert!(!initial_request_text.contains(&"A".repeat(1024)));
    assert!(!initial_request_text.contains(&"B".repeat(1024)));
    let child_tools = response_tool_names(&initial_request);
    assert_eq!(
        child_tools
            .iter()
            .filter(|name| name.as_str() == ANALYZE_WORKFLOW_INPUTS_TOOL_NAME)
            .count(),
        1,
        "downstream tools: {child_tools:?}"
    );
    for tool_name in OWNING_WORKFLOW_TOOL_NAMES {
        assert!(!child_tools.iter().any(|name| name == tool_name));
    }
    assert!(!child_tools.iter().any(|name| name == "multi_agent_v1"));
    assert!(!child_tools.iter().any(|name| name == "collaboration"));

    let followup_request = downstream_followup
        .requests()
        .into_iter()
        .find(|request| {
            request.body_json()["input"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == ANALYZE_CALL_ID
                })
        })
        .context("missing downstream AnalyzeWorkflowInputs follow-up")?
        .body_json();
    assert_eq!(
        tool_output_from_body(&followup_request, ANALYZE_CALL_ID),
        json!({
            "result": {
                "areas": ["核心", "protocol"],
                "total": 16,
                "findingCount": 2,
                "materializedBytes": 12 * 1024,
            },
            "logs": ["selected 2"],
            "logsTruncated": false,
        })
    );
    Ok(())
}
