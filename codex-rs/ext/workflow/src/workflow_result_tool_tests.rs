use super::*;
use crate::result_artifact::WorkflowResultArtifact;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionItem;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolPayload;
use codex_extension_api::TurnItemEmissionFuture;
use codex_extension_api::TurnItemEmitter;
use codex_extension_api::WorkflowResultReadItem;
use codex_extension_api::WorkflowResultReadStatus;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

#[test]
fn terminal_result_is_inline_when_complete_and_focused() {
    let value = json!({"answer": 42});
    let serialized = serde_json::to_string(&value).unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let chunk = complete_chunk(&serialized);

    let data = WorkflowResultData::from_snapshot_with_chunk(&snapshot, Some(&chunk)).unwrap();

    assert_eq!(data.result, Some(value));
    assert!(data.result_available);
    assert!(data.result_inline);
    assert!(!data.result_truncated);
    assert_eq!(data.result_preview, None);
}

#[test]
fn larger_result_preview_guides_paged_reading() {
    let serialized = serde_json::to_string(&json!({
        "value": "x".repeat(RESULT_INLINE_MAX_BYTES)
    }))
    .unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let chunk = complete_chunk(&serialized);

    let data = WorkflowResultData::from_snapshot_with_chunk(&snapshot, Some(&chunk)).unwrap();

    assert_eq!(data.result, None);
    assert!(data.result_available);
    assert!(!data.result_inline);
    assert!(data.result_truncated);
    assert!(
        data.result_preview
            .as_deref()
            .is_some_and(|preview| preview.contains("use ReadWorkflowResult starting at offset 0"))
    );
}

#[test]
fn omitted_max_bytes_reads_a_large_result_losslessly_across_safe_pages() {
    let result = json!({"value": "x".repeat(27_500)});
    let serialized = serde_json::to_string(&result).unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let requested = requested_result_bytes(None).unwrap();

    assert_eq!(requested, MODEL_TOOL_OUTPUT_MAX_BYTES);
    assert_eq!(
        read_all_chunks(&snapshot, &serialized, requested),
        serialized
    );
}

#[test]
fn caller_chosen_large_max_bytes_is_accepted_with_a_safe_page_bound() {
    assert_eq!(
        requested_result_bytes(Some(256 * 1024)),
        Ok(MODEL_TOOL_OUTPUT_MAX_BYTES)
    );
}

#[test]
fn explicit_smaller_max_bytes_paginates_exact_utf8_result() {
    let result = json!({"value": "你好世界".repeat(200)});
    let serialized = serde_json::to_string(&result).unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let requested = requested_result_bytes(Some(101)).unwrap();
    let assembled = read_all_chunks(&snapshot, &serialized, requested);

    assert_eq!(assembled, serialized);
}

#[test]
fn escaped_chunks_respect_the_final_model_output_bound() {
    let result = json!({"value": "\u{0000}\"\\\n".repeat(8_000)});
    let serialized = serde_json::to_string(&result).unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let mut offset = 0;
    let mut assembled = String::new();
    let requested = requested_result_bytes(None).unwrap();

    loop {
        let output = ReadWorkflowResultOutput::from_chunk(
            &snapshot,
            chunk_at(&serialized, offset, requested),
        )
        .unwrap();
        assert!(serde_json::to_vec(&output).unwrap().len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
        assert!(output.next_offset > offset || output.complete);
        assembled.push_str(&output.chunk);
        offset = output.next_offset;
        if output.complete {
            break;
        }
    }

    assert_eq!(
        serde_json::from_str::<JsonValue>(&assembled).unwrap(),
        result
    );
}

#[test]
fn escaped_page_stays_bounded_in_the_model_response_item() {
    let serialized = serde_json::to_string(&json!({
        "value": "\u{0000}\"\\\n".repeat(8_000)
    }))
    .unwrap();
    let snapshot = snapshot(WorkflowTaskStatus::Completed, serialized.len());
    let page = ReadWorkflowResultOutput::from_chunk(
        &snapshot,
        chunk_at(&serialized, /*offset*/ 0, MODEL_TOOL_OUTPUT_MAX_BYTES),
    )
    .unwrap();
    let value = bounded_json_value(
        READ_WORKFLOW_RESULT_TOOL_NAME,
        &page,
        MODEL_TOOL_OUTPUT_MAX_BYTES,
    )
    .unwrap();
    let response_item = JsonToolOutput::new(value).to_response_item(
        "call-read-result",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    let ResponseInputItem::FunctionCallOutput { output, .. } = response_item else {
        panic!("ReadWorkflowResult should produce a function call output");
    };
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("ReadWorkflowResult should produce model-visible text");
    };

    assert!(text.len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
}

#[test]
fn hard_text_bound_preserves_utf8_boundaries() {
    let value = "你好世界".repeat(100);

    let truncated = truncate_model_text(&value, /*max_bytes*/ 31);

    assert!(truncated.len() <= 31);
    assert!(truncated.ends_with("...[truncated]"));
}

#[test]
fn running_and_paused_results_are_unavailable() {
    for status in [WorkflowTaskStatus::Running, WorkflowTaskStatus::Paused] {
        let snapshot = snapshot(status, /*result_bytes*/ 0);

        let output = ReadWorkflowResultOutput::unavailable(&snapshot);
        let data = WorkflowResultData::from_snapshot(&snapshot).unwrap();

        assert_eq!(
            output,
            ReadWorkflowResultOutput {
                run_id: "wf_result-test".to_string(),
                status,
                available: false,
                encoding: "json",
                chunk: String::new(),
                offset: 0,
                next_offset: 0,
                total_bytes: 0,
                complete: false,
                truncated: false,
            }
        );
        assert_eq!(
            data,
            WorkflowResultData {
                result: None,
                result_available: false,
                result_inline: false,
                result_truncated: false,
                result_preview: None,
                result_bytes: None,
                result_error: None,
                next_action: None,
            }
        );
    }
}

#[test]
fn parse_errors_are_bounded_before_reaching_the_model() {
    let arguments = format!(r#"{{"{}":true}}"#, "unknown".repeat(2_000));

    let Err(FunctionCallError::RespondToModel(message)) = parse_arguments(&arguments) else {
        panic!("oversized unknown field should produce a model-visible parse error");
    };

    assert!(message.len() <= MODEL_ERROR_MAX_BYTES);
    assert!(message.ends_with("...[truncated]"));
}

#[test]
fn spec_exposes_optional_max_bytes_and_incomplete_continuation() {
    let ToolSpec::Function(spec) = read_workflow_result_tool_spec() else {
        panic!("ReadWorkflowResult should be a function tool");
    };
    let properties = spec.parameters.properties.unwrap();

    assert_eq!(spec.name, READ_WORKFLOW_RESULT_TOOL_NAME);
    assert_eq!(spec.parameters.required, Some(vec!["runId".to_string()]));
    assert!(properties.contains_key("maxBytes"));
    assert!(
        properties["offset"]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("nextOffset"))
    );
    assert!(spec.description.contains("choose maxBytes"));
    assert!(spec.description.contains("only while complete is false"));
}

#[tokio::test]
async fn missing_result_emits_a_failed_terminal_lifecycle_item() {
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let emitter = Arc::new(RecordingEmitter::default());
    let executor = ReadWorkflowResultToolExecutor::new(
        thread_id,
        WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new()),
    );
    let invocation = ToolCall {
        turn_id: "turn-read-result".to_string(),
        call_id: "call-read-result".to_string(),
        tool_name: ToolName::plain(READ_WORKFLOW_RESULT_TOOL_NAME),
        model: "gpt-test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(1024),
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: emitter.clone(),
        environments: Vec::new(),
        agent_configuration: None,
        payload: ToolPayload::Function {
            arguments: json!({"runId": "wf_missing"}).to_string(),
        },
    };

    assert!(executor.handle(invocation).await.is_err());

    let items = emitter.items.lock().unwrap();
    assert_eq!(
        *items,
        vec![
            ExtensionItem::WorkflowResultRead(WorkflowResultReadItem {
                id: "call-read-result".to_string(),
                run_id: Some("wf_missing".to_string()),
                status: WorkflowResultReadStatus::InProgress,
            }),
            ExtensionItem::WorkflowResultRead(WorkflowResultReadItem {
                id: "call-read-result".to_string(),
                run_id: Some("wf_missing".to_string()),
                status: WorkflowResultReadStatus::Failed,
            }),
        ]
    );
}

#[tokio::test]
async fn invalid_arguments_emit_a_failed_lifecycle_with_available_identity() {
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let executor = ReadWorkflowResultToolExecutor::new(
        thread_id,
        WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new()),
    );

    let invalid_arguments = [
        ("{".to_string(), None),
        ("{}".to_string(), None),
        (json!({"runId": 1}).to_string(), None),
        (
            json!({"runId": "wf_invalid", "maxBytes": 0}).to_string(),
            Some("wf_invalid"),
        ),
    ];
    for (index, (arguments, run_id)) in invalid_arguments.into_iter().enumerate() {
        let call_id = format!("call-invalid-{index}");
        let emitter = Arc::new(RecordingEmitter::default());
        let invocation = ToolCall {
            turn_id: "turn-read-result".to_string(),
            call_id: call_id.clone(),
            tool_name: ToolName::plain(READ_WORKFLOW_RESULT_TOOL_NAME),
            model: "gpt-test".to_string(),
            codex_turn_metadata: None,
            truncation_policy: TruncationPolicy::Bytes(1024),
            conversation_history: ConversationHistory::default(),
            turn_item_emitter: emitter.clone(),
            environments: Vec::new(),
            agent_configuration: None,
            payload: ToolPayload::Function { arguments },
        };

        assert!(executor.handle(invocation).await.is_err());
        assert_eq!(
            *emitter.items.lock().unwrap(),
            vec![
                ExtensionItem::WorkflowResultRead(WorkflowResultReadItem {
                    id: call_id.clone(),
                    run_id: run_id.map(str::to_string),
                    status: WorkflowResultReadStatus::InProgress,
                }),
                ExtensionItem::WorkflowResultRead(WorkflowResultReadItem {
                    id: call_id,
                    run_id: run_id.map(str::to_string),
                    status: WorkflowResultReadStatus::Failed,
                }),
            ]
        );
    }
}

#[derive(Default)]
struct RecordingEmitter {
    items: Mutex<Vec<ExtensionItem>>,
}

impl TurnItemEmitter for RecordingEmitter {
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        self.items.lock().unwrap().push(item.item);
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        self.items.lock().unwrap().push(item.item);
        Box::pin(std::future::ready(()))
    }
}

fn complete_chunk(serialized: &str) -> WorkflowResultChunk {
    let total_bytes = u64::try_from(serialized.len()).unwrap();
    WorkflowResultChunk {
        text: serialized.to_string(),
        offset: 0,
        next_offset: total_bytes,
        total_bytes,
    }
}

fn chunk_at(serialized: &str, offset: u64, max_bytes: usize) -> WorkflowResultChunk {
    let start = usize::try_from(offset).unwrap();
    let mut end = start.saturating_add(max_bytes).min(serialized.len());
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    WorkflowResultChunk {
        text: serialized[start..end].to_string(),
        offset,
        next_offset: u64::try_from(end).unwrap(),
        total_bytes: u64::try_from(serialized.len()).unwrap(),
    }
}

fn read_all_chunks(snapshot: &WorkflowTaskSnapshot, serialized: &str, max_bytes: usize) -> String {
    let mut offset = 0;
    let mut assembled = String::new();
    loop {
        let output =
            ReadWorkflowResultOutput::from_chunk(snapshot, chunk_at(serialized, offset, max_bytes))
                .unwrap();
        assert_eq!(output.offset, offset);
        assert!(output.chunk.len() <= max_bytes);
        assert!(serde_json::to_vec(&output).unwrap().len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
        assembled.push_str(&output.chunk);
        offset = output.next_offset;
        if output.complete {
            assert!(!output.truncated);
            return assembled;
        }
        assert!(output.truncated);
    }
}

fn snapshot(status: WorkflowTaskStatus, result_bytes: usize) -> WorkflowTaskSnapshot {
    let root = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_result-test".to_string(),
        workflow_name: "result-test".to_string(),
        title: None,
        status,
        summary: "summary".to_string(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: workflow_result_is_available(status).then_some(WorkflowResultArtifact {
            sha256: "0".repeat(64),
            bytes: u64::try_from(result_bytes).unwrap(),
            storage_id: "0".repeat(32),
        }),
        output_file: root.join("workflow.json"),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: None,
        started_at: 1,
        completed_at: workflow_result_is_available(status).then_some(2),
        script_sha256: "sha256".to_string(),
    }
}
