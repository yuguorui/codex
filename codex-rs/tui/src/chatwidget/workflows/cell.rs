//! Compact live and completed workflow history cells.

use super::WorkflowStatusExt;
use super::WorkflowUiState;
use super::compact_count;
use crate::history_cell::HistoryCell;
use crate::history_cell::plain_lines;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::motion::shimmer_text;
use codex_app_server_protocol::WorkflowAgentActivity;
use codex_app_server_protocol::WorkflowAgentState;
use codex_app_server_protocol::WorkflowProgressItem;
use codex_app_server_protocol::WorkflowResultReadStatus;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowTask;
use codex_app_server_protocol::WorkflowUsage;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::time::Instant;

const MAX_VISIBLE_AGENTS: usize = 8;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AgentFlags {
    pub(super) blocked: bool,
    pub(super) skipped: bool,
    pub(super) cached: bool,
    pub(super) awaiting: bool,
}

#[derive(Debug)]
pub(super) struct WorkflowSummaryCell {
    pub(super) task: WorkflowTask,
}

#[derive(Debug)]
pub(in crate::chatwidget) struct WorkflowResultReadCell {
    call_id: String,
    run_id: Option<String>,
    status: WorkflowResultReadStatus,
    animation_origin: Instant,
    animations_enabled: bool,
}

impl WorkflowResultReadCell {
    pub(in crate::chatwidget) fn new(
        call_id: String,
        run_id: Option<String>,
        animations_enabled: bool,
    ) -> Self {
        Self {
            call_id,
            run_id,
            status: WorkflowResultReadStatus::InProgress,
            animation_origin: Instant::now(),
            animations_enabled,
        }
    }

    pub(in crate::chatwidget) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(in crate::chatwidget) fn finish(&mut self, status: WorkflowResultReadStatus) {
        self.status = status;
        self.animations_enabled = false;
    }
}

impl HistoryCell for WorkflowResultReadCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let (indicator, action) = match self.status {
            WorkflowResultReadStatus::InProgress => (
                activity_indicator(
                    Some(self.animation_origin),
                    MotionMode::from_animations_enabled(self.animations_enabled),
                    ReducedMotionIndicator::StaticBullet,
                )
                .unwrap_or_else(|| "•".cyan()),
                "Reading Workflow result",
            ),
            WorkflowResultReadStatus::Completed => ("✓".green(), "Read Workflow result"),
            WorkflowResultReadStatus::Failed => ("✗".red(), "Failed to read Workflow result"),
        };
        let mut spans = vec![indicator, " ".into(), action.bold()];
        if let Some(run_id) = self.run_id.as_deref() {
            spans.push(format!("  {}", short_run_id(run_id)).dim());
        }
        vec![spans.into()]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        (self.status == WorkflowResultReadStatus::InProgress && self.animations_enabled)
            .then(|| self.animation_origin.elapsed().as_millis() as u64)
    }
}

fn short_run_id(run_id: &str) -> String {
    run_id.chars().take(12).collect()
}

impl HistoryCell for WorkflowUiState {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for task in self.tasks.iter().filter(|task| task.status.is_active()) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(workflow_task_lines(
                task,
                self.animation_origin,
                self.animations_enabled,
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        (self.animations_enabled && self.has_active_runs()).then(|| self.animation_tick())
    }
}

impl HistoryCell for WorkflowSummaryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        workflow_task_lines(
            &self.task,
            Instant::now(),
            /*animations_enabled*/ false,
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }
}

pub(super) fn workflow_task_lines(
    task: &WorkflowTask,
    animation_origin: Instant,
    animations_enabled: bool,
) -> Vec<Line<'static>> {
    let mut header = vec![workflow_status_icon_with_motion(
        task.status,
        animation_origin,
        animations_enabled,
    )];
    header.push(" ".into());
    let title = task.title.as_deref().unwrap_or(&task.workflow_name);
    if task.status.is_active() {
        let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
        let mut heading = shimmer_text(&format!("Workflow {title}"), motion_mode);
        if !animations_enabled {
            for span in &mut heading {
                span.style = span.style.bold();
            }
        }
        header.extend(heading);
    } else {
        header.push("Workflow".bold());
        header.push(" ".into());
        header.push(title.to_string().bold());
    }
    header.push("  ".into());
    header.push(workflow_status_label(task.status).to_string().dim());

    let mut lines = vec![header.into()];
    if !task.summary.is_empty() {
        lines.push(Line::from(vec!["  ".into(), task.summary.clone().dim()]));
    }
    if let Some((index, title)) = active_phase(task) {
        lines.push(Line::from(vec![
            "  Phase ".dim(),
            format!("{}: ", index + 1).dim(),
            title.to_string().magenta(),
        ]));
    }

    let agents = task
        .progress
        .iter()
        .filter(|item| matches!(item, WorkflowProgressItem::WorkflowAgent(_)))
        .collect::<Vec<_>>();
    for agent in agents.iter().take(MAX_VISIBLE_AGENTS) {
        push_agent_line(&mut lines, agent);
    }
    if agents.len() > MAX_VISIBLE_AGENTS {
        lines.push(Line::from(
            format!("  … {} more agents", agents.len() - MAX_VISIBLE_AGENTS).dim(),
        ));
    }
    if agents.is_empty() && task.status.is_active() {
        lines.push(Line::from("  Waiting for agents…".dim()));
    }
    lines.push(workflow_usage_line(&task.usage));

    for failure in task.failures.iter().take(3) {
        lines.push(Line::from(vec!["  ✗ ".red(), failure.clone().red()]));
    }
    if let Some(error) = task.error.as_deref() {
        lines.push(Line::from(vec!["  Error: ".red(), error.to_string().red()]));
    }
    lines
}

/// Builds the compact fallback: one line per active workflow, plus an omitted
/// count when the viewport cannot show all runs.
pub(in crate::chatwidget) fn workflow_overview_lines(
    state: &WorkflowUiState,
    width: u16,
    max_height: u16,
) -> Vec<Line<'static>> {
    if max_height == 0 {
        return Vec::new();
    }
    let tasks = state
        .tasks
        .iter()
        .filter(|task| task.status.is_active())
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return Vec::new();
    }

    let visible_tasks = if tasks.len() > usize::from(max_height) {
        max_height.saturating_sub(1) as usize
    } else {
        tasks.len()
    };
    let mut lines = tasks
        .iter()
        .take(visible_tasks)
        .map(|task| workflow_overview_line(task, state.animation_origin, state.animations_enabled))
        .map(|line| truncate_line_with_ellipsis_if_overflow(line, usize::from(width.max(1))))
        .collect::<Vec<_>>();
    let omitted = tasks.len() - visible_tasks;
    if omitted > 0 {
        lines.push(format!("  … {omitted} more workflows").dim().into());
    }
    lines
}

fn workflow_overview_line(
    task: &WorkflowTask,
    animation_origin: Instant,
    animations_enabled: bool,
) -> Line<'static> {
    let title = task.title.as_deref().unwrap_or(&task.workflow_name);
    let mut spans = vec![
        workflow_status_icon_with_motion(task.status, animation_origin, animations_enabled),
        " ".into(),
        title.to_string().bold(),
        "  ".into(),
        workflow_status_label(task.status).to_string().dim(),
    ];
    if task.usage.agent_count > 0 {
        spans.push(format!("  {} agents", task.usage.agent_count).dim());
    }
    if task.usage.total_tokens > 0 {
        spans.push(format!("  {} tok", compact_count(task.usage.total_tokens)).dim());
    }
    spans.into()
}

fn push_agent_line(lines: &mut Vec<Line<'static>>, item: &WorkflowProgressItem) {
    let WorkflowProgressItem::WorkflowAgent(agent) = item else {
        return;
    };
    let flags = AgentFlags {
        blocked: agent.blocked,
        skipped: agent.skipped,
        cached: agent.cached,
        awaiting: agent.awaiting_decision,
    };
    let mut spans = vec![
        "  ".into(),
        agent_status_icon(agent.state, flags),
        " ".into(),
        agent_label_span(&agent.label, agent.state, flags),
    ];
    if let Some(agent_id) = agent.agent_id.as_deref() {
        spans.push(" ".into());
        spans.push(format!("({})", short_agent_id(agent_id)).dim());
    }
    if agent.activity == Some(WorkflowAgentActivity::AnalyzingInputs) {
        spans.push("  analyzing inputs".cyan());
    }
    if let Some(tokens) = agent.tokens {
        spans.push(format!("  {} tok", compact_count(tokens)).dim());
    }
    if let Some(tool_calls) = agent.tool_calls {
        spans.push(format!("  {tool_calls} tools").dim());
    }
    lines.push(Line::from(spans));
}

fn active_phase(task: &WorkflowTask) -> Option<(usize, &str)> {
    task.progress.iter().rev().find_map(|item| match item {
        WorkflowProgressItem::WorkflowPhase { index, title, kind }
            if *kind == codex_app_server_protocol::WorkflowProgressKind::Active =>
        {
            Some((*index, title.as_str()))
        }
        WorkflowProgressItem::WorkflowPhase { .. }
        | WorkflowProgressItem::WorkflowAgent(_)
        | WorkflowProgressItem::WorkflowLog { .. } => None,
    })
}

pub(super) fn workflow_usage_line(usage: &WorkflowUsage) -> Line<'static> {
    Line::from(
        format!(
            "  {} agents · {} tokens · {} tools",
            usage.agent_count,
            compact_count(usage.total_tokens),
            usage.tool_uses
        )
        .dim(),
    )
}

fn workflow_status_icon_with_motion(
    status: WorkflowStatus,
    animation_origin: Instant,
    animations_enabled: bool,
) -> Span<'static> {
    if status.is_active() {
        let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
        let indicator = activity_indicator(
            Some(animation_origin),
            motion_mode,
            ReducedMotionIndicator::StaticBullet,
        )
        .unwrap_or_else(|| "•".into());
        return if animations_enabled {
            indicator
        } else {
            indicator.cyan()
        };
    }
    workflow_status_icon(status)
}

pub(super) fn workflow_status_icon(status: WorkflowStatus) -> Span<'static> {
    match status {
        WorkflowStatus::Pending | WorkflowStatus::Running => "•".cyan(),
        WorkflowStatus::Completed => "✓".green(),
        WorkflowStatus::Failed => "✗".red(),
        WorkflowStatus::Paused | WorkflowStatus::Killed => "■".dim(),
    }
}

pub(super) fn workflow_status_label(status: WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Pending => "queued",
        WorkflowStatus::Running => "running",
        WorkflowStatus::Completed => "completed",
        WorkflowStatus::Failed => "failed",
        WorkflowStatus::Paused => "paused",
        WorkflowStatus::Killed => "stopped",
    }
}

pub(super) fn agent_status_icon(state: WorkflowAgentState, flags: AgentFlags) -> Span<'static> {
    if flags.blocked {
        return "!".red();
    }
    if flags.skipped {
        return "−".dim();
    }
    if flags.awaiting {
        return "?".magenta();
    }
    if flags.cached {
        return "✓".dim();
    }
    match state {
        WorkflowAgentState::Queued => "○".dim(),
        WorkflowAgentState::Start => "•".cyan(),
        WorkflowAgentState::Done => "✓".green(),
        WorkflowAgentState::Error => "✗".red(),
    }
}

fn agent_label_span(label: &str, state: WorkflowAgentState, flags: AgentFlags) -> Span<'static> {
    let label = label.to_string();
    if flags.blocked {
        label.red()
    } else if flags.awaiting {
        label.magenta()
    } else if flags.skipped || flags.cached || state == WorkflowAgentState::Queued {
        label.dim()
    } else if state == WorkflowAgentState::Start {
        label.cyan()
    } else if state == WorkflowAgentState::Error {
        label.red()
    } else {
        label.into()
    }
}

fn short_agent_id(agent_id: &str) -> String {
    agent_id.chars().take(8).collect()
}

pub(super) fn agent_status_label(state: WorkflowAgentState, flags: AgentFlags) -> &'static str {
    if flags.blocked {
        "blocked"
    } else if flags.skipped {
        "skipped"
    } else if flags.awaiting {
        "awaiting retry"
    } else if flags.cached {
        "cached"
    } else {
        match state {
            WorkflowAgentState::Queued => "queued",
            WorkflowAgentState::Start => "running",
            WorkflowAgentState::Done => "completed",
            WorkflowAgentState::Error => "failed",
        }
    }
}
