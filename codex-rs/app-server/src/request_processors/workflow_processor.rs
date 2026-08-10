use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentRetryResponse;
use codex_app_server_protocol::WorkflowAgentSkipResponse;
use codex_app_server_protocol::WorkflowApprovalArtifactReadParams;
use codex_app_server_protocol::WorkflowApprovalArtifactReadResponse;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow_extension::WorkflowService;
use codex_workflow_extension::WorkflowServiceError;
use codex_workflow_extension::read_workflow_approval_artifact;
use serde::Deserialize;
use serde::Serialize;

use crate::error_code::invalid_request;

#[derive(Clone)]
pub(crate) struct WorkflowRequestProcessor {
    service: WorkflowService,
    codex_home: AbsolutePathBuf,
}

impl WorkflowRequestProcessor {
    pub(crate) fn new(service: WorkflowService, codex_home: AbsolutePathBuf) -> Self {
        Self {
            service,
            codex_home,
        }
    }

    pub(crate) async fn read_approval_artifact(
        &self,
        params: WorkflowApprovalArtifactReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let artifact = read_workflow_approval_artifact(
            &self.codex_home,
            thread_id,
            &params.artifact_id,
            params.offset.unwrap_or(0),
        )
        .await
        .map_err(invalid_request)?;
        Ok(Some(
            WorkflowApprovalArtifactReadResponse {
                sha256: artifact.sha256,
                offset: artifact.offset,
                contents: artifact.contents,
                next_offset: artifact.next_offset,
            }
            .into(),
        ))
    }

    pub(crate) async fn list(
        &self,
        params: WorkflowListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let tasks = self.service.list(thread_id).await.map_err(service_error)?;
        let cursor = params
            .cursor
            .as_deref()
            .map(serde_json::from_str::<WorkflowListCursor>)
            .transpose()
            .map_err(|_| invalid_request("invalid workflow list cursor"))?;
        let limit = usize::try_from(params.limit.unwrap_or(50).clamp(1, 100)).unwrap_or(100);
        let matching = tasks
            .iter()
            .filter(|snapshot| {
                cursor.as_ref().is_none_or(|cursor| {
                    snapshot.started_at < cursor.started_at
                        || (snapshot.started_at == cursor.started_at
                            && snapshot.run_id < cursor.run_id)
                })
            })
            .collect::<Vec<_>>();
        let page = matching.iter().take(limit).copied().collect::<Vec<_>>();
        let data = page
            .iter()
            .copied()
            .cloned()
            .map(crate::workflow_events::task)
            .collect::<Vec<_>>();
        let next_cursor = (page.len() < matching.len())
            .then(|| page.last().copied())
            .flatten()
            .map(|snapshot| {
                serde_json::to_string(&WorkflowListCursor {
                    started_at: snapshot.started_at,
                    run_id: snapshot.run_id.clone(),
                })
                .map_err(|error| invalid_request(error.to_string()))
            })
            .transpose()?;
        Ok(Some(WorkflowListResponse { data, next_cursor }.into()))
    }

    pub(crate) async fn stop(
        &self,
        params: WorkflowStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .stop(thread_id, &params.run_id)
            .await
            .map_err(service_error)?;
        Ok(Some(WorkflowStopResponse { accepted }.into()))
    }

    pub(crate) async fn skip_agent(
        &self,
        params: WorkflowAgentControlParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .skip_agent(thread_id, &params.run_id, params.agent_index)
            .await
            .map_err(service_error)?;
        Ok(Some(WorkflowAgentSkipResponse { accepted }.into()))
    }

    pub(crate) async fn retry_agent(
        &self,
        params: WorkflowAgentControlParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let accepted = self
            .service
            .retry_agent(thread_id, &params.run_id, params.agent_index)
            .await
            .map_err(service_error)?;
        Ok(Some(WorkflowAgentRetryResponse { accepted }.into()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowListCursor {
    started_at: i64,
    run_id: String,
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
