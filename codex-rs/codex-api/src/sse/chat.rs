use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::chat::ChatToolNameMap;
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
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const OPENAI_MODEL_HEADER: &str = "openai-model";
const REQUEST_ID_HEADER: &str = "x-request-id";

pub fn spawn_chat_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    tool_names: ChatToolNameMap,
) -> ResponseStream {
    let rate_limit_snapshots = parse_all_rate_limits(&stream_response.headers);
    let server_model = stream_response
        .headers
        .get(OPENAI_MODEL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
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
        if let Some(model) = server_model {
            let _ = tx_event.send(Ok(ResponseEvent::ServerModel(model))).await;
        }
        for snapshot in rate_limit_snapshots {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        process_chat_sse(
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
struct ChatChunk {
    id: Option<String>,
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatErrorEvent {
    error: ChatError,
}

#[derive(Debug, Deserialize)]
struct ChatError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ChatDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ChatToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    prompt_tokens_details: Option<PromptTokensDetails>,
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    reasoning_tokens: Option<i64>,
}

impl From<ChatUsage> for TokenUsage {
    fn from(value: ChatUsage) -> Self {
        Self {
            input_tokens: value.prompt_tokens,
            cached_input_tokens: value
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or_default(),
            cache_write_input_tokens: 0,
            output_tokens: value.completion_tokens,
            reasoning_output_tokens: value
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or_default(),
            total_tokens: value.total_tokens,
        }
    }
}

#[derive(Default)]
struct ChatStreamState {
    response_id: Option<String>,
    tool_names: ChatToolNameMap,
    content: TextAccumulator,
    reasoning: ReasoningAccumulator,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    finished: bool,
    token_usage: Option<TokenUsage>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum StreamItemState {
    #[default]
    Pending,
    Streaming,
}

impl StreamItemState {
    fn start(&mut self) -> bool {
        if *self == Self::Streaming {
            return false;
        }
        *self = Self::Streaming;
        true
    }
}

#[derive(Default)]
struct TextAccumulator {
    text: String,
    state: StreamItemState,
}

#[derive(Default)]
struct ReasoningAccumulator {
    text: String,
    state: StreamItemState,
}

enum ToolCallAccumulator {
    Pending(PendingToolCall),
    Started(StartedToolCall),
}

impl Default for ToolCallAccumulator {
    fn default() -> Self {
        Self::Pending(PendingToolCall::default())
    }
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    raw_name: String,
    arguments: String,
}

struct StartedToolCall {
    id: String,
    namespace: Option<String>,
    name: String,
    arguments: String,
    emitted_argument_bytes: usize,
}

impl TextAccumulator {
    async fn push(
        &mut self,
        content: String,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if self.state.start() {
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
        self.text.push_str(&content);
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(content)))
            .await;
    }

    async fn finish(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        if self.text.is_empty() {
            return;
        }
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: std::mem::take(&mut self.text),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })))
            .await;
        self.state = StreamItemState::Pending;
    }
}

impl ReasoningAccumulator {
    async fn push(
        &mut self,
        reasoning_content: String,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if self.state.start() {
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
        self.text.push_str(&reasoning_content);
        let _ = tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: reasoning_content,
                content_index: 0,
            }))
            .await;
    }

    async fn finish(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        if self.state != StreamItemState::Streaming {
            return;
        }
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                id: Some(reasoning_id()),
                summary: Vec::new(),
                content: Some(vec![ReasoningItemContent::ReasoningText {
                    text: std::mem::take(&mut self.text),
                }]),
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            })))
            .await;
        self.state = StreamItemState::Pending;
    }
}

impl ToolCallAccumulator {
    async fn push_delta(
        &mut self,
        index: usize,
        delta: ChatToolCallDelta,
        tool_names: &ChatToolNameMap,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        match self {
            ToolCallAccumulator::Pending(pending) => {
                pending.append_delta_fields(delta);
                let Some(mut started_tool_call) = pending.start(tool_names) else {
                    return;
                };
                started_tool_call.emit_start(tx_event).await;
                started_tool_call.emit_new_arguments(index, tx_event).await;
                *self = ToolCallAccumulator::Started(started_tool_call);
            }
            ToolCallAccumulator::Started(started) => {
                started.append_delta_fields(delta);
                started.emit_new_arguments(index, tx_event).await;
            }
        }
    }

    async fn finish(
        self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        match self {
            ToolCallAccumulator::Pending(_) => Err(ApiError::Stream(
                "chat tool call missing function name".to_string(),
            )),
            ToolCallAccumulator::Started(started) => started.finish(tx_event).await,
        }
    }
}

impl PendingToolCall {
    fn append_delta_fields(&mut self, delta: ChatToolCallDelta) {
        append_non_empty(&mut self.id, delta.id);
        if let Some(function_delta) = delta.function {
            append_non_empty(&mut self.raw_name, function_delta.name);
            append_non_empty(&mut self.arguments, function_delta.arguments);
        }
    }

    fn start(&self, tool_names: &ChatToolNameMap) -> Option<StartedToolCall> {
        if self.raw_name.is_empty() {
            return None;
        }
        let (namespace, name) = match tool_names.get(&self.raw_name) {
            Some(tool_name) => (tool_name.namespace.clone(), tool_name.name.clone()),
            None => (None, self.raw_name.clone()),
        };
        Some(StartedToolCall {
            id: self.id.clone(),
            namespace,
            name,
            arguments: self.arguments.clone(),
            emitted_argument_bytes: 0,
        })
    }
}

impl StartedToolCall {
    fn append_delta_fields(&mut self, delta: ChatToolCallDelta) {
        append_non_empty(&mut self.id, delta.id);
        if let Some(function_delta) = delta.function {
            append_non_empty(&mut self.arguments, function_delta.arguments);
        }
    }

    async fn emit_start(&self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::FunctionCall {
                    id: None,
                    name: self.name.clone(),
                    namespace: self.namespace.clone(),
                    arguments: String::new(),
                    call_id: self.id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                },
            )))
            .await;
    }

    async fn emit_new_arguments(
        &mut self,
        index: usize,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if self.emitted_argument_bytes == self.arguments.len() {
            return;
        }
        let delta = self.arguments[self.emitted_argument_bytes..].to_string();
        self.emitted_argument_bytes = self.arguments.len();
        let item_id = if self.id.is_empty() {
            format!("chat_tool_{index}")
        } else {
            self.id.clone()
        };
        let _ = tx_event
            .send(Ok(ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id: Some(self.id.clone()),
                delta,
            }))
            .await;
    }

    async fn finish(
        self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(
                ResponseItem::FunctionCall {
                    id: None,
                    name: self.name,
                    namespace: self.namespace,
                    arguments: self.arguments,
                    call_id: self.id,
                    internal_chat_message_metadata_passthrough: None,
                },
            )))
            .await;
        Ok(())
    }
}

impl ChatStreamState {
    async fn apply_chunk(
        &mut self,
        chunk: ChatChunk,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<bool, ApiError> {
        if let Some(id) = chunk.id {
            self.response_id = Some(id);
        }
        if let Some(usage) = chunk.usage {
            self.token_usage = Some(usage.into());
        }

        let mut completed = false;
        for choice in chunk.choices {
            self.apply_delta(choice.delta, tx_event).await;
            if let Some(finish_reason) = choice.finish_reason {
                match finish_reason.as_str() {
                    "stop" | "tool_calls" => {}
                    "length" | "content_filter" => {
                        return Err(ApiError::Stream(format!(
                            "chat completion finished with {finish_reason}"
                        )));
                    }
                    _ => {
                        return Err(ApiError::Stream(format!(
                            "chat completion finished with unknown finish reason {finish_reason}"
                        )));
                    }
                }
                self.finished = true;
                self.finish_items(&finish_reason, tx_event).await?;
                completed = finish_reason != "tool_calls";
            }
        }
        Ok(completed && self.token_usage.is_some())
    }

    async fn apply_delta(
        &mut self,
        delta: ChatDelta,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) {
        if let Some(content) = non_empty(delta.content) {
            self.content.push(content, tx_event).await;
        }

        if let Some(reasoning_content) = non_empty(delta.reasoning_content) {
            self.reasoning.push(reasoning_content, tx_event).await;
        }

        for tool_call in delta.tool_calls.unwrap_or_default() {
            let index = tool_call.index;
            self.tool_calls
                .entry(index)
                .or_default()
                .push_delta(index, tool_call, &self.tool_names, tx_event)
                .await;
        }
    }

    async fn finish_items(
        &mut self,
        finish_reason: &str,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        self.reasoning.finish(tx_event).await;
        self.content.finish(tx_event).await;

        if finish_reason == "tool_calls" {
            for (_, tool_call) in std::mem::take(&mut self.tool_calls) {
                tool_call.finish(tx_event).await?;
            }
        }
        Ok(())
    }

    async fn complete(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        let _ = tx_event
            .send(Ok(ResponseEvent::Completed {
                response_id: self
                    .response_id
                    .clone()
                    .unwrap_or_else(|| "chatcmpl".to_string()),
                token_usage: self.token_usage.take(),
                end_turn: None,
            }))
            .await;
    }

    async fn finish_from_done(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    ) -> Result<(), ApiError> {
        if self.finished {
            return Ok(());
        }
        let finish_reason = if self.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        self.finished = true;
        self.finish_items(finish_reason, tx_event).await
    }
}

pub async fn process_chat_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    tool_names: ChatToolNameMap,
) {
    let mut stream = stream.eventsource();
    let mut state = ChatStreamState {
        tool_names,
        ..Default::default()
    };
    let _ = tx_event.send(Ok(ResponseEvent::Created {})).await;

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                if state.finished {
                    state.complete(&tx_event).await;
                } else {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "stream closed before chat completion finished".into(),
                        )))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("SSE event: {}", &sse.data);
        if sse.data.trim() == "[DONE]" {
            match state.finish_from_done(&tx_event).await {
                Ok(()) => state.complete(&tx_event).await,
                Err(err) => {
                    let _ = tx_event.send(Err(err)).await;
                }
            }
            return;
        }

        let chunk = match serde_json::from_str::<ChatChunk>(&sse.data) {
            Ok(chunk) => chunk,
            Err(e) => {
                if let Ok(error_event) = serde_json::from_str::<ChatErrorEvent>(&sse.data) {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(error_event.error.message)))
                        .await;
                    return;
                }
                debug!("Failed to parse chat SSE event: {e}, data: {}", &sse.data);
                continue;
            }
        };

        match state.apply_chunk(chunk, &tx_event).await {
            Ok(true) => {
                state.complete(&tx_event).await;
                return;
            }
            Ok(false) => {}
            Err(err) => {
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        }
    }
}

fn reasoning_id() -> ResponseItemId {
    ResponseItemId::with_suffix("rs", "chat_reasoning_0")
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn append_non_empty(target: &mut String, value: Option<String>) {
    if let Some(value) = non_empty(value) {
        target.push_str(&value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::chat::ChatToolName;
    use assert_matches::assert_matches;
    use bytes::Bytes;
    use codex_client::TransportError;
    use futures::stream;
    use serde_json::Value;
    use serde_json::json;

    async fn run_chat_sse_results(events: Vec<Value>) -> Vec<Result<ResponseEvent, ApiError>> {
        run_chat_sse_results_with_tool_names(events, ChatToolNameMap::new()).await
    }

    async fn run_chat_sse_results_with_tool_names(
        events: Vec<Value>,
        tool_names: ChatToolNameMap,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");

        let stream = stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from(body))]);
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_sse(
            Box::pin(stream),
            tx,
            Duration::from_millis(1000),
            /*telemetry*/ None,
            tool_names,
        ));

        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event);
        }
        out
    }

    async fn run_chat_sse(events: Vec<Value>) -> Vec<ResponseEvent> {
        run_chat_sse_results(events)
            .await
            .into_iter()
            .map(|event| event.expect("event should parse"))
            .collect()
    }

    #[tokio::test]
    async fn parses_text_delta_and_usage() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{"delta": {"content": "hel"}, "finish_reason": null}],
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{"delta": {"content": "lo"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 3,
                    "completion_tokens": 2,
                    "total_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 1},
                    "completion_tokens_details": {"reasoning_tokens": 0}
                }
            }),
        ])
        .await;

        assert_matches!(events[0], ResponseEvent::Created);
        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        );
        assert_matches!(&events[2], ResponseEvent::OutputTextDelta(delta) if delta == "hel");
        assert_matches!(&events[3], ResponseEvent::OutputTextDelta(delta) if delta == "lo");
        assert_matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. })
                if role == "assistant"
                    && content == &vec![ContentItem::OutputText { text: "hello".to_string() }]
        );
        assert_matches!(
            &events[5],
            ResponseEvent::Completed {
                response_id,
                token_usage: Some(TokenUsage {
                    input_tokens: 3,
                    cached_input_tokens: 1,
                    cache_write_input_tokens: 0,
                    output_tokens: 2,
                    reasoning_output_tokens: 0,
                    total_tokens: 5,
                }),
                ..
            } if response_id == "chatcmpl-1"
        );
    }

    #[tokio::test]
    async fn parses_reasoning_delta() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"reasoning_content": "think"},
                "finish_reason": "stop"
            }],
        })])
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
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { content: Some(content), .. })
                if content == &vec![ReasoningItemContent::ReasoningText {
                    text: "think".to_string()
                }]
        );
    }

    #[tokio::test]
    async fn parses_streaming_tool_call() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "{\"cmd\":\""
        );
        assert_matches!(
            &events[3],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "ls\"}"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn empty_tool_call_id_delta_does_not_clear_existing_id() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "",
                            "function": {"arguments": "ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ])
        .await;

        assert_matches!(
            &events[3],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "ls\"}"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn buffers_tool_call_arguments_until_name_arrives() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"arguments": "{\"cmd\":\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"name": "shell", "arguments": "ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ])
        .await;

        assert_matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { .. })
        );
        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-1" && call_id == "call-1" && delta == "{\"cmd\":\"ls\"}"
        );
        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn missing_tool_call_name_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat tool call missing function name"
        );
    }

    #[tokio::test]
    async fn parses_namespaced_streaming_tool_call() {
        let events = run_chat_sse_results_with_tool_names(
            vec![json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {
                                "name": "mcp__calendar__lookup_order",
                                "arguments": "{\"id\":\"ord_123\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })],
            ChatToolNameMap::from([(
                "mcp__calendar__lookup_order".to_string(),
                ChatToolName {
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
                && call_id == "call-1"
        );
    }

    #[tokio::test]
    async fn parses_parallel_streaming_tool_calls() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call-alpha",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\"printf alpha\"}"}
                        },
                        {
                            "index": 1,
                            "id": "call-beta",
                            "function": {"name": "shell", "arguments": "{\"cmd\":\"printf beta\"}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })])
        .await;

        assert_matches!(
            &events[2],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-alpha" && call_id == "call-alpha" && delta == "{\"cmd\":\"printf alpha\"}"
        );
        assert_matches!(
            &events[4],
            ResponseEvent::ToolCallInputDelta { item_id, call_id: Some(call_id), delta }
                if item_id == "call-beta" && call_id == "call-beta" && delta == "{\"cmd\":\"printf beta\"}"
        );
        assert_matches!(
            &events[5],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell"
                && arguments == "{\"cmd\":\"printf alpha\"}"
                && call_id == "call-alpha"
        );
        assert_matches!(
            &events[6],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell"
                && arguments == "{\"cmd\":\"printf beta\"}"
                && call_id == "call-beta"
        );
    }

    #[tokio::test]
    async fn parses_usage_only_final_chunk_details() {
        let events = run_chat_sse(vec![
            json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {"content": "done"},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "chatcmpl-1",
                "choices": [],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 7,
                    "total_tokens": 18,
                    "prompt_tokens_details": {"cached_tokens": 4},
                    "completion_tokens_details": {"reasoning_tokens": 3}
                }
            }),
        ])
        .await;

        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed {
                token_usage: Some(TokenUsage {
                    input_tokens: 11,
                    cached_input_tokens: 4,
                    cache_write_input_tokens: 0,
                    output_tokens: 7,
                    reasoning_output_tokens: 3,
                    total_tokens: 18,
                }),
                ..
            })
        );
    }

    #[tokio::test]
    async fn done_without_finish_reason_flushes_text_item() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "done"},
                "finish_reason": null
            }]
        })])
        .await;

        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })
                if content == &vec![ContentItem::OutputText {
                    text: "done".to_string()
                }]
        );
        assert_matches!(&events[4], ResponseEvent::Completed { .. });
    }

    #[tokio::test]
    async fn done_without_finish_reason_flushes_tool_call() {
        let events = run_chat_sse(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": null
            }]
        })])
        .await;

        assert_matches!(
            &events[3],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) if name == "shell" && arguments == "{\"cmd\":\"ls\"}" && call_id == "call-1"
        );
        assert_matches!(&events[4], ResponseEvent::Completed { .. });
    }

    #[tokio::test]
    async fn length_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "partial"},
                "finish_reason": "length"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with length"
        );
    }

    #[tokio::test]
    async fn content_filter_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {"content": "partial"},
                "finish_reason": "content_filter"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with content_filter"
        );
    }

    #[tokio::test]
    async fn unknown_finish_reason_errors() {
        let events = run_chat_sse_results(vec![json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {},
                "finish_reason": "new_reason"
            }]
        })])
        .await;

        assert_matches!(
            events.last(),
            Some(Err(ApiError::Stream(message)))
                if message == "chat completion finished with unknown finish reason new_reason"
        );
    }
}
