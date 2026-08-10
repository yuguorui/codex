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
    let message = format_workflow_notification_message(&event);

    assert!(message.starts_with("<workflow_notification>"));
    assert!(message.ends_with("</workflow_notification>"));
    assert!(message.contains("my-workflow"));
    assert!(message.contains("\"completed\""));
    assert!(message.contains("Workflow finished"));
}

#[test]
fn workflow_notification_stays_below_manual_review_threshold() {
    let event = completed_event(
        "workflow summary ".repeat(1_000),
        Some("workflow failure ".repeat(1_000)),
        vec!["failure-progress".to_string()],
        WorkflowTaskStatus::Failed,
    );
    let message = format_workflow_notification_message(&event);

    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
    assert!(message.contains("\"failed\""));
    assert!(message.contains("\"failures\":1"));
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
        },
        completed_at: 1,
    }
}
