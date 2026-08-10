use super::*;
use codex_protocol::workflow::WorkflowTaskStatus;
use pretty_assertions::assert_eq;

fn snapshot(workflow_name: &str, summary: &str, error: Option<&str>) -> WorkflowTaskSnapshot {
    WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_budgets".to_string(),
        workflow_name: workflow_name.to_string(),
        title: None,
        status: WorkflowTaskStatus::Failed,
        summary: summary.to_string(),
        transcript_dir: codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir())
            .unwrap()
            .join("transcript"),
        script_path: codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir())
            .unwrap()
            .join("workflow.js"),
        args: serde_json::Value::Null,
        result_artifact: None,
        output_file: codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir())
            .unwrap()
            .join("workflow.json"),
        progress: Vec::new(),
        progress_version: 0,
        usage: Default::default(),
        failures: Vec::new(),
        error: error.map(str::to_string),
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    }
}

#[test]
fn run_error_gets_a_larger_budget_than_summary_text() {
    let text = "e".repeat(4_000);
    assert_eq!(bounded_error_text(&text).len(), 160);
    assert_eq!(bounded_output_text(&text).len(), 96);
    assert_eq!(compact_wait_text(&text).len(), 8);
    // Short text is never padded or marked.
    assert_eq!(bounded_error_text("boom"), "boom");
}

#[test]
fn error_budget_ladder_steps_down_and_stops_at_the_stub() {
    let mut error = bounded_error_text(&"script content changed; ".repeat(40));
    let mut lengths = vec![error.len()];
    for budget in WAIT_ERROR_BUDGET_LADDER {
        error = truncate_model_text(&error, budget);
        lengths.push(error.len());
    }
    // Re-running the last rung must be a fixed point, so the ladder terminates.
    lengths.push(truncate_model_text(&error, COMPACT_WAIT_TEXT_MAX_BYTES).len());
    assert_eq!(lengths, vec![160, 96, 8, 8]);
    assert_eq!(error, "script c");
}

#[test]
fn wait_run_text_bounds_every_identity_field_from_one_snapshot() {
    let text = WaitRunText::from_snapshot(&snapshot(
        &"n".repeat(4_000),
        &"s".repeat(4_000),
        Some(&"e".repeat(4_000)),
    ));
    assert_eq!(text.workflow_name.len(), WAIT_WORKFLOW_NAME_MAX_BYTES);
    assert_eq!(text.summary.len(), WAIT_OUTPUT_TEXT_MAX_BYTES);
    assert_eq!(
        text.error.as_deref().map(str::len),
        Some(WAIT_ERROR_TEXT_MAX_BYTES)
    );
    assert!(text.workflow_name.ends_with("...[truncated]"));
    assert!(text.summary.ends_with("...[truncated]"));
    assert!(text.error.as_deref().unwrap().ends_with("...[truncated]"));

    let short = WaitRunText::from_snapshot(&snapshot("wf", "done", None));
    assert_eq!(
        short,
        WaitRunText {
            workflow_name: "wf".to_string(),
            summary: "done".to_string(),
            error: None,
        }
    );
}
