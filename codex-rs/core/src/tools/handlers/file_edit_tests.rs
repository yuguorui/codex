use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::file_change_event::MAX_FILE_CHANGE_DIFF_INPUT_BYTES;
use crate::tools::handlers::file_change_event::path_event_key;
use crate::tools::registry::PreToolUsePayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
use codex_protocol::protocol::FileChange;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[test]
fn oversized_new_file_event_remains_a_bounded_add() {
    let path = PathUri::from_host_native_path(std::env::temp_dir().join("large-new-file.txt"))
        .expect("absolute path URI");
    let updated = "x".repeat(MAX_FILE_CHANGE_DIFF_INPUT_BYTES + 1);

    let FileChangeEvent { changes, delta } =
        file_change_event(&path, None, &updated, /*context_radius*/ 3);

    assert_eq!(
        changes,
        HashMap::from([(
            path_event_key(&path),
            FileChange::Add {
                content: format!(
                    "<content omitted: input exceeded {MAX_FILE_CHANGE_DIFF_INPUT_BYTES} bytes>\n"
                ),
            },
        )])
    );
    assert!(delta.is_none());
}

#[test]
fn regular_update_event_includes_an_exact_committed_delta() {
    let path = PathUri::from_host_native_path(std::env::temp_dir().join("updated-file.txt"))
        .expect("absolute path URI");
    let _event_path = path_event_key(&path);

    let FileChangeEvent { delta, .. } = file_change_event(
        &path,
        Some("before\n"),
        "after\n",
        /*context_radius*/ 3,
    );

    assert_eq!(
        delta,
        Some(AppliedPatchDelta::from_exact_changes(vec![
            AppliedPatchChange {
                path: path.clone(),
                change: AppliedPatchFileChange::Update {
                    move_path: None,
                    old_content: "before\n".to_string(),
                    overwritten_move_content: None,
                    new_content: "after\n".to_string(),
                },
            },
        ]))
    );
}

#[tokio::test]
async fn pre_tool_use_payload_keeps_claude_code_shape() {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let input = json!({
        "file_path": "/tmp/example.txt",
        "old_string": "old",
        "new_string": "new"
    });
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "call-edit".to_string(),
        tool_name: ToolName::plain(FILE_EDIT_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: input.to_string(),
        },
    };

    assert_eq!(
        FileEditHandler::default().pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(FILE_EDIT_TOOL_NAME),
            tool_input: input,
        })
    );
}
