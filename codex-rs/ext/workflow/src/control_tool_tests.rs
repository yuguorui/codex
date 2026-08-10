use super::*;
use codex_protocol::workflow::WorkflowAgentProgress;
use codex_protocol::workflow::WorkflowIsolation;
use codex_protocol::workflow::WorkflowProgressItem;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;

#[test]
fn specs_use_distinct_required_inputs() {
    let cases = [
        (
            WorkflowControlToolKind::Stop,
            STOP_WORKFLOW_TOOL_NAME,
            vec!["runId".to_string()],
        ),
        (
            WorkflowControlToolKind::RetryAgent,
            RETRY_WORKFLOW_AGENT_TOOL_NAME,
            vec!["runId".to_string(), "agentIndex".to_string()],
        ),
        (
            WorkflowControlToolKind::SkipAgent,
            SKIP_WORKFLOW_AGENT_TOOL_NAME,
            vec!["runId".to_string(), "agentIndex".to_string()],
        ),
    ];

    for (kind, expected_name, expected_required) in cases {
        let ToolSpec::Function(spec) = workflow_control_tool_spec(kind) else {
            panic!("workflow control should be a function tool");
        };
        assert_eq!(spec.name, expected_name);
        assert_eq!(spec.parameters.required, Some(expected_required));
        assert_eq!(spec.parameters.additional_properties, Some(false.into()));
        if kind == WorkflowControlToolKind::RetryAgent {
            assert!(
                spec.parameters
                    .properties
                    .as_ref()
                    .is_some_and(|properties| properties.contains_key("dryRun"))
            );
        }
    }
}

#[test]
fn stop_args_reject_agent_index() {
    let error = serde_json::from_str::<StopWorkflowArgs>(r#"{"runId":"wf_test","agentIndex":2}"#)
        .unwrap_err();

    assert!(error.to_string().contains("unknown field `agentIndex`"));
}

#[test]
fn parse_errors_are_bounded_before_reaching_the_model() {
    let arguments = format!(r#"{{"{}":true}}"#, "unknown".repeat(2_000));

    let Err(FunctionCallError::RespondToModel(message)) =
        parse_arguments::<StopWorkflowArgs>(WorkflowControlToolKind::Stop, &arguments)
    else {
        panic!("oversized unknown field should produce a model-visible parse error");
    };

    assert!(message.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
    assert!(message.ends_with("...[truncated]"));
}

#[test]
fn output_serialization_is_bounded_and_reports_agent_control_state() {
    let snapshot = snapshot_with_agent();
    let agent = snapshot.progress.iter().find_map(|item| match item {
        WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref().clone()),
        WorkflowProgressItem::WorkflowPhase { .. } | WorkflowProgressItem::WorkflowLog { .. } => {
            None
        }
    });
    let output = WorkflowControlOutput::new(
        snapshot,
        WorkflowControlAction::RetryAgent,
        agent.clone(),
        false,
        Some(WorkflowControlRejectionReason::AgentNotControllable),
        vec![WorkflowAgentControlCandidate::new(
            agent.expect("agent progress"),
        )],
        Some(
            "Use agentIndex=2 with RetryWorkflowAgent; dryRun=true reports the blast radius first."
                .to_string(),
        ),
        None,
        false,
    );

    let value = serde_json::to_value(output).unwrap();
    assert_eq!(
        value,
        json!({
            "runId": "wf_test",
            "action": "retryAgent",
            "accepted": false,
            "status": "running",
            "summary": bounded_output_text(&"summary ".repeat(2_000)),
            "reason": "agentNotControllable",
            "agent": {
                "index": 2,
                "state": "error",
                "awaitingDecision": true,
                "skipped": false,
                "attempt": 3,
                "error": bounded_output_text(&"error ".repeat(2_000)),
            },
            "candidates": [{
                "index": 2,
                "label": "agent",
                "state": "error",
                "awaitingDecision": true,
                "skipped": false,
                "attempt": 3,
                "error": bounded_output_text(&"error ".repeat(2_000)),
                "canRetry": true,
                "canSkip": true,
            }],
            "nextAction": "Use agentIndex=2 with RetryWorkflowAgent; dryRun=true reports the blast radius first.",
            "retryImpact": null
        })
    );
    assert!(value["summary"].as_str().unwrap().len() < "summary ".repeat(2_000).len());
    assert!(value["agent"]["error"].as_str().unwrap().len() < "error ".repeat(2_000).len());
    let ToolSpec::Function(spec) = workflow_control_tool_spec(WorkflowControlToolKind::RetryAgent)
    else {
        panic!("workflow control should be a function tool");
    };
    let schema = spec.output_schema.expect("control output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&value));
}

#[test]
fn control_candidate_output_stays_structured_in_the_worst_escape_case() {
    let candidates = (0..4)
        .map(|index| WorkflowAgentControlCandidate {
            index,
            label: "\0".repeat(CONTROL_OUTPUT_TEXT_MAX_BYTES),
            state: WorkflowAgentState::Error,
            awaiting_decision: false,
            skipped: false,
            attempt: index as u32,
            error: Some("\0".repeat(CONTROL_OUTPUT_TEXT_MAX_BYTES)),
            can_retry: true,
            can_skip: false,
        })
        .collect::<Vec<_>>();
    let output = WorkflowControlOutput::new(
        snapshot_with_agent(),
        WorkflowControlAction::RetryAgent,
        None,
        false,
        Some(WorkflowControlRejectionReason::UnknownAgent),
        candidates,
        None,
        None,
        false,
    );

    let value = model_bounded_json_value(RETRY_WORKFLOW_AGENT_TOOL_NAME, &output).unwrap();
    let ToolSpec::Function(spec) = workflow_control_tool_spec(WorkflowControlToolKind::RetryAgent)
    else {
        panic!("workflow control should be a function tool");
    };
    let schema = spec.output_schema.expect("control output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&value));

    assert!(
        serde_json::to_vec(&value).unwrap().len()
            <= crate::workflow_result_tool::MODEL_TOOL_OUTPUT_MAX_BYTES
    );
    assert_eq!(value["candidates"].as_array().unwrap().len(), 4);
}

#[test]
fn retry_output_reports_blast_radius() {
    let output = WorkflowControlOutput::new(
        snapshot_with_agent(),
        WorkflowControlAction::RetryAgent,
        None,
        true,
        None,
        Vec::new(),
        None,
        Some(WorkflowRetryImpact {
            scope: WorkflowRetryScope::SettledPrefix,
            target_agent_index: 2,
            known_rerun_agent_count: 4,
            known_replay_agent_count: 2,
        }),
        true,
    );

    assert_eq!(
        serde_json::to_value(output).unwrap()["retryImpact"],
        json!({
            "dryRun": true,
            "scope": "settledPrefix",
            "targetAgentIndex": 2,
            "knownRerunAgentCount": 4,
            "knownReplayAgentCount": 2,
        })
    );
}

#[test]
fn stop_output_has_no_agent_status() {
    let output = WorkflowControlOutput::new(
        snapshot_with_agent(),
        WorkflowControlAction::Stop,
        None,
        true,
        None,
        Vec::new(),
        None,
        None,
        false,
    );

    assert_eq!(
        serde_json::to_value(output).unwrap()["agent"],
        JsonValue::Null
    );
}

#[test]
fn rejection_reasons_distinguish_terminal_closed_unknown_and_inactive_agents() {
    let cases = [
        (
            WorkflowControlToolKind::RetryAgent,
            true,
            false,
            true,
            WorkflowControlRejectionReason::TerminalWorkflow,
        ),
        (
            WorkflowControlToolKind::RetryAgent,
            false,
            true,
            false,
            WorkflowControlRejectionReason::UnknownAgent,
        ),
        (
            WorkflowControlToolKind::RetryAgent,
            false,
            true,
            true,
            WorkflowControlRejectionReason::AgentNotControllable,
        ),
        (
            WorkflowControlToolKind::SkipAgent,
            false,
            true,
            true,
            WorkflowControlRejectionReason::AgentNotActive,
        ),
        (
            WorkflowControlToolKind::Stop,
            false,
            false,
            false,
            WorkflowControlRejectionReason::ControlClosed,
        ),
        (
            WorkflowControlToolKind::SkipAgent,
            false,
            false,
            true,
            WorkflowControlRejectionReason::ControlClosed,
        ),
    ];
    for (kind, terminal, control_open, target_recorded, expected) in cases {
        assert_eq!(
            control_rejection_reason(kind, terminal, control_open, target_recorded),
            expected
        );
    }
}

fn snapshot_with_agent() -> WorkflowTaskSnapshot {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_test".to_string(),
        workflow_name: "test".to_string(),
        title: None,
        status: WorkflowTaskStatus::Running,
        summary: "summary ".repeat(2_000),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: None,
        output_file: root.join("workflow.json"),
        progress: vec![WorkflowProgressItem::WorkflowAgent(Box::new(
            WorkflowAgentProgress {
                invocation_id: "agent".to_string(),
                index: 2,
                label: "agent".to_string(),
                phase_index: Some(1),
                phase_title: Some("phase".to_string()),
                agent_id: Some("agent-id".to_string()),
                model: Some("model".to_string()),
                fallback_model: None,
                isolation: Some(WorkflowIsolation::Worktree),
                state: WorkflowAgentState::Error,
                activity: None,
                blocked: false,
                skipped: false,
                awaiting_decision: true,
                cached: false,
                attempt: 3,
                error: Some("error ".repeat(2_000)),
                tokens: Some(10),
                tool_calls: Some(2),
                duration_ms: Some(100),
                result_preview: None,
                prompt_preview: "prompt".to_string(),
                queued_at: 1,
                started_at: Some(2),
                last_progress_at: 3,
            },
        ))],
        progress_version: 1,
        usage: Default::default(),
        failures: vec!["failure".to_string()],
        error: None,
        started_at: 1,
        completed_at: None,
        script_sha256: "sha256".to_string(),
    }
}
