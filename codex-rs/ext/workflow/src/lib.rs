#![recursion_limit = "256"]

//! Extension-backed dynamic workflows for Codex.

mod agent;
mod bundled;
mod discovery;
mod extension;
mod journal;
mod persistence;
mod service;
mod spec;
mod tool;

pub use extension::install;
pub use service::WorkflowLaunch;
pub use service::WorkflowService;
pub use service::WorkflowServiceError;
pub use service::WorkflowTaskSnapshot;
pub use spec::RUN_WORKFLOW_TOOL_ALIAS;
pub use spec::WORKFLOW_TOOL_NAME;
