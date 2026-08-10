use super::*;

const MAX_PROGRESS_SNAPSHOT_ITEMS: usize = 512;
const MAX_PROGRESS_SNAPSHOT_PHASES: usize = 64;
const MAX_PROGRESS_SNAPSHOT_LOGS: usize = 32;
const MAX_PROGRESS_LOG_STATES: usize = 256;
pub(super) const MAX_PROGRESS_FAILURES: usize = 256;
const MAX_HOT_AGENT_STATES: usize = MAX_PROGRESS_SNAPSHOT_ITEMS;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedAgentProgress {
    execution_generation: u64,
    agent_count: usize,
    agent_high_water: usize,
    progress: codex_protocol::workflow::WorkflowAgentProgress,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedPhaseProgress {
    execution_generation: u64,
    progress: WorkflowProgressItem,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLogProgress {
    execution_generation: u64,
    sequence: u64,
    progress: WorkflowProgressItem,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PersistedProgressMetadata {
    pub(super) execution_generation: u64,
    pub(super) agent_count: usize,
    pub(super) agent_high_water: usize,
    pub(super) log_high_water: u64,
    pub(super) failures: Vec<(usize, String)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct AgentOutcomeCounts {
    pub(super) successful: usize,
    pub(super) failed: usize,
    pub(super) skipped: usize,
    pub(super) null_results: usize,
}

pub(super) struct WorkflowProgressState {
    agent_state_dir: AbsolutePathBuf,
    phase_state_dir: AbsolutePathBuf,
    log_state_dir: AbsolutePathBuf,
    pub(super) metadata_path: AbsolutePathBuf,
    phases: BTreeMap<usize, (u64, WorkflowProgressItem)>,
    phase_order: VecDeque<usize>,
    hot_agents: BTreeMap<usize, PersistedAgentProgress>,
    hot_agent_order: VecDeque<usize>,
    logs: VecDeque<WorkflowProgressItem>,
    authoritative_logs: bool,
    count_generation: u64,
    agent_count: usize,
    agent_high_water: usize,
    log_high_water: u64,
    failure_index: BTreeMap<usize, String>,
    failure_order: VecDeque<usize>,
}

impl WorkflowProgressState {
    pub(super) fn from_snapshot(snapshot: &WorkflowTaskSnapshot) -> Self {
        let progress_dir = snapshot.transcript_dir.join("progress");
        let metadata_path = progress_dir.join("state.json");
        let snapshot_high_water = snapshot
            .progress
            .iter()
            .filter_map(|item| match item {
                WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.index.saturating_add(1)),
                WorkflowProgressItem::WorkflowPhase { .. }
                | WorkflowProgressItem::WorkflowLog { .. } => None,
            })
            .max()
            .unwrap_or(0);
        let metadata = read_json::<PersistedProgressMetadata>(&metadata_path);
        let has_metadata = metadata.is_some();
        let mut count_generation = metadata
            .as_ref()
            .map_or(0, |metadata| metadata.execution_generation);
        let mut agent_count = metadata
            .as_ref()
            .map_or(snapshot.usage.agent_count, |metadata| metadata.agent_count);
        let mut agent_high_water = metadata
            .as_ref()
            .map_or(snapshot_high_water, |metadata| metadata.agent_high_water);
        let agent_state_dir = progress_dir.join("agents");
        let phase_state_dir = progress_dir.join("phases");
        let log_state_dir = progress_dir.join("logs");
        let active = !workflow_status_is_terminal(snapshot.status);
        let persisted_agents = if workflow_status_is_terminal(snapshot.status) {
            Vec::new()
        } else {
            scan_agent_states(&agent_state_dir)
        };
        let persisted_phases = if active {
            scan_phase_states(&phase_state_dir)
        } else {
            Vec::new()
        };
        let persisted_logs = if active {
            scan_log_states(&log_state_dir)
        } else {
            Vec::new()
        };
        if let Some(recovered_generation) = persisted_agents
            .iter()
            .map(|persisted| persisted.execution_generation)
            .chain(
                persisted_phases
                    .iter()
                    .map(|persisted| persisted.execution_generation),
            )
            .chain(
                persisted_logs
                    .iter()
                    .map(|persisted| persisted.execution_generation),
            )
            .max()
            .filter(|generation| *generation > count_generation)
        {
            count_generation = recovered_generation;
            agent_count = 0;
            agent_high_water = 0;
        }
        for persisted in persisted_agents
            .iter()
            .filter(|persisted| persisted.execution_generation == count_generation)
        {
            agent_count = agent_count.max(persisted.agent_count);
            agent_high_water = agent_high_water.max(persisted.agent_high_water);
        }
        let metadata_matches_generation = metadata
            .as_ref()
            .is_some_and(|metadata| metadata.execution_generation == count_generation);
        let persisted_failures = metadata
            .as_ref()
            .filter(|_| metadata_matches_generation)
            .map_or_else(Vec::new, |metadata| metadata.failures.clone());
        let mut state = Self {
            agent_state_dir,
            phase_state_dir,
            log_state_dir,
            metadata_path,
            phases: BTreeMap::new(),
            phase_order: VecDeque::new(),
            hot_agents: BTreeMap::new(),
            hot_agent_order: VecDeque::new(),
            logs: VecDeque::new(),
            authoritative_logs: workflow_status_is_terminal(snapshot.status),
            count_generation,
            agent_count,
            agent_high_water,
            log_high_water: metadata
                .as_ref()
                .filter(|_| metadata_matches_generation)
                .map_or(0, |metadata| metadata.log_high_water),
            failure_index: persisted_failures.iter().cloned().collect(),
            failure_order: persisted_failures
                .into_iter()
                .map(|(index, _)| index)
                .collect(),
        };
        for persisted in persisted_agents
            .into_iter()
            .filter(|persisted| persisted.execution_generation == count_generation)
        {
            state.record_in_memory(
                count_generation,
                WorkflowProgressItem::WorkflowAgent(Box::new(persisted.progress)),
            );
        }
        for persisted in persisted_phases
            .into_iter()
            .filter(|persisted| persisted.execution_generation == count_generation)
        {
            state.record_in_memory(count_generation, persisted.progress);
        }
        let mut current_logs = persisted_logs
            .into_iter()
            .filter(|persisted| persisted.execution_generation == count_generation)
            .collect::<Vec<_>>();
        current_logs.sort_by_key(|persisted| persisted.sequence);
        let has_persisted_logs = !current_logs.is_empty();
        for persisted in current_logs {
            state.log_high_water = state
                .log_high_water
                .max(persisted.sequence.saturating_add(1));
            state.record_in_memory(count_generation, persisted.progress);
        }
        for item in &snapshot.progress {
            let current = match item {
                WorkflowProgressItem::WorkflowAgent(agent) if has_metadata => state
                    .read_agent(agent.index)
                    .filter(|persisted| {
                        persisted.execution_generation == count_generation
                            && persisted.progress.invocation_id == agent.invocation_id
                    })
                    .map(|persisted| {
                        WorkflowProgressItem::WorkflowAgent(Box::new(persisted.progress))
                    }),
                WorkflowProgressItem::WorkflowPhase { index, .. } if has_metadata => state
                    .read_phase(*index)
                    .filter(|persisted| persisted.execution_generation == count_generation)
                    .map(|persisted| persisted.progress),
                WorkflowProgressItem::WorkflowAgent(_)
                | WorkflowProgressItem::WorkflowPhase { .. }
                | WorkflowProgressItem::WorkflowLog { .. } => Some(item.clone()),
            };
            if matches!(current, Some(WorkflowProgressItem::WorkflowLog { .. }))
                && has_persisted_logs
            {
                continue;
            }
            if let Some(item) = current {
                state.record_in_memory(count_generation, item);
            }
        }
        if (!has_metadata
            || metadata.as_ref().is_some_and(|metadata| {
                metadata.execution_generation != state.count_generation
                    || metadata.agent_count != state.agent_count
                    || metadata.agent_high_water != state.agent_high_water
            }))
            && let Err(error) = state.persist_metadata()
        {
            tracing::warn!(%error, "failed to initialize workflow progress metadata");
        }
        state
    }

    pub(super) fn record(&mut self, execution_generation: u64, item: WorkflowProgressItem) {
        if execution_generation > self.count_generation {
            self.begin_execution(execution_generation);
        } else if execution_generation < self.count_generation {
            return;
        }
        let mut persist_metadata_after_record = false;
        if let WorkflowProgressItem::WorkflowAgent(agent) = &item {
            let previous = self
                .hot_agents
                .get(&agent.index)
                .cloned()
                .or_else(|| self.read_agent(agent.index));
            let new_invocation = previous.as_ref().is_none_or(|previous| {
                previous.execution_generation != execution_generation
                    || previous.progress.invocation_id != agent.invocation_id
            });
            if new_invocation {
                self.agent_count = self.agent_count.saturating_add(1);
            }
            self.agent_high_water = self.agent_high_water.max(agent.index.saturating_add(1));
            if should_persist_agent_state(execution_generation, previous.as_ref(), agent) {
                match self.persist_agent(execution_generation, agent) {
                    Ok(()) => persist_metadata_after_record = true,
                    Err(error) => {
                        tracing::warn!(agent_index = agent.index, %error, "failed to persist workflow agent progress");
                    }
                }
            }
        } else {
            match &item {
                WorkflowProgressItem::WorkflowPhase { index, .. } => {
                    if let Err(error) = self.persist_phase(execution_generation, *index, &item) {
                        tracing::warn!(phase_index = index, %error, "failed to persist workflow phase progress");
                    }
                }
                WorkflowProgressItem::WorkflowLog { .. } => {
                    if let Err(error) = self.persist_log(execution_generation, &item) {
                        tracing::warn!(%error, "failed to persist workflow log progress");
                    } else {
                        self.log_high_water = self.log_high_water.saturating_add(1);
                        persist_metadata_after_record = true;
                    }
                }
                WorkflowProgressItem::WorkflowAgent(_) => {}
            }
        }
        self.record_in_memory(execution_generation, item);
        if persist_metadata_after_record && let Err(error) = self.persist_metadata() {
            tracing::warn!(%error, "failed to persist workflow progress metadata");
        }
    }

    pub(super) fn execution_generation(&self) -> u64 {
        self.count_generation
    }

    pub(super) fn begin_execution(&mut self, execution_generation: u64) {
        self.count_generation = execution_generation;
        self.agent_count = 0;
        self.agent_high_water = 0;
        self.log_high_water = 0;
        self.phases.clear();
        self.phase_order.clear();
        self.hot_agents.clear();
        self.hot_agent_order.clear();
        self.logs.clear();
        self.authoritative_logs = false;
        self.failure_index.clear();
        self.failure_order.clear();
        if let Err(error) = self.persist_metadata() {
            tracing::warn!(%error, "failed to persist workflow progress metadata");
        }
    }

    pub(super) fn replace_logs(&mut self, execution_generation: u64, logs: Vec<String>) {
        if execution_generation > self.count_generation {
            self.begin_execution(execution_generation);
        } else if execution_generation < self.count_generation {
            return;
        }
        self.logs = logs
            .into_iter()
            .map(|message| WorkflowProgressItem::WorkflowLog { message })
            .collect();
        self.authoritative_logs = true;
    }

    fn record_in_memory(&mut self, execution_generation: u64, item: WorkflowProgressItem) {
        match item {
            WorkflowProgressItem::WorkflowPhase { index, .. } => {
                self.phase_order.retain(|phase_index| *phase_index != index);
                self.phase_order.push_back(index);
                self.phases.insert(index, (execution_generation, item));
                while self.phase_order.len() > MAX_PROGRESS_SNAPSHOT_PHASES {
                    if let Some(oldest) = self.phase_order.pop_front() {
                        self.phases.remove(&oldest);
                    }
                }
            }
            WorkflowProgressItem::WorkflowAgent(agent) => {
                self.failure_order.retain(|index| *index != agent.index);
                self.failure_index.remove(&agent.index);
                if agent.state == WorkflowAgentState::Error && !agent.skipped {
                    if let Some(error) = agent.error.as_ref() {
                        self.failure_order.push_back(agent.index);
                        self.failure_index
                            .insert(agent.index, format!("{}: {error}", agent.label));
                    }
                    while self.failure_order.len() > MAX_PROGRESS_FAILURES {
                        if let Some(oldest) = self.failure_order.pop_front() {
                            self.failure_index.remove(&oldest);
                        }
                    }
                }
                self.agent_high_water = self.agent_high_water.max(agent.index.saturating_add(1));
                self.hot_agent_order
                    .retain(|agent_index| *agent_index != agent.index);
                self.hot_agent_order.push_back(agent.index);
                self.hot_agents.insert(
                    agent.index,
                    PersistedAgentProgress {
                        execution_generation,
                        agent_count: self.agent_count,
                        agent_high_water: self.agent_high_water,
                        progress: *agent,
                    },
                );
                while self.hot_agent_order.len() > MAX_HOT_AGENT_STATES {
                    if let Some(oldest) = self.hot_agent_order.pop_front() {
                        self.hot_agents.remove(&oldest);
                    }
                }
            }
            WorkflowProgressItem::WorkflowLog { .. } => {
                if self.logs.len() == MAX_PROGRESS_LOG_STATES {
                    self.logs.pop_front();
                }
                self.logs.push_back(item);
            }
        }
    }

    pub(super) fn latest_window(&self) -> Vec<WorkflowProgressItem> {
        let phases = self
            .phase_order
            .iter()
            .rev()
            .take(MAX_PROGRESS_SNAPSHOT_PHASES)
            .filter_map(|index| self.phases.get(index))
            .filter(|(generation, _)| *generation == self.count_generation)
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>();
        let logs = if self.authoritative_logs {
            self.logs.iter().cloned().collect::<Vec<_>>()
        } else {
            self.logs
                .iter()
                .rev()
                .take(MAX_PROGRESS_SNAPSHOT_LOGS)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };
        let agent_limit = MAX_PROGRESS_SNAPSHOT_ITEMS.saturating_sub(phases.len() + logs.len());
        let agents = self
            .hot_agent_order
            .iter()
            .rev()
            .filter_map(|index| self.hot_agents.get(index))
            .filter(|agent| agent.execution_generation == self.count_generation)
            .take(agent_limit)
            .map(|agent| WorkflowProgressItem::WorkflowAgent(Box::new(agent.progress.clone())))
            .collect::<Vec<_>>();

        phases
            .into_iter()
            .rev()
            .chain(agents.into_iter().rev())
            .chain(logs)
            .collect()
    }

    pub(super) fn page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Vec<codex_protocol::workflow::WorkflowAgentProgress> {
        let mut page = Vec::with_capacity(limit.min(self.agent_count));
        let end = offset.saturating_add(limit).min(self.agent_high_water);
        for index in offset..end {
            if let Some(agent) = self.agent(index) {
                page.push(agent);
            }
        }
        page
    }

    pub(super) fn agent(
        &self,
        index: usize,
    ) -> Option<codex_protocol::workflow::WorkflowAgentProgress> {
        self.hot_agents
            .get(&index)
            .filter(|agent| agent.execution_generation == self.count_generation)
            .cloned()
            .or_else(|| self.read_agent(index))
            .filter(|agent| agent.execution_generation == self.count_generation)
            .map(|agent| agent.progress)
    }

    pub(super) fn agent_count(&self) -> usize {
        self.agent_count
    }

    pub(super) fn agent_high_water(&self) -> usize {
        self.agent_high_water
    }

    pub(super) fn failures(&self) -> Vec<String> {
        self.failure_order
            .iter()
            .filter_map(|index| self.failure_index.get(index))
            .cloned()
            .collect()
    }

    pub(super) fn agent_outcome_counts(&self) -> AgentOutcomeCounts {
        let mut counts = AgentOutcomeCounts::default();
        for index in 0..self.agent_high_water {
            let Some(agent) = self.agent(index) else {
                continue;
            };
            if agent.skipped {
                counts.skipped += 1;
            } else if agent.state == WorkflowAgentState::Done {
                counts.successful += 1;
                counts.null_results += usize::from(agent.result_preview.as_deref() == Some("null"));
            } else if agent.state == WorkflowAgentState::Error {
                counts.failed += 1;
            }
        }
        counts
    }

    fn persist_agent(
        &self,
        execution_generation: u64,
        progress: &codex_protocol::workflow::WorkflowAgentProgress,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.agent_state_dir).map_err(|error| error.to_string())?;
        let contents = serde_json::to_string(&PersistedAgentProgress {
            execution_generation,
            agent_count: self.agent_count,
            agent_high_water: self.agent_high_water,
            progress: progress.clone(),
        })
        .map_err(|error| error.to_string())?;
        codex_utils_path::write_atomically(&self.agent_path(progress.index), &contents)
            .map_err(|error| error.to_string())
    }

    fn persist_metadata(&self) -> Result<(), String> {
        if let Some(parent) = self.metadata_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let contents = serde_json::to_string(&PersistedProgressMetadata {
            execution_generation: self.count_generation,
            agent_count: self.agent_count,
            agent_high_water: self.agent_high_water,
            log_high_water: self.log_high_water,
            failures: self
                .failure_order
                .iter()
                .filter_map(|index| {
                    self.failure_index
                        .get(index)
                        .map(|failure| (*index, failure.clone()))
                })
                .collect(),
        })
        .map_err(|error| error.to_string())?;
        codex_utils_path::write_atomically(&self.metadata_path, &contents)
            .map_err(|error| error.to_string())
    }

    fn persist_phase(
        &self,
        execution_generation: u64,
        index: usize,
        progress: &WorkflowProgressItem,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.phase_state_dir).map_err(|error| error.to_string())?;
        let contents = serde_json::to_string(&PersistedPhaseProgress {
            execution_generation,
            progress: progress.clone(),
        })
        .map_err(|error| error.to_string())?;
        codex_utils_path::write_atomically(&self.phase_path(index), &contents)
            .map_err(|error| error.to_string())
    }

    fn persist_log(
        &self,
        execution_generation: u64,
        progress: &WorkflowProgressItem,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.log_state_dir).map_err(|error| error.to_string())?;
        let contents = serde_json::to_string(&PersistedLogProgress {
            execution_generation,
            sequence: self.log_high_water,
            progress: progress.clone(),
        })
        .map_err(|error| error.to_string())?;
        codex_utils_path::write_atomically(&self.log_path(self.log_high_water), &contents)
            .map_err(|error| error.to_string())
    }

    fn read_agent(&self, index: usize) -> Option<PersistedAgentProgress> {
        read_json(&self.agent_path(index))
    }

    fn read_phase(&self, index: usize) -> Option<PersistedPhaseProgress> {
        read_json(&self.phase_path(index))
    }

    fn agent_path(&self, index: usize) -> AbsolutePathBuf {
        self.agent_state_dir.join(format!("{index:020}.json"))
    }

    fn phase_path(&self, index: usize) -> AbsolutePathBuf {
        self.phase_state_dir.join(format!("{index:020}.json"))
    }

    fn log_path(&self, sequence: u64) -> AbsolutePathBuf {
        self.log_state_dir.join(format!("{sequence:020}.json"))
    }
}

fn read_json<T>(path: &AbsolutePathBuf) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let contents = std::fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

fn scan_phase_states(directory: &AbsolutePathBuf) -> Vec<PersistedPhaseProgress> {
    scan_progress_records(directory, read_json::<PersistedPhaseProgress>)
}

fn scan_agent_states(directory: &AbsolutePathBuf) -> Vec<PersistedAgentProgress> {
    scan_progress_records(directory, read_json::<PersistedAgentProgress>)
}

fn scan_log_states(directory: &AbsolutePathBuf) -> Vec<PersistedLogProgress> {
    scan_progress_records(directory, read_json::<PersistedLogProgress>)
}

fn scan_progress_records<T>(
    directory: &AbsolutePathBuf,
    read: impl Fn(&AbsolutePathBuf) -> Option<T>,
) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| AbsolutePathBuf::try_from(entry.path()).ok())
        .filter_map(|path| read(&path))
        .collect()
}

fn should_persist_agent_state(
    execution_generation: u64,
    previous: Option<&PersistedAgentProgress>,
    progress: &codex_protocol::workflow::WorkflowAgentProgress,
) -> bool {
    match progress.state {
        WorkflowAgentState::Queued | WorkflowAgentState::Done | WorkflowAgentState::Error => true,
        WorkflowAgentState::Start => previous.is_none_or(|previous| {
            previous.execution_generation != execution_generation
                || previous.progress.state != WorkflowAgentState::Start
        }),
    }
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
            if let Err(error) = task.persist_snapshot().await {
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

pub(super) async fn persist_terminal_task(
    task: &WorkflowTask,
    terminal_snapshot: &WorkflowTaskSnapshot,
) -> Result<(), String> {
    let _permit = task
        .persist_lock
        .acquire()
        .await
        .map_err(|_| "workflow persistence lock was closed".to_string())?;
    if let Err(error) = task.persist_snapshot_value(terminal_snapshot).await {
        let mut state = task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.dirty = true;
        state.terminal = false;
        return Err(error);
    }
    sync_snapshot_durably(&terminal_snapshot.output_file).await?;
    *task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = terminal_snapshot.clone();
    let mut state = task
        .persist_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.terminal = true;
    state.dirty = false;
    Ok(())
}

async fn sync_snapshot_durably(path: &AbsolutePathBuf) -> Result<(), String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&path)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("failed to sync terminal workflow snapshot: {error}"))?;
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .ok_or_else(|| "terminal workflow snapshot path has no parent".to_string())?;
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    format!("failed to sync terminal workflow snapshot directory: {error}")
                })?;
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("terminal workflow snapshot sync failed: {error}"))?
}

const PENDING_LIFECYCLE_DIRECTORY: &str = ".pending-deliveries";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum WorkflowLifecycleDelivery {
    Started,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct PendingLifecycleDelivery {
    pub(super) run_id: String,
    pub(super) lifecycle: WorkflowLifecycleDelivery,
}

impl WorkflowLifecycleDelivery {
    fn file_name(self, run_id: &str) -> String {
        match self {
            Self::Started => format!(".{run_id}.started-delivery.json"),
            Self::Completed => format!(".{run_id}.completed-delivery.json"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct LifecycleDeliveryState {
    pub(super) idempotency_key: String,
    pub(super) transport_acknowledged: bool,
    pub(super) owning_model_acknowledged: bool,
}

pub(super) fn lifecycle_delivery_path(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
) -> AbsolutePathBuf {
    snapshot
        .output_file
        .parent()
        .unwrap_or_else(|| snapshot.output_file.clone())
        .join(lifecycle.file_name(&snapshot.run_id))
}

pub(super) async fn lock_lifecycle_delivery(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
) -> Result<File, String> {
    let path = lifecycle_delivery_path(snapshot, lifecycle).with_extension("lock");
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        file.lock().map_err(|error| error.to_string())?;
        Ok(file)
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn load_lifecycle_delivery(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
) -> Option<LifecycleDeliveryState> {
    read_json(&lifecycle_delivery_path(snapshot, lifecycle))
}

pub(super) async fn persist_lifecycle_delivery(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
    state: &LifecycleDeliveryState,
) -> Result<(), String> {
    let path = lifecycle_delivery_path(snapshot, lifecycle);
    let contents = serde_json::to_string(state).map_err(|error| error.to_string())?;
    let pending = !state.transport_acknowledged
        || matches!(lifecycle, WorkflowLifecycleDelivery::Completed)
            && !state.owning_model_acknowledged;
    let marker = pending_lifecycle_delivery_path(snapshot, lifecycle);
    let marker_contents = serde_json::to_string(&PendingLifecycleDelivery {
        run_id: snapshot.run_id.clone(),
        lifecycle,
    })
    .map_err(|error| error.to_string())?;
    tokio::task::spawn_blocking(move || {
        if pending {
            write_durable_delivery_file(&marker, &marker_contents)?;
        }
        codex_utils_path::write_atomically(&path, &contents).map_err(|error| error.to_string())?;
        std::fs::File::open(&path)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        if !pending {
            match std::fs::remove_file(&marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) async fn load_pending_lifecycle_deliveries(
    snapshots_directory: AbsolutePathBuf,
) -> Result<Vec<PendingLifecycleDelivery>, String> {
    tokio::task::spawn_blocking(move || {
        let directory = snapshots_directory.join(PENDING_LIFECYCLE_DIRECTORY);
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let mut pending = Vec::new();
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(delivery) = serde_json::from_slice::<PendingLifecycleDelivery>(&bytes) else {
                continue;
            };
            if delivery.run_id.starts_with("wf_")
                && delivery.run_id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
            {
                pending.push(delivery);
            }
        }
        Ok(pending)
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn pending_lifecycle_delivery_path(
    snapshot: &WorkflowTaskSnapshot,
    lifecycle: WorkflowLifecycleDelivery,
) -> AbsolutePathBuf {
    let digest = sha256(&format!("{}:{lifecycle:?}", snapshot.run_id));
    snapshot
        .output_file
        .parent()
        .unwrap_or_else(|| snapshot.output_file.clone())
        .join(PENDING_LIFECYCLE_DIRECTORY)
        .join(format!("{digest}.json"))
}

fn write_durable_delivery_file(path: &Path, contents: &str) -> Result<(), String> {
    let parent_existed = path.parent().is_some_and(Path::exists);
    codex_utils_path::write_atomically(path, contents).map_err(|error| error.to_string())?;
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())?;
        if !parent_existed && let Some(grandparent) = parent.parent() {
            std::fs::File::open(grandparent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
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
