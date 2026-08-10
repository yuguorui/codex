use codex_extension_api::ExtensionItem;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolAvailability;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::WorkflowResultReadStatus;
use codex_protocol::ThreadId;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::result_artifact::WorkflowResultChunk;
use crate::service::WorkflowService;
use crate::service::WorkflowTaskSnapshot;

pub const READ_WORKFLOW_RESULT_TOOL_NAME: &str = "ReadWorkflowResult";
pub(crate) const MODEL_TOOL_OUTPUT_MAX_BYTES: usize = 3_500;
pub(crate) const WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES: usize = 768;
pub(crate) const MODEL_ERROR_MAX_BYTES: usize = 384;
pub(super) const RESULT_INLINE_MAX_BYTES: usize = 256;
pub(super) const RESULT_PREVIEW_MAX_BYTES: usize = 192;
const WAIT_OUTPUT_TEXT_MAX_BYTES: usize = 96;

#[derive(Clone)]
pub(crate) struct ReadWorkflowResultToolExecutor {
    thread_id: ThreadId,
    service: WorkflowService,
}

impl ReadWorkflowResultToolExecutor {
    pub(crate) fn new(thread_id: ThreadId, service: WorkflowService) -> Self {
        Self { thread_id, service }
    }
}

impl ToolExecutor<ToolCall> for ReadWorkflowResultToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_WORKFLOW_RESULT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        read_workflow_result_tool_spec()
    }

    fn availability(&self) -> ToolAvailability {
        ToolAvailability::RootSessionOnly
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args = invocation.function_arguments().and_then(parse_arguments);
            let item = ExtensionTurnItem::workflow_result_read(
                invocation.call_id.clone(),
                args.as_ref().ok().map(|args| args.run_id.clone()),
            );
            invocation
                .turn_item_emitter
                .emit_started(item.clone())
                .await;
            let result = async {
                let args = args?;
                let max_bytes =
                    requested_result_bytes(args.max_bytes).map_err(model_bounded_error)?;
                let outcome = self
                    .service
                    .wait_for_terminal(self.thread_id, &args.run_id, Duration::ZERO)
                    .await
                    .map_err(model_bounded_error)?;
                let output = if workflow_result_is_available(outcome.snapshot.status) {
                    let chunk = self
                        .service
                        .read_result_chunk(
                            self.thread_id,
                            &outcome.snapshot,
                            args.offset.unwrap_or(0),
                            max_bytes,
                        )
                        .await
                        .map_err(model_bounded_error)?;
                    ReadWorkflowResultOutput::from_chunk(&outcome.snapshot, chunk)
                } else {
                    Ok(ReadWorkflowResultOutput::unavailable(&outcome.snapshot))
                }
                .map_err(model_bounded_error)?;
                bounded_json_value(
                    READ_WORKFLOW_RESULT_TOOL_NAME,
                    &output,
                    MODEL_TOOL_OUTPUT_MAX_BYTES,
                )
            }
            .await;
            let status = if result.is_ok() {
                WorkflowResultReadStatus::Completed
            } else {
                WorkflowResultReadStatus::Failed
            };
            let mut item = item;
            let ExtensionItem::WorkflowResultRead(read) = &mut item.item else {
                unreachable!("ReadWorkflowResult must emit a Workflow result read item");
            };
            read.status = status;
            invocation.turn_item_emitter.emit_completed(item).await;
            let value = result?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadWorkflowResultArgs {
    run_id: String,
    offset: Option<u64>,
    max_bytes: Option<usize>,
}

fn requested_result_bytes(max_bytes: Option<usize>) -> Result<usize, &'static str> {
    match max_bytes {
        Some(0) => Err("choose a positive maxBytes value"),
        Some(max_bytes) => Ok(max_bytes.min(MODEL_TOOL_OUTPUT_MAX_BYTES)),
        None => Ok(MODEL_TOOL_OUTPUT_MAX_BYTES),
    }
}

fn parse_arguments(arguments: &str) -> Result<ReadWorkflowResultArgs, FunctionCallError> {
    serde_json::from_str(arguments).map_err(|error| {
        model_bounded_error(format_args!(
            "invalid {READ_WORKFLOW_RESULT_TOOL_NAME} input: {error}"
        ))
    })
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ReadWorkflowResultOutput {
    run_id: String,
    status: WorkflowTaskStatus,
    available: bool,
    encoding: &'static str,
    chunk: String,
    offset: u64,
    next_offset: u64,
    total_bytes: u64,
    complete: bool,
    truncated: bool,
}

impl ReadWorkflowResultOutput {
    fn unavailable(snapshot: &WorkflowTaskSnapshot) -> Self {
        Self {
            run_id: snapshot.run_id.clone(),
            status: snapshot.status,
            available: false,
            encoding: "json",
            chunk: String::new(),
            offset: 0,
            next_offset: 0,
            total_bytes: 0,
            complete: false,
            truncated: false,
        }
    }

    fn from_chunk(
        snapshot: &WorkflowTaskSnapshot,
        chunk: WorkflowResultChunk,
    ) -> Result<Self, String> {
        let requested = Self::terminal_chunk(snapshot, &chunk, chunk.text.len())?;
        if serialized_output_len(&requested)? <= MODEL_TOOL_OUTPUT_MAX_BYTES {
            return Ok(requested);
        }

        let mut boundaries = vec![0];
        boundaries.extend(
            chunk
                .text
                .char_indices()
                .skip(1)
                .map(|(relative, _)| relative),
        );
        if boundaries.last().copied() != Some(chunk.text.len()) {
            boundaries.push(chunk.text.len());
        }

        let mut first_too_large = 0;
        let mut end = boundaries.len();
        while first_too_large < end {
            let middle = first_too_large + (end - first_too_large) / 2;
            let candidate = Self::terminal_chunk(snapshot, &chunk, boundaries[middle])?;
            if serialized_output_len(&candidate)? <= MODEL_TOOL_OUTPUT_MAX_BYTES {
                first_too_large = middle + 1;
            } else {
                end = middle;
            }
        }
        let selected = first_too_large.saturating_sub(1);
        let selected_bytes = boundaries[selected];
        if selected_bytes == 0 && chunk.offset < chunk.total_bytes {
            return Err("continue reading from the returned nextOffset".to_string());
        }
        Self::terminal_chunk(snapshot, &chunk, selected_bytes)
    }

    fn terminal_chunk(
        snapshot: &WorkflowTaskSnapshot,
        chunk: &WorkflowResultChunk,
        selected_bytes: usize,
    ) -> Result<Self, String> {
        let selected_bytes_u64 = u64::try_from(selected_bytes).map_err(|error| {
            format!("workflow result page offset is not representable: {error}")
        })?;
        let next_offset = chunk.offset.saturating_add(selected_bytes_u64);
        let complete = next_offset == chunk.total_bytes;
        Ok(Self {
            run_id: snapshot.run_id.clone(),
            status: snapshot.status,
            available: true,
            encoding: "json",
            chunk: chunk.text[..selected_bytes].to_string(),
            offset: chunk.offset,
            next_offset,
            total_bytes: chunk.total_bytes,
            complete,
            truncated: !complete,
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkflowResultData {
    result: Option<JsonValue>,
    result_available: bool,
    result_inline: bool,
    result_truncated: bool,
    result_preview: Option<String>,
    result_bytes: Option<u64>,
    result_error: Option<String>,
    next_action: Option<String>,
}

impl WorkflowResultData {
    #[cfg(test)]
    pub(super) fn from_snapshot(snapshot: &WorkflowTaskSnapshot) -> serde_json::Result<Self> {
        Ok(Self::unavailable(snapshot))
    }

    #[cfg(test)]
    pub(super) fn from_snapshot_with_chunk(
        snapshot: &WorkflowTaskSnapshot,
        chunk: Option<&WorkflowResultChunk>,
    ) -> serde_json::Result<Self> {
        Self::from_snapshot_with_result(snapshot, chunk, None)
    }

    pub(super) fn from_snapshot_with_result(
        snapshot: &WorkflowTaskSnapshot,
        chunk: Option<&WorkflowResultChunk>,
        result_error: Option<&str>,
    ) -> serde_json::Result<Self> {
        if !workflow_result_is_available(snapshot.status) {
            return Ok(Self::unavailable(snapshot));
        }
        let Some(chunk) = chunk else {
            let mut unavailable = Self::unavailable(snapshot);
            unavailable.result_error =
                result_error.map(|error| truncate_model_text(error, WAIT_OUTPUT_TEXT_MAX_BYTES));
            if result_error.is_some() {
                unavailable.next_action = Some(format!(
                    "Call ReadWorkflowResult with runId {:?} and offset 0.",
                    snapshot.run_id
                ));
            }
            return Ok(unavailable);
        };
        if chunk.complete() && chunk.total_bytes <= RESULT_INLINE_MAX_BYTES as u64 {
            return Ok(Self {
                result: Some(serde_json::from_str(&chunk.text)?),
                result_available: true,
                result_inline: true,
                result_truncated: false,
                result_preview: None,
                result_bytes: Some(chunk.total_bytes),
                result_error: None,
                next_action: None,
            });
        }

        let preview = truncate_model_text(&chunk.text, RESULT_PREVIEW_MAX_BYTES);
        Ok(Self {
            result: None,
            result_available: true,
            result_inline: false,
            result_truncated: true,
            result_preview: Some(format!(
                "JSON text preview; use ReadWorkflowResult starting at offset 0 for the complete content:\n{preview}"
            )),
            result_bytes: Some(chunk.total_bytes),
            result_error: None,
            next_action: None,
        })
    }

    pub(super) fn compact_for_wait(&mut self, run_id: &str) {
        self.result = None;
        self.result_preview = None;
        if self.result_available {
            self.result_inline = false;
            self.result_truncated = true;
            self.next_action = Some(format!(
                "Call ReadWorkflowResult with runId {run_id:?} and offset 0."
            ));
        }
        self.result_error = self
            .result_error
            .as_deref()
            .map(|error| truncate_model_text(error, /*max_bytes*/ 32));
    }

    fn unavailable(_snapshot: &WorkflowTaskSnapshot) -> Self {
        Self {
            result: None,
            result_available: false,
            result_inline: false,
            result_truncated: false,
            result_preview: None,
            result_bytes: None,
            result_error: None,
            next_action: None,
        }
    }
}

pub(super) fn bounded_output_text(value: &str) -> String {
    truncate_model_text(value, WAIT_OUTPUT_TEXT_MAX_BYTES)
}

pub(crate) fn truncate_model_text(value: &str, max_bytes: usize) -> String {
    const MARKER: &str = "...[truncated]";

    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes <= MARKER.len() {
        return value[..floor_char_boundary(value, max_bytes)].to_string();
    }
    let prefix_end = floor_char_boundary(value, max_bytes - MARKER.len());
    let prefix = &value[..prefix_end];
    format!("{prefix}{MARKER}")
}

pub(crate) fn model_bounded_error(message: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(truncate_model_text(
        &message.to_string(),
        MODEL_ERROR_MAX_BYTES,
    ))
}

pub(crate) fn model_bounded_json_value<T>(
    tool_name: &str,
    output: &T,
) -> Result<JsonValue, FunctionCallError>
where
    T: Serialize,
{
    bounded_json_value(tool_name, output, MODEL_TOOL_OUTPUT_MAX_BYTES)
}

pub(crate) fn model_bounded_json_value_with_limit<T>(
    tool_name: &str,
    output: &T,
    max_bytes: usize,
) -> Result<JsonValue, FunctionCallError>
where
    T: Serialize,
{
    bounded_json_value(tool_name, output, max_bytes)
}

fn bounded_json_value<T>(
    tool_name: &str,
    output: &T,
    max_bytes: usize,
) -> Result<JsonValue, FunctionCallError>
where
    T: Serialize,
{
    let value = serde_json::to_value(output).map_err(|error| {
        model_bounded_error(format_args!(
            "failed to serialize {tool_name} output: {error}"
        ))
    })?;
    let output_bytes = serde_json::to_vec(&value).map_err(|error| {
        model_bounded_error(format_args!(
            "failed to measure {tool_name} output: {error}"
        ))
    })?;
    if output_bytes.len() > max_bytes {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} should return a focused response; use the available continuation or filtering fields"
        )));
    }
    Ok(value)
}

fn serialized_output_len<T>(output: &T) -> Result<usize, String>
where
    T: Serialize,
{
    serde_json::to_vec(output)
        .map(|serialized| serialized.len())
        .map_err(|error| format!("failed to measure workflow tool output: {error}"))
}

pub(super) fn workflow_result_is_available(status: WorkflowTaskStatus) -> bool {
    match status {
        WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running | WorkflowTaskStatus::Paused => {
            false
        }
        WorkflowTaskStatus::Completed | WorkflowTaskStatus::Failed | WorkflowTaskStatus::Killed => {
            true
        }
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn read_workflow_result_tool_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "runId".to_string(),
            JsonSchema::string(Some("Workflow run id owned by this thread.".to_string())),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some(
                "Zero-based UTF-8 byte offset. Continue with the previous nextOffset.".to_string(),
            )),
        ),
        (
            "maxBytes".to_string(),
            JsonSchema::integer(Some(
                "Desired number of UTF-8 result bytes for this call. Omit it to read as much of the remaining result as fits safely."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: READ_WORKFLOW_RESULT_TOOL_NAME.to_string(),
        description: "Read a terminal Workflow's serialized JSON result by runId. The caller may choose maxBytes. Start with offset 0 and continue from nextOffset only while complete is false."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["runId".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "runId": { "type": "string" },
                "status": {
                    "enum": ["pending", "running", "completed", "failed", "paused", "killed"]
                },
                "available": { "type": "boolean" },
                "encoding": { "enum": ["json"] },
                "chunk": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "nextOffset": { "type": "integer", "minimum": 0 },
                "totalBytes": { "type": "integer", "minimum": 0 },
                "complete": { "type": "boolean" },
                "truncated": { "type": "boolean" }
            },
            "required": [
                "runId",
                "status",
                "available",
                "encoding",
                "chunk",
                "offset",
                "nextOffset",
                "totalBytes",
                "complete",
                "truncated"
            ],
            "additionalProperties": false
        })),
    })
}

#[cfg(test)]
#[path = "workflow_result_tool_tests.rs"]
mod tests;
