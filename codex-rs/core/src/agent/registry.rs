use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use rand::prelude::IndexedRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `AgentControl`
/// is).
#[derive(Default)]
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    total_count: AtomicUsize,
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    thread_paths: HashMap<ThreadId, RegisteredAgent>,
    counted_agents: HashSet<ThreadId>,
    closed_agents: HashMap<ThreadId, AgentRegistration>,
    closed_agent_statuses: HashMap<ThreadId, AgentStatus>,
    closed_agent_order: VecDeque<ThreadId>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
}

struct RegisteredAgent {
    path: String,
    evicted_environments: Option<Vec<TurnEnvironmentSelection>>,
}

impl RegisteredAgent {
    fn new(path: String) -> Self {
        Self {
            path,
            evicted_environments: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentMetadata {
    pub(crate) agent_id: Option<ThreadId>,
    pub(crate) owning_root_thread_id: Option<ThreadId>,
    pub(crate) agent_path: Option<AgentPath>,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
}

const MAX_CLOSED_AGENT_TOMBSTONES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentQuota {
    Counted,
    Unmetered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentRegistration {
    pub(crate) metadata: AgentMetadata,
    pub(crate) quota: AgentQuota,
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

impl AgentRegistry {
    pub(crate) fn reserve_counted_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads,
                }));
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            quota: AgentQuota::Counted,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
        })
    }

    pub(crate) fn reserve_unmetered_spawn_slot(self: &Arc<Self>) -> SpawnReservation {
        SpawnReservation {
            state: Arc::clone(self),
            active: true,
            quota: AgentQuota::Unmetered,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
        }
    }

    #[cfg(test)]
    fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        self.reserve_counted_spawn_slot(max_threads)
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let removed = active_agents
                .thread_paths
                .remove(&thread_id)
                .and_then(|agent| active_agents.agent_tree.remove(agent.path.as_str()))
                .is_some();
            removed && active_agents.counted_agents.remove(&thread_id)
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root_path = AgentPath::ROOT.to_string();
        let root_thread_id = active_agents
            .agent_tree
            .entry(root_path.clone())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                owning_root_thread_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            })
            .agent_id;
        if let Some(root_thread_id) = root_thread_id {
            active_agents
                .thread_paths
                .insert(root_thread_id, RegisteredAgent::new(root_path));
        }
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
    }

    pub(crate) fn save_evicted_environments(
        &self,
        thread_id: ThreadId,
        environments: Vec<TurnEnvironmentSelection>,
    ) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = Some(environments);
        }
    }

    pub(crate) fn evicted_environments(
        &self,
        thread_id: ThreadId,
    ) -> Option<Vec<TurnEnvironmentSelection>> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| agent.evicted_environments.clone())
    }

    pub(crate) fn clear_evicted_environments(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn registration_for_close(&self, thread_id: ThreadId) -> Option<AgentRegistration> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
            .map(|metadata| AgentRegistration {
                metadata,
                quota: if active_agents.counted_agents.contains(&thread_id) {
                    AgentQuota::Counted
                } else {
                    AgentQuota::Unmetered
                },
            })
            .or_else(|| active_agents.closed_agents.get(&thread_id).cloned())
    }

    pub(crate) fn authorize_agent_access(
        &self,
        caller_thread_id: ThreadId,
        target_thread_id: ThreadId,
    ) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let caller_root_thread_id = active_agents
            .thread_paths
            .get(&caller_thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .and_then(|metadata| metadata.owning_root_thread_id)?;
        let target = active_agents
            .thread_paths
            .get(&target_thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
            .or_else(|| {
                active_agents
                    .closed_agents
                    .get(&target_thread_id)
                    .map(|registration| registration.metadata.clone())
            })?;
        (target.owning_root_thread_id == Some(caller_root_thread_id)).then_some(target)
    }

    pub(crate) fn remember_closed_agent(&self, registration: AgentRegistration) {
        let Some(thread_id) = registration.metadata.agent_id else {
            return;
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .closed_agent_order
            .retain(|closed_thread_id| *closed_thread_id != thread_id);
        active_agents.closed_agent_order.push_back(thread_id);
        active_agents.closed_agents.insert(thread_id, registration);
        while active_agents.closed_agents.len() > MAX_CLOSED_AGENT_TOMBSTONES {
            if let Some(evicted_thread_id) = active_agents.closed_agent_order.pop_front() {
                active_agents.closed_agents.remove(&evicted_thread_id);
                active_agents
                    .closed_agent_statuses
                    .remove(&evicted_thread_id);
            }
        }
    }

    pub(crate) fn remember_closed_agent_status(&self, thread_id: ThreadId, status: AgentStatus) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents.closed_agents.contains_key(&thread_id) {
            active_agents
                .closed_agent_statuses
                .insert(thread_id, status);
        }
    }

    pub(crate) fn closed_agent_status(&self, thread_id: ThreadId) -> Option<AgentStatus> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed_agent_statuses
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    fn register_spawned_thread_with_quota(&self, agent_metadata: AgentMetadata, quota: AgentQuota) {
        let Some(thread_id) = agent_metadata.agent_id else {
            return;
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents.closed_agents.remove(&thread_id);
        active_agents.closed_agent_statuses.remove(&thread_id);
        active_agents
            .closed_agent_order
            .retain(|closed_thread_id| *closed_thread_id != thread_id);
        match quota {
            AgentQuota::Counted => {
                active_agents.counted_agents.insert(thread_id);
            }
            AgentQuota::Unmetered => {
                active_agents.counted_agents.remove(&thread_id);
            }
        }
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        if let Some(previous_agent) = active_agents
            .thread_paths
            .insert(thread_id, RegisteredAgent::new(key.clone()))
            && previous_agent.path != key
        {
            active_agents
                .agent_tree
                .remove(previous_agent.path.as_str());
        }
        if let Some(previous_metadata) = active_agents.agent_tree.insert(key, agent_metadata)
            && let Some(previous_thread_id) = previous_metadata.agent_id
            && previous_thread_id != thread_id
        {
            active_agents.thread_paths.remove(&previous_thread_id);
            if active_agents.counted_agents.remove(&previous_thread_id) {
                self.total_count.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    #[cfg(test)]
    fn register_spawned_thread(&self, agent_metadata: AgentMetadata) {
        let quota = agent_metadata
            .agent_id
            .and_then(|thread_id| self.registration_for_close(thread_id))
            .map_or(AgentQuota::Unmetered, |registration| registration.quota);
        self.register_spawned_thread_with_quota(agent_metadata, quota);
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_names: Vec<String> = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .collect();
            if let Some(name) = available_names.choose(&mut rand::rng()) {
                name.clone()
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    quota: AgentQuota,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
            })?;
        self.reserved_agent_nickname = Some(agent_nickname.clone());
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) {
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.state
            .register_spawned_thread_with_quota(agent_metadata, self.quota);
        self.active = false;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            if self.quota == AgentQuota::Counted {
                self.state.total_count.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
