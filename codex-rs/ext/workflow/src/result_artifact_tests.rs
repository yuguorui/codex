use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;

#[tokio::test]
async fn large_artifact_reassembles_across_repeated_utf8_reads() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let value = json!({ "payload": "你好\u{0000}".repeat(200_000) });
    let serialized = Arc::<str>::from(serde_json::to_string(&value).unwrap());
    let artifact = persist_result_artifact(&snapshot_path, Arc::clone(&serialized))
        .await
        .unwrap();

    let repeated = persist_result_artifact(&snapshot_path, Arc::clone(&serialized))
        .await
        .unwrap();
    assert_eq!(repeated.sha256, artifact.sha256);
    assert_eq!(repeated.bytes, artifact.bytes);
    assert_ne!(repeated.storage_id, artifact.storage_id);

    let verified = load_verified_result_artifact(&snapshot_path, &artifact)
        .await
        .unwrap();
    let mut offset = 0;
    let mut assembled = String::new();
    while offset < artifact.bytes {
        let chunk = read_verified_result_chunk(&verified, offset, 257).unwrap();
        assert_eq!(chunk.offset, offset);
        assert!(chunk.next_offset > offset);
        assembled.push_str(&chunk.text);
        offset = chunk.next_offset;
    }

    assert_eq!(
        serde_json::from_str::<JsonValue>(&assembled).unwrap(),
        value
    );
}

#[tokio::test]
async fn desired_pages_smaller_than_utf8_scalars_still_reassemble_losslessly() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let value = json!({ "payload": "a😀你b".repeat(20) });
    let serialized = Arc::<str>::from(serde_json::to_string(&value).unwrap());
    let artifact = persist_result_artifact(&snapshot_path, Arc::clone(&serialized))
        .await
        .unwrap();
    let verified = load_verified_result_artifact(&snapshot_path, &artifact)
        .await
        .unwrap();

    for max_bytes in 1..=3 {
        let mut offset = 0;
        let mut assembled = String::new();
        while offset < artifact.bytes {
            let chunk = read_verified_result_chunk(&verified, offset, max_bytes).unwrap();
            assert!(chunk.next_offset > offset);
            assembled.push_str(&chunk.text);
            offset = chunk.next_offset;
        }
        assert_eq!(assembled, serialized.as_ref());
    }
}

#[tokio::test]
async fn concurrent_reads_share_verified_contents_safely() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let serialized = Arc::<str>::from(
        serde_json::to_string(&json!({
            "payload": "0123456789".repeat(10_000)
        }))
        .unwrap(),
    );
    let artifact = persist_result_artifact(&snapshot_path, serialized)
        .await
        .unwrap();
    let verified = load_verified_result_artifact(&snapshot_path, &artifact)
        .await
        .unwrap();

    let reads = (0..16)
        .map(|_| {
            let verified = verified.clone();
            tokio::spawn(async move { read_verified_result_chunk(&verified, 0, 511).unwrap() })
        })
        .collect::<Vec<_>>();
    let mut chunks = Vec::with_capacity(reads.len());
    for read in reads {
        chunks.push(read.await);
    }
    let first = chunks[0].as_ref().unwrap();
    for chunk in chunks.iter().skip(1) {
        assert_eq!(chunk.as_ref().unwrap(), first);
    }
}

#[tokio::test]
async fn validation_rejects_torn_or_replaced_artifacts() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let artifact = persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(serde_json::to_string(&json!({ "answer": 42 })).unwrap()),
    )
    .await
    .unwrap();
    let path = result_artifact_path(&snapshot_path, &artifact).unwrap();

    tokio::fs::write(&path, br#"{"answer":4"#).await.unwrap();

    let error = load_verified_result_artifact(&snapshot_path, &artifact)
        .await
        .unwrap_err();
    assert!(error.contains("not valid JSON") || error.contains("length"));

    let repaired = persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(serde_json::to_string(&json!({ "answer": 42 })).unwrap()),
    )
    .await
    .unwrap();
    assert_ne!(repaired.storage_id, artifact.storage_id);
    load_verified_result_artifact(&snapshot_path, &repaired)
        .await
        .unwrap();
}

#[tokio::test]
async fn offsets_must_be_returned_utf8_boundaries() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let artifact = persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(serde_json::to_string(&json!("你好")).unwrap()),
    )
    .await
    .unwrap();
    let verified = load_verified_result_artifact(&snapshot_path, &artifact)
        .await
        .unwrap();

    let error = read_verified_result_chunk(&verified, 2, 32).unwrap_err();

    assert!(error.contains("nextOffset"));
}

#[tokio::test]
async fn restart_cleanup_removes_stale_temps_and_unreferenced_artifacts() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot_path = absolute(temporary.path().join("workflows/wf_test.json"));
    let referenced = persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(serde_json::to_string(&json!({ "kept": true })).unwrap()),
    )
    .await
    .unwrap();
    let unreferenced = persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(serde_json::to_string(&json!({ "removed": true })).unwrap()),
    )
    .await
    .unwrap();
    let results_directory = snapshot_path.parent().unwrap().join("results");
    let temp_path = results_directory.join(".abandoned.tmp");
    tokio::fs::write(&temp_path, b"partial").await.unwrap();
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(2 * 60 * 60))
        .unwrap();
    for path in [
        result_artifact_path(&snapshot_path, &unreferenced).unwrap(),
        temp_path.clone(),
    ] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    let snapshots_directory = absolute(snapshot_path.parent().unwrap().to_path_buf());
    cleanup_result_artifacts(
        &snapshots_directory,
        HashSet::from([referenced.file_name()]),
    )
    .await
    .unwrap();

    assert!(
        result_artifact_path(&snapshot_path, &referenced)
            .unwrap()
            .is_file()
    );
    assert!(
        !result_artifact_path(&snapshot_path, &unreferenced)
            .unwrap()
            .exists()
    );
    assert!(!temp_path.exists());
}

fn absolute(path: std::path::PathBuf) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(path).unwrap()
}
