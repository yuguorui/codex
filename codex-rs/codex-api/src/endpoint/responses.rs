use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_response_stream;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use tracing::instrument;

pub struct ResponsesClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

#[derive(Default)]
pub struct ResponsesOptions {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

impl<T: HttpTransport> ResponsesClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    #[instrument(
        name = "responses.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses"
        )
    )]
    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        let ResponsesOptions {
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            turn_state,
        } = options;

        let mut body = serde_json::to_value(&request)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        merge_extra_body(&mut body, &self.session.provider().extra_body);
        merge_extra_body(&mut body, &request.extra_body);

        let mut headers = extra_headers;
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        self.stream(body, headers, compression, turn_state).await
    }

    fn path() -> &'static str {
        "responses"
    }

    #[instrument(
        name = "responses.stream",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses",
            turn.has_state = turn_state.is_some()
        )
    )]
    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        self.stream_encoded(body, extra_headers, compression, turn_state)
            .await
    }

    async fn stream_encoded(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        Ok(spawn_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            turn_state,
        ))
    }
}

fn merge_extra_body(body: &mut Value, extra_body: &std::collections::HashMap<String, Value>) {
    if extra_body.is_empty() {
        return;
    }
    let Some(body) = body.as_object_mut() else {
        return;
    };
    for (key, value) in extra_body {
        match body.get_mut(key) {
            Some(existing) => merge_extra_body_value(existing, value),
            None => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_extra_body_value(existing: &mut Value, extra: &Value) {
    match (existing, extra) {
        (Value::Object(existing), Value::Object(extra)) => {
            for (key, value) in extra {
                match existing.get_mut(key) {
                    Some(existing_value) => merge_extra_body_value(existing_value, value),
                    None => {
                        existing.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (Value::Array(existing), Value::Array(extra)) => {
            existing.extend(extra.iter().cloned());
        }
        (existing, extra) => {
            *existing = extra.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn merge_extra_body_deep_merges_objects_and_appends_arrays() {
        let mut body = json!({
            "model": "qwen3.7-max",
            "tools": [
                {"type": "function", "name": "shell"}
            ],
            "reasoning": {
                "summary": "auto"
            }
        });
        let extra_body = HashMap::from([
            (
                "tools".to_string(),
                json!([
                    {"type": "web_search"},
                    {"type": "code_interpreter"}
                ]),
            ),
            (
                "reasoning".to_string(),
                json!({
                    "effort": "high"
                }),
            ),
        ]);

        merge_extra_body(&mut body, &extra_body);

        assert_eq!(
            body,
            json!({
                "model": "qwen3.7-max",
                "tools": [
                    {"type": "function", "name": "shell"},
                    {"type": "web_search"},
                    {"type": "code_interpreter"}
                ],
                "reasoning": {
                    "summary": "auto",
                    "effort": "high"
                }
            })
        );
    }

    #[test]
    fn merge_extra_body_replaces_scalars() {
        let mut body = json!({
            "model": "qwen3.7-max",
            "tool_choice": "auto"
        });
        let extra_body = HashMap::from([("tool_choice".to_string(), json!("required"))]);

        merge_extra_body(&mut body, &extra_body);

        assert_eq!(
            body,
            json!({
                "model": "qwen3.7-max",
                "tool_choice": "required"
            })
        );
    }
}
