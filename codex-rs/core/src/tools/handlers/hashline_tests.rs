use super::*;
use codex_protocol::items::TurnItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::session::tests::make_session_and_context_with_rx;
use crate::turn_diff_tracker::TurnDiffTracker;

fn anchored(line_no: usize, content: &str) -> String {
    format!(
        "{line_no}:{}|{content}",
        format_short_hash(full_hash(content))
    )
}

fn mutation_args(action: HashlineAction, anchor: String, content: Option<&str>) -> HashlineArgs {
    HashlineArgs {
        action,
        path: "demo.txt".to_string(),
        environment_id: None,
        anchor: Some(anchor),
        content: content.map(ToString::to_string),
        before: false,
        context: None,
    }
}

#[test]
fn hashes_match_hashline_examples() {
    assert_eq!(xxh32(b"", 0), 0x02cc_5d05);
    assert_eq!(xxh32(b"abc", 0), 0x32d1_53ff);
    assert_eq!(
        short_hash("return decoded   "),
        short_hash("return decoded")
    );
}

#[test]
fn read_output_uses_dense_anchor_format() {
    let doc = Document::parse("alpha\n  beta\n");

    assert_eq!(
        read_output(&doc, None, DEFAULT_CONTEXT_LINES).unwrap(),
        format!("{}\n{}\n", anchored(1, "alpha"), anchored(2, "  beta"))
    );
}

#[test]
fn read_output_uses_four_hex_digit_hashes() {
    let doc = Document::parse("alpha\n");
    let output = read_output(&doc, None, DEFAULT_CONTEXT_LINES).unwrap();

    assert_eq!(
        output
            .lines()
            .next()
            .expect("line")
            .split('|')
            .next()
            .expect("anchor")
            .len(),
        6
    );
}

#[test]
fn render_preserves_missing_trailing_newline() {
    let doc = Document::parse("alpha\nbeta");

    assert_eq!(doc.render(), "alpha\nbeta");
}

#[test]
fn edit_replaces_anchor_range_with_multiline_content() {
    let mut doc = Document::parse("alpha\nbeta\ngamma\ndelta\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!(
            "2:{}..3:{}",
            format_short_hash(full_hash("beta")),
            format_short_hash(full_hash("gamma"))
        ),
        Some("B\nC"),
    );

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (1, 2, "Edited".to_string()));
    assert_eq!(doc.render(), "alpha\nB\nC\ndelta\n");
}

#[test]
fn edit_rejects_multiline_content_with_single_anchor() {
    let mut doc = Document::parse("alpha\nbeta\ngamma\ndelta\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!("2:{}", format_short_hash(full_hash("beta"))),
        Some("B\nC"),
    );

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(
        error,
        &[
            "multi-line `content` requires an anchor range",
            "a single-line anchor would only replace line 2",
        ],
    );
    assert_eq!(doc.render(), "alpha\nbeta\ngamma\ndelta\n");
}

#[test]
fn edit_with_empty_content_deletes_anchor_range() {
    let mut doc = Document::parse("alpha\nbeta\ngamma\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!("2:{}", format_short_hash(full_hash("beta"))),
        Some(""),
    );

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (1, 1, "Edited".to_string()));
    assert_eq!(doc.render(), "alpha\ngamma\n");
}

#[test]
fn insert_keeps_anchor_line() {
    let mut doc = Document::parse("alpha\nbeta\n");
    let args = mutation_args(
        HashlineAction::Insert,
        format!("1:{}", format_short_hash(full_hash("alpha"))),
        Some("new"),
    );

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (1, 1, "Inserted".to_string()));
    assert_eq!(doc.render(), "alpha\nnew\nbeta\n");
}

#[test]
fn insert_preserves_leading_whitespace_in_content() {
    let mut doc = Document::parse("fn main() {\n    let x = 1;\n}\n");
    let args = mutation_args(
        HashlineAction::Insert,
        format!("2:{}", format_short_hash(full_hash("    let x = 1;"))),
        Some("    let y = 2;"),
    );

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (2, 2, "Inserted".to_string()));
    assert_eq!(
        doc.render(),
        "fn main() {\n    let x = 1;\n    let y = 2;\n}\n"
    );
}

#[test]
fn edit_rejects_stale_line_hash_without_relocation() {
    let mut doc = Document::parse("new\nalpha\nbeta\ngamma\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!("2:{}", format_short_hash(full_hash("beta"))),
        Some("BETA"),
    );

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(error, &["content changed", ">>> 2:", "|alpha"]);
    assert_eq!(doc.render(), "new\nalpha\nbeta\ngamma\n");
}

#[test]
fn delete_rejects_stale_line_hash_that_collides_with_another_line() {
    let mut doc = Document::parse("alpha\nbravo\nCHARLIE_EDIT\ndelta\n");
    let args = mutation_args(
        HashlineAction::Delete,
        format!("3:{}", format_short_hash(full_hash("charlie"))),
        None,
    );

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(error, &["content changed", ">>> 3:", "|CHARLIE_EDIT"]);
    assert_eq!(doc.render(), "alpha\nbravo\nCHARLIE_EDIT\ndelta\n");
}

#[test]
fn edit_range_rejects_stale_endpoint() {
    let mut doc = Document::parse("alpha\nbeta\nGAMMA\ndelta\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!("2:{}..3:{}", format_short_hash(full_hash("beta")), "0000"),
        Some("B\nC"),
    );

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(error, &["content changed", ">>> 3:", "|GAMMA"]);
    assert_eq!(doc.render(), "alpha\nbeta\nGAMMA\ndelta\n");
}

#[test]
fn edit_rejects_bare_line_range() {
    let mut doc = Document::parse("alpha\nbeta\ngamma\n");
    let args = mutation_args(
        HashlineAction::Edit,
        "1..2".to_string(),
        Some("replacement"),
    );

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(error, &["bare line numbers"]);
    assert_eq!(doc.render(), "alpha\nbeta\ngamma\n");
}

#[test]
fn edit_rejects_bare_line_anchor() {
    let mut doc = Document::parse("alpha\nbeta\n");
    let args = mutation_args(HashlineAction::Edit, "1".to_string(), Some("replacement"));

    let error = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap_err();

    assert_respond_to_model_contains(error, &["bare line numbers"]);
    assert_eq!(doc.render(), "alpha\nbeta\n");
}

#[test]
fn two_digit_hash_anchors_remain_supported() {
    let mut doc = Document::parse("alpha\nbeta\n");
    let args = mutation_args(
        HashlineAction::Edit,
        format!("2:{}", format_short_hash(short_hash("beta"))),
        Some("BETA"),
    );

    let changed = apply_mutation(&mut doc, &args, args.content.as_deref()).unwrap();

    assert_eq!(changed, (1, 1, "Edited".to_string()));
    assert_eq!(doc.render(), "alpha\nBETA\n");
}

#[test]
fn hashline_unified_diff_counts_changed_lines() {
    let diff = hashline_unified_diff("alpha\nbeta\ngamma\ndelta\n", "alpha\nB\nC\ndelta\n");

    assert_diff_line_counts(&diff, 2, 2);
    assert_eq!(
        diff,
        "@@ -1,4 +1,4 @@\n alpha\n-beta\n-gamma\n+B\n+C\n delta\n"
    );
}

#[test]
fn hashline_unified_diff_counts_inserted_lines() {
    let diff = hashline_unified_diff("alpha\nbeta\n", "alpha\none\ntwo\nbeta\n");

    assert_diff_line_counts(&diff, 2, 0);
    assert_eq!(diff, "@@ -1,2 +1,4 @@\n alpha\n+one\n+two\n beta\n");
}

#[test]
fn hashline_unified_diff_counts_deleted_lines() {
    let diff = hashline_unified_diff("alpha\none\ntwo\nbeta\n", "alpha\nbeta\n");

    assert_diff_line_counts(&diff, 0, 2);
    assert_eq!(diff, "@@ -1,4 +1,2 @@\n alpha\n-one\n-two\n beta\n");
}

#[test]
fn hashline_unified_diff_counts_replaced_lines() {
    let diff = hashline_unified_diff("alpha\none\ntwo\nbeta\n", "alpha\nONE\nTWO\nbeta\n");

    assert_diff_line_counts(&diff, 2, 2);
    assert_eq!(
        diff,
        "@@ -1,4 +1,4 @@\n alpha\n-one\n-two\n+ONE\n+TWO\n beta\n"
    );
}

#[test]
fn fuzzy_resolution_relocates_qualified_anchor_to_unique_hash_match() {
    let doc = Document::parse("new\nalpha\nbeta\ngamma\n");
    let anchor = format!("2:{}", format_short_hash(full_hash("beta")));

    let resolved = resolve_anchor(
        &doc,
        parse_anchor(&anchor, /*allow_bare_line*/ false).unwrap(),
        ResolveMode::Fuzzy,
    )
    .unwrap();

    assert_eq!(resolved.index, 2);
    assert_eq!(resolved.hash, full_hash("beta"));
}

#[test]
fn strict_resolution_rejects_relocated_qualified_anchor() {
    let doc = Document::parse("new\nalpha\nbeta\ngamma\n");
    let anchor = format!("2:{}", format_short_hash(full_hash("beta")));

    let error = resolve_anchor(
        &doc,
        parse_anchor(&anchor, /*allow_bare_line*/ false).unwrap(),
        ResolveMode::Strict,
    )
    .unwrap_err();

    assert_respond_to_model_contains(error, &["content changed", ">>> 2:", "|alpha"]);
}

#[test]
fn read_range_clamps_line_numbers_past_end_of_file() {
    let doc = Document::parse("alpha\nbeta\n");

    let output = read_output(&doc, Some("1..20"), DEFAULT_CONTEXT_LINES).unwrap();

    assert_eq!(
        output,
        format!(
            "{}\n{}\n\n(showing through end of file; requested range extends past line 20 but file has 2 lines)",
            anchored(1, "alpha"),
            anchored(2, "beta")
        )
    );
}

#[test]
fn read_range_start_past_end_returns_note() {
    let doc = Document::parse("alpha\nbeta\n");

    let output = read_output(&doc, Some("20..30"), DEFAULT_CONTEXT_LINES).unwrap();

    assert_eq!(
        output,
        "(no lines: requested range starts past end of file; file has 2 lines)"
    );
}

#[test]
fn read_line_past_end_returns_note() {
    let doc = Document::parse("alpha\nbeta\n");

    let output = read_output(&doc, Some("20"), DEFAULT_CONTEXT_LINES).unwrap();

    assert_eq!(
        output,
        "(no lines: requested line 20 is past end of file; file has 2 lines)"
    );
}

#[test]
fn read_output_truncates_very_long_lines() {
    let long_line = "x".repeat(MAX_FORMATTED_LINE_CHARS + 1);
    let doc = Document::parse(&long_line);

    let output = read_output(&doc, None, DEFAULT_CONTEXT_LINES).unwrap();

    assert!(output.contains(&"x".repeat(MAX_FORMATTED_LINE_CHARS)));
    assert!(output.contains(&format!(
        "... [truncated to first {MAX_FORMATTED_LINE_CHARS} of {} chars]",
        MAX_FORMATTED_LINE_CHARS + 1
    )));
}

#[tokio::test]
async fn mutation_emits_standard_file_change_item() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let changes = HashMap::from([(
        PathBuf::from("demo.txt"),
        FileChange::Update {
            unified_diff: hashline_unified_diff("alpha\nbeta\n", "alpha\nBETA\n"),
            move_path: None,
        },
    )]);
    let emitter = ToolEmitter::apply_patch_for_environment(
        changes,
        /*auto_approved*/ true,
        String::new(),
    );
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        "call-hashline",
        Some(&tracker),
    );
    emitter.begin(event_ctx).await;
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        "call-hashline",
        Some(&tracker),
    );
    emitter
        .finish(
            event_ctx,
            Ok(hashline_output(0, String::new(), String::new())),
            None,
        )
        .await
        .expect("hashline file change succeeds");

    let started = rx_event.recv().await.expect("item started event");
    assert!(matches!(
        started.msg,
        EventMsg::ItemStarted(event)
            if matches!(
                &event.item,
                TurnItem::FileChange(file_change)
                    if file_change.id == "call-hashline"
                        && file_change.status.is_none()
                        && file_change.auto_approved == Some(true)
            )
    ));

    let completed = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("item completed event")
            .expect("channel open");
        if let EventMsg::ItemCompleted(completed) = event.msg {
            break completed;
        }
    };
    let TurnItem::FileChange(file_change) = completed.item else {
        panic!("expected completed file change item");
    };
    assert_eq!(file_change.id, "call-hashline");
    assert_eq!(file_change.status, Some(PatchApplyStatus::Completed));
    assert!(file_change.auto_approved.is_none());
}

#[tokio::test]
async fn read_emits_standard_explored_command_item() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let path =
        AbsolutePathBuf::try_from(PathBuf::from("/tmp/hashline_demo.txt")).expect("absolute path");
    let path_uri = PathUri::from_abs_path(&path);
    let cwd = turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd()
        .clone();
    let emitter = hashline_read_emitter(HashlineAction::Read, &path_uri, &cwd);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-hashline", None);
    emitter.begin(event_ctx).await;
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-hashline", None);
    emitter
        .finish(
            event_ctx,
            Ok(hashline_output(
                0,
                "1:aa|alpha\n".to_string(),
                String::new(),
            )),
            None,
        )
        .await
        .expect("hashline read event succeeds");

    let started = rx_event.recv().await.expect("exec begin event");
    let EventMsg::ExecCommandBegin(begin) = started.msg else {
        panic!("expected exec command begin");
    };
    assert_eq!(begin.call_id, "call-hashline");
    assert_eq!(
        begin.parsed_cmd,
        vec![ParsedCommand::Read {
            cmd: "fuzz_view_edit read /tmp/hashline_demo.txt".to_string(),
            name: "hashline_demo.txt".to_string(),
            path: PathBuf::from("/tmp/hashline_demo.txt"),
        }]
    );

    let completed = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
            .await
            .expect("exec command end event")
            .expect("channel open");
        if let EventMsg::ExecCommandEnd(end) = event.msg {
            break end;
        }
    };
    assert_eq!(completed.status, ExecCommandStatus::Completed);
    assert_eq!(completed.stdout, "1:aa|alpha\n");
    assert_eq!(completed.parsed_cmd, begin.parsed_cmd);
}

fn assert_diff_line_counts(diff: &str, expected_added: usize, expected_removed: usize) {
    let (added, removed) = diff
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .fold((0, 0), |(added, removed), line| {
            if line.starts_with('+') {
                (added + 1, removed)
            } else if line.starts_with('-') {
                (added, removed + 1)
            } else {
                (added, removed)
            }
        });

    assert_eq!((added, removed), (expected_added, expected_removed));
}

#[test]
fn bare_hash_rejects_ambiguous_matches() {
    let doc = Document::parse("alpha\nbeta\nalpha\n");
    let anchor = format_short_hash(full_hash("alpha"));

    let error = resolve_anchor(
        &doc,
        parse_anchor(&anchor, /*allow_bare_line*/ false).unwrap(),
        ResolveMode::Strict,
    )
    .unwrap_err();

    match error {
        FunctionCallError::RespondToModel(message) => {
            assert!(message.contains("ambiguous"));
            assert!(message.contains("1, 3"));
        }
        FunctionCallError::Fatal(message) => panic!("unexpected fatal error: {message}"),
    }
}

#[test]
fn stale_anchor_reports_fresh_line_context() {
    let doc = Document::parse("alpha\nDELTA\ngamma\n");
    let stale = format!("2:{}", format_short_hash(full_hash("beta")));

    let error = resolve_anchor(
        &doc,
        parse_anchor(&stale, /*allow_bare_line*/ false).unwrap(),
        ResolveMode::Strict,
    )
    .unwrap_err();

    assert_respond_to_model_contains(error, &["content changed", ">>> 2:", "|DELTA"]);
}

fn assert_respond_to_model_contains(error: FunctionCallError, snippets: &[&str]) {
    match error {
        FunctionCallError::RespondToModel(message) => {
            for snippet in snippets {
                assert!(
                    message.contains(snippet),
                    "expected error message to contain {snippet:?}: {message}"
                );
            }
        }
        FunctionCallError::Fatal(message) => panic!("unexpected fatal error: {message}"),
    }
}
