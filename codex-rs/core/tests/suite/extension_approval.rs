use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_core::config::Constrained;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolApprovalArtifact;
use codex_extension_api::ToolApprovalDenialSource;
use codex_extension_api::ToolApprovalOutcome;
use codex_extension_api::ToolApprovalRequest;
use codex_extension_api::ToolApprovalReviewRequest;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::AskForApproval;
use codex_tools::JsonSchema;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

const TOOL_NAME: &str = "workflow_approval_probe";
const CALL_ID: &str = "workflow-approval-call";

struct WorkflowApprovalProbe;

fn approval_action(large: bool) -> Value {
    let script = if large {
        "x".repeat(9_000)
    } else {
        "return agent(args.prompt)".to_string()
    };
    json!({
        "tool": "Workflow",
        "name": "review-codebase",
        "script": script,
        "arguments": {"prompt": "review the release"},
    })
}

impl ToolContributor for WorkflowApprovalProbe {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        vec![Arc::new(Self)]
    }
}

impl ToolExecutor<ToolCall> for WorkflowApprovalProbe {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Exercise structured Workflow approval routing.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "large".to_string(),
                    JsonSchema::boolean(Some(
                        "Whether to submit a large structured action.".to_string(),
                    )),
                )]),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, call: ToolCall) -> ToolExecutorFuture<'_> {
        Box::pin(async move {
            let arguments: Value = serde_json::from_str(call.function_arguments()?)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let large = arguments
                .get("large")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action = approval_action(large);
            let contents = serde_json::to_string(&action)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let outcome = call
                .turn_item_emitter
                .request_approval_detailed(ToolApprovalReviewRequest {
                    prompt: ToolApprovalRequest {
                        call_id: call.call_id.clone(),
                        id: format!("workflow_approval_{}", call.call_id),
                        header: "Workflow".to_string(),
                        question: "Review dynamic workflow before running".to_string(),
                        approve_label: "Run workflow".to_string(),
                        deny_label: "Cancel".to_string(),
                    },
                    action,
                    artifact: Some(ToolApprovalArtifact::from_contents(contents)),
                })
                .await;
            let output = match outcome {
                ToolApprovalOutcome::Approved => json!({"status": "approved"}),
                ToolApprovalOutcome::Denied { rejection, source } => json!({
                    "status": "denied",
                    "rejection": rejection,
                    "source": denial_source_name(source),
                }),
                ToolApprovalOutcome::TimedOut { rejection } => json!({
                    "status": "timedOut",
                    "rejection": rejection,
                }),
                ToolApprovalOutcome::Cancelled { reason } => json!({
                    "status": "cancelled",
                    "reason": reason,
                }),
                ToolApprovalOutcome::Unavailable => json!({"status": "unavailable"}),
                _ => json!({"status": "unknown"}),
            };
            Ok(Box::new(JsonToolOutput::new(output)) as Box<dyn ToolOutput>)
        })
    }
}

fn denial_source_name(source: ToolApprovalDenialSource) -> &'static str {
    match source {
        ToolApprovalDenialSource::User => "user",
        ToolApprovalDenialSource::AutomaticReviewer => "automaticReviewer",
        ToolApprovalDenialSource::Configuration => "configuration",
        ToolApprovalDenialSource::Unknown => "unknown",
        _ => "unknown",
    }
}

enum ReviewCase {
    Allow,
    AllowWithoutReadingArtifact,
    AllowAfterPartialArtifactRead,
    Deny(String),
    Large,
}

struct ProbeResult {
    requests: Vec<ResponsesRequest>,
    output: Value,
}

async fn run_probe(review: ReviewCase) -> Result<ProbeResult> {
    let server = start_mock_server().await;
    let large = matches!(
        &review,
        ReviewCase::Large | ReviewCase::AllowAfterPartialArtifactRead
    );
    let tool_arguments = serde_json::to_string(&json!({"large": large}))?;
    let artifact_contents = serde_json::to_string(&approval_action(large))?;
    let artifact_sha256 = ToolApprovalArtifact::from_contents(artifact_contents.clone())
        .sha256()
        .to_string();
    let mut response_sequence = vec![sse(vec![
        ev_response_created("resp-parent-tool"),
        ev_function_call(CALL_ID, TOOL_NAME, &tool_arguments),
        ev_completed("resp-parent-tool"),
    ])];
    if !matches!(&review, ReviewCase::AllowWithoutReadingArtifact) {
        let mut offset = 0;
        while offset < artifact_contents.len() {
            response_sequence.push(sse(vec![
                ev_response_created(&format!("resp-guardian-read-{offset}")),
                ev_function_call(
                    &format!("guardian-read-{offset}"),
                    "read_guardian_approval_artifact",
                    &json!({"sha256": artifact_sha256, "offset": offset}).to_string(),
                ),
                ev_completed(&format!("resp-guardian-read-{offset}")),
            ]));
            offset = offset.saturating_add(512).min(artifact_contents.len());
            if matches!(&review, ReviewCase::AllowAfterPartialArtifactRead) {
                break;
            }
        }
    }
    match &review {
        ReviewCase::Allow
        | ReviewCase::AllowWithoutReadingArtifact
        | ReviewCase::AllowAfterPartialArtifactRead
        | ReviewCase::Large => response_sequence.push(sse(vec![
            ev_response_created("resp-guardian-allow"),
            ev_assistant_message(
                "msg-guardian-allow",
                &json!({
                    "risk_level": "low",
                    "user_authorization": "high",
                    "outcome": "allow",
                    "rationale": "The structured Workflow action is safe.",
                })
                .to_string(),
            ),
            ev_completed("resp-guardian-allow"),
        ])),
        ReviewCase::Deny(rationale) => response_sequence.push(sse(vec![
            ev_response_created("resp-guardian-deny"),
            ev_assistant_message(
                "msg-guardian-deny",
                &json!({
                    "risk_level": "high",
                    "user_authorization": "low",
                    "outcome": "deny",
                    "rationale": rationale,
                })
                .to_string(),
            ),
            ev_completed("resp-guardian-deny"),
        ])),
    }
    response_sequence.push(sse(vec![
        ev_response_created("resp-parent-done"),
        ev_assistant_message("msg-parent-done", "done"),
        ev_completed("resp-parent-done"),
    ]));
    let responses = mount_sse_sequence(&server, response_sequence).await;

    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(WorkflowApprovalProbe));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_text_turn("run the Workflow approval probe")
        .await?;

    let requests = responses.requests();
    let output = requests
        .iter()
        .find_map(|request| request.function_call_output_text(CALL_ID))
        .context("parent model did not receive the extension approval outcome")?;
    Ok(ProbeResult {
        requests,
        output: serde_json::from_str(&output)?,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_workflow_allow_fails_closed_without_complete_artifact_read() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let result = run_probe(ReviewCase::AllowWithoutReadingArtifact).await?;

    assert_eq!(result.output["status"], "denied");
    assert_eq!(result.output["source"], "automaticReviewer");
    assert!(
        result.output["rejection"]
            .as_str()
            .context("fail-closed rejection reason")?
            .contains("did not read the complete bound approval artifact")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_workflow_allow_fails_closed_after_only_a_partial_artifact_read() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let result = run_probe(ReviewCase::AllowAfterPartialArtifactRead).await?;

    assert_eq!(result.output["status"], "denied");
    assert_eq!(result.output["source"], "automaticReviewer");
    assert!(
        result.output["rejection"]
            .as_str()
            .context("fail-closed rejection reason")?
            .contains("did not read the complete bound approval artifact")
    );
    assert_eq!(guardian_requests(&result.requests).len(), 2);
    Ok(())
}

fn guardian_requests(requests: &[ResponsesRequest]) -> Vec<&ResponsesRequest> {
    requests
        .iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["x-openai-subagent"].as_str() == Some("guardian")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_workflow_approval_routes_automatic_approve_through_guardian() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let result = run_probe(ReviewCase::Allow).await?;

    assert_eq!(result.output, json!({"status": "approved"}));
    let guardian_requests = guardian_requests(&result.requests);
    assert_eq!(guardian_requests.len(), 2);
    let guardian_request = guardian_requests[0];
    assert!(guardian_request.body_contains_text("codex-extension"));
    assert!(guardian_request.body_contains_text("workflow_approval_probe"));
    assert!(guardian_request.body_contains_text("read_guardian_approval_artifact"));
    assert!(!guardian_request.body_contains_text("return agent(args.prompt)"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_workflow_approval_propagates_bounded_automatic_denial_reason() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let rationale = format!("workflow denial marker: {}", "risk ".repeat(2_000));
    let result = run_probe(ReviewCase::Deny(rationale.clone())).await?;

    assert_eq!(guardian_requests(&result.requests).len(), 2);
    assert_eq!(result.output["status"], "denied");
    assert_eq!(result.output["source"], "automaticReviewer");
    let rejection = result.output["rejection"]
        .as_str()
        .context("denial outcome should include a rejection reason")?;
    assert!(rejection.contains("workflow denial marker"));
    assert!(rejection.len() < rationale.len());
    assert!(codex_utils_output_truncation::approx_token_count(rejection) <= 900);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_extension_workflow_action_is_pageable_by_guardian() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let result = run_probe(ReviewCase::Large).await?;

    assert_eq!(result.output, json!({"status": "approved"}));
    let guardian_requests = guardian_requests(&result.requests);
    assert!(guardian_requests.len() > 2);
    let mut reconstructed = String::new();
    for offset in (0..serde_json::to_string(&approval_action(true))?.len()).step_by(512) {
        let call_id = format!("guardian-read-{offset}");
        let output = guardian_requests
            .iter()
            .find_map(|request| request.function_call_output_text(&call_id))
            .with_context(|| format!("missing Guardian artifact page at {offset}"))?;
        assert!(codex_utils_output_truncation::approx_token_count(&output) < 1_000);
        let page: Value = serde_json::from_str(&output)?;
        reconstructed.push_str(
            page["contents"]
                .as_str()
                .context("artifact page contents")?,
        );
    }
    assert_eq!(
        reconstructed,
        serde_json::to_string(&approval_action(true))?
    );
    assert!(!guardian_requests[0].body_contains_text(&"x".repeat(1_024)));

    Ok(())
}
