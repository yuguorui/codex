//! Dynamic workflow state, live rendering, and `/workflows` popups.

use super::*;
use crate::bottom_pane::SelectionRowDisplay;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowProgressItem;
use codex_app_server_protocol::WorkflowProgressNotification;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowTask;
use codex_app_server_protocol::WorkflowUsage;
use codex_utils_path_uri::LegacyAppPathString;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::time::Duration;
use std::time::Instant;

mod agent_detail;
mod cell;
use self::agent_detail::WORKFLOW_AGENT_DETAIL_VIEW_ID;
use self::agent_detail::workflow_agent_detail_params;
use self::cell::AgentFlags;
pub(super) use self::cell::WorkflowResultReadCell;
use self::cell::WorkflowSummaryCell;
use self::cell::agent_status_icon;
use self::cell::agent_status_label;
use self::cell::workflow_overview_lines;
use self::cell::workflow_status_icon;
use self::cell::workflow_status_label;
#[cfg(test)]
use self::cell::workflow_task_lines;
use self::cell::workflow_usage_line;

const WORKFLOW_LIST_VIEW_ID: &str = "workflow-list";
const WORKFLOW_DETAIL_VIEW_ID: &str = "workflow-detail";
const MAX_STORED_RUNS: usize = 100;

#[derive(Debug)]
pub(super) struct WorkflowUiState {
    tasks: Vec<WorkflowTask>,
    detail_run_id: Option<String>,
    detail_agent_index: Option<usize>,
    animation_origin: Instant,
    animations_enabled: bool,
}

impl Default for WorkflowUiState {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            detail_run_id: None,
            detail_agent_index: None,
            animation_origin: Instant::now(),
            animations_enabled: false,
        }
    }
}

impl WorkflowUiState {
    pub(super) fn new(animations_enabled: bool) -> Self {
        Self {
            animations_enabled,
            ..Self::default()
        }
    }

    pub(super) fn has_active_runs(&self) -> bool {
        self.tasks.iter().any(|task| task.status.is_active())
    }

    /// Renders the normal live tail when it fits, otherwise one bounded row per
    /// workflow. This is the main-viewport fallback used on short terminals.
    pub(in crate::chatwidget) fn display_lines_for_height(
        &self,
        width: u16,
        max_height: u16,
    ) -> Vec<Line<'static>> {
        let full_lines = self.display_lines(width);
        let full_height = Paragraph::new(full_lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        if full_height <= usize::from(max_height) {
            return full_lines;
        }
        workflow_overview_lines(self, width, max_height)
    }

    pub(super) fn merge_tasks(&mut self, tasks: Vec<WorkflowTask>) {
        for task in tasks {
            self.upsert(task);
        }
    }

    fn started(&mut self, notification: WorkflowStartedNotification) {
        self.upsert(WorkflowTask {
            thread_id: notification.thread_id,
            turn_id: notification.turn_id,
            task_id: notification.task_id,
            run_id: notification.run_id,
            workflow_name: notification.workflow_name,
            title: notification.title,
            status: WorkflowStatus::Running,
            summary: notification.summary,
            transcript_dir: notification.transcript_dir,
            script_path: notification.script_path,
            output_file: LegacyAppPathString::from_string(String::new()),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at: notification.started_at,
            completed_at: None,
        });
    }

    fn progress(&mut self, notification: WorkflowProgressNotification) {
        let Some(task) = self
            .tasks
            .iter_mut()
            .find(|task| task.run_id == notification.run_id)
        else {
            tracing::warn!(
                run_id = notification.run_id,
                "workflow progress arrived before start"
            );
            return;
        };
        task.progress = notification.progress;
        task.usage = notification.usage;
        task.progress_version = task.progress_version.saturating_add(1);
    }

    fn completed(&mut self, notification: WorkflowCompletedNotification) -> Option<WorkflowTask> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.run_id == notification.run_id)?;
        let was_active = task.status.is_active();
        task.workflow_name = notification.workflow_name;
        task.status = notification.status;
        task.summary = notification.summary;
        task.output_file = notification.output_file;
        task.error = notification.error;
        task.failures = notification.failures;
        task.usage = notification.usage;
        task.completed_at = Some(notification.completed_at);
        was_active.then(|| task.clone())
    }

    fn upsert(&mut self, task: WorkflowTask) {
        if let Some(existing) = self
            .tasks
            .iter_mut()
            .find(|existing| existing.run_id == task.run_id)
        {
            *existing = task;
        } else {
            self.tasks.push(task);
        }
        self.tasks
            .sort_by_key(|task| std::cmp::Reverse(task.started_at));
        self.tasks.truncate(MAX_STORED_RUNS);
    }

    fn task(&self, run_id: &str) -> Option<&WorkflowTask> {
        self.tasks.iter().find(|task| task.run_id == run_id)
    }

    fn list_params(&self, selected: Option<usize>) -> SelectionViewParams {
        let items = if self.tasks.is_empty() {
            vec![SelectionItem {
                name: "No workflows in this thread".to_string(),
                description: Some("Workflow runs appear here after launch.".to_string()),
                is_disabled: true,
                ..Default::default()
            }]
        } else {
            self.tasks.iter().map(workflow_list_item).collect()
        };
        SelectionViewParams {
            view_id: Some(WORKFLOW_LIST_VIEW_ID),
            title: Some("Workflows".to_string()),
            subtitle: Some("Live and completed runs for this thread".to_string()),
            items,
            initial_selected_idx: selected,
            is_searchable: self.tasks.len() > 8,
            search_placeholder: Some("Search workflows".to_string()),
            row_display: SelectionRowDisplay::SingleLine,
            ..Default::default()
        }
    }

    fn detail_params(&self, selected: Option<usize>) -> Option<SelectionViewParams> {
        let task = self.task(self.detail_run_id.as_deref()?)?;
        let mut items = Vec::new();
        if task.status.is_active() {
            let run_id = task.run_id.clone();
            items.push(SelectionItem {
                name: "Stop workflow".to_string(),
                name_prefix_spans: vec!["■ ".red()],
                description: Some("Stop all agents in this run".to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::StopWorkflow {
                        run_id: run_id.clone(),
                    });
                })],
                ..Default::default()
            });
        }
        for progress in &task.progress {
            match progress {
                WorkflowProgressItem::WorkflowPhase { index, title, .. } => {
                    items.push(SelectionItem {
                        name: format!("Phase {}: {title}", index + 1),
                        name_prefix_spans: vec!["◇ ".magenta()],
                        is_disabled: true,
                        ..Default::default()
                    });
                }
                progress @ WorkflowProgressItem::WorkflowAgent(_) => {
                    items.push(workflow_agent_item(task, progress));
                }
                WorkflowProgressItem::WorkflowLog { .. } => {}
            }
        }
        if items.is_empty() {
            items.push(SelectionItem {
                name: "Waiting for workflow progress".to_string(),
                is_disabled: true,
                ..Default::default()
            });
        }
        Some(SelectionViewParams {
            view_id: Some(WORKFLOW_DETAIL_VIEW_ID),
            title: Some(
                task.title
                    .as_deref()
                    .unwrap_or(&task.workflow_name)
                    .to_string(),
            ),
            subtitle: Some(format!(
                "{}  {}",
                workflow_status_label(task.status),
                task.summary
            )),
            footer_note: Some(workflow_usage_line(&task.usage)),
            items,
            initial_selected_idx: selected,
            row_display: SelectionRowDisplay::SingleLine,
            ..Default::default()
        })
    }

    fn agent_detail_params(&self, selected: Option<usize>) -> Option<SelectionViewParams> {
        let task = self.task(self.detail_run_id.as_deref()?)?;
        let agent_index = self.detail_agent_index?;
        let agent = task.progress.iter().find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) if agent.index == agent_index => {
                Some(agent.as_ref())
            }
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowAgent(_)
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })?;
        Some(workflow_agent_detail_params(task, agent, selected))
    }

    fn animation_tick(&self) -> u64 {
        self.animation_origin.elapsed().as_millis() as u64 / 600
    }
}

impl ChatWidget {
    pub(super) fn on_workflow_started(&mut self, notification: WorkflowStartedNotification) {
        self.workflows.started(notification);
        self.workflow_state_changed();
    }

    pub(super) fn on_workflow_progress(&mut self, notification: WorkflowProgressNotification) {
        self.workflows.progress(notification);
        self.workflow_state_changed();
    }

    pub(super) fn on_workflow_completed(&mut self, notification: WorkflowCompletedNotification) {
        let completed = self.workflows.completed(notification);
        if let Some(task) = completed {
            self.add_to_history(WorkflowSummaryCell { task });
        }
        self.workflow_state_changed();
    }

    pub(crate) fn open_workflows(&mut self, tasks: Vec<WorkflowTask>) {
        self.workflows.merge_tasks(tasks);
        self.workflows.detail_run_id = None;
        self.workflows.detail_agent_index = None;
        self.bottom_pane
            .show_selection_view(self.workflows.list_params(/*selected*/ None));
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(crate) fn open_workflow_detail(&mut self, run_id: &str) {
        if self.workflows.task(run_id).is_none() {
            self.add_error_message(format!("Workflow run '{run_id}' is no longer available."));
            return;
        }
        self.workflows.detail_run_id = Some(run_id.to_string());
        self.workflows.detail_agent_index = None;
        if let Some(params) = self.workflows.detail_params(/*selected*/ None) {
            self.bottom_pane.show_selection_view(params);
            self.request_redraw();
        }
    }

    pub(crate) fn open_workflow_agent_detail(&mut self, run_id: String, agent_index: usize) {
        self.workflows.detail_run_id = Some(run_id.clone());
        self.workflows.detail_agent_index = Some(agent_index);
        let Some(params) = self.workflows.agent_detail_params(/*selected*/ None) else {
            self.add_error_message(format!(
                "Workflow agent {agent_index} from run '{run_id}' is no longer available."
            ));
            return;
        };
        self.bottom_pane.show_selection_view(params);
        self.request_redraw();
    }

    pub(super) fn schedule_workflow_frame_if_needed(&self) {
        if self.config.animations && self.workflows.has_active_runs() {
            self.frame_requester
                .schedule_frame_in(Duration::from_millis(50));
        }
    }

    fn workflow_state_changed(&mut self) {
        self.bump_active_cell_revision();
        self.refresh_workflow_popups();
        self.request_redraw();
    }

    fn refresh_workflow_popups(&mut self) {
        let list_selected = self
            .bottom_pane
            .selected_index_for_active_view(WORKFLOW_LIST_VIEW_ID);
        self.bottom_pane.replace_selection_view_if_present(
            WORKFLOW_LIST_VIEW_ID,
            self.workflows.list_params(list_selected),
        );

        let detail_selected = self
            .bottom_pane
            .selected_index_for_active_view(WORKFLOW_DETAIL_VIEW_ID);
        if let Some(params) = self.workflows.detail_params(detail_selected) {
            self.bottom_pane
                .replace_selection_view_if_present(WORKFLOW_DETAIL_VIEW_ID, params);
        }

        let agent_detail_selected = self
            .bottom_pane
            .selected_index_for_active_view(WORKFLOW_AGENT_DETAIL_VIEW_ID);
        if let Some(params) = self.workflows.agent_detail_params(agent_detail_selected) {
            self.bottom_pane
                .replace_selection_view_if_present(WORKFLOW_AGENT_DETAIL_VIEW_ID, params);
        }
    }
}

fn workflow_list_item(task: &WorkflowTask) -> SelectionItem {
    let run_id = task.run_id.clone();
    SelectionItem {
        name: task
            .title
            .as_deref()
            .unwrap_or(&task.workflow_name)
            .to_string(),
        name_prefix_spans: vec![workflow_status_icon(task.status), " ".into()],
        description: Some(format!(
            "{} · {} agents · {} tokens · {}",
            workflow_status_label(task.status),
            task.usage.agent_count,
            compact_count(task.usage.total_tokens),
            task.run_id
        )),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::OpenWorkflowDetail {
                run_id: run_id.clone(),
            });
        })],
        dismiss_on_select: false,
        search_value: Some(format!("{} {}", task.workflow_name, task.run_id)),
        ..Default::default()
    }
}

fn workflow_agent_item(task: &WorkflowTask, progress: &WorkflowProgressItem) -> SelectionItem {
    let WorkflowProgressItem::WorkflowAgent(agent) = progress else {
        unreachable!("workflow_agent_item requires an agent progress item");
    };
    let flags = AgentFlags {
        blocked: agent.blocked,
        skipped: agent.skipped,
        cached: agent.cached,
        awaiting: agent.awaiting_decision,
    };
    let mut description = agent_status_label(agent.state, flags).to_string();
    if let Some(tokens) = agent.tokens {
        description.push_str(&format!(" · {} tokens", compact_count(tokens)));
    }
    if let Some(tool_calls) = agent.tool_calls {
        description.push_str(&format!(" · {tool_calls} tools"));
    }

    let run_id = task.run_id.clone();
    let agent_index = agent.index;
    SelectionItem {
        name: agent.label.clone(),
        name_prefix_spans: vec![agent_status_icon(agent.state, flags), " ".into()],
        description: Some(description),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::OpenWorkflowAgentDetail {
                run_id: run_id.clone(),
                agent_index,
            });
        })],
        dismiss_on_select: false,
        ..Default::default()
    }
}

fn compact_count(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        return format!("{:.1}K", value as f64 / 1_000.0);
    }
    format!("{:.1}M", value as f64 / 1_000_000.0)
}

trait WorkflowStatusExt {
    fn is_active(&self) -> bool;
}

impl WorkflowStatusExt for WorkflowStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

#[cfg(test)]
#[path = "workflows_tests.rs"]
mod tests;
