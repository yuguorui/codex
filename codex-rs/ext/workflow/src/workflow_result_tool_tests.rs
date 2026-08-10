use super::*;
use crate::result_artifact::WorkflowResultArtifact;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionItem;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolCallSource;
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
use codex_utils_path_uri::PathUri;
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
    assert!(data.result_preview.as_deref().is_some_and(|preview| {
        preview.contains("page with ReadWorkflowResult starting at offset 0")
    }));
    assert!(
        data.result_preview
            .as_deref()
            .is_some_and(|preview| preview.contains("pass writePath"))
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
fn written_result_reports_metadata_without_inlining_content() {
    let snapshot = snapshot(WorkflowTaskStatus::Completed, /*result_bytes*/ 12);
    let root = PathUri::from_abs_path(&AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap());
    let write = WorkflowResultWrite {
        path: root.join("reports/result.json").unwrap(),
        bytes: 12,
        sha256: "0".repeat(64),
    };

    let output = ReadWorkflowResultOutput::from_write(&snapshot, write, None).unwrap();

    assert_eq!(
        output,
        ReadWorkflowResultOutput {
            run_id: snapshot.run_id.clone(),
            status: snapshot.status,
            available: true,
            encoding: "json",
            chunk: String::new(),
            offset: 0,
            next_offset: 12,
            total_bytes: 12,
            complete: true,
            truncated: false,
            written: true,
            write_path: Some(
                root.join("reports/result.json")
                    .unwrap()
                    .inferred_native_path_string(),
            ),
            json_pointer: None,
            value: None,
            sha256: Some("0".repeat(64)),
            next_action: None,
        }
    );
}

#[test]
fn projected_result_returns_the_selected_value_and_its_digest() {
    let snapshot = snapshot(WorkflowTaskStatus::Completed, /*result_bytes*/ 10);
    let projected = crate::workflow_result_projection::project_workflow_result(
        r#"{"answer":{"value":"chosen"}}"#,
        "/answer/value",
    )
    .unwrap();
    let output = ReadWorkflowResultOutput::from_projection(
        &snapshot, &projected, /*include_value*/ true,
    );

    assert_eq!(output.json_pointer.as_deref(), Some("/answer/value"));
    assert_eq!(output.value.as_ref(), Some(&json!("chosen")));
    assert_eq!(output.chunk, "");
    assert_eq!(output.total_bytes, "\"chosen\"".len() as u64);
    assert_eq!(output.complete, true);
    assert_eq!(output.truncated, false);
    assert_eq!(output.sha256.as_deref(), Some(projected.sha256.as_str()));
}

#[test]
fn maximum_length_projection_pointer_stays_within_the_output_schema() {
    let snapshot = snapshot(WorkflowTaskStatus::Completed, /*result_bytes*/ 10);
    let key = "k".repeat(511);
    let projected = crate::workflow_result_projection::project_workflow_result(
        &format!("{{\"{key}\":\"chosen\"}}"),
        &format!("/{key}"),
    )
    .unwrap();
    let output = ReadWorkflowResultOutput::from_projected_result(&snapshot, &projected).unwrap();

    assert_eq!(
        output.json_pointer.as_deref(),
        Some(format!("/{key}").as_str())
    );
    assert_eq!(output.value.as_ref(), Some(&json!("chosen")));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::MODEL_TOOL_OUTPUT_MAX_BYTES
    );
    validate_read_result_output_schema(&output);
}

#[test]
fn oversized_projection_returns_metadata_and_a_write_next_action() {
    let snapshot = snapshot(WorkflowTaskStatus::Completed, /*result_bytes*/ 10);
    let projected = crate::workflow_result_projection::project_workflow_result(
        &serde_json::to_string(&json!({"value": "x".repeat(4_000)})).unwrap(),
        "/value",
    )
    .unwrap();
    let output = ReadWorkflowResultOutput::from_projected_result(&snapshot, &projected).unwrap();

    assert_eq!(output.value, None);
    assert_eq!(output.truncated, true);
    assert!(output.next_action.as_deref().is_some_and(|action| {
        action.contains("same jsonPointer") && action.contains("writePath")
    }));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::MODEL_TOOL_OUTPUT_MAX_BYTES
    );
    validate_read_result_output_schema(&output);
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
                written: false,
                write_path: None,
                json_pointer: None,
                value: None,
                sha256: None,
                next_action: None,
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
                result_written: false,
                result_write_path: None,
                result_sha256: None,
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
    assert!(properties.contains_key("writePath"));
    assert!(properties.contains_key("jsonPointer"));
    assert!(
        properties["writePath"]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("selected execution environment"))
    );
    assert!(
        properties["jsonPointer"]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("RFC 6901"))
    );
    assert!(
        properties["offset"]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("nextOffset"))
    );
    assert!(spec.description.contains("choose maxBytes"));
    assert!(spec.description.contains("only while complete is false"));
    assert!(
        spec.description
            .contains("project one value with RFC 6901 jsonPointer")
    );
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
        source: ToolCallSource::Direct,
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
        (
            json!({"runId": "wf_invalid", "writePath": "result.json", "offset": 0}).to_string(),
            Some("wf_invalid"),
        ),
        (
            json!({"runId": "wf_invalid", "jsonPointer": "/answer", "offset": 0}).to_string(),
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
            source: ToolCallSource::Direct,
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

fn validate_read_result_output_schema(output: &ReadWorkflowResultOutput) {
    let ToolSpec::Function(spec) = read_workflow_result_tool_spec() else {
        panic!("ReadWorkflowResult should be a function tool");
    };
    let schema = spec
        .output_schema
        .expect("ReadWorkflowResult output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serde_json::to_value(output).unwrap()));
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

#[test]
fn result_availability_follows_the_artifact_not_the_status_alone() {
    // A terminal run whose inline read failed still has a result the model can page
    // through, so no view may report it unavailable while another offers a read. A run
    // that ended without an artifact has nothing at any offset, so no view may offer one.
    let readable = snapshot(WorkflowTaskStatus::Failed, 4);
    let mut ended_without_artifact = snapshot(WorkflowTaskStatus::Failed, 4);
    ended_without_artifact.result_artifact = None;
    let running = snapshot(WorkflowTaskStatus::Running, 4);

    assert!(run_result_is_available(&readable));
    assert!(!run_result_is_available(&ended_without_artifact));
    assert!(!run_result_is_available(&running));

    // Chunk-free wait views: a read failure keeps the result reachable and says how,
    // while a missing artifact drops the flag and the hint that would point at nothing.
    let failed_read = WorkflowResultData::without_chunk(&readable, Some("inline read failed"));
    assert!(failed_read.result_available);
    assert_eq!(
        failed_read.next_action.as_deref(),
        Some("ReadWorkflowResult: offset=0.")
    );
    let no_artifact =
        WorkflowResultData::without_chunk(&ended_without_artifact, Some("no result artifact"));
    assert!(!no_artifact.result_available);
    assert_eq!(no_artifact.next_action, None);
    assert!(!WorkflowResultData::without_chunk(&running, None).result_available);

    // The write views share the rule, so a missing artifact is never advertised as
    // something a repeated write could produce, and never gets a retry hint.
    let failed_write = WorkflowResultData::from_write_error(&readable, "disk full");
    assert!(failed_write.result_available);
    assert_eq!(
        failed_write.next_action.as_deref(),
        Some("Fix writePath and repeat WaitWorkflow.")
    );
    let nothing_to_write =
        WorkflowResultData::from_write_error(&ended_without_artifact, "no result artifact");
    assert!(!nothing_to_write.result_available);
    assert_eq!(nothing_to_write.next_action, None);

    // The exception that proves the rule is about artifacts, not status: this view is
    // holding the content, so it reports available even with no artifact metadata.
    let inline = WorkflowResultData::from_snapshot_with_result(
        &ended_without_artifact,
        Some(&complete_chunk("null")),
        None,
    )
    .unwrap();
    assert!(inline.result_available);
    assert!(inline.result_inline);
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
