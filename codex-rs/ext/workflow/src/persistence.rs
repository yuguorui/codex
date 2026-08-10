use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

use crate::service::WorkflowTaskSnapshot;

pub(crate) fn workflow_session_dir(
    codex_home: &AbsolutePathBuf,
    thread_id: ThreadId,
) -> AbsolutePathBuf {
    codex_home.join("sessions").join(thread_id.to_string())
}

pub(crate) fn snapshot_path(snapshot: &WorkflowTaskSnapshot) -> Result<AbsolutePathBuf, String> {
    Ok(snapshot
        .script_path
        .parent()
        .and_then(|scripts| scripts.parent())
        .ok_or_else(|| "persisted workflow script path has no workflow parent".to_string())?
        .join(format!("{}.json", snapshot.run_id)))
}

pub(crate) fn journal_path(transcript_dir: &AbsolutePathBuf) -> PathBuf {
    transcript_dir.join("journal.jsonl").to_path_buf()
}

pub(crate) async fn write_json(
    path: impl AsRef<Path>,
    value: &impl Serialize,
) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let path = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || codex_utils_path::write_atomically(&path, &contents))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

pub(crate) async fn load_snapshots(
    codex_home: &AbsolutePathBuf,
    thread_id: ThreadId,
) -> Result<Vec<WorkflowTaskSnapshot>, String> {
    let directory = workflow_session_dir(codex_home, thread_id).join("workflows");
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut snapshots: Vec<WorkflowTaskSnapshot> = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to read workflow snapshot");
                continue;
            }
        };
        match serde_json::from_slice::<WorkflowTaskSnapshot>(&bytes) {
            Ok(mut snapshot) => {
                let output_file = match AbsolutePathBuf::try_from(path.clone()) {
                    Ok(output_file) => output_file,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "workflow snapshot path is not absolute");
                        continue;
                    }
                };
                snapshot.output_file = output_file;
                snapshots.push(snapshot);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to parse workflow snapshot");
            }
        }
    }
    snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.started_at));
    Ok(snapshots)
}
