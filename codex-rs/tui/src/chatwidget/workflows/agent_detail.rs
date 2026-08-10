use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_app_server_protocol::WorkflowAgentActivity;
use codex_app_server_protocol::WorkflowAgentProgress;
use codex_app_server_protocol::WorkflowAgentState;
use codex_app_server_protocol::WorkflowIsolation;
use codex_app_server_protocol::WorkflowTask;
use codex_protocol::ThreadId;
use codex_utils_elapsed::format_duration;
use ratatui::style::Stylize;

use super::AgentFlags;
use super::WorkflowStatusExt;
use super::agent_status_label;
use super::compact_count;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionDescriptionLayout;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionRowDisplay;
use crate::bottom_pane::SelectionViewParams;

pub(super) const WORKFLOW_AGENT_DETAIL_VIEW_ID: &str = "workflow-agent-detail";

pub(super) fn workflow_agent_detail_params(
    task: &WorkflowTask,
    agent: &WorkflowAgentProgress,
    selected: Option<usize>,
) -> SelectionViewParams {
    let mut items = vec![transcript_item(agent)];
    if task.status.is_active() {
        if agent.state == WorkflowAgentState::Start || agent.awaiting_decision {
            items.extend(agent_control_items(task, agent.index));
        } else {
            items.push(retry_agent_item(task, agent.index));
        }
    }
    items.extend([
        info_item("Runtime", runtime_description(agent)),
        info_item("Usage", usage_description(agent)),
        info_item("Timing", timing_description(agent)),
        info_item("Prompt", prompt_description(agent)),
        info_item(result_label(agent), result_description(agent)),
    ]);

    SelectionViewParams {
        view_id: Some(WORKFLOW_AGENT_DETAIL_VIEW_ID),
        title: Some(agent.label.clone()),
        subtitle: Some(agent_subtitle(agent)),
        items,
        initial_selected_idx: selected,
        row_display: SelectionRowDisplay::Wrapped,
        description_layout: SelectionDescriptionLayout::StackBelowWhenNarrow {
            min_description_width: 36,
        },
        name_column_width: Some(18),
        ..Default::default()
    }
}

fn transcript_item(agent: &WorkflowAgentProgress) -> SelectionItem {
    let thread_id = agent
        .agent_id
        .as_deref()
        .and_then(|agent_id| ThreadId::from_string(agent_id).ok());
    let Some(thread_id) = thread_id else {
        return SelectionItem {
            name: "Open transcript".to_string(),
            name_prefix_spans: vec!["↗ ".dim()],
            description: Some("Available after this agent starts".to_string()),
            is_disabled: true,
            ..Default::default()
        };
    };

    SelectionItem {
        name: "Open transcript".to_string(),
        name_prefix_spans: vec!["↗ ".cyan()],
        description: Some("Agent conversation".to_string()),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::SelectAgentThread(thread_id));
        })],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn retry_agent_item(task: &WorkflowTask, agent_index: usize) -> SelectionItem {
    let run_id = task.run_id.clone();
    SelectionItem {
        name: "Retry agent".to_string(),
        name_prefix_spans: vec!["↻ ".cyan()],
        description: Some(
            "Re-run this agent and recompute downstream stages that already ran".to_string(),
        ),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::RetryWorkflowAgent {
                run_id: run_id.clone(),
                agent_index,
            });
        })],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn agent_control_items(task: &WorkflowTask, agent_index: usize) -> [SelectionItem; 2] {
    let retry_run_id = task.run_id.clone();
    let skip_run_id = task.run_id.clone();
    [
        SelectionItem {
            name: "Retry attempt".to_string(),
            name_prefix_spans: vec!["↻ ".cyan()],
            description: Some("Cancel this attempt and start another".to_string()),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::RetryWorkflowAgent {
                    run_id: retry_run_id.clone(),
                    agent_index,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        },
        SelectionItem {
            name: "Skip agent".to_string(),
            name_prefix_spans: vec!["− ".dim()],
            description: Some("Cancel this agent and return null".to_string()),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::SkipWorkflowAgent {
                    run_id: skip_run_id.clone(),
                    agent_index,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        },
    ]
}

fn info_item(name: &str, description: String) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        description: Some(description),
        is_disabled: true,
        ..Default::default()
    }
}

fn agent_subtitle(agent: &WorkflowAgentProgress) -> String {
    let flags = AgentFlags {
        blocked: agent.blocked,
        skipped: agent.skipped,
        cached: agent.cached,
        awaiting: agent.awaiting_decision,
    };
    let status = agent_status_label(agent.state, flags);
    let agent_id = agent
        .agent_id
        .as_deref()
        .map(short_agent_id)
        .map(|agent_id| format!("agent {agent_id}"))
        .unwrap_or_else(|| "agent id unavailable".to_string());
    let activity = if agent.activity == Some(WorkflowAgentActivity::AnalyzingInputs) {
        Some("analyzing inputs")
    } else if agent.awaiting_decision {
        Some("awaiting retry or skip")
    } else {
        None
    };
    match (agent.phase_index, agent.phase_title.as_deref()) {
        (Some(index), Some(title)) => activity.map_or_else(
            || format!("{status} · Phase {}: {title} · {agent_id}", index + 1),
            |activity| {
                format!(
                    "{status} · {activity} · Phase {}: {title} · {agent_id}",
                    index + 1
                )
            },
        ),
        (Some(index), None) => activity.map_or_else(
            || format!("{status} · Phase {} · {agent_id}", index + 1),
            |activity| format!("{status} · {activity} · Phase {} · {agent_id}", index + 1),
        ),
        (None, Some(title)) => activity.map_or_else(
            || format!("{status} · {title} · {agent_id}"),
            |activity| format!("{status} · {activity} · {title} · {agent_id}"),
        ),
        (None, None) => activity.map_or_else(
            || format!("{status} · {agent_id}"),
            |activity| format!("{status} · {activity} · {agent_id}"),
        ),
    }
}

fn short_agent_id(agent_id: &str) -> String {
    agent_id.chars().take(8).collect()
}

fn runtime_description(agent: &WorkflowAgentProgress) -> String {
    let mut details = vec![
        agent
            .model
            .as_deref()
            .map_or_else(|| "session model".to_string(), ToString::to_string),
    ];
    if let Some(fallback_model) = agent.fallback_model.as_deref() {
        details.push(format!("fallback {fallback_model}"));
    }
    let isolation = match agent.isolation {
        Some(WorkflowIsolation::Worktree) => "worktree isolation",
        Some(WorkflowIsolation::Remote) => "remote isolation",
        None => "shared workspace",
    };
    details.push(isolation.to_string());
    details.push(format!("attempt {}", agent.attempt));
    details.join(" · ")
}

fn usage_description(agent: &WorkflowAgentProgress) -> String {
    let tokens = agent.tokens.map_or_else(
        || "tokens unavailable".to_string(),
        |tokens| format!("{} tokens", compact_count(tokens)),
    );
    let tools = agent.tool_calls.map_or_else(
        || "tools unavailable".to_string(),
        |tool_calls| format!("{tool_calls} tools"),
    );
    let duration = agent.duration_ms.map_or_else(
        || "duration unavailable".to_string(),
        |duration_ms| format_duration(Duration::from_millis(duration_ms)),
    );
    format!("{tokens} · {tools} · {duration}")
}

fn timing_description(agent: &WorkflowAgentProgress) -> String {
    let queued = format_timestamp(agent.queued_at);
    let started = agent
        .started_at
        .map_or_else(|| "not started".to_string(), format_timestamp);
    let updated = format_timestamp(agent.last_progress_at);
    format!("queued {queued} · started {started} · updated {updated}")
}

fn format_timestamp(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("Unix {seconds}"))
}

fn prompt_description(agent: &WorkflowAgentProgress) -> String {
    if agent.prompt_preview.is_empty() {
        "No prompt preview reported".to_string()
    } else {
        agent.prompt_preview.clone()
    }
}

fn result_label(agent: &WorkflowAgentProgress) -> &'static str {
    if agent.error.is_some() || agent.state == WorkflowAgentState::Error {
        "Error"
    } else {
        "Result"
    }
}

fn result_description(agent: &WorkflowAgentProgress) -> String {
    if let Some(error) = agent.error.as_deref() {
        return error.to_string();
    }
    if let Some(result) = agent.result_preview.as_deref() {
        return result.to_string();
    }
    if agent.blocked {
        return "Agent was blocked".to_string();
    }
    if agent.skipped {
        return "Agent was skipped".to_string();
    }
    match agent.state {
        WorkflowAgentState::Queued => "Waiting to start".to_string(),
        WorkflowAgentState::Start => "Waiting for completion".to_string(),
        WorkflowAgentState::Done => "No result preview reported".to_string(),
        WorkflowAgentState::Error => "No error details reported".to_string(),
    }
}
