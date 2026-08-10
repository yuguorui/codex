use crate::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "protocol/")]
pub enum WorkflowTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Paused,
    Killed,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum WorkflowProgressKind {
    Declared,
    Active,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum WorkflowAgentState {
    Queued,
    Start,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum WorkflowAgentActivity {
    AnalyzingInputs,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum WorkflowIsolation {
    Worktree,
    Remote,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct WorkflowAgentProgress {
    /// Stable invocation identity within one workflow definition.
    pub invocation_id: String,
    pub index: usize,
    pub label: String,
    pub phase_index: Option<usize>,
    pub phase_title: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub isolation: Option<WorkflowIsolation>,
    pub state: WorkflowAgentState,
    pub activity: Option<WorkflowAgentActivity>,
    pub blocked: bool,
    pub skipped: bool,
    /// True while the workflow waits for the user to retry or skip this agent.
    #[serde(default)]
    pub awaiting_decision: bool,
    pub cached: bool,
    pub attempt: u32,
    pub error: Option<String>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub duration_ms: Option<u64>,
    pub result_preview: Option<String>,
    pub prompt_preview: String,
    /// Unix timestamp in seconds when the call entered the workflow queue.
    pub queued_at: u64,
    /// Unix timestamp in seconds when agent execution began.
    pub started_at: Option<u64>,
    /// Unix timestamp in seconds when this progress item was last updated.
    pub last_progress_at: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", export_to = "protocol/")]
pub enum WorkflowProgressItem {
    WorkflowPhase {
        index: usize,
        title: String,
        kind: WorkflowProgressKind,
    },
    WorkflowAgent(Box<WorkflowAgentProgress>),
    WorkflowLog {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct WorkflowUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
    pub agent_count: usize,
    #[serde(default)]
    pub successful_agent_count: usize,
    #[serde(default)]
    pub failed_agent_count: usize,
    #[serde(default)]
    pub skipped_agent_count: usize,
    #[serde(default)]
    pub null_agent_result_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct WorkflowStartedEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub title: Option<String>,
    pub summary: String,
    pub transcript_dir: AbsolutePathBuf,
    pub script_path: AbsolutePathBuf,
    pub started_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct WorkflowProgressEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub progress: Vec<WorkflowProgressItem>,
    pub usage: WorkflowUsage,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct WorkflowCompletedEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub status: WorkflowTaskStatus,
    pub summary: String,
    /// Path to the persisted run snapshot, including the terminal result.
    pub output_file: AbsolutePathBuf,
    pub error: Option<String>,
    pub failures: Vec<String>,
    pub usage: WorkflowUsage,
    pub completed_at: i64,
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
