use anyhow::Context;
use anyhow::Result;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_no_remote_env;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

const EDIT_BEFORE_READ_CALL_ID: &str = "edit-before-read";
const READ_CALL_ID: &str = "read-file";
const REPEATED_READ_CALL_ID: &str = "read-file-again";
const FIRST_EDIT_CALL_ID: &str = "edit-beta";
const STALE_EDIT_CALL_ID: &str = "edit-after-external-change";

fn tool_response(response_id: &str, call_id: &str, tool_name: &str, args: &str) -> String {
    responses::sse(vec![
        ev_response_created(response_id),
        ev_function_call(call_id, tool_name, args),
        ev_completed(response_id),
    ])
}

fn done_response(response_id: &str, message_id: &str) -> String {
    responses::sse(vec![
        ev_response_created(response_id),
        ev_assistant_message(message_id, "done"),
        ev_completed(response_id),
    ])
}

async fn submit_turn_without_wait(test: &TestCodex, prompt: &str) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) = turn_permission_fields(
        PermissionProfile::workspace_write(),
        test.config.cwd.as_path(),
    );
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    Ok(())
}

async fn submit_turn(test: &TestCodex, prompt: &str) -> Result<()> {
    submit_turn_without_wait(test, prompt).await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(180),
    )
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_emits_standard_file_change_items_and_aggregated_turn_diff() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let cwd = &test.executor_environment().selection().cwd;
    let existing_path = cwd.join("event-existing.txt")?;
    let new_path = cwd.join("event-new.txt")?;
    let fs = test.fs();
    fs.write_file(
        &existing_path,
        b"alpha\nbeta\n".to_vec(),
        /*sandbox*/ None,
    )
    .await?;

    const UPDATE_CALL_ID: &str = "event-update";
    const ADD_CALL_ID: &str = "event-add";
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-event-read",
                "event-read",
                "Read",
                &json!({
                    "file_path": existing_path.inferred_native_path_string(),
                })
                .to_string(),
            ),
            tool_response(
                "resp-event-update",
                UPDATE_CALL_ID,
                "Edit",
                &json!({
                    "file_path": existing_path.inferred_native_path_string(),
                    "old_string": "beta",
                    "new_string": "BETA",
                })
                .to_string(),
            ),
            tool_response(
                "resp-event-add",
                ADD_CALL_ID,
                "Edit",
                &json!({
                    "file_path": new_path.inferred_native_path_string(),
                    "old_string": "",
                    "new_string": "created\n",
                })
                .to_string(),
            ),
            done_response("resp-event-done", "msg-event-done"),
        ],
    )
    .await;

    submit_turn_without_wait(&test, "Read, update, and create files").await?;
    let mut started = HashMap::<String, FileChangeItem>::new();
    let mut completed = HashMap::<String, FileChangeItem>::new();
    let mut read_started = None::<CommandExecutionItem>;
    let mut read_completed = None::<CommandExecutionItem>;
    let mut last_turn_diff = None;
    wait_for_event_with_timeout(
        &test.codex,
        |event| match event {
            EventMsg::ItemStarted(event) => {
                if let TurnItem::FileChange(item) = &event.item {
                    started.insert(item.id.clone(), item.clone());
                } else if let TurnItem::CommandExecution(item) = &event.item
                    && item.id == "event-read"
                {
                    read_started = Some(item.clone());
                }
                false
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::FileChange(item) = &event.item {
                    completed.insert(item.id.clone(), item.clone());
                } else if let TurnItem::CommandExecution(item) = &event.item
                    && item.id == "event-read"
                {
                    read_completed = Some(item.clone());
                }
                false
            }
            EventMsg::TurnDiff(event) => {
                last_turn_diff = Some(event.unified_diff.clone());
                false
            }
            EventMsg::TurnComplete(_) => true,
            _ => false,
        },
        Duration::from_secs(180),
    )
    .await;

    assert_eq!(mock.requests().len(), 4);
    assert_eq!(
        read_started.context("missing started Read event")?.status,
        CommandExecutionStatus::InProgress
    );
    let read_completed = read_completed.context("missing completed Read event")?;
    assert_eq!(read_completed.status, CommandExecutionStatus::Completed);
    assert_eq!(
        read_completed.stdout.as_deref(),
        Some("1\talpha\n2\tbeta\n3\t")
    );
    assert_eq!(started.len(), 2);
    assert_eq!(completed.len(), 2);
    for item in started.values() {
        assert_eq!(item.status, None);
        assert_eq!(item.auto_approved, Some(true));
    }
    for item in completed.values() {
        assert_eq!(item.status, Some(PatchApplyStatus::Completed));
        assert_eq!(item.auto_approved, None);
    }

    let updated = completed
        .get(UPDATE_CALL_ID)
        .context("missing completed update event")?;
    assert!(matches!(
        updated.changes.get(&existing_path.to_path_buf()),
        Some(FileChange::Update { unified_diff, move_path: None })
            if unified_diff.contains("-beta") && unified_diff.contains("+BETA")
    ));
    let added = completed
        .get(ADD_CALL_ID)
        .context("missing completed add event")?;
    assert_eq!(
        added.changes.get(&new_path.to_path_buf()),
        Some(&FileChange::Add {
            content: "created\n".to_string(),
        })
    );
    let turn_diff = last_turn_diff.context("missing aggregated turn diff")?;
    assert!(turn_diff.contains("BETA"));
    assert!(turn_diff.contains("created"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_file_tools_require_read_track_edits_and_reject_external_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let file_path = test
        .executor_environment()
        .selection()
        .cwd
        .join("example.txt")?;
    let file_path_arg = file_path.inferred_native_path_string();
    let fs = test.fs();
    fs.write_file(
        &file_path,
        b"alpha\nbeta\ngamma\n".to_vec(),
        /*sandbox*/ None,
    )
    .await?;

    let edit_args = |old_string: &str, new_string: &str| {
        json!({
            "file_path": file_path_arg,
            "old_string": old_string,
            "new_string": new_string,
        })
        .to_string()
    };
    let read_args = json!({ "file_path": file_path_arg }).to_string();
    let first_turn = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-1",
                EDIT_BEFORE_READ_CALL_ID,
                "Edit",
                &edit_args("beta", "BETA"),
            ),
            tool_response("resp-2", READ_CALL_ID, "Read", &read_args),
            tool_response("resp-2b", REPEATED_READ_CALL_ID, "Read", &read_args),
            tool_response(
                "resp-3",
                FIRST_EDIT_CALL_ID,
                "Edit",
                &edit_args("beta", "BETA"),
            ),
            done_response("resp-4", "msg-1"),
        ],
    )
    .await;

    submit_turn(&test, "Read and edit the file").await?;

    let requests = first_turn.requests();
    assert_eq!(requests.len(), 5);
    let first_body = requests[0].body_json();
    let tools = first_body["tools"]
        .as_array()
        .context("first request should expose tools")?;
    let tool_names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"Read"));
    assert!(tool_names.contains(&"Edit"));
    assert!(!tool_names.contains(&"apply_patch"));
    assert!(!tool_names.contains(&"fuzz_view_edit"));
    assert_eq!(
        requests[1].function_call_output_text(EDIT_BEFORE_READ_CALL_ID),
        Some("File has not been read yet. Read it first before writing to it.".to_string())
    );
    assert_eq!(
        requests[2].function_call_output_text(READ_CALL_ID),
        Some("1\talpha\n2\tbeta\n3\tgamma\n4\t".to_string())
    );
    let expected_edit_output = format!("The file {file_path_arg} has been updated successfully.");
    assert_eq!(
        requests[3].function_call_output_text(FIRST_EDIT_CALL_ID),
        None
    );
    assert_eq!(
        requests[3].function_call_output_text(REPEATED_READ_CALL_ID),
        Some("File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current \u{2014} refer to that instead of re-reading.".to_string())
    );
    assert_eq!(
        requests[4].function_call_output_text(FIRST_EDIT_CALL_ID),
        Some(expected_edit_output)
    );
    assert_eq!(
        fs.read_file(&file_path, /*sandbox*/ None).await?,
        b"alpha\nBETA\ngamma\n"
    );

    let ranged_path = test
        .executor_environment()
        .selection()
        .cwd
        .join("ranged.txt")?;
    let ranged_path_arg = ranged_path.inferred_native_path_string();
    fs.write_file(
        &ranged_path,
        b"alpha\nbeta\ngamma\n".to_vec(),
        /*sandbox*/ None,
    )
    .await?;
    let utf16_path = test
        .executor_environment()
        .selection()
        .cwd
        .join("utf16.txt")?;
    let utf16_path_arg = utf16_path.inferred_native_path_string();
    let utf16_content = "\u{feff}hello\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    fs.write_file(&utf16_path, utf16_content, /*sandbox*/ None)
        .await?;
    let crlf_path = test
        .executor_environment()
        .selection()
        .cwd
        .join("crlf.txt")?;
    let crlf_path_arg = crlf_path.inferred_native_path_string();
    fs.write_file(
        &crlf_path,
        b"alpha\r\nbeta\r\n".to_vec(),
        /*sandbox*/ None,
    )
    .await?;
    let range_and_utf16 = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-range-read",
                "range-read",
                "Read",
                &json!({
                    "file_path": ranged_path_arg,
                    "offset": 2,
                    "limit": 1
                })
                .to_string(),
            ),
            tool_response(
                "resp-range-edit",
                "range-edit",
                "Edit",
                &json!({
                    "file_path": ranged_path_arg,
                    "old_string": "beta",
                    "new_string": "BETA"
                })
                .to_string(),
            ),
            tool_response(
                "resp-utf16-read",
                "utf16-read",
                "Read",
                &json!({
                    "file_path": utf16_path_arg,
                    "offset": 1,
                    "limit": 1
                })
                .to_string(),
            ),
            tool_response(
                "resp-utf16-edit",
                "utf16-edit",
                "Edit",
                &json!({
                    "file_path": utf16_path_arg,
                    "old_string": "hello",
                    "new_string": "HELLO"
                })
                .to_string(),
            ),
            tool_response(
                "resp-crlf-read",
                "crlf-read",
                "Read",
                &json!({ "file_path": crlf_path_arg }).to_string(),
            ),
            tool_response(
                "resp-crlf-edit",
                "crlf-edit",
                "Edit",
                &json!({
                    "file_path": crlf_path_arg,
                    "old_string": "beta",
                    "new_string": "BETA"
                })
                .to_string(),
            ),
            done_response("resp-range-done", "msg-range-done"),
        ],
    )
    .await;

    submit_turn(&test, "Edit range-read, UTF-16LE, and CRLF files").await?;

    let requests = range_and_utf16.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests[1].function_call_output_text("range-read"),
        Some("2\tbeta".to_string())
    );
    assert_eq!(
        requests[3].function_call_output_text("utf16-read"),
        Some("1\thello".to_string())
    );
    assert_eq!(
        requests[5].function_call_output_text("crlf-read"),
        Some("1\talpha\n2\tbeta\n3\t".to_string())
    );
    assert_eq!(
        fs.read_file(&ranged_path, /*sandbox*/ None).await?,
        b"alpha\nBETA\ngamma\n"
    );
    let utf16_bytes = fs.read_file(&utf16_path, /*sandbox*/ None).await?;
    assert_eq!(&utf16_bytes[..2], &[0xff, 0xfe]);
    let utf16_units = utf16_bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    assert_eq!(String::from_utf16(&utf16_units)?, "\u{feff}HELLO\n");
    assert_eq!(
        fs.read_file(&crlf_path, /*sandbox*/ None).await?,
        b"alpha\r\nBETA\r\n"
    );

    fs.write_file(
        &file_path,
        b"external\nBETA\ngamma\n".to_vec(),
        /*sandbox*/ None,
    )
    .await?;

    let second_turn = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-6",
                STALE_EDIT_CALL_ID,
                "Edit",
                &edit_args("BETA", "beta"),
            ),
            done_response("resp-7", "msg-2"),
        ],
    )
    .await;

    submit_turn(&test, "Try another edit").await?;

    let requests = second_turn.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .function_call_output_text(STALE_EDIT_CALL_ID)
        .context("missing stale Edit output")?;
    assert_eq!(
        output,
        "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
    );
    assert_eq!(
        fs.read_file(&file_path, /*sandbox*/ None).await?,
        b"external\nBETA\ngamma\n"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_full_read_authorizes_edit_when_the_file_is_unchanged() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut initial_builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let initial = initial_builder.build(&server).await?;
    let native_path = initial.home.path().join("resume-edit.txt");
    std::fs::write(&native_path, "alpha\nbeta\n")?;
    let file_path = native_path.to_string_lossy().to_string();
    let read_args = json!({ "file_path": file_path }).to_string();
    let initial_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response("resp-resume-read", "resume-read", "Read", &read_args),
            done_response("resp-resume-read-done", "msg-resume-read-done"),
        ],
    )
    .await;

    submit_turn(&initial, "Read the file before editing it later").await?;
    assert_eq!(initial_mock.requests().len(), 2);
    initial.codex.flush_rollout().await?;
    let rollout_path = initial
        .codex
        .rollout_path()
        .expect("initial session should have a rollout path");
    let home = initial.home.clone();
    drop(initial);

    let edit_args = json!({
        "file_path": file_path,
        "old_string": "beta",
        "new_string": "BETA",
    })
    .to_string();
    let resumed_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response("resp-resume-edit", "resume-edit", "Edit", &edit_args),
            done_response("resp-resume-edit-done", "msg-resume-edit-done"),
        ],
    )
    .await;
    let mut resume_builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;

    resumed
        .submit_turn("Apply the previously planned edit")
        .await?;

    assert_eq!(resumed_mock.requests().len(), 2);
    assert_eq!(std::fs::read_to_string(native_path)?, "alpha\nBETA\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_removes_cached_read_authorization() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let test = builder.build(&server).await?;
    let file_path = test.workspace_path("rollback-edit.txt");
    std::fs::write(&file_path, "alpha\nbeta\n")?;
    let file_path = file_path.to_string_lossy().to_string();
    let read_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-rollback-read",
                "rollback-read",
                "Read",
                &json!({ "file_path": file_path }).to_string(),
            ),
            done_response("resp-rollback-read-done", "msg-rollback-read-done"),
        ],
    )
    .await;

    submit_turn(&test, "Read the file").await?;
    assert_eq!(read_mock.requests().len(), 2);
    test.codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    let rollback = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_))
    })
    .await;
    assert!(matches!(rollback, EventMsg::ThreadRolledBack(_)));

    let edit_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-rollback-edit",
                "rollback-edit",
                "Edit",
                &json!({
                    "file_path": file_path,
                    "old_string": "beta",
                    "new_string": "BETA",
                })
                .to_string(),
            ),
            done_response("resp-rollback-edit-done", "msg-rollback-edit-done"),
        ],
    )
    .await;

    submit_turn(&test, "Apply the edit after rollback").await?;
    let requests = edit_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].function_call_output_text("rollback-edit"),
        Some("File has not been read yet. Read it first before writing to it.".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(test.workspace_path("rollback-edit.txt"))?,
        "alpha\nbeta\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_reads_are_authorized_only_while_their_selected_content_is_current() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut initial_builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let initial = initial_builder.build(&server).await?;
    let stale_full_path = initial.home.path().join("resume-stale-full.txt");
    let stable_range_path = initial.home.path().join("resume-stable-range.txt");
    let stale_range_path = initial.home.path().join("resume-stale-range.txt");
    for path in [&stale_full_path, &stable_range_path, &stale_range_path] {
        std::fs::write(path, "alpha\nbeta\n")?;
    }
    let stale_full_arg = stale_full_path.to_string_lossy().to_string();
    let stable_range_arg = stable_range_path.to_string_lossy().to_string();
    let stale_range_arg = stale_range_path.to_string_lossy().to_string();
    let initial_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-resume-stale-full-read",
                "resume-stale-full-read",
                "Read",
                &json!({ "file_path": stale_full_arg }).to_string(),
            ),
            tool_response(
                "resp-resume-stable-range-read",
                "resume-stable-range-read",
                "Read",
                &json!({
                    "file_path": stable_range_arg,
                    "offset": 2,
                    "limit": 1,
                })
                .to_string(),
            ),
            tool_response(
                "resp-resume-stale-range-read",
                "resume-stale-range-read",
                "Read",
                &json!({
                    "file_path": stale_range_arg,
                    "offset": 2,
                    "limit": 1,
                })
                .to_string(),
            ),
            done_response("resp-resume-range-done", "msg-resume-range-done"),
        ],
    )
    .await;

    submit_turn(&initial, "Read the files for a later edit").await?;
    assert_eq!(initial_mock.requests().len(), 4);
    initial.codex.flush_rollout().await?;
    let rollout_path = initial
        .codex
        .rollout_path()
        .expect("initial session should have a rollout path");
    let home = initial.home.clone();
    drop(initial);

    std::fs::write(&stale_full_path, "alpha\nchanged\n")?;
    std::fs::write(&stable_range_path, "changed\nbeta\n")?;
    std::fs::write(&stale_range_path, "alpha\nchanged\n")?;
    let edit_args = |file_path: &str| {
        json!({
            "file_path": file_path,
            "old_string": "beta",
            "new_string": "BETA",
        })
        .to_string()
    };
    let resumed_mock = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-resume-stale-full-edit",
                "resume-stale-full-edit",
                "Edit",
                &edit_args(&stale_full_arg),
            ),
            tool_response(
                "resp-resume-stable-range-edit",
                "resume-stable-range-edit",
                "Edit",
                &edit_args(&stable_range_arg),
            ),
            tool_response(
                "resp-resume-stale-range-edit",
                "resume-stale-range-edit",
                "Edit",
                &edit_args(&stale_range_arg),
            ),
            done_response("resp-resume-edits-done", "msg-resume-edits-done"),
        ],
    )
    .await;
    let mut resume_builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;

    resumed
        .submit_turn("Apply the edits after resuming")
        .await?;

    let requests = resumed_mock.requests();
    assert_eq!(requests.len(), 4);
    let not_read =
        Some("File has not been read yet. Read it first before writing to it.".to_string());
    assert_eq!(
        requests[1].function_call_output_text("resume-stale-full-edit"),
        not_read
    );
    assert_eq!(
        requests[3].function_call_output_text("resume-stale-range-edit"),
        not_read
    );
    assert_eq!(
        std::fs::read_to_string(stale_full_path)?,
        "alpha\nchanged\n"
    );
    assert_eq!(
        std::fs::read_to_string(stable_range_path)?,
        "changed\nBETA\n"
    );
    assert_eq!(
        std::fs::read_to_string(stale_range_path)?,
        "alpha\nchanged\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_environment_id_routes_read_and_edit_to_the_selected_filesystem() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.apply_patch_tool_type = Some(ApplyPatchToolType::ClaudeCode);
    });
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let remote = test.executor_environment().selection().clone();
    let remote_path = remote.cwd.join("routed.txt")?;
    test.fs()
        .write_file(&remote_path, b"remote\n".to_vec(), /*sandbox*/ None)
        .await?;
    let local_root = tempfile::tempdir()?;
    let local_root_abs = AbsolutePathBuf::try_from(local_root.path().to_path_buf())?;
    let local = local(local_root_abs);
    let local_path = local_root.path().join("routed.txt");
    std::fs::write(&local_path, "local\n")?;
    let responses = responses::mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-route-remote-read",
                "route-remote-read",
                "Read",
                &json!({
                    "file_path": "routed.txt",
                    "environment_id": remote.environment_id,
                })
                .to_string(),
            ),
            tool_response(
                "resp-route-local-read",
                "route-local-read",
                "Read",
                &json!({
                    "file_path": "routed.txt",
                    "environment_id": local.environment_id,
                })
                .to_string(),
            ),
            tool_response(
                "resp-route-remote-edit",
                "route-remote-edit",
                "Edit",
                &json!({
                    "file_path": "routed.txt",
                    "old_string": "remote",
                    "new_string": "REMOTE",
                    "environment_id": remote.environment_id,
                })
                .to_string(),
            ),
            tool_response(
                "resp-route-local-edit",
                "route-local-edit",
                "Edit",
                &json!({
                    "file_path": "routed.txt",
                    "old_string": "local",
                    "new_string": "LOCAL",
                    "environment_id": local.environment_id,
                })
                .to_string(),
            ),
            done_response("resp-route-done", "msg-route-done"),
        ],
    )
    .await;

    test.submit_turn_with_environments(
        "Read and edit both environments",
        Some(vec![remote.clone(), local.clone()]),
    )
    .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[1].function_call_output_text("route-remote-read"),
        Some("1\tremote\n2\t".to_string())
    );
    assert_eq!(
        requests[2].function_call_output_text("route-local-read"),
        Some("1\tlocal\n2\t".to_string())
    );
    assert_eq!(
        test.fs().read_file(&remote_path, /*sandbox*/ None).await?,
        b"REMOTE\n"
    );
    assert_eq!(std::fs::read_to_string(local_path)?, "LOCAL\n");
    Ok(())
}
