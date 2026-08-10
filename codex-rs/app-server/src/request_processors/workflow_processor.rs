use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentRetryResponse;
use codex_app_server_protocol::WorkflowAgentSkipResponse;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_protocol::ThreadId;
use codex_workflow_extension::WorkflowService;
use codex_workflow_extension::WorkflowServiceError;

use crate::error_code::invalid_request;

#[derive(Clone)]
pub(crate) struct WorkflowRequestProcessor {
    service: WorkflowService,
}

impl WorkflowRequestProcessor {
    pub(crate) fn new(service: WorkflowService) -> Self {
        Self { service }
    }

    pub(crate) fn list(
        &self,
        params: WorkflowListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let tasks = self.service.list(thread_id);
        let offset = params
            .cursor
            .as_deref()
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|_| invalid_request("invalid workflow list cursor"))?
            .unwrap_or(0);
        let limit = usize::try_from(params.limit.unwrap_or(50).clamp(1, 100)).unwrap_or(100);
        let data = tasks
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .map(crate::workflow_events::task)
            .collect::<Vec<_>>();
        let next_offset = offset.saturating_add(data.len());
        let next_cursor = (next_offset < tasks.len()).then(|| next_offset.to_string());
        Ok(Some(WorkflowListResponse { data, next_cursor }.into()))
    }

    pub(crate) fn stop(
        &self,
        params: WorkflowStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .stop(thread_id, &params.run_id)
            .map_err(service_error)?;
        Ok(Some(WorkflowStopResponse { accepted }.into()))
    }

    pub(crate) fn skip_agent(
        &self,
        params: WorkflowAgentControlParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .skip_agent(thread_id, &params.run_id, params.agent_index)
            .map_err(service_error)?;
        Ok(Some(WorkflowAgentSkipResponse { accepted }.into()))
    }

    pub(crate) fn retry_agent(
        &self,
        params: WorkflowAgentControlParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .retry_agent(thread_id, &params.run_id, params.agent_index)
            .map_err(service_error)?;
        Ok(Some(WorkflowAgentRetryResponse { accepted }.into()))
    }
}

fn parse_thread_id(value: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value).map_err(|error| invalid_request(error.to_string()))
}

fn service_error(error: WorkflowServiceError) -> JSONRPCErrorError {
    match error {
        WorkflowServiceError::NotFound
        | WorkflowServiceError::WrongThread
        | WorkflowServiceError::StillRunning
        | WorkflowServiceError::Persistence(_) => invalid_request(error.to_string()),
    }
}
