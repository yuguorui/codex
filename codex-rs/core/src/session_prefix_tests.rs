use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::workflow::WorkflowCompletedEvent;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::WORKFLOW_NOTIFICATION_MAX_TOKENS;
use super::WORKFLOW_NOTIFICATION_TRUNCATION_MARKER;
use super::WorkflowNotificationResult;
use super::format_inter_agent_completion_message;
use super::format_workflow_notification_message;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn workflow_notification_includes_bounded_completion_fields() {
    let event = completed_event(
        "Workflow finished".to_string(),
        None,
        Vec::new(),
        WorkflowTaskStatus::Completed,
    );
    let result =
        WorkflowNotificationResult::from_chunk(r#"{"answer":"direct result"}"#, 26, 26).unwrap();
    let message = format_workflow_notification_message(&event, Some(result));

    assert!(message.starts_with("<workflow_notification>"));
    assert!(message.ends_with("</workflow_notification>"));
    assert!(message.contains("my-workflow"));
    assert!(message.contains("\"completed\""));
    assert!(message.contains("Workflow finished"));
    assert!(message.contains("direct result"));
    assert!(message.contains("\"result_available\":true"));
    assert!(!message.contains("output_file"));
    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
}

#[test]
fn workflow_notification_stays_below_manual_review_threshold() {
    let event = completed_event(
        "界😀\"\\\n".repeat(1_000),
        Some("\u{0000}\"\\界😀".repeat(1_000)),
        vec!["failure-progress".to_string()],
        WorkflowTaskStatus::Failed,
    );
    let message = format_workflow_notification_message(&event, None);

    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
    assert!(message.contains(WORKFLOW_NOTIFICATION_TRUNCATION_MARKER));
    assert!(message.contains("\"run_id\":\"wf_test\""));
    assert!(message.contains("\"failed\""));
    assert!(message.contains("\"failures\":1"));
}

#[test]
fn complete_result_uses_result_tool_when_the_notification_would_be_too_large() {
    let event = completed_event(
        "Workflow finished".to_string(),
        None,
        Vec::new(),
        WorkflowTaskStatus::Completed,
    );
    let serialized = serde_json::to_string(&serde_json::json!({
        "answer": "x".repeat(8_000)
    }))
    .unwrap();
    let total_bytes = u64::try_from(serialized.len()).unwrap();
    let result =
        WorkflowNotificationResult::from_chunk(&serialized, total_bytes, total_bytes).unwrap();

    let message = format_workflow_notification_message(&event, Some(result));

    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
    assert!(message.contains("ReadWorkflowResult"));
    assert!(message.contains("\"result\":null"));
    assert!(message.contains("\"next_offset\":0"));
}

#[test]
fn truncated_workflow_result_directs_the_model_to_the_result_tool() {
    let event = completed_event(
        "Workflow finished".to_string(),
        None,
        Vec::new(),
        WorkflowTaskStatus::Completed,
    );
    let result = WorkflowNotificationResult::from_chunk("{\"partial\":", 11, 100).unwrap();

    let message = format_workflow_notification_message(&event, Some(result));

    assert!(message.contains("ReadWorkflowResult"));
    assert!(message.contains("\"next_offset\":0"));
    assert!(message.contains("\"result_truncated\":true"));
    assert!(!message.contains("output_file"));
    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
}

fn completed_event(
    summary: String,
    error: Option<String>,
    failures: Vec<String>,
    status: WorkflowTaskStatus,
) -> WorkflowCompletedEvent {
    WorkflowCompletedEvent {
        thread_id: ThreadId::from_string("22222222-2222-4222-8222-222222222222")
            .expect("valid thread id"),
        turn_id: "turn-1".to_string(),
        task_id: "task-1".to_string(),
        run_id: "wf_test".to_string(),
        workflow_name: "my-workflow".to_string(),
        status,
        summary,
        output_file: AbsolutePathBuf::try_from("/tmp/workflow.json").expect("absolute path"),
        error,
        failures,
        usage: WorkflowUsage {
            total_tokens: 10,
            tool_uses: 2,
            duration_ms: 5,
            agent_count: 1,
            ..WorkflowUsage::default()
        },
        completed_at: 1,
    }
}
