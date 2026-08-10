#![recursion_limit = "256"]

//! Extension-backed dynamic workflows for Codex.

mod agent;
mod analyze_inputs;
mod bundled;
mod composition;
mod control_tool;
mod declared_inputs;
mod discovery;
mod extension;
mod input_artifacts;
mod journal;
mod persistence;
mod result_artifact;
mod service;
mod spec;
mod tool;
mod wait_tool;
mod workflow_recovery;
mod workflow_result_projection;
mod workflow_result_tool;
mod workflow_result_write;
mod workflow_status_tool;

pub use control_tool::RETRY_WORKFLOW_AGENT_TOOL_NAME;
pub use control_tool::SKIP_WORKFLOW_AGENT_TOOL_NAME;
pub use control_tool::STOP_WORKFLOW_TOOL_NAME;
pub use extension::install;
pub use result_artifact::WorkflowResultArtifact;
pub use service::WorkflowLaunch;
pub use service::WorkflowService;
pub use service::WorkflowServiceError;
pub use service::WorkflowTaskSnapshot;
pub use spec::WORKFLOW_TOOL_NAME;
pub use tool::WorkflowApprovalArtifactData;
pub use tool::read_workflow_approval_artifact;
pub use tool::workflow_approval_artifact_reference;
pub use wait_tool::WAIT_WORKFLOW_TOOL_NAME;
pub use workflow_result_tool::READ_WORKFLOW_RESULT_TOOL_NAME;
pub use workflow_status_tool::LIST_WORKFLOW_AGENTS_TOOL_NAME;
pub use workflow_status_tool::LIST_WORKFLOWS_TOOL_NAME;
pub use workflow_status_tool::WAIT_WORKFLOWS_TOOL_NAME;
