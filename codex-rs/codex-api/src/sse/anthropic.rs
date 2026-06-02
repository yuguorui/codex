use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::anthropic::AnthropicToolNameMap;
use crate::error::ApiError;
use crate::rate_limits::parse_all_rate_limits;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const REQUEST_ID_HEADER: &str = "request-id";

pub fn spawn_anthropic_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    tool_names: AnthropicToolNameMap,
) -> ResponseStream {
    let rate_limit_snapshots = parse_all_rate_limits(&stream_response.headers);
    let upstream_request_id = stream_response
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(turn_state) = turn_state.as_ref()
        && let Some(header_value) = stream_response
            .headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
    {
        let _ = turn_state.set(header_value.to_string());
    }
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        for snapshot in rate_limit_snapshots {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        process_anthropic_sse(
            stream_response.bytes,
            tx_event,
            idle_timeout,
            telemetry,
            tool_names,
        )
        .await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicContentBlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicError,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageStart {
    id: String,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockStart {
    Text {
        text: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Option<Value>,
    },
    RedactedThinking {
        data: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    message: String,
}

#[derive(Default)]
struct AnthropicStreamState {
    response_id: Option<String>,
    tool_names: AnthropicToolNameMap,
    content: String,
    content_started: bool,
    tool_calls: BTreeMap<usize, ToolCallState>,
    reasoning: String,
    reasoning_signature: String,
    redacted_reasoning: Option<String>,
    reasoning_started: bool,
    completed_tool_calls: bool,
    token_usage: TokenUsage,
    raw_input_tokens: i64,
    cache_creation_input_tokens: i64,
    seen_usage: bool,
    stop_reason: Option<String>,
}

#[derive(Default)]
struct ToolCallState {
    id: Option<String>,
    namespace: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted_argument_bytes: usize,
    started: bool,
}

impl AnthropicStreamState {
    async fn apply_event(
        &mut self,
        event: AnthropicStreamEvent,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<bool, ApiError> {
        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                self.response_id = Some(message.id);
                if let Some(usage) = message.usage {
                    self.apply_usage(usage);
                }
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.apply_content_block_start(index, content_block, tx_event)
                    .await
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                self.apply_content_block_delta(index, delta, tx_event).await
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                // Deltas carry the data; stop only marks the block boundary.
                let _ = index;
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.apply_usage(usage);
                }
                self.stop_reason = delta.stop_reason;
            }
            AnthropicStreamEvent::MessageStop => {
                self.finish_items(tx_event).await?;
                self.complete(tx_event).await;
                return Ok(true);
            }
            AnthropicStreamEvent::Ping => {}
            AnthropicStreamEvent::Error { error } => {
                return Err(ApiError::Stream(error.message));
            }
        }
        Ok(false)
    }

    fn apply_usage(&mut self, usage: AnthropicUsage) {
        if let Some(input_tokens) = usage.input_tokens {
            self.raw_input_tokens = input_tokens;
        }
        if let Some(output_tokens) = usage.output_tokens {
            self.token_usage.output_tokens = output_tokens;
        }
        if let Some(cache_creation_input_tokens) = usage.cache_creation_input_tokens {
            self.cache_creation_input_tokens = cache_creation_input_tokens;
            self.token_usage.cache_creation_input_tokens = cache_creation_input_tokens;
        }
        if let Some(cached_input_tokens) = usage.cache_read_input_tokens {
            self.token_usage.cached_input_tokens = cached_input_tokens;
        }
        self.token_usage.input_tokens = self.raw_input_tokens
            + self.cache_creation_input_tokens
            + self.token_usage.cached_input_tokens;
        self.token_usage.total_tokens =
            self.token_usage.input_tokens + self.token_usage.output_tokens;
        self.seen_usage = true;
    }

    async fn apply_content_block_start(
        &mut self,
        index: usize,
        content_block: AnthropicContentBlockStart,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        match content_block {
            AnthropicContentBlockStart::Text { text } => {
                if let Some(text) = text
                    && !text.is_empty()
                {
                    self.emit_text_delta(text, tx_event).await;
                }
            }
            AnthropicContentBlockStart::ToolUse { id, name, input } => {
                let state = self.tool_calls.entry(index).or_default();
                state.id = Some(id);
                if let Some(tool_name) = self.tool_names.get(&name) {
                    state.namespace = tool_name.namespace.clone();
                    state.name = Some(tool_name.name.clone());
                } else {
                    state.namespace = None;
                    state.name = Some(name);
                }
                if let Some(input) = input
                    && input != Value::Object(Default::default())
                {
                    state.arguments.push_str(&input.to_string());
                }
                self.start_tool_call(index, tx_event).await;
            }
            AnthropicContentBlockStart::RedactedThinking { data } => {
                self.emit_redacted_reasoning(data, tx_event).await;
            }
            AnthropicContentBlockStart::Unknown => {}
        }
    }

    async fn apply_content_block_delta(
        &mut self,
        index: usize,
        delta: AnthropicContentBlockDelta,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        match delta {
            AnthropicContentBlockDelta::TextDelta { text } => {
                if !text.is_empty() {
                    self.emit_text_delta(text, tx_event).await;
                }
            }
            AnthropicContentBlockDelta::InputJsonDelta { partial_json } => {
                if partial_json.is_empty() {
                    return;
                }
                let state = self.tool_calls.entry(index).or_default();
                state.arguments.push_str(&partial_json);
                self.start_tool_call(index, tx_event).await;
                self.emit_tool_delta(index, tx_event).await;
            }
            AnthropicContentBlockDelta::ThinkingDelta { thinking } => {
                if !thinking.is_empty() {
                    self.emit_reasoning_delta(thinking, tx_event).await;
                }
            }
            AnthropicContentBlockDelta::SignatureDelta { signature } => {
                self.start_reasoning(tx_event).await;
                self.reasoning_signature.push_str(&signature);
            }
            AnthropicContentBlockDelta::Unknown => {}
        }
    }

    async fn start_reasoning(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        if self.reasoning_started {
            return;
        }
        self.reasoning_started = true;
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::Reasoning {
                    id: Some(reasoning_id()),
                    summary: Vec::new(),
                    content: Some(Vec::new()),
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            )))
            .await;
    }

    async fn emit_reasoning_delta(
        &mut self,
        thinking: String,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        self.start_reasoning(tx_event).await;
        self.reasoning.push_str(&thinking);
        let _ = tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: thinking,
                content_index: 0,
            }))
            .await;
    }

    async fn emit_redacted_reasoning(
        &mut self,
        data: String,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if data.is_empty() {
            return;
        }
        self.start_reasoning(tx_event).await;
        self.redacted_reasoning = Some(data);
    }

    async fn emit_text_delta(
        &mut self,
        text: String,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if !self.content_started {
            self.content_started = true;
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: Vec::new(),
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                })))
                .await;
        }
        self.content.push_str(&text);
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text)))
            .await;
    }

    async fn start_tool_call(
        &mut self,
        index: usize,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        let Some(state) = self.tool_calls.get_mut(&index) else {
            return;
        };
        if state.started {
            return;
        }
        let Some(name) = state.name.clone() else {
            return;
        };
        state.started = true;
        let call_id = state.id.clone().unwrap_or_default();
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::FunctionCall {
                    id: None,
                    name,
                    namespace: state.namespace.clone(),
                    arguments: String::new(),
                    call_id,
                    internal_chat_message_metadata_passthrough: None,
                },
            )))
            .await;
    }

    async fn emit_tool_delta(
        &mut self,
        index: usize,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        let Some(state) = self.tool_calls.get_mut(&index) else {
            return;
        };
        if !state.started || state.emitted_argument_bytes >= state.arguments.len() {
            return;
        }
        let call_id = state.id.clone();
        let item_id = call_id
            .clone()
            .unwrap_or_else(|| format!("anthropic_tool_{index}"));
        let delta = state.arguments[state.emitted_argument_bytes..].to_string();
        state.emitted_argument_bytes = state.arguments.len();
        let _ = tx_event
            .send(Ok(ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            }))
            .await;
    }

    async fn finish_items(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        if self.reasoning_started {
            let redacted_reasoning = self.redacted_reasoning.take();
            let content = redacted_reasoning.is_none().then(|| {
                vec![ReasoningItemContent::ReasoningText {
                    text: std::mem::take(&mut self.reasoning),
                }]
            });
            let encrypted_content = redacted_reasoning.or_else(|| {
                (!self.reasoning_signature.is_empty())
                    .then(|| std::mem::take(&mut self.reasoning_signature))
            });
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                    id: Some(reasoning_id()),
                    summary: Vec::new(),
                    content,
                    encrypted_content,
                    internal_chat_message_metadata_passthrough: None,
                })))
                .await;
            self.reasoning_started = false;
        }

        if !self.content.is_empty() {
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: std::mem::take(&mut self.content),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                })))
                .await;
            self.content_started = false;
        }

        let tool_calls = std::mem::take(&mut self.tool_calls);
        self.completed_tool_calls = !tool_calls.is_empty();
        for (_, tool_call) in tool_calls {
            let Some(name) = tool_call.name else {
                return Err(ApiError::Stream(
                    "Anthropic tool call missing function name".to_string(),
                ));
            };
            let call_id = tool_call.id.unwrap_or_default();
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(
                    ResponseItem::FunctionCall {
                        id: None,
                        name,
                        namespace: tool_call.namespace,
                        arguments: tool_call.arguments,
                        call_id,
                        internal_chat_message_metadata_passthrough: None,
                    },
                )))
                .await;
        }
        Ok(())
    }

    async fn complete(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        let _ = tx_event
            .send(Ok(ResponseEvent::Completed {
                response_id: self
                    .response_id
                    .clone()
                    .unwrap_or_else(|| "anthropic_msg".to_string()),
                token_usage: self.seen_usage.then(|| self.token_usage.clone()),
                end_turn: Some(
                    !self.completed_tool_calls && self.stop_reason.as_deref() == Some("end_turn"),
                ),
            }))
            .await;
    }
}

fn reasoning_id() -> ResponseItemId {
    // Anthropic emits at most one thinking block per message.
    ResponseItemId::with_suffix("rs", "anthropic_reasoning_0")
}

pub async fn process_anthropic_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    tool_names: AnthropicToolNameMap,
) {
    let mut stream = stream.eventsource();
    let mut state = AnthropicStreamState {
        tool_names,
        ..Default::default()
    };
    let _ = tx_event.send(Ok(ResponseEvent::Created)).await;

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("Anthropic SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "stream closed before Anthropic message_stop".into(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("Anthropic SSE event: {}", &sse.data);
        let event = match serde_json::from_str::<AnthropicStreamEvent>(&sse.data) {
            Ok(event) => event,
            Err(e) => {
                debug!(
                    "Failed to parse Anthropic SSE event: {e}, data: {}",
                    &sse.data
                );
                continue;
            }
        };

        match state.apply_event(event, &tx_event).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(err) => {
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use bytes::Bytes;
    use codex_client::TransportError;
    use futures::stream;
    use serde_json::json;

    async fn run_anthropic_sse(events: Vec<Value>) -> Vec<ResponseEvent> {
        run_anthropic_sse_results(events, AnthropicToolNameMap::new())
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect()
    }

    async fn run_anthropic_sse_results(
        events: Vec<Value>,
        tool_names: AnthropicToolNameMap,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }

        let stream = stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from(body))]);
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_anthropic_sse(
            Box::pin(stream),
            tx,
            Duration::from_secs(5),
            None,
            tool_names,
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn parses_text_delta_and_usage() {
        let events = run_anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg-1",
                    "usage": {"input_tokens": 10, "output_tokens": 1}
                }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello"}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}
            }),
            json!({"type": "message_stop"}),
        ])
        .await;

        assert_matches!(events[0], ResponseEvent::Created);
        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::OutputTextDelta(text) if text == "hello"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })
                if content == &vec![ContentItem::OutputText { text: "hello".to_string() }]
        );
        assert_matches!(
            &events[4],
            ResponseEvent::Completed { response_id, token_usage: Some(usage), end_turn }
                if response_id == "msg-1"
                    && usage.input_tokens == 10
                    && usage.output_tokens == 2
                    && usage.total_tokens == 12
                    && end_turn == &Some(true)
        );
    }

    #[tokio::test]
    async fn usage_includes_cached_input_tokens_in_context_total() {
        let events = run_anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg-1",
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 20,
                        "cache_read_input_tokens": 30,
                        "output_tokens": 1
                    }
                }
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}
            }),
            json!({"type": "message_stop"}),
        ])
        .await;

        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed { token_usage: Some(usage), .. })
                if usage.input_tokens == 60
                    && usage.cached_input_tokens == 30
                    && usage.cache_creation_input_tokens == 20
                    && usage.output_tokens == 2
                    && usage.total_tokens == 62
        );
    }

    #[tokio::test]
    async fn parses_thinking_delta_as_reasoning() {
        let events = run_anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {"id": "msg-1", "usage": {"input_tokens": 1, "output_tokens": 1}}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "think"}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "signature"}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}
            }),
            json!({"type": "message_stop"}),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ReasoningContentDelta { delta, content_index: 0 }
                if delta == "think"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                content: Some(content),
                encrypted_content: Some(encrypted_content),
                ..
            }) if encrypted_content == "signature"
                && content == &vec![ReasoningItemContent::ReasoningText {
                    text: "think".to_string(),
                }]
        );
        assert_matches!(
            &events[4],
            ResponseEvent::Completed { response_id, end_turn, .. }
                if response_id == "msg-1" && end_turn == &Some(true)
        );
    }

    #[tokio::test]
    async fn parses_redacted_thinking_as_reasoning() {
        let events = run_anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {"id": "msg-1", "usage": {"input_tokens": 1, "output_tokens": 1}}
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "redacted-data"}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}
            }),
            json!({"type": "message_stop"}),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                content: None,
                encrypted_content: Some(encrypted_content),
                ..
            }) if encrypted_content == "redacted-data"
        );
    }

    #[tokio::test]
    async fn parses_streaming_tool_use() {
        let events = run_anthropic_sse_results(
            vec![
                json!({
                    "type": "message_start",
                    "message": {"id": "msg-1", "usage": {"input_tokens": 1, "output_tokens": 1}}
                }),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu-1",
                        "name": "mcp__calendar__lookup_order",
                        "input": {}
                    }
                }),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"id\":\"ord_123\"}"}
                }),
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 5}
                }),
                json!({"type": "message_stop"}),
            ],
            AnthropicToolNameMap::from([(
                "mcp__calendar__lookup_order".to_string(),
                crate::endpoint::anthropic::AnthropicToolName {
                    namespace: Some("mcp__calendar__".to_string()),
                    name: "lookup_order".to_string(),
                },
            )]),
        )
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "toolu-1" && call_id == "toolu-1" && delta == "{\"id\":\"ord_123\"}"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                namespace,
                name,
                arguments,
                call_id,
                ..
            }) if namespace.as_deref() == Some("mcp__calendar__")
                && name == "lookup_order"
                && arguments == "{\"id\":\"ord_123\"}"
                && call_id == "toolu-1"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::Completed { end_turn, .. } if end_turn == &Some(false)
        );
    }

    #[tokio::test]
    async fn tool_use_forces_end_turn_false_even_when_stop_reason_is_end_turn() {
        let events = run_anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {"id": "msg-1", "usage": {"input_tokens": 1, "output_tokens": 1}}
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu-1",
                    "name": "get_weather",
                    "input": {}
                }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"Hangzhou\"}"}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 5}
            }),
            json!({"type": "message_stop"}),
        ])
        .await;

        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed { end_turn, .. }) if end_turn == &Some(false)
        );
    }
}
