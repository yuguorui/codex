use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::approvals::NetworkPolicyAmendment;
use pretty_assertions::assert_eq;

#[test]
fn approval_resolution_rejects_denied_network_policy_amendment() {
    let resolution = ApprovalResolution {
        decision: ReviewDecision::NetworkPolicyAmendment {
            network_policy_amendment: NetworkPolicyAmendment {
                host: "denied.example.com".to_string(),
                action: NetworkPolicyRuleAction::Deny,
            },
        },
        source: ApprovalResolutionSource::User,
    };

    assert!(matches!(
        resolution.into_tool_result(&model_info_from_slug("acting-model")),
        Err(ToolError::Rejected(rejection)) if rejection == "rejected by user"
    ));
}

#[test]
fn approval_resolution_rejects_mcp_policy_amendment() {
    let resolution = ApprovalResolution {
        decision: ReviewDecision::ApprovedMcpPolicyAmendment,
        source: ApprovalResolutionSource::User,
    };

    assert!(matches!(
        resolution.into_tool_result(&model_info_from_slug("acting-model")),
        Err(ToolError::Rejected(rejection)) if rejection == "Error while requesting approval"
    ));
}

#[test]
fn approval_resolution_aborts_turn_when_approval_is_aborted() {
    let resolution = ApprovalResolution {
        decision: ReviewDecision::Abort,
        source: ApprovalResolutionSource::User,
    };

    assert!(matches!(
        resolution.into_tool_result(&model_info_from_slug("acting-model")),
        Err(ToolError::Codex(error))
            if matches!(
                error.details(),
                codex_protocol::error::CodexErrorDetails::TurnAborted
            )
    ));
}

#[test]
fn approval_resolution_uses_acting_model_timeout_instructions() {
    let mut model = model_info_from_slug("acting-model");
    for timeout_instructions in ["Catalog timeout instructions.", ""] {
        model.model_messages = Some(
            serde_json::from_value(serde_json::json!({
                "auto_review": {
                    "timeout_instructions": timeout_instructions,
                },
            }))
            .expect("model messages should deserialize"),
        );
        let resolution = ApprovalResolution {
            decision: ReviewDecision::TimedOut,
            source: ApprovalResolutionSource::Guardian,
        };

        assert!(matches!(
            resolution.into_tool_result(&model),
            Err(ToolError::Rejected(rejection)) if rejection == timeout_instructions
        ));
    }
}

#[test]
fn extension_resolution_preserves_denial_source_and_bounds_rejection() {
    let resolution = ApprovalResolution {
        decision: ReviewDecision::denied("denied detail ".repeat(10_000)),
        source: ApprovalResolutionSource::Guardian,
    };

    let ToolApprovalOutcome::Denied { rejection, source } =
        resolution.into_extension_outcome(&model_info_from_slug("acting-model"))
    else {
        panic!("expected detailed denial");
    };
    assert_eq!(source, ToolApprovalDenialSource::AutomaticReviewer);
    assert!(codex_utils_output_truncation::approx_token_count(&rejection) <= 900);
}

#[test]
fn extension_action_reuses_complete_structured_guardian_action() {
    let action = serde_json::json!({
        "tool": "workflow",
        "script": "return agent(args.prompt)",
        "arguments": { "prompt": "review the release" },
    });
    let guardian_request = ApprovalAction::ExtensionTool {
        id: "call-workflow".to_string(),
        tool_name: "Workflow".to_string(),
        hook_tool_name: HookToolName::new("Workflow"),
        prompt: extension_approval_prompt(),
        action: action.clone(),
        artifact: Some(codex_tools::ToolApprovalArtifact::from_contents(
            serde_json::to_string(&action).expect("serialize action"),
        )),
    }
    .into_guardian_request()
    .expect("bounded action should be reviewable");

    let crate::guardian::GuardianApprovalRequest::ExtensionTool {
        id,
        tool_name,
        artifact,
    } = guardian_request
    else {
        panic!("extension actions should use the existing structured Guardian request");
    };
    assert_eq!(id, "call-workflow");
    assert_eq!(tool_name, "Workflow");
    let expected = codex_tools::ToolApprovalArtifact::from_contents(
        serde_json::to_string(&action).expect("serialize action"),
    );
    assert_eq!(artifact.sha256(), expected.sha256());
    assert_eq!(artifact.byte_length(), expected.contents().len());
}

#[test]
fn large_extension_action_uses_content_addressed_guardian_artifact() {
    let action = serde_json::json!({ "script": "!".repeat(100_000) });
    let request = ApprovalAction::ExtensionTool {
        id: "call-workflow".to_string(),
        tool_name: "Workflow".to_string(),
        hook_tool_name: HookToolName::new("Workflow"),
        prompt: extension_approval_prompt(),
        action: action.clone(),
        artifact: Some(codex_tools::ToolApprovalArtifact::from_contents(
            serde_json::to_string(&action).expect("serialize action"),
        )),
    }
    .into_guardian_request()
    .expect("large artifact should remain automatically reviewable");

    assert!(matches!(
        request,
        crate::guardian::GuardianApprovalRequest::ExtensionTool { .. }
    ));
}

#[test]
fn extension_artifact_must_match_structured_action() {
    let action = (0..70).fold(serde_json::json!("leaf"), |value, _| {
        serde_json::Value::Array(vec![value])
    });
    let compact_bytes = serde_json::to_string(&action)
        .expect("nested action should serialize")
        .len();
    assert!(compact_bytes < 8_000);

    let result = ApprovalAction::ExtensionTool {
        id: "call-workflow".to_string(),
        tool_name: "Workflow".to_string(),
        hook_tool_name: HookToolName::new("Workflow"),
        prompt: extension_approval_prompt(),
        action,
        artifact: Some(codex_tools::ToolApprovalArtifact::from_contents(
            "{}".to_string(),
        )),
    }
    .into_guardian_request();

    let error = result.expect_err("artifact contents must match the action");
    assert!(error.to_string().contains("does not match"));
}

#[test]
fn extension_artifact_hash_must_match_its_contents() {
    let action = serde_json::json!({"tool": "Workflow"});
    let result = ApprovalAction::ExtensionTool {
        id: "call-workflow".to_string(),
        tool_name: "Workflow".to_string(),
        hook_tool_name: HookToolName::new("Workflow"),
        prompt: extension_approval_prompt(),
        action: action.clone(),
        artifact: Some(codex_tools::ToolApprovalArtifact::new(
            "0".repeat(64),
            serde_json::to_string(&action).expect("serialize action"),
        )),
    }
    .into_guardian_request();

    let error = result.expect_err("artifact hash must bind its contents");
    assert!(error.to_string().contains("SHA-256"));
}

fn extension_approval_prompt() -> ToolApprovalRequest {
    ToolApprovalRequest {
        call_id: "call-workflow".to_string(),
        id: "workflow-approval".to_string(),
        header: "Workflow".to_string(),
        question: "Run this workflow?".to_string(),
        approve_label: "Run workflow".to_string(),
        deny_label: "Cancel".to_string(),
    }
}

#[test]
fn guardian_cwd_preserves_drive_shaped_local_posix_path() {
    let native_cwd = AbsolutePathBuf::try_from(std::path::PathBuf::from("/C:/workspace"))
        .expect("drive-shaped POSIX path should be absolute");
    let cwd = PathUri::from_abs_path(&native_cwd);

    assert_eq!(
        guardian_cwd(codex_exec_server::LOCAL_ENVIRONMENT_ID, cwd)
            .expect("local cwd should retain the host path convention"),
        native_cwd
    );
}

#[test]
fn guardian_cwd_rejects_foreign_remote_path() {
    let cwd = PathUri::parse("file:///C:/workspace").expect("valid Windows path URI");

    assert!(guardian_cwd(codex_exec_server::REMOTE_ENVIRONMENT_ID, cwd).is_err());
}
#[tokio::test]
async fn explicit_mcp_reviewer_override_takes_precedence_over_action_context() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let action = ApprovalAction::McpToolCall {
        id: "mcp-override".to_string(),
        server: "example".to_string(),
        tool_name: "dangerous".to_string(),
        arguments: None,
        connector_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        tool_title: None,
        tool_description: None,
        annotations: None,
        hook_tool_name: HookToolName::new("mcp__example__dangerous"),
        approval_policy: AskForApproval::OnRequest,
        reviewer: ApprovalsReviewer::User,
        approval_mode: AppToolApproval::Prompt,
        allow_session_remember: false,
        allow_persistent_approval: false,
    };
    let mut review_context = GuardianReviewContext::from(&turn);
    review_context.approval_policy = AskForApproval::OnRequest;
    review_context.approvals_reviewer = ApprovalsReviewer::AutoReview;
    let context = ApprovalContext {
        review_context,
        cancellation_token: None,
        call_id: "mcp-override".to_string(),
        tool_name: ToolName::plain("dangerous"),
        strict_auto_review: false,
        approval_reason: None,
        retry_reason: None,
        network_approval_context: None,
    };

    tokio::select! {
        resolution = session.request_reviewer_approval(action, &context) => {
            panic!("expected a user approval request, got {resolution:?}");
        }
        event = events.recv() => {
            let codex_protocol::protocol::EventMsg::ElicitationRequest(request) =
                event.expect("receive user approval request").msg
            else {
                panic!("expected an MCP user approval request");
            };
            assert_eq!(request.server_name, "example");
            assert_eq!(
                request.id,
                codex_protocol::mcp::RequestId::String(
                    "mcp_tool_call_approval_mcp-override".to_string()
                )
            );
        }
    }
}
