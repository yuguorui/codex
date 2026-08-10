use super::*;
use codex_workflow::WorkflowAgentFailureKind;
use codex_workflow::WorkflowAgentOutcome;
use codex_workflow::WorkflowAgentResult;
use codex_workflow::WorkflowTokenUsage;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::Ordering;

const OLD_ACTIVE_INVOCATION_THRESHOLD: usize = 4_096;

fn result(value: &str) -> WorkflowJournalResult {
    WorkflowJournalResult {
        result: WorkflowAgentResult {
            value: json!(value),
            usage: WorkflowTokenUsage {
                total_tokens: 12,
                tool_uses: 3,
            },
            agent_id: Some("agent-1".to_string()),
            model: Some("model".to_string()),
            fallback_model: None,
        },
        outcome: WorkflowAgentOutcome::Success,
    }
}

fn failed_result(kind: WorkflowAgentFailureKind, message: &str) -> WorkflowJournalResult {
    WorkflowJournalResult {
        result: WorkflowAgentResult {
            value: serde_json::Value::Null,
            usage: WorkflowTokenUsage {
                total_tokens: 91,
                tool_uses: 7,
            },
            agent_id: None,
            model: None,
            fallback_model: None,
        },
        outcome: WorkflowAgentOutcome::Failure {
            kind,
            message: message.to_string(),
        },
    }
}

fn keys_by_segment(keys_per_segment: usize) -> Vec<Vec<String>> {
    let mut keys = vec![Vec::new(); JOURNAL_SEGMENT_COUNT];
    let mut candidate = 0_usize;
    while keys.iter().any(|segment| segment.len() < keys_per_segment) {
        let key = format!("segment-key-{candidate}");
        let segment = key_segment(&key);
        if keys[segment].len() < keys_per_segment {
            keys[segment].push(key);
        }
        candidate += 1;
    }
    keys
}

#[tokio::test]
async fn current_generation_round_trips_without_a_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal.append_started("one".to_string()).await.unwrap();
    journal
        .append_result("one".to_string(), result("first"))
        .await
        .unwrap();
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(reopened.replay("one").await.unwrap(), Some(result("first")));
}

#[tokio::test]
async fn source_miss_invalidates_only_that_key_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let current_path = directory.path().join("current");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result("one".to_string(), result("first"))
        .await
        .unwrap();
    source
        .append_result("three".to_string(), result("third"))
        .await
        .unwrap();
    let current = FileWorkflowJournal::open(current_path.clone(), Some(&source_path))
        .await
        .unwrap();

    assert_eq!(current.replay("one").await.unwrap(), Some(result("first")));
    assert_eq!(current.replay("two").await.unwrap(), None);
    assert_eq!(
        current.replay("three").await.unwrap(),
        Some(result("third"))
    );
    source
        .append_result("two".to_string(), result("late"))
        .await
        .unwrap();
    drop(current);

    let reopened = FileWorkflowJournal::open(current_path, Some(&source_path))
        .await
        .unwrap();
    assert_eq!(reopened.replay("two").await.unwrap(), None);
    assert_eq!(
        reopened.replay("three").await.unwrap(),
        Some(result("third"))
    );
}

#[tokio::test]
async fn started_tombstone_does_not_hide_other_source_results() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    for key in ["one", "two"] {
        source
            .append_result(key.to_string(), result(key))
            .await
            .unwrap();
    }
    let current = FileWorkflowJournal::open(directory.path().join("current"), Some(&source_path))
        .await
        .unwrap();
    current.append_started("one".to_string()).await.unwrap();
    assert_eq!(current.file_state.started_syncs.load(Ordering::Relaxed), 1);

    assert_eq!(current.replay("one").await.unwrap(), None);
    assert_eq!(current.replay("two").await.unwrap(), Some(result("two")));
}

#[tokio::test]
async fn prefix_result_remains_available_after_an_unrelated_source_miss() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result("cached".to_string(), result("cached"))
        .await
        .unwrap();
    let current = FileWorkflowJournal::open(directory.path().join("current"), Some(&source_path))
        .await
        .unwrap();

    assert_eq!(
        current.replay("cached").await.unwrap(),
        Some(result("cached"))
    );
    assert_eq!(current.replay("missing").await.unwrap(), None);
    assert_eq!(
        current.replay("cached").await.unwrap(),
        Some(result("cached"))
    );
}

#[tokio::test]
async fn failure_and_skip_outcomes_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let failed = failed_result(WorkflowAgentFailureKind::TerminalApi, "terminal failure");
    let skipped = failed_result(WorkflowAgentFailureKind::Skipped, "skipped by user");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal
        .append_result("failed".to_string(), failed.clone())
        .await
        .unwrap();
    journal
        .append_result("skipped".to_string(), skipped.clone())
        .await
        .unwrap();
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(reopened.replay("failed").await.unwrap(), Some(failed));
    assert_eq!(reopened.replay("skipped").await.unwrap(), Some(skipped));
}

#[tokio::test]
async fn independent_instances_observe_each_others_durable_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let first = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let second = FileWorkflowJournal::open(path, None).await.unwrap();
    first
        .append_result("one".to_string(), result("old"))
        .await
        .unwrap();
    assert_eq!(second.replay("one").await.unwrap(), Some(result("old")));

    second.append_started("one".to_string()).await.unwrap();
    assert_eq!(first.replay("one").await.unwrap(), None);
}

#[cfg(unix)]
#[tokio::test]
async fn hard_link_aliases_share_locking_and_storage_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let first = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let alias = directory.path().join("journal-alias");
    std::fs::hard_link(&path, &alias).unwrap();
    let second = FileWorkflowJournal::open(alias, None).await.unwrap();

    assert!(Arc::ptr_eq(&first.file_state, &second.file_state));
    first
        .append_result("one".to_string(), result("shared"))
        .await
        .unwrap();
    assert_eq!(second.replay("one").await.unwrap(), Some(result("shared")));
}

#[tokio::test]
async fn same_path_resume_separates_source_and_current_generations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let source = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    source
        .append_result("one".to_string(), result("source-one"))
        .await
        .unwrap();
    source
        .append_result("three".to_string(), result("source-three"))
        .await
        .unwrap();

    let current = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(
        current.replay("one").await.unwrap(),
        Some(result("source-one"))
    );
    assert_eq!(current.replay("two").await.unwrap(), None);
    assert_eq!(
        current.replay("three").await.unwrap(),
        Some(result("source-three"))
    );

    source.append_started("one".to_string()).await.unwrap();
    assert_eq!(
        current.replay("one").await.unwrap(),
        Some(result("source-one"))
    );

    source
        .append_result("two".to_string(), result("late-source"))
        .await
        .unwrap();
    assert_eq!(current.replay("two").await.unwrap(), None);
    current
        .append_result("one".to_string(), result("current-one"))
        .await
        .unwrap();
    drop(current);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(
        reopened.replay("one").await.unwrap(),
        Some(result("current-one"))
    );
    assert_eq!(reopened.replay("two").await.unwrap(), None);
    assert_eq!(reopened.replay("three").await.unwrap(), None);
}

#[tokio::test]
async fn repeated_resume_retains_untouched_results_from_every_prior_generation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let first = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    for key in ["read", "untouched", "invalidated"] {
        first
            .append_result(key.to_string(), result(key))
            .await
            .unwrap();
    }
    first.close().await.unwrap();
    drop(first);

    let second = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(second.replay("read").await.unwrap(), Some(result("read")));
    second
        .append_started("invalidated".to_string())
        .await
        .unwrap();
    drop(second);

    let third = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(
        third.replay("untouched").await.unwrap(),
        Some(result("untouched"))
    );
    assert_eq!(third.replay("invalidated").await.unwrap(), None);
    drop(third);

    let fourth = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(fourth.replay("read").await.unwrap(), Some(result("read")));
    assert_eq!(
        fourth.replay("untouched").await.unwrap(),
        Some(result("untouched"))
    );
    assert_eq!(fourth.replay("invalidated").await.unwrap(), None);
}

#[tokio::test]
async fn different_path_resumes_persist_the_complete_source_lineage() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first");
    let first = FileWorkflowJournal::open(first_path.clone(), None)
        .await
        .unwrap();
    first
        .append_result("untouched".to_string(), result("first"))
        .await
        .unwrap();
    first.close().await.unwrap();
    drop(first);

    let second_path = directory.path().join("second");
    let second = FileWorkflowJournal::open(second_path.clone(), Some(&first_path))
        .await
        .unwrap();
    assert_eq!(
        second.replay("untouched").await.unwrap(),
        Some(result("first"))
    );
    drop(second);

    let reopened = FileWorkflowJournal::open(second_path.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        reopened.replay("untouched").await.unwrap(),
        Some(result("first"))
    );
    drop(reopened);

    let third =
        FileWorkflowJournal::open(directory.path().join("third"), Some(second_path.as_path()))
            .await
            .unwrap();
    assert_eq!(
        third.replay("untouched").await.unwrap(),
        Some(result("first"))
    );
}

#[tokio::test]
async fn marker_reopens_from_the_last_complete_generation_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.jsonl");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal
        .append_result("agent-a".to_string(), result("durable"))
        .await
        .unwrap();
    let mut marker = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut marker, b"incomplete marker").unwrap();
    marker.sync_all().unwrap();
    drop(marker);
    drop(journal);

    let reopened = FileWorkflowJournal::open(path.clone(), None).await.unwrap();

    assert_eq!(
        reopened.replay("agent-a").await.unwrap(),
        Some(result("durable"))
    );
    drop(reopened);

    let resumed = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(
        resumed.replay("agent-a").await.unwrap(),
        Some(result("durable"))
    );
    assert!(
        !std::fs::read(path)
            .unwrap()
            .windows(b"incomplete marker".len())
            .any(|window| window == b"incomplete marker")
    );
}

#[tokio::test]
async fn checksum_invalid_final_record_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal
        .append_result("one".to_string(), result("untampered"))
        .await
        .unwrap();
    let segment = segment_path(&journal.storage_directory, key_segment("one"));
    let mut bytes = std::fs::read(&segment).unwrap();
    let value = bytes
        .windows(b"untampered".len())
        .position(|window| window == b"untampered")
        .unwrap();
    bytes[value] = b'x';
    std::fs::write(&segment, bytes).unwrap();
    std::fs::File::open(&segment).unwrap().sync_all().unwrap();
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();

    assert_eq!(reopened.replay("one").await.unwrap(), None);
}

#[tokio::test]
async fn checksum_invalid_final_tombstone_does_not_expose_an_older_result() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result("one".to_string(), result("stale"))
        .await
        .unwrap();
    source.close().await.unwrap();
    drop(source);

    let current_path = directory.path().join("current");
    let current = FileWorkflowJournal::open(current_path.clone(), Some(&source_path))
        .await
        .unwrap();
    current.append_started("one".to_string()).await.unwrap();
    let segment = segment_path(&current.storage_directory, key_segment("one"));
    let mut bytes = std::fs::read(&segment).unwrap();
    let state = bytes
        .windows(b"started".len())
        .rposition(|window| window == b"started")
        .unwrap();
    bytes[state] = b'x';
    std::fs::write(&segment, bytes).unwrap();
    drop(current);

    let resumed = FileWorkflowJournal::open(
        directory.path().join("resumed"),
        Some(current_path.as_path()),
    )
    .await
    .unwrap();
    assert_eq!(resumed.replay("one").await.unwrap(), None);
    drop(resumed);

    let reopened = FileWorkflowJournal::open(directory.path().join("resumed"), None)
        .await
        .unwrap();
    assert_eq!(reopened.replay("one").await.unwrap(), None);
}

#[tokio::test]
async fn unrelated_append_preserves_a_checksum_invalid_final_tombstone() {
    let directory = tempfile::tempdir().unwrap();
    let keys = keys_by_segment(/*keys_per_segment*/ 2);
    let invalidated = &keys[0][0];
    let unrelated = &keys[0][1];
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result(invalidated.clone(), result("stale"))
        .await
        .unwrap();
    source.close().await.unwrap();
    drop(source);

    let current_path = directory.path().join("current");
    let current = FileWorkflowJournal::open(current_path.clone(), Some(&source_path))
        .await
        .unwrap();
    current.append_started(invalidated.clone()).await.unwrap();
    let segment = segment_path(&current.storage_directory, key_segment(invalidated));
    let mut bytes = std::fs::read(&segment).unwrap();
    bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' };
    std::fs::write(&segment, bytes).unwrap();

    let error = current.append_started(unrelated.clone()).await.unwrap_err();
    assert_eq!(error, CHECKSUM_MISMATCH_ERROR);
    drop(current);

    let reopened = FileWorkflowJournal::open(current_path, None).await.unwrap();
    assert_eq!(reopened.replay(invalidated).await.unwrap(), None);
}

#[tokio::test]
async fn malformed_complete_middle_marker_record_is_fatal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let valid = serde_json::to_vec(&JournalMarker {
        storage_directory: journal.storage_directory.clone(),
        source_storage_directories: journal.source_storage_directories.clone(),
    })
    .unwrap();
    let mut marker = OpenOptions::new().append(true).open(&path).unwrap();
    marker.write_all(b"not-json\n").unwrap();
    marker.write_all(&valid).unwrap();
    marker.write_all(b"\n").unwrap();
    marker.sync_all().unwrap();
    drop(marker);
    drop(journal);

    let error = FileWorkflowJournal::open(path, None)
        .await
        .err()
        .expect("complete marker corruption should fail");
    assert!(error.contains("workflow journal marker record 1 is invalid"));
}

#[tokio::test]
async fn checksum_invalid_middle_record_is_fatal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let keys = keys_by_segment(/*keys_per_segment*/ 2);
    let first = &keys[0][0];
    let second = &keys[0][1];
    journal
        .append_result(first.clone(), result("first"))
        .await
        .unwrap();
    journal
        .append_result(second.clone(), result("second"))
        .await
        .unwrap();
    journal.close().await.unwrap();
    let segment = segment_path(&journal.storage_directory, key_segment(first));
    let mut bytes = std::fs::read(&segment).unwrap();
    let value = bytes
        .windows(b"first".len())
        .position(|window| window == b"first")
        .unwrap();
    bytes[value] = b'x';
    std::fs::write(&segment, bytes).unwrap();
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(
        reopened.replay(second).await.unwrap_err(),
        CHECKSUM_MISMATCH_ERROR
    );
}

#[tokio::test]
async fn append_rejects_a_checksum_invalid_final_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let keys = keys_by_segment(/*keys_per_segment*/ 3);
    let first = &keys[0][0];
    let damaged = &keys[0][1];
    let replacement = &keys[0][2];
    journal
        .append_result(first.clone(), result("first"))
        .await
        .unwrap();
    journal
        .append_result(damaged.clone(), result("damaged"))
        .await
        .unwrap();
    journal.close().await.unwrap();
    let segment = segment_path(&journal.storage_directory, key_segment(first));
    let mut bytes = std::fs::read(&segment).unwrap();
    let value = bytes
        .windows(b"damaged".len())
        .position(|window| window == b"damaged")
        .unwrap();
    bytes[value] = b'x';
    std::fs::write(&segment, bytes).unwrap();

    let error = journal
        .append_result(replacement.clone(), result("replacement"))
        .await
        .unwrap_err();
    assert_eq!(error, CHECKSUM_MISMATCH_ERROR);
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(reopened.replay(first).await.unwrap(), None);
    assert_eq!(reopened.replay(damaged).await.unwrap(), None);
    assert_eq!(reopened.replay(replacement).await.unwrap(), None);
}

#[tokio::test]
async fn torn_tail_is_ignored_and_truncated_before_the_next_append() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal
        .append_result("one".to_string(), result("first"))
        .await
        .unwrap();
    let segment = segment_path(&journal.storage_directory, key_segment("one"));
    let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(b"incomplete record").unwrap();
    file.sync_all().unwrap();
    drop(file);
    drop(journal);

    let reopened = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    assert_eq!(reopened.replay("one").await.unwrap(), Some(result("first")));
    reopened
        .append_result("one".to_string(), result("second"))
        .await
        .unwrap();
    drop(reopened);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(
        reopened.replay("one").await.unwrap(),
        Some(result("second"))
    );
    let bytes = std::fs::read(segment).unwrap();
    assert!(bytes.ends_with(b"\n"));
    assert!(
        !bytes
            .windows(b"incomplete record".len())
            .any(|window| { window == b"incomplete record" })
    );
}

#[tokio::test]
async fn different_generation_paths_replay_independently() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result("one".to_string(), result("source"))
        .await
        .unwrap();
    let current = FileWorkflowJournal::open(directory.path().join("current"), Some(&source_path))
        .await
        .unwrap();
    current
        .append_result("one".to_string(), result("current"))
        .await
        .unwrap();

    assert_eq!(source.replay("one").await.unwrap(), Some(result("source")));
    assert_eq!(
        current.replay("one").await.unwrap(),
        Some(result("current"))
    );
}

#[tokio::test]
async fn source_generation_io_errors_propagate() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source
        .append_result("one".to_string(), result("source"))
        .await
        .unwrap();
    source.close().await.unwrap();
    let source_segment = segment_path(&source.storage_directory, key_segment("one"));
    drop(source);
    let current = FileWorkflowJournal::open(directory.path().join("current"), Some(&source_path))
        .await
        .unwrap();
    std::fs::remove_file(source_segment).unwrap();

    let error = current.replay("one").await.unwrap_err();
    assert!(!error.is_empty());
}

#[tokio::test]
async fn source_generation_checksum_errors_propagate() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    let keys = keys_by_segment(/*keys_per_segment*/ 2);
    let first = &keys[0][0];
    let second = &keys[0][1];
    source
        .append_result(first.clone(), result("first"))
        .await
        .unwrap();
    source
        .append_result(second.clone(), result("second"))
        .await
        .unwrap();
    source.close().await.unwrap();
    let segment = segment_path(&source.storage_directory, key_segment(first));
    let mut bytes = std::fs::read(&segment).unwrap();
    let value = bytes
        .windows(b"first".len())
        .position(|window| window == b"first")
        .unwrap();
    bytes[value] = b'x';
    std::fs::write(&segment, bytes).unwrap();
    drop(source);
    let current = FileWorkflowJournal::open(directory.path().join("current"), Some(&source_path))
        .await
        .unwrap();

    assert_eq!(
        current.replay(second).await.unwrap_err(),
        CHECKSUM_MISMATCH_ERROR
    );
}

#[tokio::test]
async fn sequential_invocations_continue_beyond_the_old_threshold() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    assert_eq!(
        std::fs::read_dir(&journal.storage_directory)
            .unwrap()
            .count(),
        JOURNAL_SEGMENT_COUNT
    );
    for index in 0..=OLD_ACTIVE_INVOCATION_THRESHOLD {
        let key = format!("agent-{index}");
        journal.append_started(key.clone()).await.unwrap();
        journal
            .append_result(key, result(&format!("result-{index}")))
            .await
            .unwrap();
    }
    assert_eq!(journal.file_state.started_syncs.load(Ordering::Relaxed), 0);
    let result_syncs = journal.file_state.result_syncs.load(Ordering::Relaxed);
    assert!(result_syncs > 0);
    assert!(result_syncs <= (OLD_ACTIVE_INVOCATION_THRESHOLD + 1) / RESULT_GROUP_COMMIT_RECORDS);
    assert_eq!(
        std::fs::read_dir(&journal.storage_directory)
            .unwrap()
            .count(),
        JOURNAL_SEGMENT_COUNT
    );
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(
        reopened.replay("agent-0").await.unwrap(),
        Some(result("result-0"))
    );
    assert_eq!(
        reopened
            .replay(&format!("agent-{OLD_ACTIVE_INVOCATION_THRESHOLD}"))
            .await
            .unwrap(),
        Some(result(&format!("result-{OLD_ACTIVE_INVOCATION_THRESHOLD}")))
    );
}

#[tokio::test]
async fn explicit_close_flushes_a_partial_result_group() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    let file_state = Arc::clone(&journal.file_state);
    journal
        .append_result("one".to_string(), result("committed-on-close"))
        .await
        .unwrap();
    assert_eq!(file_state.result_syncs.load(Ordering::Relaxed), 0);

    journal.close().await.unwrap();
    assert_eq!(file_state.result_syncs.load(Ordering::Relaxed), 1);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    assert_eq!(
        reopened.replay("one").await.unwrap(),
        Some(result("committed-on-close"))
    );
}

#[tokio::test]
async fn large_journal_replay_reads_only_the_requested_segment() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    for index in 0..=OLD_ACTIVE_INVOCATION_THRESHOLD {
        let key = format!("agent-{index}");
        journal.append_started(key.clone()).await.unwrap();
        journal.append_result(key, result("result")).await.unwrap();
    }
    journal.close().await.unwrap();
    drop(journal);
    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    reopened.file_state.replay_reads.store(0, Ordering::Relaxed);

    assert_eq!(
        reopened.replay("agent-0").await.unwrap(),
        Some(result("result"))
    );
    assert_eq!(reopened.file_state.replay_reads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn started_syncs_only_when_invalidating_a_replayable_result() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    journal
        .append_started("first-ever".to_string())
        .await
        .unwrap();
    assert_eq!(journal.file_state.started_syncs.load(Ordering::Relaxed), 0);
    journal
        .append_result("replayable".to_string(), result("old"))
        .await
        .unwrap();
    journal.close().await.unwrap();
    drop(journal);

    let reopened = FileWorkflowJournal::open(path, None).await.unwrap();
    reopened
        .append_started("replayable".to_string())
        .await
        .unwrap();
    assert_eq!(reopened.file_state.started_syncs.load(Ordering::Relaxed), 1);
    assert_eq!(reopened.replay("replayable").await.unwrap(), None);
}

#[tokio::test]
async fn one_owner_has_at_most_4032_pending_results() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path, None).await.unwrap();
    let keys = keys_by_segment(RESULT_GROUP_COMMIT_RECORDS);

    for segment_keys in &keys {
        for key in &segment_keys[..RESULT_GROUP_COMMIT_RECORDS - 1] {
            journal
                .append_result(key.clone(), result("pending"))
                .await
                .unwrap();
        }
    }
    let pending = *journal.file_state.pending_result_records.lock().unwrap();
    assert_eq!(
        pending,
        [RESULT_GROUP_COMMIT_RECORDS - 1; JOURNAL_SEGMENT_COUNT]
    );
    assert_eq!(pending.iter().sum::<usize>(), MAX_PENDING_RESULTS_PER_OWNER);
    assert_eq!(journal.file_state.result_syncs.load(Ordering::Relaxed), 0);

    for segment_keys in &keys {
        journal
            .append_result(
                segment_keys[RESULT_GROUP_COMMIT_RECORDS - 1].clone(),
                result("group boundary"),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        *journal.file_state.pending_result_records.lock().unwrap(),
        [0; JOURNAL_SEGMENT_COUNT]
    );
    assert_eq!(
        journal.file_state.result_syncs.load(Ordering::Relaxed),
        JOURNAL_SEGMENT_COUNT
    );
}

#[tokio::test]
async fn close_flushes_every_segment_and_returns_the_first_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = FileWorkflowJournal::open(path, None).await.unwrap();
    let keys = keys_by_segment(/*keys_per_segment*/ 1);
    for segment in 0..3 {
        journal
            .append_result(keys[segment][0].clone(), result("pending"))
            .await
            .unwrap();
    }
    journal.file_state.flush_failures.lock().unwrap().extend([
        (0, "first sync failure".to_string()),
        (1, "second sync failure".to_string()),
    ]);

    assert_eq!(journal.close().await.unwrap_err(), "first sync failure");
    assert_eq!(
        *journal.file_state.flush_attempts.lock().unwrap(),
        vec![0, 1, 2]
    );
    let pending = *journal.file_state.pending_result_records.lock().unwrap();
    assert_eq!(&pending[..3], &[1, 1, 0]);

    journal.file_state.flush_failures.lock().unwrap().clear();
    journal.file_state.flush_attempts.lock().unwrap().clear();
    journal.close().await.unwrap();
    assert_eq!(
        *journal.file_state.flush_attempts.lock().unwrap(),
        vec![0, 1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_io_does_not_hold_an_async_runtime_worker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = Arc::new(FileWorkflowJournal::open(path, None).await.unwrap());
    journal
        .append_result("one".to_string(), result("pending"))
        .await
        .unwrap();
    let hook = Arc::new(AppendTestHook {
        entered: std::sync::Barrier::new(2),
        proceed: std::sync::Barrier::new(2),
    });
    *journal.file_state.flush_hook.lock().unwrap() = Some(Arc::clone(&hook));
    let close = {
        let journal = Arc::clone(&journal);
        tokio::spawn(async move { journal.close().await })
    };
    let entered = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || entered.entered.wait())
        .await
        .unwrap();
    assert_eq!(tokio::spawn(async { 42_u8 }).await.unwrap(), 42);
    let proceed = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || proceed.proceed.wait())
        .await
        .unwrap();
    close.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_io_does_not_hold_an_async_runtime_worker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = Arc::new(FileWorkflowJournal::open(path, None).await.unwrap());
    let hook = Arc::new(AppendTestHook {
        entered: std::sync::Barrier::new(2),
        proceed: std::sync::Barrier::new(2),
    });
    *journal.file_state.append_hook.lock().unwrap() = Some(Arc::clone(&hook));
    let append = {
        let journal = Arc::clone(&journal);
        tokio::spawn(async move { journal.append_started("one".to_string()).await })
    };
    let entered = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || entered.entered.wait())
        .await
        .unwrap();
    let runtime_progress = tokio::spawn(async { 42_u8 });
    assert_eq!(runtime_progress.await.unwrap(), 42);
    let proceed = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || proceed.proceed.wait())
        .await
        .unwrap();
    append.await.unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn replay_waiting_for_a_durable_writer_does_not_block_a_current_thread_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal");
    let journal = Arc::new(FileWorkflowJournal::open(path, None).await.unwrap());
    journal
        .append_result("one".to_string(), result("old"))
        .await
        .unwrap();
    let hook = Arc::new(AppendTestHook {
        entered: std::sync::Barrier::new(2),
        proceed: std::sync::Barrier::new(2),
    });
    *journal.file_state.append_hook.lock().unwrap() = Some(Arc::clone(&hook));
    let append = {
        let journal = Arc::clone(&journal);
        tokio::spawn(async move { journal.append_started("one".to_string()).await })
    };
    let entered = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || entered.entered.wait())
        .await
        .unwrap();
    let replay_hook = Arc::new(AppendTestHook {
        entered: std::sync::Barrier::new(2),
        proceed: std::sync::Barrier::new(2),
    });
    *journal.file_state.replay_hook.lock().unwrap() = Some(Arc::clone(&replay_hook));
    let replay = {
        let journal = Arc::clone(&journal);
        tokio::spawn(async move { journal.replay("one").await })
    };
    let replay_entered = Arc::clone(&replay_hook);
    tokio::task::spawn_blocking(move || replay_entered.entered.wait())
        .await
        .unwrap();
    let replay_proceed = Arc::clone(&replay_hook);
    tokio::task::spawn_blocking(move || replay_proceed.proceed.wait())
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(tokio::spawn(async { 42_u8 }).await.unwrap(), 42);
    let proceed = Arc::clone(&hook);
    tokio::task::spawn_blocking(move || proceed.proceed.wait())
        .await
        .unwrap();
    append.await.unwrap().unwrap();
    assert_eq!(replay.await.unwrap().unwrap(), None);
}
