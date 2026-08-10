use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

pub use codex_protocol::workflow::WorkflowAgentActivity;
pub use codex_protocol::workflow::WorkflowAgentProgress;
pub use codex_protocol::workflow::WorkflowAgentState;
pub use codex_protocol::workflow::WorkflowIsolation;
pub use codex_protocol::workflow::WorkflowProgressItem as WorkflowEvent;
pub use codex_protocol::workflow::WorkflowProgressKind;

pub const MAX_WORKFLOW_PROGRESS_TEXT_BYTES: usize = 256;
pub const MAX_WORKFLOW_AGENT_STALL_MS: u64 = 30 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub phases: Vec<WorkflowPhase>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowPhase {
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowAgentOptions {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub schema: Option<JsonValue>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<WorkflowEffort>,
    #[serde(default)]
    pub isolation: Option<WorkflowIsolation>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub stall_ms: Option<u64>,
    #[serde(default)]
    pub inputs: Option<BTreeMap<String, crate::WorkflowInputArtifactRef>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowAgentRequest {
    pub index: usize,
    pub invocation_id: String,
    pub prompt: String,
    pub options: WorkflowAgentOptions,
    #[serde(skip)]
    pub inputs: Option<crate::WorkflowAgentInputs>,
    pub attempt: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTokenUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowAgentProgressUpdate {
    pub usage: WorkflowTokenUsage,
    pub activity: Option<WorkflowAgentActivity>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentResult {
    pub value: JsonValue,
    #[serde(default)]
    pub usage: WorkflowTokenUsage,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback_model: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowAgentFailureKind {
    Failed,
    TerminalApi,
    Cancelled,
    Stalled,
    Throttled,
    Blocked,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum WorkflowAgentOutcome {
    Success,
    Failure {
        kind: WorkflowAgentFailureKind,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowJournalResult {
    pub result: WorkflowAgentResult,
    pub outcome: WorkflowAgentOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowAgentFailure {
    pub kind: WorkflowAgentFailureKind,
    pub message: String,
    pub usage: WorkflowTokenUsage,
}

impl WorkflowAgentFailure {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: WorkflowAgentFailureKind::Failed,
            message: message.into(),
            usage: WorkflowTokenUsage::default(),
        }
    }

    pub fn with_usage(mut self, usage: WorkflowTokenUsage) -> Self {
        self.usage = usage;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkflowRunOutcome {
    pub result: JsonValue,
    pub agent_count: usize,
    pub logs: Vec<String>,
    pub failures: Vec<String>,
    pub total_tokens: u64,
    pub total_tool_calls: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkflowExecutionError {
    #[error("workflow was cancelled")]
    Cancelled,
    #[error("workflow rerun was requested")]
    RerunRequested,
    #[error("workflow runtime failed: {0}")]
    Runtime(String),
}
