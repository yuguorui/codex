use codex_protocol::ThreadId;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::service::LoadedWorkflowMetadata;
use crate::service::WorkflowTaskSnapshot;

const WORKFLOW_INDEX_DIRECTORY: &str = "index";
const WORKFLOW_INDEX_MANIFEST: &str = "manifest.json";
const WORKFLOW_INDEX_LOCK_FILE: &str = ".index.lock";
const WORKFLOW_INDEX_DIRTY_FILE: &str = ".dirty.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowIndexEntry {
    sequence: u64,
    run_id: String,
    first_started_at: i64,
    status: WorkflowTaskStatus,
    result_artifact: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowIndexManifest {
    next_sequence: u64,
    status_counts: [usize; 6],
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowIndexTransaction {
    sequence: u64,
    run_id: String,
    first_started_at: i64,
}

pub(crate) struct LoadedWorkflowSnapshot {
    pub(crate) snapshot: WorkflowTaskSnapshot,
    pub(crate) metadata: LoadedWorkflowMetadata,
}

pub(crate) struct RestoredWorkflowSnapshots {
    pub(crate) loaded: Vec<LoadedWorkflowSnapshot>,
    pub(crate) referenced_results: HashSet<String>,
}

pub(crate) struct IndexedWorkflowPage {
    pub(crate) snapshots: Vec<WorkflowTaskSnapshot>,
    pub(crate) snapshot_sequences: Vec<u64>,
    pub(crate) total_matched: usize,
    pub(crate) next_sequence: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedSnapshotEnvelope {
    #[serde(flatten)]
    snapshot: WorkflowTaskSnapshot,
    execution_context: crate::service::PersistedWorkflowExecutionContext,
    composition: crate::composition::PersistedWorkflowComposition,
}

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

pub(crate) fn journal_path(transcript_dir: &AbsolutePathBuf, task_id: &str) -> PathBuf {
    transcript_dir
        .join(format!("journal-{task_id}.jsonl"))
        .to_path_buf()
}

#[cfg(test)]
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

pub(crate) async fn write_indexed_snapshot(
    path: AbsolutePathBuf,
    contents: String,
    snapshot: &WorkflowTaskSnapshot,
) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "workflow snapshot path has no index directory".to_string())?
        .to_path_buf();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || {
        write_indexed_snapshot_blocking(&directory, &path, &contents, &snapshot)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn write_indexed_snapshot_blocking(
    directory: &Path,
    snapshot_path: &Path,
    contents: &str,
    snapshot: &WorkflowTaskSnapshot,
) -> Result<(), String> {
    validate_run_id(&snapshot.run_id)?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let lock_path = directory.join(WORKFLOW_INDEX_LOCK_FILE);
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;

    let index_directory = directory.join(WORKFLOW_INDEX_DIRECTORY);
    std::fs::create_dir_all(&index_directory).map_err(|error| error.to_string())?;
    repair_index_if_dirty_locked(directory, &index_directory)?;
    let manifest_path = index_directory.join(WORKFLOW_INDEX_MANIFEST);
    let mut manifest = read_json_file::<WorkflowIndexManifest>(&manifest_path)?.unwrap_or_default();
    let pointer_path = index_pointer_path(&index_directory, &snapshot.run_id);
    let existing_sequence = read_json_file::<u64>(&pointer_path)?;
    let existing = existing_sequence
        .and_then(|sequence| read_index_entry(&index_directory, sequence).ok().flatten());
    let existing_is_registered = existing
        .as_ref()
        .is_some_and(|entry| entry.sequence < manifest.next_sequence);
    let sequence = existing.as_ref().map_or_else(
        || {
            let sequence = manifest.next_sequence;
            manifest.next_sequence = manifest.next_sequence.saturating_add(1);
            sequence
        },
        |entry| entry.sequence,
    );
    let first_started_at = existing
        .as_ref()
        .map_or(snapshot.started_at, |entry| entry.first_started_at);
    let transaction = WorkflowIndexTransaction {
        sequence,
        run_id: snapshot.run_id.clone(),
        first_started_at,
    };
    let dirty_path = index_directory.join(WORKFLOW_INDEX_DIRTY_FILE);
    // Make the repair marker durable before either side of the snapshot/index commit changes.
    write_durable_json(&dirty_path, &transaction)?;
    sync_directory(&index_directory)?;

    codex_utils_path::write_atomically(snapshot_path, contents)
        .map_err(|error| error.to_string())?;
    std::fs::File::open(snapshot_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    sync_directory(directory)?;

    manifest.next_sequence = manifest.next_sequence.max(sequence.saturating_add(1));
    if let Some(existing) = existing.as_ref().filter(|_| existing_is_registered) {
        if existing.status != snapshot.status {
            decrement_status_count(&mut manifest.status_counts, existing.status);
            increment_status_count(&mut manifest.status_counts, snapshot.status);
        }
    } else {
        increment_status_count(&mut manifest.status_counts, snapshot.status);
    }
    let entry = WorkflowIndexEntry {
        sequence,
        run_id: snapshot.run_id.clone(),
        first_started_at,
        status: snapshot.status,
        result_artifact: snapshot
            .result_artifact
            .as_ref()
            .map(crate::result_artifact::WorkflowResultArtifact::file_name),
    };
    write_durable_json(&index_entry_path(&index_directory, sequence), &entry)?;
    if existing_sequence.is_none() {
        write_durable_json(&pointer_path, &sequence)?;
    }
    write_durable_json(&manifest_path, &manifest)?;
    sync_directory(&index_directory)?;
    std::fs::remove_file(&dirty_path).map_err(|error| error.to_string())?;

    lock.set_len(0).map_err(|error| error.to_string())?;
    lock.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    lock.write_all(snapshot.run_id.as_bytes())
        .map_err(|error| error.to_string())?;
    lock.sync_data().map_err(|error| error.to_string())
}

pub(crate) async fn load_restore_snapshots(
    codex_home: &AbsolutePathBuf,
    thread_id: ThreadId,
    retained_inactive_limit: usize,
) -> Result<RestoredWorkflowSnapshots, String> {
    let directory = workflow_session_dir(codex_home, thread_id).join("workflows");
    let index_directory = directory.join(WORKFLOW_INDEX_DIRECTORY);
    let owner = thread_id.to_string();
    tokio::task::spawn_blocking(move || {
        let _lock = lock_index(&directory)?;
        repair_index_if_dirty_locked(&directory, &index_directory)?;
        let entries = load_index_entries(&index_directory)?;
        let mut selected = Vec::new();
        let mut retained_inactive = 0_usize;
        let mut referenced_results = HashSet::new();
        for entry in entries {
            if let Some(result_artifact) = entry.result_artifact.clone() {
                referenced_results.insert(result_artifact);
            }
            let active = matches!(
                entry.status,
                WorkflowTaskStatus::Pending | WorkflowTaskStatus::Running
            );
            if active || retained_inactive < retained_inactive_limit {
                if !active {
                    retained_inactive = retained_inactive.saturating_add(1);
                }
                selected.push(entry);
            }
        }
        let mut loaded = Vec::with_capacity(selected.len());
        for entry in selected {
            let path = directory.join(format!("{}.json", entry.run_id));
            match load_validated_snapshot_blocking(&path, &entry.run_id, &owner) {
                Ok(mut value) if value.snapshot.status == entry.status => {
                    value.snapshot.started_at = entry.first_started_at;
                    loaded.push(value);
                }
                Ok(_) => {
                    tracing::warn!(path = %path.display(), "ignoring workflow snapshot whose status is not committed in the index");
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "ignoring invalid indexed workflow snapshot");
                }
            }
        }
        Ok(RestoredWorkflowSnapshots {
            loaded,
            referenced_results,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(crate) async fn load_snapshot(
    codex_home: &AbsolutePathBuf,
    thread_id: ThreadId,
    run_id: &str,
) -> Result<Option<LoadedWorkflowSnapshot>, String> {
    if validate_run_id(run_id).is_err() {
        return Ok(None);
    }
    let path = workflow_session_dir(codex_home, thread_id)
        .join("workflows")
        .join(format!("{run_id}.json"));
    match load_validated_snapshot(&path, run_id, &thread_id.to_string()).await {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(_) if !path.exists() => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) async fn load_snapshot_page(
    codex_home: &AbsolutePathBuf,
    thread_id: ThreadId,
    statuses: &[WorkflowTaskStatus],
    cursor: Option<u64>,
    limit: usize,
) -> Result<IndexedWorkflowPage, String> {
    let directory = workflow_session_dir(codex_home, thread_id).join("workflows");
    let index_directory = directory.join(WORKFLOW_INDEX_DIRECTORY);
    let owner = thread_id.to_string();
    let statuses = statuses.to_vec();
    tokio::task::spawn_blocking(move || {
        let _lock = lock_index(&directory)?;
        repair_index_if_dirty_locked(&directory, &index_directory)?;
        load_snapshot_page_blocking(
            &directory,
            &index_directory,
            &owner,
            &statuses,
            cursor,
            limit,
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

fn load_snapshot_page_blocking(
    directory: &AbsolutePathBuf,
    index_directory: &AbsolutePathBuf,
    owner: &str,
    statuses: &[WorkflowTaskStatus],
    cursor: Option<u64>,
    limit: usize,
) -> Result<IndexedWorkflowPage, String> {
    let manifest =
        read_json_file::<WorkflowIndexManifest>(&index_directory.join(WORKFLOW_INDEX_MANIFEST))?
            .unwrap_or_default();
    let mut sequence = cursor
        .unwrap_or(manifest.next_sequence)
        .min(manifest.next_sequence);
    let mut snapshots = Vec::new();
    let mut snapshot_sequences = Vec::new();
    let mut invalid_matching = 0_usize;
    let mut has_more = false;
    while sequence > 0 {
        sequence -= 1;
        let entry_path = index_entry_path(index_directory, sequence);
        let entry = match std::fs::read(&entry_path) {
            Ok(bytes) => serde_json::from_slice::<WorkflowIndexEntry>(&bytes).ok(),
            Err(_) => None,
        };
        let Some(entry) = entry
            .filter(|entry| entry.sequence == sequence && validate_run_id(&entry.run_id).is_ok())
        else {
            continue;
        };
        if !statuses.is_empty() && !statuses.contains(&entry.status) {
            continue;
        }
        if snapshots.len() == limit {
            has_more = true;
            break;
        }
        let path = directory.join(format!("{}.json", entry.run_id));
        match load_validated_snapshot_blocking(&path, &entry.run_id, owner) {
            Ok(mut loaded) if loaded.snapshot.status == entry.status => {
                loaded.snapshot.started_at = entry.first_started_at;
                snapshots.push(loaded.snapshot);
                snapshot_sequences.push(sequence);
            }
            Ok(_) | Err(_) => {
                invalid_matching = invalid_matching.saturating_add(1);
                tracing::warn!(path = %path.display(), "ignoring invalid indexed workflow snapshot");
            }
        }
    }
    let total_matched =
        status_total(&manifest.status_counts, statuses).saturating_sub(invalid_matching);
    let next_sequence = has_more
        .then(|| snapshot_sequences.last().copied())
        .flatten();
    Ok(IndexedWorkflowPage {
        snapshots,
        snapshot_sequences,
        total_matched,
        next_sequence,
    })
}

async fn load_validated_snapshot(
    path: &AbsolutePathBuf,
    expected_run_id: &str,
    expected_owner: &str,
) -> Result<LoadedWorkflowSnapshot, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| error.to_string())?;
    parse_validated_snapshot(path, expected_run_id, expected_owner, &bytes)
}

fn load_validated_snapshot_blocking(
    path: &AbsolutePathBuf,
    expected_run_id: &str,
    expected_owner: &str,
) -> Result<LoadedWorkflowSnapshot, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    parse_validated_snapshot(path, expected_run_id, expected_owner, &bytes)
}

pub(crate) fn parse_validated_snapshot(
    path: &AbsolutePathBuf,
    expected_run_id: &str,
    expected_owner: &str,
    bytes: &[u8],
) -> Result<LoadedWorkflowSnapshot, String> {
    let envelope: PersistedSnapshotEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse workflow snapshot: {error}"))?;
    if envelope.snapshot.run_id != expected_run_id {
        return Err("workflow snapshot run id does not match its file name".to_string());
    }
    if envelope.snapshot.thread_id != expected_owner {
        return Err("workflow snapshot thread id does not match its session directory".to_string());
    }
    let mut snapshot = envelope.snapshot;
    snapshot.output_file = path.clone();
    Ok(LoadedWorkflowSnapshot {
        snapshot,
        metadata: LoadedWorkflowMetadata {
            execution_context: envelope.execution_context,
            composition: envelope.composition,
        },
    })
}

fn load_index_entries(index_directory: &Path) -> Result<Vec<WorkflowIndexEntry>, String> {
    let manifest =
        read_json_file::<WorkflowIndexManifest>(&index_directory.join(WORKFLOW_INDEX_MANIFEST))?
            .unwrap_or_default();
    let mut entries = Vec::new();
    for sequence in (0..manifest.next_sequence).rev() {
        if let Some(entry) = read_index_entry(index_directory, sequence)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn lock_index(directory: &Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let lock_path = directory.join(WORKFLOW_INDEX_LOCK_FILE);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;
    Ok(lock)
}

fn repair_index_if_dirty_locked(directory: &Path, index_directory: &Path) -> Result<(), String> {
    let dirty_path = index_directory.join(WORKFLOW_INDEX_DIRTY_FILE);
    let Some(transaction) = read_json_file::<WorkflowIndexTransaction>(&dirty_path)? else {
        return Ok(());
    };
    validate_run_id(&transaction.run_id)?;
    let snapshot_path = directory.join(format!("{}.json", transaction.run_id));
    if let Ok(bytes) = std::fs::read(&snapshot_path)
        && let Ok(envelope) = serde_json::from_slice::<PersistedSnapshotEnvelope>(&bytes)
        && envelope.snapshot.run_id == transaction.run_id
    {
        let entry = WorkflowIndexEntry {
            sequence: transaction.sequence,
            run_id: transaction.run_id.clone(),
            first_started_at: transaction.first_started_at,
            status: envelope.snapshot.status,
            result_artifact: envelope
                .snapshot
                .result_artifact
                .as_ref()
                .map(crate::result_artifact::WorkflowResultArtifact::file_name),
        };
        write_durable_json(
            &index_entry_path(index_directory, transaction.sequence),
            &entry,
        )?;
        write_durable_json(
            &index_pointer_path(index_directory, &transaction.run_id),
            &transaction.sequence,
        )?;
    }
    rebuild_manifest(index_directory, transaction.sequence.saturating_add(1))?;
    sync_directory(index_directory)?;
    // A resurrected marker is harmless: repair is idempotent and the repaired index is durable.
    match std::fs::remove_file(&dirty_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

fn rebuild_manifest(index_directory: &Path, minimum_next_sequence: u64) -> Result<(), String> {
    let old_manifest =
        read_json_file::<WorkflowIndexManifest>(&index_directory.join(WORKFLOW_INDEX_MANIFEST))?
            .unwrap_or_default();
    let next_sequence = old_manifest.next_sequence.max(minimum_next_sequence);
    let mut manifest = WorkflowIndexManifest {
        next_sequence,
        status_counts: [0; 6],
    };
    for sequence in 0..next_sequence {
        if let Some(entry) = read_index_entry(index_directory, sequence)? {
            increment_status_count(&mut manifest.status_counts, entry.status);
        }
    }
    write_durable_json(&index_directory.join(WORKFLOW_INDEX_MANIFEST), &manifest)
}

fn read_index_entry(
    index_directory: &Path,
    sequence: u64,
) -> Result<Option<WorkflowIndexEntry>, String> {
    let entry = read_json_file::<WorkflowIndexEntry>(&index_entry_path(index_directory, sequence))?;
    Ok(entry.filter(|entry| entry.sequence == sequence && validate_run_id(&entry.run_id).is_ok()))
}

fn index_entry_path(index_directory: &Path, sequence: u64) -> PathBuf {
    index_directory.join(format!("{sequence:020}.json"))
}

fn index_pointer_path(index_directory: &Path, run_id: &str) -> PathBuf {
    index_directory.join(format!("run-{run_id}.json"))
}

fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.starts_with("wf_")
        && run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        Ok(())
    } else {
        Err("invalid workflow run id".to_string())
    }
}

fn read_json_file<T>(path: &Path) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_durable_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let contents = serde_json::to_string(value).map_err(|error| error.to_string())?;
    codex_utils_path::write_atomically(path, &contents).map_err(|error| error.to_string())?;
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())
}

fn status_index(status: WorkflowTaskStatus) -> usize {
    match status {
        WorkflowTaskStatus::Pending => 0,
        WorkflowTaskStatus::Running => 1,
        WorkflowTaskStatus::Completed => 2,
        WorkflowTaskStatus::Failed => 3,
        WorkflowTaskStatus::Paused => 4,
        WorkflowTaskStatus::Killed => 5,
    }
}

fn increment_status_count(counts: &mut [usize; 6], status: WorkflowTaskStatus) {
    let index = status_index(status);
    counts[index] = counts[index].saturating_add(1);
}

fn decrement_status_count(counts: &mut [usize; 6], status: WorkflowTaskStatus) {
    let index = status_index(status);
    counts[index] = counts[index].saturating_sub(1);
}

fn status_total(counts: &[usize; 6], statuses: &[WorkflowTaskStatus]) -> usize {
    if statuses.is_empty() {
        counts.iter().copied().sum()
    } else {
        statuses
            .iter()
            .map(|status| counts[status_index(*status)])
            .sum()
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}
