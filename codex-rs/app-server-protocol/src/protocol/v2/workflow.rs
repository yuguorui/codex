use crate::JsonSchema;
use crate::TS;
pub use codex_protocol::workflow::WorkflowAgentProgress;
pub use codex_protocol::workflow::WorkflowAgentState;
pub use codex_protocol::workflow::WorkflowIsolation;
pub use codex_protocol::workflow::WorkflowProgressKind;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Paused,
    Killed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
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

impl From<codex_protocol::workflow::WorkflowProgressItem> for WorkflowProgressItem {
    fn from(item: codex_protocol::workflow::WorkflowProgressItem) -> Self {
        match item {
            codex_protocol::workflow::WorkflowProgressItem::WorkflowPhase {
                index,
                title,
                kind,
            } => Self::WorkflowPhase { index, title, kind },
            codex_protocol::workflow::WorkflowProgressItem::WorkflowAgent(agent) => {
                Self::WorkflowAgent(agent)
            }
            codex_protocol::workflow::WorkflowProgressItem::WorkflowLog { message } => {
                Self::WorkflowLog { message }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
    pub agent_count: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowTask {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub title: Option<String>,
    pub status: WorkflowStatus,
    pub summary: String,
    pub transcript_dir: LegacyAppPathString,
    pub script_path: LegacyAppPathString,
    /// Path to the persisted run snapshot, including the terminal result.
    pub output_file: LegacyAppPathString,
    pub progress: Vec<WorkflowProgressItem>,
    pub progress_version: u64,
    pub usage: WorkflowUsage,
    pub failures: Vec<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListResponse {
    pub data: Vec<WorkflowTask>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStopParams {
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStopResponse {
    pub accepted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentControlParams {
    pub thread_id: String,
    pub run_id: String,
    pub agent_index: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentSkipResponse {
    pub accepted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentRetryResponse {
    pub accepted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStartedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub title: Option<String>,
    pub summary: String,
    pub transcript_dir: LegacyAppPathString,
    pub script_path: LegacyAppPathString,
    pub started_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowProgressNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub progress: Vec<WorkflowProgressItem>,
    pub usage: WorkflowUsage,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowCompletedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub status: WorkflowStatus,
    pub summary: String,
    /// Path to the persisted run snapshot, including the terminal result.
    pub output_file: LegacyAppPathString,
    pub error: Option<String>,
    pub failures: Vec<String>,
    pub usage: WorkflowUsage,
    pub completed_at: i64,
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
