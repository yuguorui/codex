use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow::WorkflowJournal;
use codex_workflow::WorkflowJournalFuture;
use codex_workflow::WorkflowJournalReplayFuture;
use codex_workflow::WorkflowJournalResult;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

const MAX_HOT_JOURNAL_RESULTS: usize = 4_096;
const JOURNAL_SEGMENT_COUNT: usize = 64;
const RESULT_GROUP_COMMIT_RECORDS: usize = 64;
const MAX_PENDING_RESULTS_PER_OWNER: usize =
    JOURNAL_SEGMENT_COUNT * (RESULT_GROUP_COMMIT_RECORDS - 1);
const TAIL_SCAN_BYTES: usize = 8 * 1_024;
const TAIL_SCAN_BYTES_U64: u64 = 8 * 1_024;
const CHECKSUM_MISMATCH_ERROR: &str = "workflow journal record checksum does not match its payload";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JournalMarker {
    storage_directory: AbsolutePathBuf,
    source_storage_directories: Vec<AbsolutePathBuf>,
}

impl JournalMarker {
    fn replay_directories(&self) -> Vec<AbsolutePathBuf> {
        let mut directories = Vec::with_capacity(self.source_storage_directories.len() + 1);
        directories.push(self.storage_directory.clone());
        directories.extend(self.source_storage_directories.iter().cloned());
        directories
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
enum DurableKeyState {
    Started {
        key: String,
    },
    Result {
        key: String,
        result: WorkflowJournalResult,
    },
}

enum ReadState {
    Complete(Option<DurableKeyState>),
    CorruptTail,
}

#[derive(Clone, Copy)]
enum StartedDurability {
    Deferred,
    Immediate,
}

#[derive(Default)]
struct ReplayState {
    prefix_results: HashMap<String, WorkflowJournalResult>,
    current_results: HashMap<String, WorkflowJournalResult>,
    invalidated_keys: HashSet<String>,
}

struct JournalFileState {
    replay: Mutex<ReplayState>,
    // Shared by every handle for this generation in the owning process. Separate processes keep
    // separate groups, while marker and segment locks preserve record ordering and stale safety.
    pending_result_records: Mutex<[usize; JOURNAL_SEGMENT_COUNT]>,
    #[cfg(test)]
    replay_reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    result_syncs: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    started_syncs: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    append_hook: Mutex<Option<Arc<AppendTestHook>>>,
    #[cfg(test)]
    replay_hook: Mutex<Option<Arc<AppendTestHook>>>,
    #[cfg(test)]
    flush_hook: Mutex<Option<Arc<AppendTestHook>>>,
    #[cfg(test)]
    flush_failures: Mutex<HashMap<usize, String>>,
    #[cfg(test)]
    flush_attempts: Mutex<Vec<usize>>,
}

impl Default for JournalFileState {
    fn default() -> Self {
        Self {
            replay: Mutex::new(ReplayState::default()),
            pending_result_records: Mutex::new([0; JOURNAL_SEGMENT_COUNT]),
            #[cfg(test)]
            replay_reads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            result_syncs: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            started_syncs: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            append_hook: Mutex::new(None),
            #[cfg(test)]
            replay_hook: Mutex::new(None),
            #[cfg(test)]
            flush_hook: Mutex::new(None),
            #[cfg(test)]
            flush_failures: Mutex::new(HashMap::new()),
            #[cfg(test)]
            flush_attempts: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
struct AppendTestHook {
    entered: std::sync::Barrier,
    proceed: std::sync::Barrier,
}

pub(crate) struct FileWorkflowJournal {
    marker_path: PathBuf,
    storage_directory: AbsolutePathBuf,
    source_storage_directories: Vec<AbsolutePathBuf>,
    file_state: Arc<JournalFileState>,
}

impl FileWorkflowJournal {
    pub(crate) async fn open(path: PathBuf, replay_path: Option<&Path>) -> Result<Self, String> {
        let source_path = replay_path.map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || Self::open_blocking(path, source_path))
            .await
            .map_err(|error| error.to_string())?
    }

    fn open_blocking(path: PathBuf, replay_path: Option<PathBuf>) -> Result<Self, String> {
        let marker_path = canonical_journal_path(&path);
        let source_path = replay_path.as_deref().map(canonical_journal_path);
        let (marker, source_storage_directories) = match source_path {
            Some(source_path) if same_file(&marker_path, &source_path) => {
                initialize_marker(&marker_path, Vec::new())?;
                let current = advance_marker_generation(&marker_path)?;
                let sources = current.source_storage_directories.clone();
                (current, sources)
            }
            Some(source_path) => {
                let source = load_markers(&source_path)?
                    .into_iter()
                    .next_back()
                    .ok_or_else(|| {
                        "workflow journal marker has no complete generation record".to_string()
                    })?;
                let marker = initialize_marker(&marker_path, source.replay_directories())?;
                let sources = marker.source_storage_directories.clone();
                (marker, sources)
            }
            None => {
                let marker = initialize_marker(&marker_path, Vec::new())?;
                let sources = marker.source_storage_directories.clone();
                (marker, sources)
            }
        };
        let file_state = shared_file_state(&marker.storage_directory);
        Ok(Self {
            marker_path,
            storage_directory: marker.storage_directory,
            source_storage_directories,
            file_state,
        })
    }

    async fn write_state(&self, state: DurableKeyState) -> Result<(), String> {
        let marker_path = self.marker_path.clone();
        let storage_directory = self.storage_directory.clone();
        let source_storage_directories = self.source_storage_directories.clone();
        let file_state = Arc::clone(&self.file_state);
        tokio::task::spawn_blocking(move || {
            write_state_blocking(
                &marker_path,
                &storage_directory,
                &source_storage_directories,
                &state,
                &file_state,
            )?;
            apply_hot_state(&file_state, state);
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) async fn close(&self) -> Result<(), String> {
        let marker_path = self.marker_path.clone();
        let storage_directory = self.storage_directory.clone();
        let file_state = Arc::clone(&self.file_state);
        tokio::task::spawn_blocking(move || {
            flush_pending_results(&marker_path, &storage_directory, &file_state)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    fn replay_blocking(
        marker_path: &Path,
        storage_directory: &AbsolutePathBuf,
        source_storage_directories: &[AbsolutePathBuf],
        file_state: &JournalFileState,
        key: &str,
    ) -> Result<Option<WorkflowJournalResult>, String> {
        #[cfg(test)]
        if let Some(hook) = file_state
            .replay_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hook.entered.wait();
            hook.proceed.wait();
        }
        let marker = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(marker_path)
            .and_then(|marker| {
                marker.lock()?;
                Ok(marker)
            }) {
            Ok(marker) => marker,
            Err(error) => {
                tracing::warn!(path = %marker_path.display(), %error, "failed to lock workflow journal for replay");
                return Err(error.to_string());
            }
        };
        match read_state(storage_directory, key, file_state) {
            Ok(ReadState::Complete(Some(DurableKeyState::Result { result, .. }))) => {
                let mut replay = file_state
                    .replay
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                replay.invalidated_keys.remove(key);
                insert_hot_result(&mut replay.current_results, key.to_string(), result.clone());
                return Ok(Some(result));
            }
            Ok(ReadState::Complete(Some(DurableKeyState::Started { .. }))) => {
                Self::remember_invalidated(file_state, key);
                return Ok(None);
            }
            Ok(ReadState::CorruptTail) => {
                Self::remember_invalidated(file_state, key);
                return Ok(None);
            }
            Err(error) => return Err(error),
            Ok(ReadState::Complete(None)) => {}
        }

        {
            let replay = file_state
                .replay
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if replay.invalidated_keys.contains(key) {
                return Ok(None);
            }
            if let Some(result) = replay.prefix_results.get(key) {
                return Ok(Some(result.clone()));
            }
        }

        for directory in source_storage_directories {
            match read_state(directory, key, file_state)? {
                ReadState::Complete(Some(DurableKeyState::Result { result, .. })) => {
                    let mut replay = file_state
                        .replay
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    insert_hot_result(&mut replay.prefix_results, key.to_string(), result.clone());
                    return Ok(Some(result));
                }
                ReadState::Complete(Some(DurableKeyState::Started { .. })) => {
                    Self::remember_invalidated(file_state, key);
                    return Ok(None);
                }
                ReadState::CorruptTail => {
                    Self::remember_invalidated(file_state, key);
                    return Ok(None);
                }
                ReadState::Complete(None) => {}
            }
        }

        if !source_storage_directories.is_empty() {
            persist_replay_tombstone(storage_directory, key, file_state)?;
        }
        drop(marker);
        Self::remember_invalidated(file_state, key);
        Ok(None)
    }

    fn remember_invalidated(file_state: &JournalFileState, key: &str) {
        let mut replay = file_state
            .replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        replay.prefix_results.remove(key);
        replay.current_results.remove(key);
        if replay.invalidated_keys.len() >= MAX_HOT_JOURNAL_RESULTS
            && !replay.invalidated_keys.contains(key)
            && let Some(evicted) = replay.invalidated_keys.iter().next().cloned()
        {
            replay.invalidated_keys.remove(&evicted);
        }
        replay.invalidated_keys.insert(key.to_string());
    }
}

impl Drop for FileWorkflowJournal {
    fn drop(&mut self) {
        if !has_pending_results(&self.file_state) {
            return;
        }
        let marker_path = self.marker_path.clone();
        let storage_directory = self.storage_directory.clone();
        let file_state = Arc::clone(&self.file_state);
        let log_path = marker_path.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("workflow-journal-flush".to_string())
            .spawn(move || {
                if let Err(error) =
                    flush_pending_results(&marker_path, &storage_directory, &file_state)
                {
                    tracing::warn!(
                        path = %log_path.display(),
                        %error,
                        "failed to flush pending workflow journal results during drop"
                    );
                }
            })
        {
            tracing::warn!(
                path = %self.marker_path.display(),
                %error,
                "failed to start workflow journal drop flush"
            );
        }
    }
}

impl WorkflowJournal for FileWorkflowJournal {
    fn replay<'a>(&'a self, key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        let marker_path = self.marker_path.clone();
        let storage_directory = self.storage_directory.clone();
        let source_storage_directories = self.source_storage_directories.clone();
        let file_state = Arc::clone(&self.file_state);
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                Self::replay_blocking(
                    &marker_path,
                    &storage_directory,
                    &source_storage_directories,
                    &file_state,
                    &key,
                )
            })
            .await
            .map_err(|error| error.to_string())?
        })
    }

    fn append_started(&self, key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(self.write_state(DurableKeyState::Started { key }))
    }

    fn append_result(
        &self,
        key: String,
        result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(self.write_state(DurableKeyState::Result { key, result }))
    }

    fn close(&self) -> WorkflowJournalFuture<'_> {
        Box::pin(FileWorkflowJournal::close(self))
    }
}

fn initialize_marker(
    path: &Path,
    source_storage_directories: Vec<AbsolutePathBuf>,
) -> Result<JournalMarker, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "workflow journal path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.lock().map_err(|error| error.to_string())?;
    let marker = if file.metadata().map_err(|error| error.to_string())?.len() == 0 {
        let marker = create_generation(parent, source_storage_directories)?;
        append_marker_record(&mut file, &marker)?;
        file.sync_all().map_err(|error| error.to_string())?;
        sync_directory(parent)?;
        marker
    } else {
        read_markers_from_locked_file(&mut file)?
            .into_iter()
            .next_back()
            .ok_or_else(|| {
                "workflow journal marker has no complete generation record".to_string()
            })?
    };
    ensure_storage_available(&marker)?;
    for directory in &marker.source_storage_directories {
        ensure_storage_directory_available(directory)?;
    }
    Ok(marker)
}

fn load_markers(path: &Path) -> Result<Vec<JournalMarker>, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.lock().map_err(|error| error.to_string())?;
    let markers = read_markers_from_locked_file(&mut file)?;
    for marker in &markers {
        ensure_storage_available(marker)?;
        for directory in &marker.source_storage_directories {
            ensure_storage_directory_available(directory)?;
        }
    }
    Ok(markers)
}

fn read_markers_from_locked_file(file: &mut std::fs::File) -> Result<Vec<JournalMarker>, String> {
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let complete_len = if bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |newline| newline + 1)
    };
    let mut markers = Vec::new();
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        markers.push(
            serde_json::from_slice::<JournalMarker>(line).map_err(|error| {
                format!("workflow journal marker record {index} is invalid: {error}")
            })?,
        );
    }
    if markers.is_empty() {
        return Err("workflow journal marker has no complete generation record".to_string());
    }
    Ok(markers)
}

fn ensure_storage_available(marker: &JournalMarker) -> Result<(), String> {
    ensure_storage_directory_available(&marker.storage_directory)
}

fn ensure_storage_directory_available(directory: &AbsolutePathBuf) -> Result<(), String> {
    if !directory.is_dir() {
        return Err("workflow journal storage directory is unavailable".to_string());
    }
    Ok(())
}

fn advance_marker_generation(path: &Path) -> Result<JournalMarker, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "workflow journal path has no parent".to_string())?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.lock().map_err(|error| error.to_string())?;
    let previous = read_markers_from_locked_file(&mut file)?
        .into_iter()
        .next_back()
        .ok_or_else(|| "workflow journal marker has no complete generation record".to_string())?;
    ensure_storage_available(&previous)?;
    for directory in &previous.source_storage_directories {
        ensure_storage_directory_available(directory)?;
    }
    let current = create_generation(parent, previous.replay_directories())?;
    append_marker_record(&mut file, &current)?;
    file.sync_all().map_err(|error| error.to_string())?;
    sync_directory(parent)?;
    Ok(current)
}

fn create_generation(
    parent: &Path,
    source_storage_directories: Vec<AbsolutePathBuf>,
) -> Result<JournalMarker, String> {
    let storage_directory = AbsolutePathBuf::try_from(parent.join(format!(
        ".workflow-journal-{}",
        uuid::Uuid::new_v4().simple()
    )))
    .map_err(|error| error.to_string())?;
    std::fs::create_dir(&storage_directory).map_err(|error| error.to_string())?;
    for segment in 0..JOURNAL_SEGMENT_COUNT {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(segment_path(&storage_directory, segment))
            .map_err(|error| error.to_string())?;
    }
    // Segment directory entries precede the marker; later appends only need to sync file data.
    sync_directory(&storage_directory)?;
    Ok(JournalMarker {
        storage_directory,
        source_storage_directories,
    })
}

fn append_marker_record(file: &mut std::fs::File, marker: &JournalMarker) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(marker).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length > 0 {
        file.seek(std::io::SeekFrom::End(-1))
            .map_err(|error| error.to_string())?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)
            .map_err(|error| error.to_string())?;
        if last[0] != b'\n' {
            truncate_after_last_newline(file, length).map_err(|error| error.to_string())?;
        }
    }
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}

fn write_state_blocking(
    marker_path: &Path,
    storage_directory: &AbsolutePathBuf,
    source_storage_directories: &[AbsolutePathBuf],
    state: &DurableKeyState,
    _file_state: &JournalFileState,
) -> Result<(), String> {
    let marker = OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker_path)
        .map_err(|error| error.to_string())?;
    marker.lock().map_err(|error| error.to_string())?;
    #[cfg(test)]
    if let Some(hook) = _file_state
        .append_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        hook.entered.wait();
        hook.proceed.wait();
    }
    let started_durability = match state {
        DurableKeyState::Started { key } => {
            let current_has_result = has_replayable_result(storage_directory, key, _file_state)?;
            let source_has_result =
                has_replayable_result_in_sources(source_storage_directories, key, _file_state)?;
            if current_has_result || source_has_result {
                StartedDurability::Immediate
            } else {
                StartedDurability::Deferred
            }
        }
        DurableKeyState::Result { .. } => StartedDurability::Deferred,
    };
    write_state_locked(storage_directory, state, started_durability, _file_state)
}

fn write_state_locked(
    storage_directory: &AbsolutePathBuf,
    state: &DurableKeyState,
    started_durability: StartedDurability,
    file_state: &JournalFileState,
) -> Result<(), String> {
    let key = state_key(state);
    let payload = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    let checksum = format!("{:x}", Sha256::digest(&payload));
    let segment = key_segment(key);
    let path = segment_path(storage_directory, segment);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.lock().map_err(|error| error.to_string())?;
    truncate_torn_tail(&mut file, segment).map_err(|error| error.to_string())?;
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    file.write_all(checksum.as_bytes())
        .and_then(|()| file.write_all(b" "))
        .and_then(|()| file.write_all(&payload))
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| error.to_string())?;

    match state {
        DurableKeyState::Started { .. }
            if matches!(started_durability, StartedDurability::Immediate) =>
        {
            // Syncing the Started frame and the earlier Result in one operation makes stale
            // replay impossible before the replacement agent is allowed to run.
            file.sync_data().map_err(|error| error.to_string())?;
            file_state
                .pending_result_records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[segment] = 0;
            #[cfg(test)]
            file_state
                .started_syncs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        DurableKeyState::Result { .. } => {
            let should_sync = {
                let mut pending = file_state
                    .pending_result_records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending[segment] += 1;
                pending[segment] >= RESULT_GROUP_COMMIT_RECORDS
            };
            if should_sync {
                file.sync_data().map_err(|error| error.to_string())?;
                file_state
                    .pending_result_records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)[segment] = 0;
                #[cfg(test)]
                file_state
                    .result_syncs
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            debug_assert!(
                file_state
                    .pending_result_records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .sum::<usize>()
                    <= MAX_PENDING_RESULTS_PER_OWNER
            );
        }
        DurableKeyState::Started { .. } => {}
    }
    Ok(())
}

fn flush_pending_results(
    marker_path: &Path,
    storage_directory: &AbsolutePathBuf,
    file_state: &JournalFileState,
) -> Result<(), String> {
    let marker = OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker_path)
        .map_err(|error| error.to_string())?;
    marker.lock().map_err(|error| error.to_string())?;
    #[cfg(test)]
    if let Some(hook) = file_state
        .flush_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        hook.entered.wait();
        hook.proceed.wait();
    }
    let pending_segments = {
        let pending = file_state
            .pending_result_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .iter()
            .enumerate()
            .filter_map(|(segment, count)| (*count > 0).then_some(segment))
            .collect::<Vec<_>>()
    };
    let mut first_error = None;
    for segment in pending_segments {
        #[cfg(test)]
        file_state
            .flush_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(segment);
        #[cfg(test)]
        let injected_error = file_state
            .flush_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment)
            .cloned();
        #[cfg(not(test))]
        let injected_error: Option<String> = None;
        let result = if let Some(error) = injected_error {
            Err(error)
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(segment_path(storage_directory, segment))
                .and_then(|file| {
                    file.lock()?;
                    file.sync_data()
                })
                .map_err(|error| error.to_string())
        };
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(error);
            }
            continue;
        }
        file_state
            .pending_result_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[segment] = 0;
        #[cfg(test)]
        file_state
            .result_syncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    first_error.map_or(Ok(()), Err)
}

fn has_pending_results(file_state: &JournalFileState) -> bool {
    file_state
        .pending_result_records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|count| *count > 0)
}

fn has_replayable_result(
    storage_directory: &AbsolutePathBuf,
    key: &str,
    file_state: &JournalFileState,
) -> Result<bool, String> {
    Ok(matches!(
        read_state(storage_directory, key, file_state)?,
        ReadState::Complete(Some(DurableKeyState::Result { .. })) | ReadState::CorruptTail
    ))
}

fn has_replayable_result_in_sources(
    storage_directories: &[AbsolutePathBuf],
    key: &str,
    file_state: &JournalFileState,
) -> Result<bool, String> {
    for storage_directory in storage_directories {
        match read_state(storage_directory, key, file_state)? {
            ReadState::Complete(Some(DurableKeyState::Result { .. })) | ReadState::CorruptTail => {
                return Ok(true);
            }
            ReadState::Complete(Some(DurableKeyState::Started { .. })) => return Ok(false),
            ReadState::Complete(None) => {}
        }
    }
    Ok(false)
}

fn read_state(
    storage_directory: &AbsolutePathBuf,
    key: &str,
    _file_state: &JournalFileState,
) -> Result<ReadState, String> {
    #[cfg(test)]
    _file_state
        .replay_reads
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let expected_segment = key_segment(key);
    let path = segment_path(storage_directory, expected_segment);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.lock().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(&file);
    let mut record = Vec::new();
    let mut matched = None;
    loop {
        record.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut record)
            .map_err(|error| error.to_string())?;
        if bytes_read == 0 {
            break;
        }
        if record.last() != Some(&b'\n') {
            break;
        }
        record.pop();
        let is_final_record = reader
            .fill_buf()
            .map_err(|error| error.to_string())?
            .is_empty();
        let state = match parse_record(&record, expected_segment) {
            Ok(state) => state,
            Err(error) if is_final_record && error == CHECKSUM_MISMATCH_ERROR => {
                return Ok(ReadState::CorruptTail);
            }
            Err(error) => return Err(error),
        };
        if state_key(&state) == key {
            matched = Some(state);
        }
    }
    Ok(ReadState::Complete(matched))
}

fn persist_replay_tombstone(
    storage_directory: &AbsolutePathBuf,
    key: &str,
    file_state: &JournalFileState,
) -> Result<(), String> {
    let tombstone = DurableKeyState::Started {
        key: key.to_string(),
    };
    write_state_locked(
        storage_directory,
        &tombstone,
        StartedDurability::Immediate,
        file_state,
    )
    .map_err(|error| format!("failed to persist workflow replay tombstone: {error}"))
}

fn state_key(state: &DurableKeyState) -> &str {
    match state {
        DurableKeyState::Started { key } | DurableKeyState::Result { key, .. } => key,
    }
}

fn key_segment(key: &str) -> usize {
    usize::from(Sha256::digest(key.as_bytes())[0]) % JOURNAL_SEGMENT_COUNT
}

fn segment_path(storage_directory: &AbsolutePathBuf, segment: usize) -> AbsolutePathBuf {
    storage_directory.join(format!("segment-{segment:02x}.jsonl"))
}

fn parse_record(record: &[u8], expected_segment: usize) -> Result<DurableKeyState, String> {
    let Some(separator) = record.iter().position(|byte| *byte == b' ') else {
        return Err("workflow journal record is missing its checksum".to_string());
    };
    let (checksum, payload_with_separator) = record.split_at(separator);
    let payload = &payload_with_separator[1..];
    let actual = format!("{:x}", Sha256::digest(payload));
    if checksum != actual.as_bytes() {
        return Err(CHECKSUM_MISMATCH_ERROR.to_string());
    }
    let state: DurableKeyState =
        serde_json::from_slice(payload).map_err(|error| error.to_string())?;
    if key_segment(state_key(&state)) != expected_segment {
        return Err("workflow journal record is stored in the wrong segment".to_string());
    }
    Ok(state)
}

fn truncate_torn_tail(file: &mut std::fs::File, expected_segment: usize) -> std::io::Result<()> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        return truncate_after_last_newline(file, length);
    }

    let record_end = length - 1;
    let record_start = find_line_start(file, record_end)?;
    let record_length =
        usize::try_from(record_end - record_start).map_err(std::io::Error::other)?;
    let mut record = vec![0_u8; record_length];
    file.seek(std::io::SeekFrom::Start(record_start))?;
    file.read_exact(&mut record)?;
    match parse_record(&record, expected_segment) {
        Ok(_) => Ok(()),
        Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    }
}

fn truncate_after_last_newline(file: &mut std::fs::File, length: u64) -> std::io::Result<()> {
    let line_start = find_line_start(file, length)?;
    file.set_len(line_start)
}

fn find_line_start(file: &mut std::fs::File, mut end: u64) -> std::io::Result<u64> {
    let mut buffer = [0_u8; TAIL_SCAN_BYTES];
    while end > 0 {
        let start = end.saturating_sub(TAIL_SCAN_BYTES_U64);
        let bytes = usize::try_from(end - start).map_err(std::io::Error::other)?;
        file.seek(std::io::SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..bytes])?;
        if let Some(newline) = buffer[..bytes].iter().rposition(|byte| *byte == b'\n') {
            let newline = u64::try_from(newline).map_err(std::io::Error::other)?;
            return Ok(start + newline + 1);
        }
        end = start;
    }
    Ok(0)
}

fn apply_hot_state(file_state: &JournalFileState, state: DurableKeyState) {
    let mut replay = file_state
        .replay
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match state {
        DurableKeyState::Started { key } => {
            replay.prefix_results.remove(&key);
            replay.current_results.remove(&key);
            if replay.invalidated_keys.len() >= MAX_HOT_JOURNAL_RESULTS
                && !replay.invalidated_keys.contains(&key)
                && let Some(evicted) = replay.invalidated_keys.iter().next().cloned()
            {
                replay.invalidated_keys.remove(&evicted);
            }
            replay.invalidated_keys.insert(key);
        }
        DurableKeyState::Result { key, result } => {
            replay.invalidated_keys.remove(&key);
            insert_hot_result(&mut replay.current_results, key, result);
        }
    }
}

fn insert_hot_result(
    results: &mut HashMap<String, WorkflowJournalResult>,
    key: String,
    result: WorkflowJournalResult,
) {
    if results.len() >= MAX_HOT_JOURNAL_RESULTS
        && !results.contains_key(&key)
        && let Some(evicted) = results.keys().next().cloned()
    {
        results.remove(&evicted);
    }
    results.insert(key, result);
}

fn shared_file_state(path: &Path) -> Arc<JournalFileState> {
    static STATES: OnceLock<Mutex<HashMap<JournalFileIdentity, Weak<JournalFileState>>>> =
        OnceLock::new();
    let key = journal_file_identity(path);
    let mut states = STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    states.retain(|_, state| state.strong_count() > 0);
    if let Some(state) = states.get(&key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(JournalFileState::default());
    states.insert(key, Arc::downgrade(&state));
    state
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum JournalFileIdentity {
    #[cfg(unix)]
    Unix {
        device: u64,
        inode: u64,
    },
    #[cfg(windows)]
    Windows {
        volume: u32,
        file_index: u64,
    },
    Path(PathBuf),
}

fn journal_file_identity(path: &Path) -> JournalFileIdentity {
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        return JournalFileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
    }
    #[cfg(windows)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::windows::fs::MetadataExt;
        if let (Some(volume), Some(file_index)) =
            (metadata.volume_serial_number(), metadata.file_index())
        {
            return JournalFileIdentity::Windows { volume, file_index };
        }
    }
    JournalFileIdentity::Path(path.to_path_buf())
}

fn same_file(left: &Path, right: &Path) -> bool {
    journal_file_identity(left) == journal_file_identity(right)
}

fn canonical_journal_path(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
    let Ok(mut canonical) = existing.canonicalize() else {
        return path.to_path_buf();
    };
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    canonical
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

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
