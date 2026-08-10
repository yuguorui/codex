use super::*;
use crate::wait_text::COMPACT_WAIT_TEXT_MAX_BYTES;
use crate::wait_text::WAIT_ERROR_TEXT_MAX_BYTES;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

/// Test-side adapter for the wait output builder.
///
/// The production builder takes already-resolved `WorkflowResultData` so it stays
/// total; these assertions are written against the chunk/write vocabulary, so the
/// adaptation lives here instead of in the implementation.
#[allow(clippy::too_many_arguments)]
fn from_outcome_with_result_chunk(
    outcome: WorkflowWaitOutcome,
    timeout_ms: i64,
    interrupted_by_user_input: bool,
    result_chunk: Option<&crate::result_artifact::WorkflowResultChunk>,
    result_error: Option<&str>,
    written_result: Option<&crate::workflow_result_write::WorkflowResultWrite>,
    write_error: Option<&str>,
) -> serde_json::Result<WaitWorkflowOutput> {
    let result_data = if let Some(write) = written_result {
        WorkflowResultData::from_written_result(&outcome.snapshot, write)
    } else if let Some(error) = write_error {
        WorkflowResultData::from_write_error(&outcome.snapshot, error)
    } else {
        WorkflowResultData::from_snapshot_with_result(
            &outcome.snapshot,
            result_chunk,
            result_error,
        )?
    };
    WaitWorkflowOutput::from_outcome_with_result(
        outcome,
        timeout_ms,
        interrupted_by_user_input,
        result_data,
    )
}

#[tokio::test]
async fn spec_requires_run_id_without_exposing_timeout_capacity_values() {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    config.multi_agent_v2.min_wait_timeout_ms = 25;
    config.multi_agent_v2.max_wait_timeout_ms = 250;
    config.multi_agent_v2.default_wait_timeout_ms = 100;

    let ToolSpec::Function(spec) = wait_workflow_tool_spec(&config) else {
        panic!("WaitWorkflow should be a function tool");
    };

    assert_eq!(spec.name, WAIT_WORKFLOW_TOOL_NAME);
    assert_eq!(spec.parameters.required, Some(vec!["runId".to_string()]));
    let properties = spec.parameters.properties.unwrap();
    assert_eq!(
        properties.keys().cloned().collect::<Vec<_>>(),
        vec![
            "runId".to_string(),
            "timeoutMs".to_string(),
            "writePath".to_string(),
        ]
    );
    let timeout_description = properties["timeoutMs"].description.as_deref().unwrap();
    assert!(timeout_description.contains("configured default"));
    assert!(!timeout_description.contains("100"));
    let output_schema = spec.output_schema.expect("WaitWorkflow output schema");
    assert_eq!(
        output_schema["required"],
        json!([
            "runId",
            "workflowName",
            "status",
            "summary",
            "error",
            "failureCount",
            "usage",
            "completedAt",
            "timedOut",
            "interruptedByUserInput",
            "timeoutMs",
            "recovery",
            "result",
            "resultAvailable",
            "resultInline",
            "resultTruncated",
            "resultPreview",
            "resultBytes",
            "resultError",
            "resultWritten",
            "resultWritePath",
            "resultSha256",
            "nextAction"
        ])
    );
}

#[tokio::test]
async fn output_bounds_text_and_returns_the_terminal_result() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: "null".to_string(),
        offset: 0,
        next_offset: 4,
        total_bytes: 4,
    };
    let output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_test".to_string(),
                workflow_name: "test".to_string(),
                title: None,
                status: WorkflowTaskStatus::Failed,
                summary: "summary ".repeat(2_000),
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                    sha256: "0".repeat(64),
                    bytes: 4,
                    storage_id: "0".repeat(32),
                }),
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: vec!["failure".to_string()],
                error: Some("error ".repeat(2_000)),
                started_at: 1,
                completed_at: Some(2),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        Some(&result_chunk),
        /*result_error*/ None,
        /*written_result*/ None,
        /*write_error*/ None,
    )
    .unwrap();

    assert_eq!(output.run_id, "wf_test");
    assert_eq!(output.failure_count, 1);
    assert!(!output.interrupted_by_user_input);
    assert!(output.summary.len() < "summary ".repeat(2_000).len());
    assert!(output.error.as_deref().unwrap().len() < "error ".repeat(2_000).len());
    let serialized = serde_json::to_value(&output).unwrap();
    assert_eq!(
        serialized,
        json!({
            "runId": "wf_test",
            "workflowName": "test",
            "status": "failed",
            "summary": output.summary,
            "error": output.error,
            "failureCount": 1,
            "usage": {
                "totalTokens": 0,
                "toolUses": 0,
                "durationMs": 0,
                "agentCount": 0,
                "successfulAgentCount": 0,
                "failedAgentCount": 0,
                "skippedAgentCount": 0,
                "nullAgentResultCount": 0
            },
            "completedAt": 2,
            "timedOut": false,
            "interruptedByUserInput": false,
            "timeoutMs": 100,
            "recovery": {
                "recoveryEligible": true,
                "reason": "failed",
                "mayRequireReapproval": true,
                "identityRequirements": ["sameApprovedWorkflowIdentity"],
                "observedRestoreIncompatibilities": []
            },
            "result": null,
            "resultAvailable": true,
            // The complete 4-byte result stays inline: trading it for a read hint would
            // grow the item and cost the model a call it does not need. Only the
            // re-obtainable identity text gives way here.
            "resultInline": true,
            "resultTruncated": false,
            "resultPreview": null,
            "resultBytes": 4,
            "resultError": null,
            "resultWritten": false,
            "resultWritePath": null,
            "resultSha256": null,
            "nextAction": null
        })
    );
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let ToolSpec::Function(spec) = wait_workflow_tool_spec(&config) else {
        panic!("WaitWorkflow should be a function tool");
    };
    let schema = spec.output_schema.expect("WaitWorkflow output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serialized));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES
    );
}

#[test]
fn low_compression_wait_output_stays_below_the_context_item_cap() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let low_compression = (0..8_000)
        .map(|index| char::from_u32(0x21 + (index * 73 % 90)).unwrap())
        .collect::<String>();
    let mut snapshot = crate::service::WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_low-compression".to_string(),
        workflow_name: low_compression.clone(),
        title: None,
        status: WorkflowTaskStatus::Failed,
        summary: low_compression.clone(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
            sha256: "0".repeat(64),
            bytes: 4,
            storage_id: "0".repeat(32),
        }),
        output_file: root.join(low_compression),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: Some("!@#$%^&*()[]{}<>?/|\\".repeat(400)),
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    };
    snapshot.output_file = root.join("workflow.json");
    let mut output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot,
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        /*result_chunk*/ None,
        Some("corrupt ".repeat(1_000).as_str()),
        /*written_result*/ None,
        /*write_error*/ None,
    )
    .unwrap();

    // The builder compacts proactively; the ladder is the backstop that keeps the
    // response inside the fixed cap now that a readable error is never stubbed.
    bound_wait_workflow_output(&mut output).unwrap();

    let serialized_bytes = serde_json::to_vec(&output).unwrap().len();
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    // The inline read failed, but the artifact is still there to page through, which is
    // exactly what the `nextAction` below asks for.
    assert_eq!(result_data["resultAvailable"], true);
    assert!(result_data["resultError"].as_str().is_some());
    assert_eq!(result_data["nextAction"], "ReadWorkflowResult: offset=0.");
    assert!(!result_data["nextAction"].as_str().unwrap().contains('/'));
    assert!(
        serialized_bytes <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES,
        "compacted wait output must fit the fixed cap: {serialized_bytes} bytes"
    );
}

#[test]
fn failed_run_error_survives_wait_compaction() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let low_compression = (0..8_000)
        .map(|index| char::from_u32(0x21 + (index * 73 % 90)).unwrap())
        .collect::<String>();
    let error = format!("workflow runtime failed: {low_compression}");
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: low_compression.clone(),
        offset: 0,
        next_offset: 8_000,
        total_bytes: 64_000,
    };
    let output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_error-compaction".to_string(),
                workflow_name: low_compression.clone(),
                title: None,
                status: WorkflowTaskStatus::Failed,
                summary: low_compression,
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                    sha256: "0".repeat(64),
                    bytes: 64_000,
                    storage_id: "0".repeat(32),
                }),
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: Vec::new(),
                error: Some(error.clone()),
                started_at: 1,
                completed_at: Some(2),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        Some(&result_chunk),
        /*result_error*/ None,
        /*written_result*/ None,
        /*write_error*/ None,
    )
    .unwrap();

    // The response is compacted, and the summary is reduced to a stub.
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES
    );
    assert_eq!(output.summary.len(), COMPACT_WAIT_TEXT_MAX_BYTES);
    assert_eq!(output.workflow_name.len(), COMPACT_WAIT_TEXT_MAX_BYTES);

    // The failure reason is exempt: a failed run can also carry a partial result,
    // and a stub error next to a large result leaves the model with no explanation.
    let compacted_error = output.error.as_deref().unwrap();
    assert!(compacted_error.len() > COMPACT_WAIT_TEXT_MAX_BYTES);
    assert!(compacted_error.starts_with("workflow runtime failed: !"));
    assert!(compacted_error.ends_with("...[truncated]"));
    assert!(compacted_error.len() < error.len());
    assert!(
        compacted_error.len() <= WAIT_ERROR_TEXT_MAX_BYTES,
        "error should still be bounded, just not stubbed: {} bytes",
        compacted_error.len()
    );
}

#[test]
fn failed_run_with_restore_conflicts_stays_within_the_wait_context_cap() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    // Five distinct restore conflicts, which is the largest advisory block a wait
    // response can carry, plus an error long enough to hit its full budget.
    let restore_conflict = "script content changed; workflow arguments changed; declared inputs changed; workflow execution identity changed; failed to restore frozen child workflow composition"
        .to_string();
    let low_compression = (0..8_000)
        .map(|index| char::from_u32(0x21 + (index * 73 % 90)).unwrap())
        .collect::<String>();
    let outcome = WorkflowWaitOutcome {
        snapshot: crate::service::WorkflowTaskSnapshot {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            task_id: "task".to_string(),
            run_id: "wf_0123456789abcdef-0123456789abcdef".to_string(),
            workflow_name: low_compression.clone(),
            title: Some(low_compression.clone()),
            status: WorkflowTaskStatus::Failed,
            summary: low_compression,
            transcript_dir: root.join("transcript"),
            script_path: root.join("workflow.js"),
            args: JsonValue::Null,
            result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                sha256: "0".repeat(64),
                bytes: 64_000,
                storage_id: "0".repeat(32),
            }),
            output_file: root.join("workflow.json"),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage {
                total_tokens: 123_456,
                tool_uses: 789,
                duration_ms: 1_234_567,
                agent_count: 12,
                successful_agent_count: 10,
                failed_agent_count: 2,
                skipped_agent_count: 1,
                null_agent_result_count: 3,
            },
            failures: vec!["failure ".repeat(64)],
            error: Some(restore_conflict.clone()),
            started_at: 1,
            completed_at: Some(2),
            script_sha256: "sha256".to_string(),
        },
        timed_out: false,
    };
    // A writePath failure keeps the artifact digest, which is the widest result
    // metadata shape a wait response can carry.
    let result_data =
        WorkflowResultData::from_write_error(&outcome.snapshot, &"write failed ".repeat(64));
    let mut output = WaitWorkflowOutput::from_outcome_with_result(
        outcome,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        result_data,
    )
    .unwrap();

    assert_eq!(output.error.as_deref().map(str::len), Some(160));
    let before = serde_json::to_vec(&output).unwrap().len();
    bound_wait_workflow_output(&mut output).unwrap();
    let after = serde_json::to_vec(&output).unwrap().len();

    assert!(
        before > crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES,
        "this case should need the ladder: {before} bytes"
    );
    assert!(
        after <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES,
        "the ladder must fit the fixed cap instead of failing the call: {after} bytes"
    );

    // Status, usage, failure count, and result availability survive every rung.
    let value = serde_json::to_value(&output).unwrap();
    assert_eq!(value["status"], json!("failed"));
    assert_eq!(value["failureCount"], json!(1));
    assert_eq!(value["usage"]["totalTokens"], json!(123_456));
    assert_eq!(value["resultWritten"], json!(false));
    assert!(value["resultError"].as_str().is_some());
    assert_eq!(value["resultBytes"], json!(64_000));

    // Give-up order: advisory restore detail and the re-obtainable digest go before
    // the failure reason, so a shortened error implies both were already spent.
    let shrunk_error = output.error.as_deref().unwrap();
    assert_eq!(
        value["recovery"]["observedRestoreIncompatibilities"],
        json!([]),
        "advisory restore detail is the first thing the ladder drops"
    );
    if shrunk_error.len() < WAIT_ERROR_TEXT_MAX_BYTES {
        assert_eq!(
            value["resultSha256"],
            json!(null),
            "the re-obtainable digest must give way before the failure reason"
        );
    }
    assert!(shrunk_error.starts_with("script"));
    assert!(shrunk_error.len() < restore_conflict.len());
}

#[test]
fn written_wait_result_compacts_without_truncating_a_usable_path() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let low_compression = (0..8_000)
        .map(|index| char::from_u32(0x21 + (index * 73 % 90)).unwrap())
        .collect::<String>();
    let write = crate::workflow_result_write::WorkflowResultWrite {
        path: PathUri::parse(&format!("file:///tmp/{}.json", "p".repeat(1_000))).unwrap(),
        bytes: 4,
        sha256: "0".repeat(64),
    };
    let output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_written".to_string(),
                workflow_name: low_compression.clone(),
                title: None,
                status: WorkflowTaskStatus::Completed,
                summary: low_compression,
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                    sha256: "0".repeat(64),
                    bytes: 4,
                    storage_id: "0".repeat(32),
                }),
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: Vec::new(),
                error: None,
                started_at: 1,
                completed_at: Some(2),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        /*result_chunk*/ None,
        /*result_error*/ None,
        Some(&write),
        /*write_error*/ None,
    )
    .unwrap();

    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES
    );
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result_data["resultWritten"], true);
    assert_eq!(result_data["resultWritePath"], serde_json::Value::Null);
    assert_eq!(result_data["resultSha256"], "0".repeat(64));
    assert_eq!(result_data["resultBytes"], 4);
}

#[test]
fn interrupted_wait_is_not_reported_as_a_timeout() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let output = WaitWorkflowOutput::from_outcome(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_interrupted".to_string(),
                workflow_name: "test".to_string(),
                title: None,
                status: WorkflowTaskStatus::Running,
                summary: "running".to_string(),
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: None,
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: Vec::new(),
                error: None,
                started_at: 1,
                completed_at: None,
                script_sha256: "sha256".to_string(),
            },
            timed_out: true,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ true,
    )
    .unwrap();

    assert!(output.interrupted_by_user_input);
    assert!(!output.timed_out);
    assert_eq!(output.status, WorkflowTaskStatus::Running);
}

#[test]
fn paused_wait_is_terminal_without_exposing_a_result() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let mut snapshot = crate::service::WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_paused".to_string(),
        workflow_name: "test".to_string(),
        title: None,
        status: WorkflowTaskStatus::Paused,
        summary: "paused".to_string(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: None,
        output_file: root.join("workflow.json"),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: None,
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    };
    let output = WaitWorkflowOutput::from_outcome(
        WorkflowWaitOutcome {
            snapshot: snapshot.clone(),
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    )
    .unwrap();

    assert_eq!(output.status, WorkflowTaskStatus::Paused);
    assert!(!output.timed_out);
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result_data["resultAvailable"], false);
    assert_eq!(result_data["resultInline"], false);
    snapshot.status = WorkflowTaskStatus::Completed;
    snapshot.result_artifact = Some(crate::result_artifact::WorkflowResultArtifact {
        sha256: "0".repeat(64),
        bytes: 4,
        storage_id: "0".repeat(32),
    });
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: "null".to_string(),
        offset: 0,
        next_offset: 4,
        total_bytes: 4,
    };
    let completed_result_data = serde_json::to_value(
        from_outcome_with_result_chunk(
            WorkflowWaitOutcome {
                snapshot,
                timed_out: false,
            },
            /*timeout_ms*/ 100,
            /*interrupted_by_user_input*/ false,
            Some(&result_chunk),
            /*result_error*/ None,
            /*written_result*/ None,
            /*write_error*/ None,
        )
        .unwrap()
        .result_data,
    )
    .unwrap();
    assert_eq!(completed_result_data["resultAvailable"], true);
}

#[test]
fn parse_errors_are_bounded_before_reaching_the_model() {
    let arguments = format!(r#"{{"{}":true}}"#, "unknown".repeat(2_000));

    let Err(FunctionCallError::RespondToModel(message)) = parse_arguments(&arguments) else {
        panic!("oversized unknown field should produce a model-visible parse error");
    };

    assert!(message.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
    assert!(message.ends_with("...[truncated]"));
}

#[tokio::test]
async fn repeated_waits_observe_the_same_latched_user_input() {
    let activity = Arc::new(LatchedTurnActivity::default());
    activity.signal_user_input();

    for _ in 0..2 {
        let result = race_with_turn_activity(
            std::future::pending::<()>(),
            Some(activity.clone() as Arc<dyn TurnActivitySubscription>),
        )
        .await;
        assert!(matches!(result, InterruptibleWait::InterruptedByUserInput));
    }
}

#[tokio::test]
async fn user_input_wakes_an_already_running_wait() {
    let activity = Arc::new(LatchedTurnActivity::default());
    let wait_activity = Arc::clone(&activity);
    let wait = tokio::spawn(async move {
        race_with_turn_activity(
            std::future::pending::<()>(),
            Some(wait_activity as Arc<dyn TurnActivitySubscription>),
        )
        .await
    });
    timeout(Duration::from_secs(1), activity.wait_started.notified())
        .await
        .expect("wait should subscribe to turn activity");
    assert!(!wait.is_finished());

    activity.signal_user_input();

    assert!(matches!(
        timeout(Duration::from_secs(1), wait)
            .await
            .expect("user input should wake the active wait")
            .expect("wait task should complete"),
        InterruptibleWait::InterruptedByUserInput
    ));
}

#[derive(Default)]
struct LatchedTurnActivity {
    observed: AtomicBool,
    notify: Notify,
    wait_started: Notify,
}

impl LatchedTurnActivity {
    fn signal_user_input(&self) {
        self.observed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl TurnActivitySubscription for LatchedTurnActivity {
    fn observed(&self) -> Option<TurnActivity> {
        self.observed
            .load(Ordering::Acquire)
            .then_some(TurnActivity::UserInput)
    }

    fn wait<'a>(&'a self) -> codex_extension_api::TurnActivityFuture<'a> {
        Box::pin(async move {
            self.wait_started.notify_one();
            if !self.observed.load(Ordering::Acquire) {
                self.notify.notified().await;
            }
            self.observed()
        })
    }
}

#[test]
fn small_inline_result_survives_without_a_compaction_round_trip() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let body = r#"{"run":"first"}"#;
    let chunk = crate::result_artifact::WorkflowResultChunk {
        text: body.to_string(),
        offset: 0,
        next_offset: u64::try_from(body.len()).unwrap(),
        total_bytes: u64::try_from(body.len()).unwrap(),
    };
    let mut output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "01a079ff-7cad-73d0-8c78-ffb33d428c9b".to_string(),
                turn_id: "turn".to_string(),
                task_id: "wc64f0fd1".to_string(),
                run_id: "wf_67ab0feab87f464ba39b299fc5aed64d".to_string(),
                workflow_name: "multi-workflow-first".to_string(),
                title: None,
                status: WorkflowTaskStatus::Completed,
                summary: "Workflow multi-workflow-first completed".to_string(),
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                    sha256: "0".repeat(64),
                    bytes: u64::try_from(body.len()).unwrap(),
                    storage_id: "0".repeat(32),
                }),
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage {
                    duration_ms: 520,
                    ..WorkflowUsage::default()
                },
                failures: Vec::new(),
                error: None,
                started_at: 1_788_753_181,
                completed_at: Some(1_788_753_182),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 30_000,
        /*interrupted_by_user_input*/ false,
        Some(&chunk),
        /*result_error*/ None,
        /*written_result*/ None,
        /*write_error*/ None,
    )
    .unwrap();

    // A realistic completed run has roughly 800 bytes of fixed metadata. A result
    // that already fits inside the cap must stay inline: dropping it and adding a
    // longer nextAction used to make the response both bigger and less useful.
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result_data["resultInline"], json!(true));
    assert_eq!(result_data["resultTruncated"], json!(false));
    assert_eq!(result_data["result"], json!({ "run": "first" }));
    assert_eq!(result_data["nextAction"], json!(null));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES
    );

    bound_wait_workflow_output(&mut output).unwrap();
    let bounded = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(
        bounded["result"],
        json!({ "run": "first" }),
        "the ladder must leave a response that already fits untouched"
    );
}

#[test]
fn result_read_failure_is_stubbed_before_the_run_error_is() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    // A failed run whose inline read also failed, with no restore conflicts and no
    // artifact digest in the response: the first two ladder rungs reclaim nothing, so the
    // result read/write detail has to give way before the run's own failure reason does.
    // The artifact itself must exist, because a run without one is never read at all.
    let snapshot = crate::service::WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_0123456789abcdef0123456789abcdef".to_string(),
        workflow_name: "read-failed".to_string(),
        title: None,
        status: WorkflowTaskStatus::Failed,
        summary: "Workflow read-failed failed".to_string(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
            sha256: "0".repeat(64),
            bytes: 4,
            storage_id: "0".repeat(32),
        }),
        output_file: root.join("workflow.json"),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage {
            total_tokens: 123_456,
            tool_uses: 789,
            duration_ms: 1_234_567,
            agent_count: 12,
            successful_agent_count: 10,
            failed_agent_count: 2,
            skipped_agent_count: 1,
            null_agent_result_count: 3,
        },
        failures: Vec::new(),
        error: Some("runtime failure without any restore conflict keywords ".repeat(8)),
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    };
    let result_data = WorkflowResultData::without_chunk(
        &snapshot,
        Some(&"result artifact could not be read back ".repeat(8)),
    );
    assert_eq!(
        serde_json::to_value(&result_data).unwrap()["resultSha256"],
        json!(null),
        "the digest rung must have nothing to reclaim for this snapshot"
    );
    let mut output = WaitWorkflowOutput::from_outcome_with_result(
        WorkflowWaitOutcome {
            snapshot,
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        result_data,
    )
    .unwrap();

    let error_before = output.error.clone();
    assert_eq!(
        error_before.as_deref().map(str::len),
        Some(WAIT_ERROR_TEXT_MAX_BYTES)
    );
    bound_wait_workflow_output(&mut output).unwrap();

    // The run error keeps its full budget; the read failure detail is what shrinks.
    assert_eq!(output.error, error_before);
    let value = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(
        value["resultError"].as_str().map(str::len),
        Some(COMPACT_WAIT_TEXT_MAX_BYTES),
        "the third rung should stub the result read failure: {value}"
    );
    assert_eq!(value["nextAction"], json!("ReadWorkflowResult: offset=0."));
}

#[test]
fn identity_text_stays_compacted_when_result_compaction_is_reverted() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    // The case the scoped revert exists for: a completed run with a maximum-length run
    // id, a small inline result worth keeping, and nothing for the ladder to spend — no
    // restore conflicts, no digest, no result error, and no run error. Reverting the
    // identity stubs along with the result would hand all of those bytes back for
    // nothing, since the result compaction is the only step that grew.
    let body = r#"{"run":"first"}"#;
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: body.to_string(),
        offset: 0,
        next_offset: u64::try_from(body.len()).unwrap(),
        total_bytes: u64::try_from(body.len()).unwrap(),
    };
    let mut output = from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: format!("wf_{}", "a".repeat(MAX_WAIT_WORKFLOW_ID_BYTES - 3)),
                workflow_name: "n".repeat(4_000),
                title: None,
                status: WorkflowTaskStatus::Completed,
                summary: "s".repeat(4_000),
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: None,
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: Vec::new(),
                error: None,
                started_at: 1,
                completed_at: Some(2),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        Some(&result_chunk),
        /*result_error*/ None,
        /*written_result*/ None,
        /*write_error*/ None,
    )
    .unwrap();

    // The revert is scoped to the result: the cheap identity text stays stubbed.
    assert_eq!(output.workflow_name.len(), COMPACT_WAIT_TEXT_MAX_BYTES);
    assert_eq!(output.summary.len(), COMPACT_WAIT_TEXT_MAX_BYTES);
    let result = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result["resultInline"], json!(true));
    assert_eq!(result["result"], json!({ "run": "first" }));
    assert_eq!(result["nextAction"], json!(null));

    // The two stub lengths above are what pin the scoped revert: the old all-or-nothing
    // guard handed the identity bytes back with the result's, and this fixture fits the
    // cap either way. The size assert is a bound, not a discriminator, and the empty
    // ladder rungs below are the reason the identity bytes cannot be handed back.
    let bounded_bytes = serde_json::to_vec(&output).unwrap().len();
    assert!(
        bounded_bytes <= WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES,
        "the builder alone must fit this run: {bounded_bytes} bytes"
    );
    bound_wait_workflow_output(&mut output).unwrap();
    assert_eq!(output.error, None);
    assert_eq!(
        serde_json::to_value(&output.recovery).unwrap()["observedRestoreIncompatibilities"],
        json!([]),
        "a completed run leaves the ladder nothing to spend"
    );
}
