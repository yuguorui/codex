use std::sync::Arc;

use codex_workflow::WorkflowInputArtifactStore;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::FileWorkflowInputArtifactStore;

#[tokio::test]
async fn persists_and_verifies_content_addressed_inputs() {
    let root = tempfile::tempdir().unwrap();
    let store = FileWorkflowInputArtifactStore::new(root.path().join("artifacts"), None);
    let value = json!({"report": "x".repeat(2 * 1024 * 1024)});

    let reference = store.put(value.clone()).await.unwrap();
    let loaded = store.get(&reference).await.unwrap();

    assert_eq!(loaded.as_ref(), &value);
    assert!(
        root.path()
            .join("artifacts")
            .join(format!("{}.json", reference.sha256))
            .is_file()
    );
}

#[tokio::test]
async fn resumed_store_reads_source_artifacts_by_content_hash() {
    let root = tempfile::tempdir().unwrap();
    let source_directory = root.path().join("source");
    let source = FileWorkflowInputArtifactStore::new(source_directory.clone(), None);
    let value = json!({"reports": ["one", "two", "three"]});
    let reference = source.put(value.clone()).await.unwrap();
    drop(source);

    let resumed =
        FileWorkflowInputArtifactStore::new(root.path().join("current"), Some(source_directory));

    assert_eq!(resumed.get(&reference).await.unwrap().as_ref(), &value);
}

#[tokio::test]
async fn concurrent_equal_values_share_one_artifact() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(FileWorkflowInputArtifactStore::new(
        root.path().join("artifacts"),
        None,
    ));
    let value = json!({"body": "same content"});
    let (left, right) = tokio::join!(store.put(value.clone()), store.put(value));

    assert_eq!(left.unwrap(), right.unwrap());
    assert_eq!(
        std::fs::read_dir(root.path().join("artifacts"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn rejects_content_replaced_after_reference_creation() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("artifacts");
    let store = FileWorkflowInputArtifactStore::new(directory.clone(), None);
    let reference = store.put(json!({"trusted": true})).await.unwrap();
    std::fs::write(
        directory.join(format!("{}.json", reference.sha256)),
        br#"{"trusted":false}"#,
    )
    .unwrap();

    assert!(
        store
            .get(&reference)
            .await
            .unwrap_err()
            .contains("content hash")
    );
}
