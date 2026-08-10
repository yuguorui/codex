use super::*;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;

#[test]
fn recovery_is_available_for_resumable_terminal_states() {
    for status in [
        WorkflowTaskStatus::Paused,
        WorkflowTaskStatus::Failed,
        WorkflowTaskStatus::Killed,
    ] {
        let recovery = workflow_recovery_status(&snapshot(status, None));

        assert_eq!(
            recovery,
            WorkflowRecoveryStatus {
                recovery_eligible: true,
                reason: match status {
                    WorkflowTaskStatus::Paused => "paused",
                    WorkflowTaskStatus::Failed => "failed",
                    WorkflowTaskStatus::Killed => "killed",
                    _ => unreachable!("tested statuses are fixed above"),
                },
                may_require_reapproval: true,
                identity_requirements: vec![
                    "scriptSha256",
                    "args",
                    "childWorkflowDefinition",
                    "declaredInputs",
                    "executionIdentity",
                ],
                observed_restore_incompatibilities: Vec::new(),
            }
        );
    }
}

#[test]
fn recovery_is_disabled_for_non_resumable_states() {
    for status in [
        WorkflowTaskStatus::Pending,
        WorkflowTaskStatus::Running,
        WorkflowTaskStatus::Completed,
    ] {
        let recovery = workflow_recovery_status(&snapshot(status, None));

        assert_eq!(recovery.recovery_eligible, false);
        assert_eq!(recovery.identity_requirements, Vec::<&str>::new());
    }
}

#[test]
fn paused_restore_errors_name_known_identity_incompatibilities() {
    let error = "script content changed since it was approved; captured workflow execution identity changed; declared inputs changed";
    let recovery = workflow_recovery_status(&snapshot(WorkflowTaskStatus::Paused, Some(error)));

    assert_eq!(
        recovery.observed_restore_incompatibilities,
        vec!["scriptSha256", "declaredInputs", "executionIdentity",]
    );
}

#[test]
fn compact_recovery_keeps_an_explicit_identity_requirement() {
    let mut recovery = workflow_recovery_status(&snapshot(WorkflowTaskStatus::Failed, None));
    recovery.compact_for_wait();

    assert_eq!(
        recovery.identity_requirements,
        vec!["sameApprovedWorkflowIdentity"]
    );
}

#[test]
fn compact_recovery_invents_no_requirement_for_a_run_that_cannot_resume() {
    let mut recovery = workflow_recovery_status(&snapshot(WorkflowTaskStatus::Completed, None));
    let before = recovery.clone();

    recovery.compact_for_wait();

    assert_eq!(
        recovery, before,
        "compaction must not grow the response or claim resume semantics"
    );
}

fn snapshot(status: WorkflowTaskStatus, error: Option<&str>) -> WorkflowTaskSnapshot {
    let root = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_recovery".to_string(),
        workflow_name: "recovery".to_string(),
        title: None,
        status,
        summary: "summary".to_string(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: None,
        output_file: root.join("workflow.json"),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: error.map(str::to_string),
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    }
}
