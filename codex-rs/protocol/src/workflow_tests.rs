use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::protocol::EventMsg;

#[test]
fn v1_event_keeps_the_workflow_progress_discriminator() {
    let event = EventMsg::WorkflowProgress(WorkflowProgressEvent {
        thread_id: ThreadId::from_string("22222222-2222-4222-8222-222222222222").unwrap(),
        turn_id: "turn-1".to_string(),
        task_id: "task-1".to_string(),
        run_id: "wf_123456".to_string(),
        progress: vec![WorkflowProgressItem::WorkflowLog {
            message: "started".to_string(),
        }],
        usage: WorkflowUsage::default(),
    });

    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["type"], "workflow_progress");
    assert_eq!(value["threadId"], "22222222-2222-4222-8222-222222222222");
    assert_eq!(value["progress"][0]["type"], "workflow_log");
}

#[test]
fn v1_progress_uses_snake_case_tag_and_camel_case_payload_fields() {
    let item = WorkflowProgressItem::WorkflowAgent(Box::new(WorkflowAgentProgress {
        index: 3,
        label: "verify".to_string(),
        phase_index: Some(1),
        phase_title: Some("Verify".to_string()),
        agent_id: Some("agent-3".to_string()),
        model: Some("gpt-5".to_string()),
        fallback_model: None,
        isolation: Some(WorkflowIsolation::Worktree),
        state: WorkflowAgentState::Start,
        blocked: false,
        skipped: false,
        cached: false,
        attempt: 0,
        error: None,
        tokens: None,
        tool_calls: None,
        duration_ms: None,
        result_preview: None,
        prompt_preview: "inspect".to_string(),
        queued_at: 10,
        started_at: Some(11),
        last_progress_at: 12,
    }));

    assert_eq!(
        serde_json::to_value(item).unwrap(),
        json!({
            "type": "workflow_agent",
            "index": 3,
            "label": "verify",
            "phaseIndex": 1,
            "phaseTitle": "Verify",
            "agentId": "agent-3",
            "model": "gpt-5",
            "fallbackModel": null,
            "isolation": "worktree",
            "state": "start",
            "blocked": false,
            "skipped": false,
            "cached": false,
            "attempt": 0,
            "error": null,
            "tokens": null,
            "toolCalls": null,
            "durationMs": null,
            "resultPreview": null,
            "promptPreview": "inspect",
            "queuedAt": 10,
            "startedAt": 11,
            "lastProgressAt": 12,
        })
    );
}
