use super::*;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use crate::keymap::RuntimeKeymap;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::WorkflowAgentActivity;
use codex_app_server_protocol::WorkflowAgentProgress;
use codex_app_server_protocol::WorkflowAgentState;
use codex_app_server_protocol::WorkflowIsolation;
use codex_app_server_protocol::WorkflowProgressKind;
use codex_app_server_protocol::WorkflowResultReadStatus;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn workflow_live_cell_snapshot_covers_agent_states() {
    let task = workflow_task(WorkflowStatus::Running);
    let lines = workflow_task_lines(&task, Instant::now(), /*animations_enabled*/ false);

    insta::assert_snapshot!("workflow_live_cell", visible_text(&lines));
}

#[test]
fn workflow_result_read_lifecycle_snapshots() {
    let mut cell = WorkflowResultReadCell::new(
        "call-read-result".to_string(),
        Some("wf_1234567890abcdef".to_string()),
        /*animations_enabled*/ false,
    );
    insta::assert_snapshot!(
        "workflow_result_read_in_progress",
        visible_text(&cell.display_lines(80))
    );

    cell.finish(WorkflowResultReadStatus::Completed);
    insta::assert_snapshot!(
        "workflow_result_read_completed",
        visible_text(&cell.display_lines(80))
    );

    cell.finish(WorkflowResultReadStatus::Failed);
    insta::assert_snapshot!(
        "workflow_result_read_failed",
        visible_text(&cell.display_lines(80))
    );
    assert_eq!(
        cell.display_lines(80)[0].spans[0].style.fg,
        Some(Color::Red)
    );
}

#[test]
fn workflow_status_colors_match_semantics() {
    let workflow_colors = [
        WorkflowStatus::Running,
        WorkflowStatus::Completed,
        WorkflowStatus::Failed,
        WorkflowStatus::Paused,
        WorkflowStatus::Killed,
    ]
    .map(|status| workflow_status_icon(status).style.fg);
    assert_eq!(
        workflow_colors,
        [
            Some(Color::Cyan),
            Some(Color::Green),
            Some(Color::Red),
            None,
            None,
        ]
    );

    let agent_colors = [
        agent_status_icon(WorkflowAgentState::Queued, AgentFlags::default()),
        agent_status_icon(WorkflowAgentState::Start, AgentFlags::default()),
        agent_status_icon(WorkflowAgentState::Done, AgentFlags::default()),
        agent_status_icon(WorkflowAgentState::Error, AgentFlags::default()),
        agent_status_icon(
            WorkflowAgentState::Error,
            AgentFlags {
                blocked: true,
                ..AgentFlags::default()
            },
        ),
        agent_status_icon(
            WorkflowAgentState::Error,
            AgentFlags {
                skipped: true,
                ..AgentFlags::default()
            },
        ),
        agent_status_icon(
            WorkflowAgentState::Done,
            AgentFlags {
                cached: true,
                ..AgentFlags::default()
            },
        ),
        agent_status_icon(
            WorkflowAgentState::Error,
            AgentFlags {
                awaiting: true,
                ..AgentFlags::default()
            },
        ),
    ]
    .map(|span| span.style.fg);
    assert_eq!(
        agent_colors,
        [
            None,
            Some(Color::Cyan),
            Some(Color::Green),
            Some(Color::Red),
            Some(Color::Red),
            None,
            None,
            Some(Color::Magenta),
        ]
    );
}

#[test]
fn workflow_list_popup_snapshot() {
    let mut state = WorkflowUiState::new(/*animations_enabled*/ false);
    state.merge_tasks(vec![
        workflow_task(WorkflowStatus::Running),
        WorkflowTask {
            run_id: "wf_completed".to_string(),
            title: Some("Deep research".to_string()),
            status: WorkflowStatus::Completed,
            summary: "Research report ready".to_string(),
            completed_at: Some(1_725_000_100),
            ..workflow_task(WorkflowStatus::Completed)
        },
        WorkflowTask {
            run_id: "wf_failed".to_string(),
            title: Some("Release audit".to_string()),
            status: WorkflowStatus::Failed,
            summary: "Verification failed".to_string(),
            completed_at: Some(1_725_000_200),
            ..workflow_task(WorkflowStatus::Failed)
        },
    ]);
    insta::assert_snapshot!(
        "workflow_list_popup",
        render_view(state.list_params(/*selected*/ None))
    );
}

#[test]
fn workflow_detail_popup_snapshot() {
    let mut state = WorkflowUiState::new(/*animations_enabled*/ false);
    let task = workflow_task(WorkflowStatus::Running);
    state.detail_run_id = Some(task.run_id.clone());
    state.merge_tasks(vec![task]);

    insta::assert_snapshot!(
        "workflow_detail_popup",
        render_view_with_height(
            state
                .detail_params(/*selected*/ None)
                .expect("workflow detail params"),
            18,
        )
    );
}

#[test]
fn workflow_running_agent_detail_popup_snapshot() {
    insta::assert_snapshot!(
        "workflow_running_agent_detail_popup",
        render_view_with_height(agent_detail_params_for(WorkflowStatus::Running, 1), 25)
    );
}

#[test]
fn workflow_running_agent_detail_narrow_popup_snapshot() {
    insta::assert_snapshot!(
        "workflow_running_agent_detail_narrow_popup",
        render_view_with_size(agent_detail_params_for(WorkflowStatus::Running, 1), 52, 32,)
    );
}

#[test]
fn workflow_completed_agent_detail_popup_snapshot() {
    insta::assert_snapshot!(
        "workflow_completed_agent_detail_popup",
        render_view_with_height(agent_detail_params_for(WorkflowStatus::Completed, 2), 22)
    );
}
#[test]
fn workflow_awaiting_agent_detail_popup_snapshot() {
    insta::assert_snapshot!(
        "workflow_awaiting_agent_detail_popup",
        render_view_with_height(agent_detail_params_for(WorkflowStatus::Running, 7), 25)
    );
}

#[test]
fn workflow_done_agent_detail_retry_snapshot() {
    insta::assert_snapshot!(
        "workflow_done_agent_detail_retry",
        render_view_with_height(agent_detail_params_for(WorkflowStatus::Running, 2), 22)
    );
}

#[test]
fn workflow_failed_agent_detail_popup_snapshot() {
    insta::assert_snapshot!(
        "workflow_failed_agent_detail_popup",
        render_view_with_height(agent_detail_params_for(WorkflowStatus::Failed, 3), 22)
    );
}

#[test]
fn completed_workflow_agent_item_opens_detail() {
    let task = workflow_task(WorkflowStatus::Completed);
    let item = workflow_agent_item(&task, &task.progress[3]);
    let (tx, mut rx) = unbounded_channel();

    item.actions[0](&AppEventSender::new(tx));

    let AppEvent::OpenWorkflowAgentDetail {
        run_id,
        agent_index,
    } = rx.try_recv().expect("workflow agent detail event")
    else {
        panic!("expected workflow agent detail event");
    };
    assert_eq!((run_id, agent_index), ("wf_review".to_string(), 2));
}

#[test]
fn workflow_agent_detail_opens_agent_transcript() {
    let task = workflow_task(WorkflowStatus::Completed);
    let WorkflowProgressItem::WorkflowAgent(agent) = &task.progress[3] else {
        panic!("expected workflow agent progress");
    };
    let params = workflow_agent_detail_params(&task, agent, /*selected*/ None);
    let transcript_item = params
        .items
        .iter()
        .find(|item| item.name == "Open transcript")
        .expect("open transcript item");
    let expected_thread_id = ThreadId::from_string(
        agent
            .agent_id
            .as_deref()
            .expect("started agents have a thread id"),
    )
    .expect("valid agent thread id");
    let (tx, mut rx) = unbounded_channel();

    transcript_item.actions[0](&AppEventSender::new(tx));

    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::SelectAgentThread(thread_id)) if thread_id == expected_thread_id
    ));
}

fn agent_detail_params_for(status: WorkflowStatus, agent_index: usize) -> SelectionViewParams {
    let task = workflow_task(status);
    let agent = task.progress.iter().find_map(|progress| match progress {
        WorkflowProgressItem::WorkflowAgent(agent) if agent.index == agent_index => {
            Some(agent.as_ref())
        }
        WorkflowProgressItem::WorkflowPhase { .. }
        | WorkflowProgressItem::WorkflowAgent(_)
        | WorkflowProgressItem::WorkflowLog { .. } => None,
    });
    workflow_agent_detail_params(
        &task,
        agent.expect("workflow agent progress"),
        /*selected*/ None,
    )
}

fn workflow_task(status: WorkflowStatus) -> WorkflowTask {
    WorkflowTask {
        thread_id: "00000000-0000-0000-0000-000000000001".to_string(),
        turn_id: "turn-1".to_string(),
        task_id: "w12345678".to_string(),
        run_id: "wf_review".to_string(),
        workflow_name: "code-review".to_string(),
        title: Some("Review changes".to_string()),
        status,
        summary: "Check candidate findings".to_string(),
        transcript_dir: LegacyAppPathString::from_string("/tmp/workflow"),
        script_path: LegacyAppPathString::from_string("/tmp/workflow.js"),
        output_file: LegacyAppPathString::from_string(String::new()),
        progress: vec![
            WorkflowProgressItem::WorkflowPhase {
                index: 1,
                title: "Verify".to_string(),
                kind: WorkflowProgressKind::Active,
            },
            agent(
                0,
                "queued",
                WorkflowAgentState::Queued,
                AgentFlags::default(),
            ),
            agent(
                1,
                "finder",
                WorkflowAgentState::Start,
                AgentFlags::default(),
            ),
            agent(
                2,
                "verified",
                WorkflowAgentState::Done,
                AgentFlags::default(),
            ),
            agent(
                3,
                "failed",
                WorkflowAgentState::Error,
                AgentFlags::default(),
            ),
            agent(
                4,
                "blocked",
                WorkflowAgentState::Error,
                AgentFlags {
                    blocked: true,
                    ..AgentFlags::default()
                },
            ),
            agent(
                5,
                "skipped",
                WorkflowAgentState::Error,
                AgentFlags {
                    skipped: true,
                    ..AgentFlags::default()
                },
            ),
            agent(
                6,
                "cached",
                WorkflowAgentState::Done,
                AgentFlags {
                    cached: true,
                    ..AgentFlags::default()
                },
            ),
            agent(
                7,
                "awaiting",
                WorkflowAgentState::Error,
                AgentFlags {
                    awaiting: true,
                    ..AgentFlags::default()
                },
            ),
        ],
        progress_version: 1,
        usage: WorkflowUsage {
            total_tokens: 12_500,
            tool_uses: 19,
            duration_ms: 4_200,
            agent_count: 7,
        },
        failures: Vec::new(),
        error: None,
        started_at: 1_725_000_000,
        completed_at: None,
    }
}

fn agent(
    index: usize,
    label: &str,
    state: WorkflowAgentState,
    flags: AgentFlags,
) -> WorkflowProgressItem {
    WorkflowProgressItem::WorkflowAgent(Box::new(WorkflowAgentProgress {
        invocation_id: format!("agent-{index}"),
        index,
        label: label.to_string(),
        phase_index: Some(1),
        phase_title: Some("Verify".to_string()),
        agent_id: Some(format!("00000000-0000-0000-0000-{:012}", index + 2)),
        model: Some("gpt-5".to_string()),
        fallback_model: (index == 1).then(|| "gpt-4.1".to_string()),
        isolation: (index == 1).then_some(WorkflowIsolation::Worktree),
        state,
        activity: (index == 1).then_some(WorkflowAgentActivity::AnalyzingInputs),
        blocked: flags.blocked,
        skipped: flags.skipped,
        awaiting_decision: flags.awaiting,
        cached: flags.cached,
        attempt: if index == 1 { 2 } else { 1 },
        error: (state == WorkflowAgentState::Error && !flags.blocked && !flags.skipped)
            .then(|| "Agent failed while validating the candidate finding".to_string()),
        tokens: Some(1_250),
        tool_calls: Some(2),
        duration_ms: Some(1_000),
        result_preview: (state == WorkflowAgentState::Done)
            .then(|| "Verified the candidate against its callers and tests".to_string()),
        prompt_preview: format!(
            "Inspect subsystem {index} and report concrete findings with file references."
        ),
        queued_at: 1_725_000_000 + index as u64 * 10,
        started_at: (state != WorkflowAgentState::Queued)
            .then_some(1_725_000_001 + index as u64 * 10),
        last_progress_at: 1_725_000_005 + index as u64 * 10,
    }))
}

fn visible_text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area().height)
        .map(|row| {
            let line = (0..buffer.area().width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn render_view(params: SelectionViewParams) -> String {
    render_view_with_height(params, 13)
}

fn render_view_with_height(params: SelectionViewParams, height: u16) -> String {
    render_view_with_size(params, 86, height)
}

fn render_view_with_size(params: SelectionViewParams, width: u16, height: u16) -> String {
    let (tx, _rx) = unbounded_channel();
    let view = ListSelectionView::new(
        params,
        AppEventSender::new(tx),
        RuntimeKeymap::defaults().list,
    );
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    buffer_text(&buffer)
}
