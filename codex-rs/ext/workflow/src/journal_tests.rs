use super::*;
use codex_workflow::WorkflowTokenUsage;
use pretty_assertions::assert_eq;
use serde_json::json;

fn result(value: &str) -> WorkflowAgentResult {
    WorkflowAgentResult {
        value: json!(value),
        usage: WorkflowTokenUsage {
            total_tokens: 12,
            tool_uses: 3,
        },
        agent_id: Some("agent-1".to_string()),
        model: Some("model".to_string()),
        fallback_model: None,
    }
}

#[tokio::test]
async fn persists_results_and_stops_replay_after_first_miss() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.jsonl");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source.append_started("one".to_string()).await.unwrap();
    source
        .append_result("one".to_string(), result("first"))
        .await
        .unwrap();
    source
        .append_result("three".to_string(), result("third"))
        .await
        .unwrap();

    let resumed =
        FileWorkflowJournal::open(directory.path().join("resumed.jsonl"), Some(&source_path))
            .await
            .unwrap();
    assert_eq!(resumed.replay("one"), Some(result("first")));
    assert_eq!(resumed.replay("two"), None);
    assert_eq!(resumed.replay("three"), None);
}

#[tokio::test]
async fn unfinished_entry_disables_all_later_replay() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.jsonl");
    let source = FileWorkflowJournal::open(source_path.clone(), None)
        .await
        .unwrap();
    source.append_started("one".to_string()).await.unwrap();
    source.append_started("two".to_string()).await.unwrap();
    source
        .append_result("two".to_string(), result("second"))
        .await
        .unwrap();

    let resumed =
        FileWorkflowJournal::open(directory.path().join("resumed.jsonl"), Some(&source_path))
            .await
            .unwrap();

    assert_eq!(resumed.replay("one"), None);
    assert_eq!(resumed.replay("two"), None);
}

#[tokio::test]
async fn a_journal_can_be_resumed_repeatedly_after_its_call_chain_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.jsonl");
    let original = FileWorkflowJournal::open(path.clone(), None).await.unwrap();
    for (key, value) in [("one", "first"), ("old-two", "old second")] {
        original.append_started(key.to_string()).await.unwrap();
        original
            .append_result(key.to_string(), result(value))
            .await
            .unwrap();
    }

    let edited = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(edited.replay("one"), Some(result("first")));
    assert_eq!(edited.replay("new-two"), None);
    edited.append_started("new-two".to_string()).await.unwrap();
    edited
        .append_result("new-two".to_string(), result("new second"))
        .await
        .unwrap();

    let resumed_again = FileWorkflowJournal::open(path.clone(), Some(&path))
        .await
        .unwrap();
    assert_eq!(resumed_again.replay("one"), Some(result("first")));
    assert_eq!(resumed_again.replay("new-two"), Some(result("new second")));
}

#[tokio::test]
async fn rejects_journal_with_too_many_entries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oversized.jsonl");
    let contents = (0..=MAX_JOURNAL_ENTRIES)
        .map(|index| format!(r#"{{"type":"started","key":"{index}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, contents).await.unwrap();

    let error = FileWorkflowJournal::open(path, None)
        .await
        .err()
        .expect("oversized journal should fail");

    assert_eq!(
        error,
        format!("workflow journal exceeds the {MAX_JOURNAL_ENTRIES}-entry limit")
    );
}

#[tokio::test]
async fn rejects_journal_larger_than_byte_limit_before_reading_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-large.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();

    let error = FileWorkflowJournal::open(path, None)
        .await
        .err()
        .expect("oversized journal should fail");

    assert_eq!(
        error,
        format!("workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit")
    );
}

#[tokio::test]
async fn rejects_append_that_would_exceed_byte_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nearly-full.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_JOURNAL_BYTES - 1).unwrap();
    let journal = FileWorkflowJournal {
        path: path.clone(),
        replay: Mutex::new(ReplayState {
            enabled: false,
            results: HashMap::new(),
        }),
        write_lock: Semaphore::new(1),
        entry_count: AtomicUsize::new(0),
    };

    let error = journal
        .append_started("one".to_string())
        .await
        .expect_err("append should exceed the byte limit");

    assert_eq!(
        error,
        format!("workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit")
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        MAX_JOURNAL_BYTES - 1
    );
}
