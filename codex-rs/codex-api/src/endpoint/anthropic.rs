use crate::auth::SharedAuthProvider;
use crate::common::Reasoning;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_anthropic_response_stream;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use tracing::instrument;

const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 64000;

/// Process-global device identifier: 32 random bytes encoded as 64 hex chars.
/// Generated once per process lifetime, mirroring Claude Code's `crypto.randomBytes(32).toString("hex")`.
fn process_device_id() -> &'static str {
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    DEVICE_ID.get_or_init(|| {
        let mut bytes = [0u8; 32];
        use rand::RngCore;
        rand::rng().fill_bytes(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// Metadata attached to every Anthropic Messages request for prompt-cache routing.
///
/// - `device_id`: random 64-hex string, stable for the process lifetime.
/// - `account_uuid`: always empty (no OAuth account context).
/// - `session_id`: UUID v4, regenerated each session.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct AnthropicMetadata {
    device_id: String,
    account_uuid: String,
    session_id: String,
}

/// Wrapper that serializes as `{"user_id": "<stringified metadata>"}` per the Anthropic Messages API contract.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AnthropicRequestMetadata {
    user_id: String,
}

impl Serialize for AnthropicRequestMetadata {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("user_id", &self.user_id)?;
        map.end()
    }
}

pub struct AnthropicClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

#[derive(Default)]
pub struct AnthropicOptions {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AnthropicToolName {
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
}

pub(crate) type AnthropicToolNameMap = HashMap<String, AnthropicToolName>;

#[derive(Debug)]
struct AnthropicRequestBody {
    body: Value,
    tool_names: AnthropicToolNameMap,
    extra_body: HashMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<AnthropicRequestMetadata>,
}

#[derive(Debug, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: AnthropicRole,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum AnthropicRole {
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource {
    Url { url: String },
    Base64 { media_type: String, data: String },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: CacheType,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CacheType {
    Ephemeral,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

/// Controls the Anthropic extended thinking mode sent in the request body.
///
/// Anthropic supports two thinking modes:
/// - `Adaptive`: The model decides when to think (supported by Claude 4+ models).
/// - `Enabled`: Explicitly enable thinking with a token budget (supported by all Claude models,
///   but may not be accepted by some newer models that require `Adaptive`).
///
/// When no reasoning effort is specified, the default is `Adaptive` for broad model compatibility.
/// When a reasoning effort is explicitly set, `Enabled` is used with a token budget mapped from
/// the effort level. To override the thinking mode per provider, use `extra_body` in the provider
/// config, e.g. `extra_body = { thinking = { type = "enabled", budget_tokens = 16000 } }`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicThinking {
    /// Let the model decide when to engage extended thinking.
    Adaptive,
    /// Explicitly enable thinking with a fixed token budget.
    Enabled {
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_tokens: Option<u32>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesTool {
    Function(ResponsesFunctionTool),
    Namespace(ResponsesNamespaceTool),
    Custom(ResponsesCustomTool),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ResponsesCustomTool {
    name: String,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesFunctionTool {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ResponsesNamespaceTool {
    name: String,
    tools: Vec<ResponsesFunctionTool>,
}

impl<T: HttpTransport> AnthropicClient<T> {
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
        name = "anthropic.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "messages"
        )
    )]
    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        options: AnthropicOptions,
    ) -> Result<ResponseStream, ApiError> {
        let AnthropicOptions {
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            turn_state,
        } = options;

        let mut request_body =
            anthropic_body_from_responses_request(request, session_id.as_deref())?;
        merge_extra_body(&mut request_body.body, &self.session.provider().extra_body)?;
        merge_extra_body(&mut request_body.body, &request_body.extra_body)?;

        let mut headers = extra_headers;
        if !headers.contains_key("anthropic-version") {
            headers.insert(
                "anthropic-version",
                HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
            );
        }
        if !headers.contains_key("anthropic-beta") {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("interleaved-thinking-2025-05-14"),
            );
        }
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        self.stream(
            request_body.body,
            headers,
            compression,
            turn_state,
            request_body.tool_names,
        )
        .await
    }

    fn path() -> &'static str {
        "messages"
    }

    #[instrument(
        name = "anthropic.stream",
        level = "info",
        skip_all,
        fields(
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "messages",
            turn.has_state = turn_state.is_some()
        )
    )]
    async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
        tool_names: AnthropicToolNameMap,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode anthropic request: {e}")))?;
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

        Ok(spawn_anthropic_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            turn_state,
            tool_names,
        ))
    }
}

fn merge_extra_body(
    body: &mut Value,
    extra_body: &std::collections::HashMap<String, Value>,
) -> Result<(), ApiError> {
    if extra_body.is_empty() {
        return Ok(());
    }
    let Some(body) = body.as_object_mut() else {
        return Err(ApiError::Stream(
            "Anthropic request body must be a JSON object to merge extra_body".to_string(),
        ));
    };
    for (key, value) in extra_body {
        if key == "stream" && value != &Value::Bool(true) {
            return Err(ApiError::Stream(
                "extra_body.stream must remain true for Anthropic streaming".to_string(),
            ));
        }
        body.insert(key.clone(), value.clone());
    }
    body.insert("stream".to_string(), Value::Bool(true));
    Ok(())
}

fn anthropic_body_from_responses_request(
    request: ResponsesApiRequest,
    session_id: Option<&str>,
) -> Result<AnthropicRequestBody, ApiError> {
    let mut tool_names = AnthropicToolNameMap::new();
    let tools = anthropic_tools_from_responses_tools(
        request
            .tools
            .as_ref()
            .map(|t| serde_json::from_str(t.as_raw_value().get()).unwrap_or_default())
            .unwrap_or_default(),
        &mut tool_names,
    )?;
    let has_tools = !tools.is_empty();
    let (system, messages) = anthropic_messages_from_items(&request.instructions, request.input)?;
    let extra_body = request.extra_body;
    let metadata = session_id
        .map(|sid| {
            let inner = serde_json::to_string(&AnthropicMetadata {
                device_id: process_device_id().to_string(),
                account_uuid: String::new(),
                session_id: sid.to_string(),
            })
            .map_err(|err| ApiError::Stream(format!("failed to encode metadata: {err}")))?;
            Ok::<_, ApiError>(AnthropicRequestMetadata { user_id: inner })
        })
        .transpose()?;
    let mut request = AnthropicMessagesRequest {
        model: request.model,
        max_tokens: DEFAULT_MAX_TOKENS,
        messages,
        stream: true,
        system,
        tools,
        tool_choice: has_tools
            .then(|| anthropic_tool_choice(&request.tool_choice))
            .flatten(),
        thinking: anthropic_thinking_from_reasoning(request.reasoning.as_ref()),
        metadata,
    };
    apply_cache_breakpoints(&mut request);
    let body = serde_json::to_value(request)
        .map_err(|err| ApiError::Stream(format!("failed to encode Anthropic request: {err}")))?;
    Ok(AnthropicRequestBody {
        body,
        tool_names,
        extra_body,
    })
}

/// Maps Codex reasoning effort to the corresponding Anthropic thinking configuration.
///
/// - No reasoning effort → `Adaptive` (broadest model compatibility).
/// - `None` → No thinking block sent.
/// - `Minimal` / `Low` → `Enabled` with 1024 budget tokens.
/// - `Medium` → `Enabled` with 2048 budget tokens.
/// - `High` / `XHigh` → `Enabled` with 3072 budget tokens.
///
/// Providers can override this via `extra_body`, e.g.
/// `extra_body = { thinking = { type = "adaptive" } }` to force adaptive even when reasoning
/// effort is set, or `extra_body = { thinking = { type = "enabled", budget_tokens = 16000 } }`
/// to increase the budget.
fn anthropic_thinking_from_reasoning(reasoning: Option<&Reasoning>) -> Option<AnthropicThinking> {
    let Some(effort) = reasoning.and_then(|reasoning| reasoning.effort.as_ref()) else {
        return Some(AnthropicThinking::Adaptive);
    };
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal | ReasoningEffort::Low => Some(AnthropicThinking::Enabled {
            budget_tokens: Some(1024),
        }),
        ReasoningEffort::Medium => Some(AnthropicThinking::Enabled {
            budget_tokens: Some(2048),
        }),
        ReasoningEffort::High
        | ReasoningEffort::XHigh
        | ReasoningEffort::Max
        | ReasoningEffort::Ultra => Some(AnthropicThinking::Enabled {
            budget_tokens: Some(3072),
        }),
        ReasoningEffort::Custom(_) => Some(AnthropicThinking::Adaptive),
    }
}

fn anthropic_tool_choice(tool_choice: &str) -> Option<AnthropicToolChoice> {
    match tool_choice {
        "required" => Some(AnthropicToolChoice::Any),
        "auto" => Some(AnthropicToolChoice::Auto),
        "none" => None,
        name => Some(AnthropicToolChoice::Tool {
            name: name.to_string(),
        }),
    }
}

fn anthropic_messages_from_items(
    instructions: &str,
    items: Vec<ResponseItem>,
) -> Result<(Option<Vec<AnthropicSystemBlock>>, Vec<AnthropicMessage>), ApiError> {
    let mut system: Vec<AnthropicSystemBlock> = Vec::new();
    if !instructions.is_empty() {
        system.push(AnthropicSystemBlock {
            block_type: "text".to_string(),
            text: instructions.to_string(),
            cache_control: None,
        });
    }

    let mut messages = Vec::new();
    let mut pending_assistant_content = Vec::new();
    let mut pending_tool_results = Vec::new();

    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => match role.as_str() {
                "system" | "developer" => {
                    flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
                    flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                    let text = anthropic_content_from_items(content).into_text();
                    if !text.is_empty() {
                        system.push(AnthropicSystemBlock {
                            block_type: "text".to_string(),
                            text,
                            cache_control: None,
                        });
                    }
                }
                "assistant" => {
                    flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                    let text = anthropic_content_from_items(content).into_text();
                    if !text.is_empty() {
                        pending_assistant_content.push(AnthropicContentBlock::Text {
                            text,
                            cache_control: None,
                        });
                    }
                }
                _ => {
                    flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
                    flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                    messages.push(AnthropicMessage {
                        role: AnthropicRole::User,
                        content: anthropic_content_from_items(content).blocks,
                    });
                }
            },
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => {
                flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                let name = anthropic_tool_name(namespace.as_deref(), &name);
                pending_assistant_content.push(AnthropicContentBlock::ToolUse {
                    id: call_id,
                    name,
                    input: serde_json::from_str(&arguments).unwrap_or(Value::String(arguments)),
                    cache_control: None,
                });
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                pending_assistant_content.push(AnthropicContentBlock::ToolUse {
                    id: call_id,
                    name,
                    input: serde_json::json!({ "input": input }),
                    cache_control: None,
                });
            }
            ResponseItem::Reasoning {
                content,
                encrypted_content,
                ..
            } => {
                flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                let thinking = reasoning_content_to_text(&content);
                if let Some(data) = encrypted_content.as_ref()
                    && content.is_none()
                {
                    pending_assistant_content
                        .push(AnthropicContentBlock::RedactedThinking { data: data.clone() });
                } else if !thinking.is_empty() || encrypted_content.is_some() {
                    pending_assistant_content.push(AnthropicContentBlock::Thinking {
                        thinking,
                        signature: encrypted_content,
                    });
                }
            }
            ResponseItem::FunctionCallOutput {
                call_id,
                output,
                internal_chat_message_metadata_passthrough: _,
                ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
                pending_tool_results.push(AnthropicContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: function_output_to_anthropic_content(output.body),
                    is_error: (output.success == Some(false)).then_some(true),
                    cache_control: None,
                });
            }
            ResponseItem::Compaction {
                encrypted_content,
                internal_chat_message_metadata_passthrough: _,
                ..
            }
            | ResponseItem::ContextCompaction {
                encrypted_content: Some(encrypted_content),
                internal_chat_message_metadata_passthrough: _,
                ..
            } => {
                flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
                flush_pending_tool_results(&mut messages, &mut pending_tool_results);
                system.push(AnthropicSystemBlock {
                    block_type: "text".to_string(),
                    text: encrypted_content,
                    cache_control: None,
                });
            }
            _ => {
                flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
                flush_pending_tool_results(&mut messages, &mut pending_tool_results);
            }
        }
    }

    flush_pending_assistant_content(&mut messages, &mut pending_assistant_content);
    flush_pending_tool_results(&mut messages, &mut pending_tool_results);
    Ok(((!system.is_empty()).then_some(system), messages))
}

struct AnthropicContent {
    blocks: Vec<AnthropicContentBlock>,
}

impl AnthropicContent {
    fn into_text(self) -> String {
        self.blocks
            .into_iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text, .. } => Some(text),
                AnthropicContentBlock::Image { .. }
                | AnthropicContentBlock::ToolUse { .. }
                | AnthropicContentBlock::ToolResult { .. }
                | AnthropicContentBlock::Thinking { .. }
                | AnthropicContentBlock::RedactedThinking { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn reasoning_content_to_text(content: &Option<Vec<ReasoningItemContent>>) -> String {
    content
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|content| match content {
            ReasoningItemContent::ReasoningText { text } | ReasoningItemContent::Text { text } => {
                text
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn anthropic_content_from_items(content: Vec<ContentItem>) -> AnthropicContent {
    let mut blocks = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                blocks.push(AnthropicContentBlock::Text {
                    text,
                    cache_control: None,
                });
            }
            ContentItem::InputImage { image_url, detail } => {
                let _detail: Option<ImageDetail> = detail;
                blocks.push(AnthropicContentBlock::Image {
                    source: anthropic_image_source(image_url),
                    cache_control: None,
                });
            }
            ContentItem::InputAudio { .. } => {}
        }
    }
    AnthropicContent { blocks }
}

fn flush_pending_assistant_content(
    messages: &mut Vec<AnthropicMessage>,
    pending_assistant_content: &mut Vec<AnthropicContentBlock>,
) {
    if pending_assistant_content.is_empty() {
        return;
    }
    let content = std::mem::take(pending_assistant_content);
    let (thinking, other): (Vec<_>, Vec<_>) = content.into_iter().partition(|block| {
        matches!(
            block,
            AnthropicContentBlock::Thinking { .. } | AnthropicContentBlock::RedactedThinking { .. }
        )
    });
    let mut sorted = thinking;
    sorted.extend(other);
    messages.push(AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: sorted,
    });
}

fn flush_pending_tool_results(
    messages: &mut Vec<AnthropicMessage>,
    pending_tool_results: &mut Vec<AnthropicContentBlock>,
) {
    if pending_tool_results.is_empty() {
        return;
    }
    messages.push(AnthropicMessage {
        role: AnthropicRole::User,
        content: std::mem::take(pending_tool_results),
    });
}

fn function_output_to_anthropic_content(output: FunctionCallOutputBody) -> Value {
    match output {
        FunctionCallOutputBody::Text(text) => Value::String(text),
        FunctionCallOutputBody::ContentItems(items) => Value::Array(
            items
                .into_iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        Some(AnthropicContentBlock::Text {
                            text,
                            cache_control: None,
                        })
                    }
                    FunctionCallOutputContentItem::InputImage {
                        image_url,
                        detail: _,
                    } => Some(AnthropicContentBlock::Image {
                        source: anthropic_image_source(image_url),
                        cache_control: None,
                    }),
                    FunctionCallOutputContentItem::InputAudio { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
                .filter_map(|block| serde_json::to_value(block).ok())
                .collect(),
        ),
    }
}

fn anthropic_image_source(image_url: String) -> AnthropicImageSource {
    if let Some(rest) = image_url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
    {
        return AnthropicImageSource::Base64 {
            media_type: media_type.to_string(),
            data: data.to_string(),
        };
    }
    AnthropicImageSource::Url { url: image_url }
}

fn anthropic_tools_from_responses_tools(
    tools: Vec<Value>,
    tool_names: &mut AnthropicToolNameMap,
) -> Result<Vec<AnthropicTool>, ApiError> {
    let mut converted = Vec::new();
    for tool in tools {
        match serde_json::from_value::<ResponsesTool>(tool) {
            Ok(ResponsesTool::Function(tool)) => {
                converted.push(anthropic_function_tool(tool, None, tool_names)?);
            }
            Ok(ResponsesTool::Namespace(namespace)) => {
                for tool in namespace.tools {
                    converted.push(anthropic_function_tool(
                        tool,
                        Some(&namespace.name),
                        tool_names,
                    )?);
                }
            }
            Ok(ResponsesTool::Custom(tool)) => {
                converted.push(anthropic_custom_tool(tool, tool_names)?);
            }
            Ok(ResponsesTool::Unknown) => {}
            Err(err) => {
                return Err(ApiError::Stream(format!(
                    "invalid Anthropic tool object: {err}"
                )));
            }
        }
    }
    Ok(converted)
}

fn anthropic_function_tool(
    tool: ResponsesFunctionTool,
    namespace: Option<&str>,
    tool_names: &mut AnthropicToolNameMap,
) -> Result<AnthropicTool, ApiError> {
    let anthropic_name = anthropic_tool_name(namespace, &tool.name);
    insert_tool_name_mapping(
        tool_names,
        anthropic_name.clone(),
        AnthropicToolName {
            namespace: namespace.map(str::to_string),
            name: tool.name,
        },
    )?;
    Ok(AnthropicTool {
        name: anthropic_name,
        description: tool.description,
        input_schema: tool
            .parameters
            .unwrap_or_else(|| serde_json::json!({"type": "object"})),
        cache_control: None,
    })
}

fn anthropic_custom_tool(
    tool: ResponsesCustomTool,
    tool_names: &mut AnthropicToolNameMap,
) -> Result<AnthropicTool, ApiError> {
    let name = tool.name;
    insert_tool_name_mapping(
        tool_names,
        name.clone(),
        AnthropicToolName {
            namespace: None,
            name: name.clone(),
        },
    )?;
    let input_description = custom_tool_input_description(&name);
    Ok(AnthropicTool {
        name,
        description: tool.description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": input_description
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }),
        cache_control: None,
    })
}

fn custom_tool_input_description(name: &str) -> &'static str {
    if name == "apply_patch" {
        "Raw apply_patch patch text. Do not wrap it in JSON. It must start with `*** Begin Patch`, use only `*** Add File:`, `*** Delete File:`, or `*** Update File:` hunks, and end with `*** End Patch`."
    } else {
        "Raw input for the custom tool."
    }
}

fn anthropic_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if namespace.ends_with('_') || name.starts_with('_') => {
            format!("{namespace}{name}")
        }
        Some(namespace) => format!("{namespace}_{name}"),
        None => name.to_string(),
    }
}

fn apply_cache_breakpoints(request: &mut AnthropicMessagesRequest) {
    let ephemeral = || CacheControl {
        cache_type: CacheType::Ephemeral,
    };

    // Breakpoint 1: system last block
    if let Some(ref mut system_blocks) = request.system
        && let Some(last) = system_blocks.last_mut()
    {
        last.cache_control = Some(ephemeral());
    }

    // Breakpoint 2: tools last
    if let Some(last_tool) = request.tools.last_mut() {
        last_tool.cache_control = Some(ephemeral());
    }

    // Breakpoint 3: second-to-last user message
    let user_indices: Vec<usize> = request
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, AnthropicRole::User))
        .map(|(i, _)| i)
        .collect();

    if user_indices.len() >= 2 {
        let target_idx = user_indices[user_indices.len() - 2];
        let msg = &mut request.messages[target_idx];
        if let Some(block) = msg.content.iter_mut().rev().find(|b| {
            !matches!(
                b,
                AnthropicContentBlock::Thinking { .. }
                    | AnthropicContentBlock::RedactedThinking { .. }
            )
        }) {
            set_cache_control_on_block(block, ephemeral());
        }
    }
}

fn set_cache_control_on_block(block: &mut AnthropicContentBlock, cc: CacheControl) {
    match block {
        AnthropicContentBlock::Text { cache_control, .. }
        | AnthropicContentBlock::Image { cache_control, .. }
        | AnthropicContentBlock::ToolUse { cache_control, .. }
        | AnthropicContentBlock::ToolResult { cache_control, .. } => {
            *cache_control = Some(cc);
        }
        AnthropicContentBlock::Thinking { .. } | AnthropicContentBlock::RedactedThinking { .. } => {
        }
    }
}

fn insert_tool_name_mapping(
    tool_names: &mut AnthropicToolNameMap,
    anthropic_name: String,
    tool_name: AnthropicToolName,
) -> Result<(), ApiError> {
    if let Some(existing) = tool_names.get(&anthropic_name)
        && existing != &tool_name
    {
        return Err(ApiError::Stream(format!(
            "duplicate Anthropic tool name after namespace encoding: {anthropic_name}"
        )));
    }
    tool_names.insert(anthropic_name, tool_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResponsesApiTools;
    use crate::common::ResponsesApiRequest;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use serde_json::value::RawValue;

    fn request(input: Vec<ResponseItem>, tools: Vec<Value>) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "claude-sonnet-4-5".to_string(),
            instructions: "system prompt".to_string(),
            input,
            tools: Some(ResponsesApiTools::from(Arc::from(
                RawValue::from_string(serde_json::to_string(&tools).unwrap()).unwrap(),
            ))),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
            extra_body: HashMap::new(),
        }
    }

    fn body_from(request: ResponsesApiRequest) -> Value {
        anthropic_body_from_responses_request(request, None)
            .expect("Anthropic body should convert")
            .body
    }

    #[test]
    fn converts_text_messages_and_tool_results() {
        let body = body_from(request(
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "hello".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Reasoning {
                    id: None,
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: "think".to_string(),
                    }]),
                    encrypted_content: Some("signature".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "about to call a tool".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"ls\"}".to_string(),
                    call_id: "call-1".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-1".to_string(),
                    output: FunctionCallOutputPayload::from_text("ok".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                    id: None,
                },
            ],
            Vec::new(),
        ));

        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "system prompt", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "think",
                            "signature": "signature"
                        },
                        {
                            "type": "text",
                            "text": "about to call a tool"
                        },
                        {
                            "type": "tool_use",
                            "id": "call-1",
                            "name": "shell",
                            "input": {"cmd": "ls"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call-1",
                        "content": "ok"
                    }]
                },
            ])
        );
    }

    #[test]
    fn preserves_multimodal_tool_results_and_error_status() {
        let body = body_from(request(
            vec![
                ResponseItem::FunctionCall {
                    id: None,
                    name: "Read".to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    call_id: "call-image".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "call-image".to_string(),
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::ContentItems(vec![
                            FunctionCallOutputContentItem::InputText {
                                text: "page 1".to_string(),
                            },
                            FunctionCallOutputContentItem::InputImage {
                                image_url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                                detail: None,
                            },
                        ]),
                        success: Some(false),
                    },
                    internal_chat_message_metadata_passthrough: None,
                    id: None,
                },
            ],
            Vec::new(),
        ));

        let tool_result = &body["messages"][1]["content"][0];
        assert_eq!(tool_result["is_error"], true);
        assert_eq!(tool_result["content"][0]["text"], "page 1");
        assert_eq!(tool_result["content"][1]["type"], "image");
        assert_eq!(tool_result["content"][1]["source"]["type"], "base64");
    }

    #[test]
    fn converts_redacted_reasoning_for_replay() {
        let body = body_from(request(
            vec![ResponseItem::Reasoning {
                id: None,
                summary: Vec::new(),
                content: None,
                encrypted_content: Some("redacted-data".to_string()),
                internal_chat_message_metadata_passthrough: None,
            }],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([{
                "role": "assistant",
                "content": [{"type": "redacted_thinking", "data": "redacted-data"}]
            }])
        );
    }

    #[test]
    fn converts_function_and_namespace_tools() {
        let request_body = anthropic_body_from_responses_request(
            request(
                Vec::new(),
                vec![
                    json!({
                        "type": "function",
                        "name": "shell",
                        "description": "run",
                        "parameters": {"type": "object"}
                    }),
                    json!({
                        "type": "namespace",
                        "name": "mcp__calendar__",
                        "tools": [{
                            "type": "function",
                            "name": "lookup_order",
                            "description": "lookup",
                            "parameters": {"type": "object"}
                        }]
                    }),
                ],
            ),
            None,
        )
        .expect("Anthropic body should convert");

        assert_eq!(
            request_body.body["tools"],
            json!([
                {
                    "name": "shell",
                    "description": "run",
                    "input_schema": {"type": "object"}
                },
                {
                    "name": "mcp__calendar__lookup_order",
                    "description": "lookup",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ])
        );
        assert_eq!(
            request_body.tool_names.get("mcp__calendar__lookup_order"),
            Some(&AnthropicToolName {
                namespace: Some("mcp__calendar__".to_string()),
                name: "lookup_order".to_string(),
            })
        );
    }

    #[test]
    fn converts_custom_tools_to_function_tools() {
        let request_body = anthropic_body_from_responses_request(
            request(
                Vec::new(),
                vec![json!({
                    "type": "custom",
                    "name": "apply_patch",
                    "description": "patch",
                    "format": {"type": "grammar", "syntax": "lark", "definition": "start: /x/"}
                })],
            ),
            None,
        )
        .expect("Anthropic body should convert");

        assert_eq!(
            request_body.body["tools"],
            json!([{
                "name": "apply_patch",
                "description": "patch",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "Raw apply_patch patch text. Do not wrap it in JSON. It must start with `*** Begin Patch`, use only `*** Add File:`, `*** Delete File:`, or `*** Update File:` hunks, and end with `*** End Patch`."
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                },
                "cache_control": {"type": "ephemeral"}
            }])
        );
    }

    #[test]
    fn preserves_custom_tool_call_history_as_function_input() {
        let body = body_from(request(
            vec![ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call-custom".to_string(),
                name: "apply_patch".to_string(),
                input: "*** Begin Patch\n*** End Patch".to_string(),
                internal_chat_message_metadata_passthrough: None,
                namespace: None,
            }],
            Vec::new(),
        ));

        assert_eq!(
            body["messages"],
            json!([{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call-custom",
                    "name": "apply_patch",
                    "input": {"input": "*** Begin Patch\n*** End Patch"}
                }]
            }])
        );
    }

    #[test]
    fn merges_extra_body_but_keeps_streaming_enabled() {
        let mut body = json!({"model": "claude", "stream": true});
        merge_extra_body(
            &mut body,
            &HashMap::from([
                ("max_tokens".to_string(), json!(1024)),
                ("metadata".to_string(), json!({"user_id": "u"})),
            ]),
        )
        .expect("extra body should merge");

        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["metadata"], json!({"user_id": "u"}));

        let err = merge_extra_body(
            &mut body,
            &HashMap::from([("stream".to_string(), json!(false))]),
        )
        .expect_err("stream false should be rejected");
        assert!(err.to_string().contains("stream must remain true"));

        let mut non_object_body = json!("not an object");
        let err = merge_extra_body(
            &mut non_object_body,
            &HashMap::from([("metadata".to_string(), json!({"user_id": "u"}))]),
        )
        .expect_err("non-object body should be rejected");
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn tool_choice_none_omits_tool_choice() {
        let mut request = request(
            Vec::new(),
            vec![json!({
                "type": "function",
                "name": "shell",
                "parameters": {"type": "object"}
            })],
        );
        request.tool_choice = "none".to_string();

        let body = body_from(request);

        assert_eq!(body.get("tool_choice"), None);
    }

    #[test]
    fn maps_reasoning_effort_to_anthropic_thinking() {
        let mut request = request(Vec::new(), Vec::new());
        request.reasoning = Some(Reasoning {
            effort: Some(ReasoningEffort::Low),
            summary: None,
            context: None,
        });

        let body = body_from(request);

        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
    }

    #[test]
    fn missing_reasoning_defaults_to_adaptive_anthropic_thinking() {
        let request = request(Vec::new(), Vec::new());

        let body = body_from(request);

        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    }

    #[test]
    fn reasoning_effort_none_disables_anthropic_thinking() {
        let mut request = request(Vec::new(), Vec::new());
        request.reasoning = Some(Reasoning {
            effort: Some(ReasoningEffort::None),
            summary: None,
            context: None,
        });

        let body = body_from(request);

        assert_eq!(body.get("thinking"), None);
    }

    #[test]
    fn cache_breakpoints_on_system_tools_and_second_to_last_user() {
        let body = body_from(request(
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "first".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "reply".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "second".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            vec![json!({
                "type": "function",
                "name": "shell",
                "parameters": {"type": "object"}
            })],
        ));

        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "system prompt", "cache_control": {"type": "ephemeral"}}])
        );

        assert_eq!(
            body["tools"],
            json!([{
                "name": "shell",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }])
        );

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(messages[2]["content"][0].get("cache_control"), None);
    }

    #[test]
    fn thinking_blocks_never_get_cache_control() {
        let body = body_from(request(
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "first".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Reasoning {
                    id: None,
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: "think".to_string(),
                    }]),
                    encrypted_content: Some("sig".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "reply".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "second".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            Vec::new(),
        ));

        let messages = body["messages"].as_array().unwrap();
        let assistant_msg = &messages[1];
        let content = assistant_msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0].get("cache_control"), None);
        assert_eq!(content[1]["type"], "text");
        // Breakpoint targets user messages, not assistant; assistant text has no cache_control
        assert_eq!(content[1].get("cache_control"), None);
        // First user message (second-to-last user) is the breakpoint target
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn single_turn_skips_message_breakpoint() {
        let body = body_from(request(
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "only".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            Vec::new(),
        ));

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"][0].get("cache_control"), None);
    }

    #[test]
    fn cache_control_none_fields_are_omitted() {
        let body = body_from(request(
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            Vec::new(),
        ));

        let messages = body["messages"].as_array().unwrap();
        let user_content = &messages[0]["content"][0];
        assert!(
            user_content.get("cache_control").is_none(),
            "cache_control should be omitted when None"
        );
    }

    #[test]
    fn base64_image_data_url_uses_base64_source() {
        let body = body_from(request(
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                    detail: None,
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            Vec::new(),
        ));

        let messages = body["messages"].as_array().unwrap();
        let image = &messages[0]["content"][0];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["type"], "base64");
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(image["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn thinking_blocks_are_reordered_before_text() {
        let body = body_from(request(
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "text first".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Reasoning {
                    id: None,
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: "thinking".to_string(),
                    }]),
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            Vec::new(),
        ));

        let messages = body["messages"].as_array().unwrap();
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "text");
    }
}
