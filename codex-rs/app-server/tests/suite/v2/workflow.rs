use anyhow::Context;
use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentRetryResponse;
use codex_app_server_protocol::WorkflowAgentSkipResponse;
use codex_app_server_protocol::WorkflowAgentState;
use codex_app_server_protocol::WorkflowApprovalArtifactReadParams;
use codex_app_server_protocol::WorkflowApprovalArtifactReadResponse;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowProgressItem;
use codex_app_server_protocol::WorkflowProgressNotification;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_core::find_thread_path_by_id_str;
use codex_core::test_support::all_model_presets;
use codex_features::Feature;
use core_test_support::responses;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Match;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::read_jsonrpc_message;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(25);

const WORKFLOW_CALL_ID: &str = "workflow-call-1";
const WAIT_WORKFLOW_CALL_ID: &str = "wait-workflow-call-1";
const WORKFLOW_AGENT_PROMPT: &str = "Inspect Agent protocol compatibility";
const ANALYZE_WORKFLOW_INPUTS_TOOL_NAME: &str = "AnalyzeWorkflowInputs";
const OWNING_WORKFLOW_TOOL_NAMES: [&str; 8] = [
    "Workflow",
    "WaitWorkflow",
    "ListWorkflows",
    "WaitWorkflows",
    "ReadWorkflowResult",
    "StopWorkflow",
    "RetryWorkflowAgent",
    "SkipWorkflowAgent",
];

const MULTI_WORKFLOW_FIRST_CALL_ID: &str = "multi-workflow-first";
const MULTI_WORKFLOW_SECOND_CALL_ID: &str = "multi-workflow-second";
const MULTI_WORKFLOW_LIST_CALL_ID: &str = "multi-workflow-list";
const MULTI_WORKFLOW_WAIT_ANY_CALL_ID: &str = "multi-workflow-wait-any";
const MULTI_WORKFLOW_READ_FIRST_CALL_ID: &str = "multi-workflow-read-first";
const MULTI_WORKFLOW_STOP_SECOND_CALL_ID: &str = "multi-workflow-stop-second";
const MULTI_WORKFLOW_WAIT_ALL_CALL_ID: &str = "multi-workflow-wait-all";
const MULTI_WORKFLOW_WAIT_FIRST_CALL_ID: &str = "multi-workflow-wait-first";
const MULTI_WORKFLOW_WAIT_FIRST_AGAIN_CALL_ID: &str = "multi-workflow-wait-first-again";
const STEER_SECOND_WORKFLOW_CALL_ID: &str = "workflow-steer-second";
const STEER_WAIT_WORKFLOWS_CALL_ID: &str = "workflow-steer-wait-all";
const STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID: &str = "workflow-steer-wait-all-again";

#[derive(Clone, Copy)]
enum ParentAgentProtocol {
    V1,
    V2,
}

mod agent_execution;
mod agent_runtime;
mod approvals_isolation;
mod controls_ownership;
mod fixture;
mod inputs;
mod lifecycle;
mod notification_routing;
mod responders;
mod steering_waits;
mod support;
mod waits;

use fixture::*;
use responders::*;
use support::*;
