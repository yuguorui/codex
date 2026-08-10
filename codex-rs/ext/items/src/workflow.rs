use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Display item emitted while a Workflow agent analyzes its injected inputs.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct WorkflowInputAnalysisItem {
    pub id: String,
}

/// Display item emitted while the owning model reads a Workflow result.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct WorkflowResultReadItem {
    pub id: String,
    pub run_id: Option<String>,
    pub status: WorkflowResultReadStatus,
}

/// Lifecycle state of a Workflow result read displayed by the host.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum WorkflowResultReadStatus {
    InProgress,
    Completed,
    Failed,
}
