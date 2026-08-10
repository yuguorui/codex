use codex_agent_extension::AgentRunner;
use codex_core::config::Config;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ToolTokenBudget;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::workflow::WorkflowAgentState;
use codex_protocol::workflow::WorkflowCompletedEvent;
use codex_protocol::workflow::WorkflowProgressEvent;
use codex_protocol::workflow::WorkflowProgressItem;
use codex_protocol::workflow::WorkflowStartedEvent;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow::ValidatedWorkflowScript;
use codex_workflow::WorkflowBudget;
use codex_workflow::WorkflowBudgetSource;
use codex_workflow::WorkflowControl;
use codex_workflow::WorkflowEvent;
use codex_workflow::WorkflowExecutionError;
use codex_workflow::WorkflowRunOutcome;
use codex_workflow::WorkflowRuntimeConfig;
use codex_workflow::execute_workflow;
use codex_workflow::validate_workflow_script;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Semaphore;

use crate::agent::CodexWorkflowAgentRuntime;
use crate::agent::WorktreeCleanupMode;
use crate::discovery::PluginWorkflowRoot;
use crate::discovery::ResolvedWorkflow;
use crate::discovery::SavedWorkflowChildResolver;
use crate::journal::FileWorkflowJournal;
use crate::persistence::journal_path;
use crate::persistence::load_snapshots;
use crate::persistence::snapshot_path;
use crate::persistence::workflow_session_dir;
use crate::persistence::write_json;

mod support;
use support::*;

const MAX_PROGRESS_ITEMS: usize = 4096;
const MAX_RETAINED_TERMINAL_TASKS: usize = 256;
const SNAPSHOT_PERSIST_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLaunch {
    pub status: String,
    pub task_id: String,
    pub task_type: String,
    pub workflow_name: String,
    pub run_id: String,
    pub summary: String,
    pub transcript_dir: String,
    pub script_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskSnapshot {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub title: Option<String>,
    pub status: WorkflowTaskStatus,
    pub summary: String,
    pub transcript_dir: AbsolutePathBuf,
    pub script_path: AbsolutePathBuf,
    #[serde(default)]
    pub args: JsonValue,
    #[serde(default)]
    pub result: JsonValue,
    /// Canonical path to this serialized snapshot.
    pub output_file: AbsolutePathBuf,
    pub progress: Vec<WorkflowProgressItem>,
    pub progress_version: u64,
    pub usage: WorkflowUsage,
    pub failures: Vec<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub script_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowServiceError {
    #[error("workflow task was not found")]
    NotFound,
    #[error("workflow run belongs to a different thread")]
    WrongThread,
    #[error("workflow run is still running; stop it before resuming")]
    StillRunning,
    #[error("failed to persist workflow state: {0}")]
    Persistence(String),
}

struct WorkflowTask {
    snapshot: Mutex<WorkflowTaskSnapshot>,
    persist_lock: Semaphore,
    persist_state: Mutex<PersistState>,
    control: WorkflowControl,
}

#[derive(Default)]
struct PersistState {
    running: bool,
    dirty: bool,
    terminal: bool,
}

struct WorkflowTaskStart {
    task: Arc<WorkflowTask>,
    thread_id: ThreadId,
    config: Config,
    script: ValidatedWorkflowScript,
    args: JsonValue,
    agent_runner: AgentRunner,
    journal: Arc<FileWorkflowJournal>,
    token_budget: Option<Arc<dyn ToolTokenBudget>>,
    plugin_roots: Vec<PluginWorkflowRoot>,
}

pub(crate) struct WorkflowLaunchRequest {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: String,
    pub(crate) config: Config,
    pub(crate) resolved: ResolvedWorkflow,
    pub(crate) agent_runner: AgentRunner,
    pub(crate) token_budget: Option<Arc<dyn ToolTokenBudget>>,
    pub(crate) plugin_roots: Vec<PluginWorkflowRoot>,
}

struct HostWorkflowBudget(Arc<dyn ToolTokenBudget>);

impl WorkflowBudget for HostWorkflowBudget {
    fn total(&self) -> u64 {
        self.0.total()
    }

    fn spent(&self) -> u64 {
        self.0.spent()
    }
}

#[derive(Clone)]
pub struct WorkflowService {
    tasks: Arc<Mutex<HashMap<String, Arc<WorkflowTask>>>>,
    event_sink: Arc<dyn ExtensionEventSink>,
}

impl WorkflowService {
    pub fn new(event_sink: Arc<dyn ExtensionEventSink>) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            event_sink,
        }
    }

    pub(crate) async fn restore_thread(
        &self,
        thread_id: ThreadId,
        config: Config,
        agent_runner: AgentRunner,
        plugin_roots: Vec<PluginWorkflowRoot>,
    ) -> Result<(), WorkflowServiceError> {
        let snapshots = load_snapshots(&config.codex_home, thread_id)
            .await
            .map_err(WorkflowServiceError::Persistence)?;
        let mut retained_terminal_snapshots = 0_usize;
        for mut snapshot in snapshots {
            if snapshot.thread_id != thread_id.to_string()
                || self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&snapshot.run_id)
            {
                continue;
            }
            let active = matches!(
                snapshot.status,
                WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
            );
            if !active {
                if retained_terminal_snapshots >= MAX_RETAINED_TERMINAL_TASKS {
                    continue;
                }
                retained_terminal_snapshots += 1;
            }
            let script = if active {
                match tokio::fs::read_to_string(&snapshot.script_path).await {
                    Ok(source) if sha256(&source) == snapshot.script_sha256 => {
                        match validate_workflow_script(source) {
                            Ok(script) => Some(script),
                            Err(error) => {
                                pause_unadoptable(&mut snapshot, error.to_string());
                                None
                            }
                        }
                    }
                    Ok(_) => {
                        pause_unadoptable(
                            &mut snapshot,
                            "script content changed since it was approved; resume via the Workflow tool to re-approve"
                                .to_string(),
                        );
                        None
                    }
                    Err(error) => {
                        pause_unadoptable(
                            &mut snapshot,
                            format!("failed to read approved workflow script: {error}"),
                        );
                        None
                    }
                }
            } else {
                None
            };
            let task = Arc::new(WorkflowTask {
                snapshot: Mutex::new(snapshot.clone()),
                persist_lock: Semaphore::new(1),
                persist_state: Mutex::new(PersistState::default()),
                control: WorkflowControl::new(),
            });
            {
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tasks.insert(snapshot.run_id.clone(), Arc::clone(&task));
                prune_terminal_tasks(&mut tasks, MAX_RETAINED_TERMINAL_TASKS);
            }

            let Some(script) = script else {
                if active {
                    persist_terminal_task(&task).await;
                }
                continue;
            };
            let current_journal_path = journal_path(&snapshot.transcript_dir);
            let journal = Arc::new(
                FileWorkflowJournal::open(
                    current_journal_path.clone(),
                    Some(&current_journal_path),
                )
                .await
                .map_err(WorkflowServiceError::Persistence)?,
            );
            self.emit_started(&snapshot, thread_id);
            self.emit_progress_snapshot(&snapshot, thread_id);
            self.start_task(WorkflowTaskStart {
                task,
                thread_id,
                config: config.clone(),
                script,
                args: snapshot.args,
                agent_runner: agent_runner.clone(),
                journal,
                token_budget: None,
                plugin_roots: plugin_roots.clone(),
            });
        }
        Ok(())
    }

    pub(crate) async fn launch(
        &self,
        request: WorkflowLaunchRequest,
    ) -> Result<WorkflowLaunch, WorkflowServiceError> {
        let WorkflowLaunchRequest {
            thread_id,
            turn_id,
            config,
            resolved,
            agent_runner,
            token_budget,
            plugin_roots,
        } = request;
        let script_sha256 = sha256(&resolved.script.source);
        let resume_run_id = resolved.resume_from_run_id.clone();
        let resume_snapshot = resume_run_id
            .as_deref()
            .map(|run_id| self.validate_resume(thread_id, run_id))
            .transpose()?;

        let run_id =
            resume_run_id.unwrap_or_else(|| format!("wf_{}", uuid::Uuid::new_v4().simple()));
        let task_id = format!("w{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let session_dir = workflow_session_dir(&config.codex_home, thread_id);
        let transcript_dir = session_dir
            .join("subagents/workflows")
            .join(run_id.as_str());
        let scripts_dir = session_dir.join("workflows/scripts");
        let snapshots_dir = session_dir.join("workflows");
        tokio::fs::create_dir_all(&transcript_dir)
            .await
            .map_err(persistence_error)?;
        tokio::fs::create_dir_all(&scripts_dir)
            .await
            .map_err(persistence_error)?;
        tokio::fs::create_dir_all(&snapshots_dir)
            .await
            .map_err(persistence_error)?;
        let slug = slugify(&resolved.script.meta.name);
        let script_path = scripts_dir.join(format!("{slug}-{run_id}.js"));
        tokio::fs::write(&script_path, resolved.script.source.as_bytes())
            .await
            .map_err(persistence_error)?;
        let output_file = snapshots_dir.join(format!("{run_id}.json"));
        let summary = format!("Running workflow {}", resolved.script.meta.name);
        let started_at = unix_seconds();
        let snapshot = WorkflowTaskSnapshot {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.clone(),
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            workflow_name: resolved.script.meta.name.clone(),
            title: resolved.script.meta.title.clone(),
            status: WorkflowTaskStatus::Running,
            summary: summary.clone(),
            transcript_dir: transcript_dir.clone(),
            script_path: script_path.clone(),
            args: resolved.args.clone(),
            result: JsonValue::Null,
            output_file: output_file.clone(),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at,
            completed_at: None,
            script_sha256,
        };
        let snapshot_file = snapshot_path(&snapshot).map_err(WorkflowServiceError::Persistence)?;
        write_json(snapshot_file, &snapshot)
            .await
            .map_err(WorkflowServiceError::Persistence)?;
        let replay_path = resume_snapshot
            .as_ref()
            .map(|snapshot| journal_path(&snapshot.transcript_dir));
        let journal = Arc::new(
            FileWorkflowJournal::open(
                journal_path(&snapshot.transcript_dir),
                replay_path.as_deref(),
            )
            .await
            .map_err(WorkflowServiceError::Persistence)?,
        );
        let task = Arc::new(WorkflowTask {
            snapshot: Mutex::new(snapshot.clone()),
            persist_lock: Semaphore::new(1),
            persist_state: Mutex::new(PersistState::default()),
            control: WorkflowControl::new(),
        });
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tasks.insert(run_id.clone(), Arc::clone(&task));
            prune_terminal_tasks(&mut tasks, MAX_RETAINED_TERMINAL_TASKS);
        }
        self.emit_started(&snapshot, thread_id);
        let workflow_name = resolved.script.meta.name.clone();
        self.start_task(WorkflowTaskStart {
            task,
            thread_id,
            config,
            script: resolved.script,
            args: resolved.args,
            agent_runner,
            journal,
            token_budget,
            plugin_roots,
        });

        Ok(WorkflowLaunch {
            status: "async_launched".to_string(),
            task_id,
            task_type: "local_workflow".to_string(),
            workflow_name,
            run_id,
            summary,
            transcript_dir: transcript_dir.display().to_string(),
            script_path: script_path.display().to_string(),
        })
    }

    fn start_task(&self, start: WorkflowTaskStart) {
        let WorkflowTaskStart {
            task,
            thread_id,
            config,
            script,
            args,
            agent_runner,
            journal,
            token_budget,
            plugin_roots,
        } = start;
        let service = self.clone();
        tokio::spawn(async move {
            let run_id = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .run_id
                .clone();
            let child_resolver = Arc::new(SavedWorkflowChildResolver::new(
                config.cwd.clone(),
                config.codex_home.clone(),
                plugin_roots,
            ));
            let fallback_token_budget = config
                .rollout_budget
                .as_ref()
                .and_then(|budget| u64::try_from(budget.limit_tokens).ok());
            let max_child_sessions = config.workflow_max_child_sessions;
            let agent_runtime = Arc::new(CodexWorkflowAgentRuntime::new(
                agent_runner,
                thread_id,
                config,
                run_id,
            ));
            let event_task = Arc::clone(&task);
            let event_service = service.clone();
            let event_sink = Arc::new(move |event: WorkflowEvent| {
                event_service.record_progress(&event_task, thread_id, event);
            });
            let result = execute_workflow(
                &script,
                args,
                Arc::clone(&agent_runtime) as Arc<dyn codex_workflow::WorkflowAgentRuntime>,
                event_sink,
                WorkflowRuntimeConfig {
                    max_child_sessions,
                    budget: token_budget
                        .map(|budget| {
                            WorkflowBudgetSource::Shared(
                                Arc::new(HostWorkflowBudget(budget)) as Arc<dyn WorkflowBudget>
                            )
                        })
                        .or_else(|| fallback_token_budget.map(WorkflowBudgetSource::Fixed)),
                    child_resolver: Some(child_resolver),
                    journal: Some(journal),
                    ..WorkflowRuntimeConfig::default()
                },
                task.control.clone(),
            )
            .await;
            let cleanup_mode = if result.is_ok() {
                WorktreeCleanupMode::Completed
            } else {
                WorktreeCleanupMode::Interrupted
            };
            for message in agent_runtime.cleanup_worktrees(cleanup_mode).await {
                service.record_progress(&task, thread_id, WorkflowEvent::WorkflowLog { message });
            }
            service.finish_task(task, thread_id, result).await;
        });
    }

    pub fn list(&self, thread_id: ThreadId) -> Vec<WorkflowTaskSnapshot> {
        let mut snapshots = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|task| {
                let snapshot = task
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (snapshot.thread_id == thread_id.to_string()).then(|| snapshot.clone())
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.started_at));
        snapshots
    }

    pub fn stop(&self, thread_id: ThreadId, run_id: &str) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id)?;
        let active = {
            let snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            matches!(
                snapshot.status,
                WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
            )
        };
        if !active {
            return Ok(false);
        }
        task.control.stop();
        Ok(true)
    }

    pub fn skip_agent(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        agent_index: usize,
    ) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id)?;
        Ok(task.control.skip_agent(agent_index))
    }

    pub fn retry_agent(
        &self,
        thread_id: ThreadId,
        run_id: &str,
        agent_index: usize,
    ) -> Result<bool, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id)?;
        Ok(task.control.retry_agent(agent_index))
    }

    fn validate_resume(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<WorkflowTaskSnapshot, WorkflowServiceError> {
        let task = self.task_for_thread(thread_id, run_id)?;
        let snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            snapshot.status,
            WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
        ) {
            return Err(WorkflowServiceError::StillRunning);
        }
        Ok(snapshot.clone())
    }

    fn task_for_thread(
        &self,
        thread_id: ThreadId,
        run_id: &str,
    ) -> Result<Arc<WorkflowTask>, WorkflowServiceError> {
        let task = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(run_id)
            .cloned()
            .ok_or(WorkflowServiceError::NotFound)?;
        let owner = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_id
            .clone();
        if owner != thread_id.to_string() {
            return Err(WorkflowServiceError::WrongThread);
        }
        Ok(task)
    }

    fn record_progress(&self, task: &Arc<WorkflowTask>, thread_id: ThreadId, event: WorkflowEvent) {
        let progress_event = {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            upsert_progress(&mut snapshot.progress, event);
            snapshot.progress_version = snapshot.progress_version.saturating_add(1);
            update_usage_from_progress(&mut snapshot);

            WorkflowProgressEvent {
                thread_id,
                turn_id: snapshot.turn_id.clone(),
                task_id: snapshot.task_id.clone(),
                run_id: snapshot.run_id.clone(),
                progress: snapshot.progress.clone(),
                usage: snapshot.usage.clone(),
            }
        };
        persist_task_background(Arc::clone(task));
        self.event_sink.emit(Event {
            id: progress_event.turn_id.clone(),
            msg: EventMsg::WorkflowProgress(progress_event),
        });
    }

    async fn finish_task(
        &self,
        task: Arc<WorkflowTask>,
        thread_id: ThreadId,
        result: Result<WorkflowRunOutcome, WorkflowExecutionError>,
    ) {
        let completed_event = {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let completed_at = unix_seconds();
            snapshot.completed_at = Some(completed_at);
            let result_value = match result {
                Ok(outcome) => {
                    snapshot.status = WorkflowTaskStatus::Completed;
                    snapshot.summary = format!("Workflow {} completed", snapshot.workflow_name);
                    snapshot.failures = outcome.failures;
                    snapshot.usage = WorkflowUsage {
                        total_tokens: outcome.total_tokens,
                        tool_uses: outcome.total_tool_calls,
                        duration_ms: outcome.duration_ms,
                        agent_count: outcome.agent_count,
                    };
                    outcome.result
                }
                Err(WorkflowExecutionError::Cancelled) => {
                    snapshot.status = WorkflowTaskStatus::Killed;
                    snapshot.summary = format!("Workflow {} stopped", snapshot.workflow_name);
                    snapshot.failures = failures_from_progress(&snapshot.progress);
                    JsonValue::Null
                }
                Err(error) => {
                    snapshot.status = WorkflowTaskStatus::Failed;
                    snapshot.summary = format!("Workflow {} failed", snapshot.workflow_name);
                    snapshot.error = Some(error.to_string());
                    snapshot.failures = failures_from_progress(&snapshot.progress);
                    JsonValue::Null
                }
            };
            if snapshot.usage.duration_ms == 0 {
                snapshot.usage.duration_ms =
                    u64::try_from(completed_at.saturating_sub(snapshot.started_at))
                        .unwrap_or(0)
                        .saturating_mul(1_000);
            }
            snapshot.result = result_value;
            WorkflowCompletedEvent {
                thread_id,
                turn_id: snapshot.turn_id.clone(),
                task_id: snapshot.task_id.clone(),
                run_id: snapshot.run_id.clone(),
                workflow_name: snapshot.workflow_name.clone(),
                status: snapshot.status,
                summary: snapshot.summary.clone(),
                output_file: snapshot.output_file.clone(),
                error: snapshot.error.clone(),
                failures: snapshot.failures.clone(),
                usage: snapshot.usage.clone(),
                completed_at,
            }
        };
        persist_terminal_task(&task).await;
        self.event_sink.emit(Event {
            id: completed_event.turn_id.clone(),
            msg: EventMsg::WorkflowCompleted(completed_event),
        });
        prune_terminal_tasks(
            &mut self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            MAX_RETAINED_TERMINAL_TASKS,
        );
    }

    fn emit_started(&self, snapshot: &WorkflowTaskSnapshot, thread_id: ThreadId) {
        let event = WorkflowStartedEvent {
            thread_id,
            turn_id: snapshot.turn_id.clone(),
            task_id: snapshot.task_id.clone(),
            run_id: snapshot.run_id.clone(),
            workflow_name: snapshot.workflow_name.clone(),
            title: snapshot.title.clone(),
            summary: snapshot.summary.clone(),
            transcript_dir: snapshot.transcript_dir.clone(),
            script_path: snapshot.script_path.clone(),
            started_at: snapshot.started_at,
        };
        self.event_sink.emit(Event {
            id: event.turn_id.clone(),
            msg: EventMsg::WorkflowStarted(event),
        });
    }

    fn emit_progress_snapshot(&self, snapshot: &WorkflowTaskSnapshot, thread_id: ThreadId) {
        let event = WorkflowProgressEvent {
            thread_id,
            turn_id: snapshot.turn_id.clone(),
            task_id: snapshot.task_id.clone(),
            run_id: snapshot.run_id.clone(),
            progress: snapshot.progress.clone(),
            usage: snapshot.usage.clone(),
        };
        self.event_sink.emit(Event {
            id: event.turn_id.clone(),
            msg: EventMsg::WorkflowProgress(event),
        });
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
