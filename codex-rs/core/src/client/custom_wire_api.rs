//! HTTP streaming paths for non-Responses wire APIs.

use super::*;
use tracing::instrument;

#[allow(clippy::too_many_arguments)]
#[instrument(
    name = "model_client.stream_chat_api",
    level = "info",
    skip_all,
    fields(
        model = %model_info.slug,
        wire_api = %session.client.state.provider.info().wire_api,
        transport = "chat_http",
        http.method = "POST",
        api.path = "chat/completions",
        turn.has_metadata_header = responses_metadata.has_turn_metadata()
    )
)]
pub(super) async fn stream_chat_api(
    session: &ModelClientSession,
    prompt: &Prompt,
    model_info: &ModelInfo,
    session_telemetry: &SessionTelemetry,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    service_tier: Option<String>,
    responses_metadata: &CodexResponsesMetadata,
    inference_trace: &InferenceTraceContext,
    retry_notifier: Option<RequestRetryNotifier>,
) -> Result<ResponseStream> {
    let auth_manager = session.client.state.provider.auth_manager();
    let mut auth_recovery = auth_manager
        .as_ref()
        .map(AuthManager::unauthorized_recovery);
    let mut pending_retry = PendingUnauthorizedRetry::default();
    loop {
        let client_setup = session.client.current_client_setup().await?;
        let transport =
            ReqwestTransport::from_http_client(codex_login::default_client::create_client());
        let request_auth_context = AuthRequestTelemetryContext::new(
            client_setup.auth.as_ref().map(CodexAuth::auth_mode),
            client_setup.api_auth.as_ref(),
            client_setup.agent_identity_telemetry.clone(),
            pending_retry,
        );
        let (request_telemetry, sse_telemetry) = ModelClientSession::build_streaming_telemetry(
            session_telemetry,
            request_auth_context,
            RequestRouteTelemetry::for_endpoint("/chat/completions"),
            session.client.state.auth_env_telemetry.clone(),
            retry_notifier.clone(),
        );
        let compression = session.responses_request_compression(client_setup.auth.as_ref());
        let responses_options = session
            .build_responses_options(
                responses_metadata,
                compression,
                model_info.use_responses_lite,
            )
            .await;

        let request = session.client.build_responses_request(
            &client_setup.api_provider,
            prompt,
            model_info,
            effort.clone(),
            summary,
            service_tier.clone(),
            responses_metadata,
        )?;
        let inference_trace_attempt = inference_trace.start_attempt();
        let mut options = ApiChatOptions {
            session_id: responses_options.session_id,
            thread_id: responses_options.thread_id,
            session_source: responses_options.session_source,
            extra_headers: responses_options.extra_headers,
            compression: responses_options.compression,
            turn_state: responses_options.turn_state,
        };
        inference_trace_attempt.add_request_headers(&mut options.extra_headers);
        inference_trace_attempt.record_started(&request);
        let client =
            ApiChatClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry), Some(sse_telemetry));
        let stream_result = client.stream_request(request, options).await;

        match stream_result {
            Ok(stream) => {
                let (stream, _) = map_response_stream(
                    stream,
                    session_telemetry.clone(),
                    inference_trace_attempt,
                    session.client.state.provider.clone(),
                );
                return Ok(stream);
            }
            Err(ApiError::Transport(
                unauthorized_transport @ TransportError::Http { status, .. },
            )) if status == StatusCode::UNAUTHORIZED => {
                let response_debug_context =
                    extract_response_debug_context(&unauthorized_transport);
                inference_trace_attempt.record_failed(
                    &unauthorized_transport,
                    response_debug_context.request_id.as_deref(),
                    /*output_items*/ &[],
                );
                pending_retry = PendingUnauthorizedRetry::from_recovery(
                    handle_unauthorized(
                        unauthorized_transport,
                        &mut auth_recovery,
                        session_telemetry,
                        &session.client.state.provider,
                    )
                    .await?,
                );
            }
            Err(err) => {
                let response_debug_context = extract_response_debug_context_from_api_error(&err);
                let err = session.client.state.provider.map_api_error(err);
                inference_trace_attempt.record_failed(
                    &err,
                    response_debug_context.request_id.as_deref(),
                    /*output_items*/ &[],
                );
                return Err(err);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[instrument(
    name = "model_client.stream_anthropic_api",
    level = "info",
    skip_all,
    fields(
        model = %model_info.slug,
        wire_api = %session.client.state.provider.info().wire_api,
        transport = "anthropic_http",
        http.method = "POST",
        api.path = "messages",
        turn.has_metadata_header = responses_metadata.has_turn_metadata()
    )
)]
pub(super) async fn stream_anthropic_api(
    session: &ModelClientSession,
    prompt: &Prompt,
    model_info: &ModelInfo,
    session_telemetry: &SessionTelemetry,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    service_tier: Option<String>,
    responses_metadata: &CodexResponsesMetadata,
    inference_trace: &InferenceTraceContext,
    retry_notifier: Option<RequestRetryNotifier>,
) -> Result<ResponseStream> {
    let auth_manager = session.client.state.provider.auth_manager();
    let mut auth_recovery = auth_manager
        .as_ref()
        .map(AuthManager::unauthorized_recovery);
    let mut pending_retry = PendingUnauthorizedRetry::default();
    loop {
        let client_setup = session.client.current_client_setup().await?;
        let transport =
            ReqwestTransport::from_http_client(codex_login::default_client::create_client());
        let request_auth_context = AuthRequestTelemetryContext::new(
            client_setup.auth.as_ref().map(CodexAuth::auth_mode),
            client_setup.api_auth.as_ref(),
            client_setup.agent_identity_telemetry.clone(),
            pending_retry,
        );
        let (request_telemetry, sse_telemetry) = ModelClientSession::build_streaming_telemetry(
            session_telemetry,
            request_auth_context,
            RequestRouteTelemetry::for_endpoint("/messages"),
            session.client.state.auth_env_telemetry.clone(),
            retry_notifier.clone(),
        );
        let compression = session.responses_request_compression(client_setup.auth.as_ref());
        let responses_options = session
            .build_responses_options(
                responses_metadata,
                compression,
                model_info.use_responses_lite,
            )
            .await;

        let request = session.client.build_responses_request(
            &client_setup.api_provider,
            prompt,
            model_info,
            effort.clone(),
            summary,
            service_tier.clone(),
            responses_metadata,
        )?;
        let inference_trace_attempt = inference_trace.start_attempt();
        let mut options = ApiAnthropicOptions {
            session_id: responses_options.session_id,
            thread_id: responses_options.thread_id,
            session_source: responses_options.session_source,
            extra_headers: responses_options.extra_headers,
            compression: responses_options.compression,
            turn_state: responses_options.turn_state,
        };
        inference_trace_attempt.add_request_headers(&mut options.extra_headers);
        inference_trace_attempt.record_started(&request);
        let client =
            ApiAnthropicClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry), Some(sse_telemetry));
        let stream_result = client.stream_request(request, options).await;

        match stream_result {
            Ok(stream) => {
                let (stream, _) = map_response_stream(
                    stream,
                    session_telemetry.clone(),
                    inference_trace_attempt,
                    session.client.state.provider.clone(),
                );
                return Ok(stream);
            }
            Err(ApiError::Transport(
                unauthorized_transport @ TransportError::Http { status, .. },
            )) if status == StatusCode::UNAUTHORIZED => {
                let response_debug_context =
                    extract_response_debug_context(&unauthorized_transport);
                inference_trace_attempt.record_failed(
                    &unauthorized_transport,
                    response_debug_context.request_id.as_deref(),
                    /*output_items*/ &[],
                );
                pending_retry = PendingUnauthorizedRetry::from_recovery(
                    handle_unauthorized(
                        unauthorized_transport,
                        &mut auth_recovery,
                        session_telemetry,
                        &session.client.state.provider,
                    )
                    .await?,
                );
            }
            Err(err) => {
                let response_debug_context = extract_response_debug_context_from_api_error(&err);
                let err = session.client.state.provider.map_api_error(err);
                inference_trace_attempt.record_failed(
                    &err,
                    response_debug_context.request_id.as_deref(),
                    /*output_items*/ &[],
                );
                return Err(err);
            }
        }
    }
}
