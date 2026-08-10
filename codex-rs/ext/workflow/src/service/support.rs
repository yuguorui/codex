use super::*;

pub(super) fn upsert_progress(
    progress: &mut Vec<WorkflowProgressItem>,
    item: WorkflowProgressItem,
) {
    let position = progress
        .iter()
        .position(|existing| match (existing, &item) {
            (
                WorkflowProgressItem::WorkflowPhase { index: left, .. },
                WorkflowProgressItem::WorkflowPhase { index: right, .. },
            ) => left == right,
            (
                WorkflowProgressItem::WorkflowAgent(left),
                WorkflowProgressItem::WorkflowAgent(right),
            ) => left.index == right.index,
            _ => false,
        });
    if let Some(position) = position {
        progress[position] = item;
    } else if progress.len() < MAX_PROGRESS_ITEMS {
        progress.push(item);
    }
}

pub(super) fn update_usage_from_progress(snapshot: &mut WorkflowTaskSnapshot) {
    let mut total_tokens = 0_u64;
    let mut tool_uses = 0_u64;
    let mut agent_count = 0_usize;
    for item in &snapshot.progress {
        if let WorkflowProgressItem::WorkflowAgent(agent) = item {
            agent_count += 1;
            total_tokens = total_tokens.saturating_add(agent.tokens.unwrap_or(0));
            tool_uses = tool_uses.saturating_add(agent.tool_calls.unwrap_or(0));
        }
    }
    snapshot.usage.total_tokens = total_tokens;
    snapshot.usage.tool_uses = tool_uses;
    snapshot.usage.agent_count = agent_count;
}

pub(super) fn failures_from_progress(progress: &[WorkflowProgressItem]) -> Vec<String> {
    progress
        .iter()
        .filter_map(|item| match item {
            WorkflowProgressItem::WorkflowAgent(agent)
                if agent.state == WorkflowAgentState::Error && !agent.skipped =>
            {
                agent
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {error}", agent.label))
            }
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowAgent(_)
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .collect()
}

pub(super) fn persist_task_background(task: Arc<WorkflowTask>) {
    {
        let mut state = task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal {
            return;
        }
        state.dirty = true;
        if state.running {
            return;
        }
        state.running = true;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SNAPSHOT_PERSIST_INTERVAL).await;
            {
                let mut state = task
                    .persist_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.terminal {
                    state.running = false;
                    return;
                }
                state.dirty = false;
            }
            let Ok(_permit) = task.persist_lock.acquire().await else {
                tracing::warn!("workflow persistence lock was closed");
                task.persist_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .running = false;
                return;
            };
            {
                let mut state = task
                    .persist_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.terminal {
                    state.running = false;
                    return;
                }
            }
            let snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Err(error) = write_json(&snapshot.output_file, &snapshot).await {
                tracing::warn!(%error, "failed to persist workflow progress snapshot");
            }
            let mut state = task
                .persist_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.terminal {
                state.running = false;
                break;
            }
            if state.dirty {
                continue;
            }
            state.running = false;
            break;
        }
    });
}

pub(super) async fn persist_terminal_task(task: &WorkflowTask) {
    {
        let mut state = task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.terminal = true;
        state.dirty = false;
    }
    let Ok(_permit) = task.persist_lock.acquire().await else {
        tracing::warn!("workflow persistence lock was closed");
        return;
    };
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Err(error) = write_json(&snapshot.output_file, &snapshot).await {
        tracing::warn!(%error, "failed to write terminal workflow snapshot");
    }
}

pub(super) fn prune_terminal_tasks(
    tasks: &mut HashMap<String, Arc<WorkflowTask>>,
    max_terminal_tasks: usize,
) {
    let mut terminal_tasks = tasks
        .iter()
        .filter_map(|(run_id, task)| {
            let snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (!matches!(
                snapshot.status,
                WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
            ))
            .then(|| (snapshot.started_at, run_id.clone()))
        })
        .collect::<Vec<_>>();
    terminal_tasks.sort_by(|left, right| right.cmp(left));
    for (_, run_id) in terminal_tasks.into_iter().skip(max_terminal_tasks) {
        tasks.remove(&run_id);
    }
}

pub(super) fn pause_unadoptable(snapshot: &mut WorkflowTaskSnapshot, error: String) {
    snapshot.status = WorkflowTaskStatus::Paused;
    snapshot.summary = format!("Workflow {} paused", snapshot.workflow_name);
    snapshot.error = Some(error);
}

pub(super) fn persistence_error(error: std::io::Error) -> WorkflowServiceError {
    WorkflowServiceError::Persistence(error.to_string())
}

pub(super) fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub(super) fn slugify(name: &str) -> String {
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug.trim_matches('-').to_string()
}

pub(super) fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}
