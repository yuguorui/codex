use super::*;
use codex_app_server_protocol::WorkflowResultReadItem;
use pretty_assertions::assert_eq;

#[test]
fn workflow_result_read_transcript_reports_each_lifecycle_state() {
    let rendered = [
        (WorkflowResultReadStatus::InProgress, Some("wf_running")),
        (WorkflowResultReadStatus::Completed, Some("wf_completed")),
        (WorkflowResultReadStatus::Failed, None),
    ]
    .map(|(status, run_id)| {
        let item = ThreadItem::WorkflowResultRead(WorkflowResultReadItem {
            id: "read-result".to_string(),
            run_id: run_id.map(str::to_string),
            status,
        });
        fallback_transcript_cell(&item)
            .expect("workflow result read should render")
            .display_lines(/*width*/ 80)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    });

    assert_eq!(
        rendered,
        [
            "workflow result read in progress: wf_running",
            "workflow result read: wf_completed",
            "workflow result read failed",
        ]
    );
}
