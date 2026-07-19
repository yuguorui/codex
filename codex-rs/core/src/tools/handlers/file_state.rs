use codex_extension_api::ExtensionData;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

const MAX_FILE_STATE_ENTRIES: usize = 100;
const MAX_FILE_STATE_BYTES: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct FileStateKey {
    environment_id: String,
    path: PathUri,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    content: Arc<str>,
    modified_at_ms: i64,
    source: FileStateSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileStateSource {
    Read { offset: usize, limit: Option<usize> },
    Edit,
}

#[derive(Debug, Default)]
struct FileStateCacheInner {
    entries: HashMap<FileStateKey, FileState>,
    lru: VecDeque<FileStateKey>,
    content_bytes: usize,
    history_version: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct FileStateCache {
    inner: Mutex<FileStateCacheInner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditStateError {
    NotRead,
    Modified,
}

pub(super) struct ReadFileState {
    pub content: String,
    pub modified_at_ms: i64,
    pub offset: usize,
    pub limit: Option<usize>,
}

pub(super) struct EditedFileState {
    pub content: String,
    pub modified_at_ms: i64,
}

pub(super) fn session_file_state_cache(
    session_store: &ExtensionData,
    history_version: u64,
) -> Arc<FileStateCache> {
    let cache = session_store.get_or_init(FileStateCache::default);
    cache.prepare_history_version(history_version);
    cache
}

impl FileStateCache {
    pub(super) fn record_read(&self, environment_id: &str, path: &PathUri, state: ReadFileState) {
        self.insert(
            FileStateKey {
                environment_id: environment_id.to_string(),
                path: path.clone(),
            },
            FileState {
                content: Arc::from(state.content),
                modified_at_ms: state.modified_at_ms,
                source: FileStateSource::Read {
                    offset: state.offset,
                    limit: state.limit,
                },
            },
        );
    }

    pub(super) fn record_edit(&self, environment_id: &str, path: &PathUri, state: EditedFileState) {
        self.insert(
            FileStateKey {
                environment_id: environment_id.to_string(),
                path: path.clone(),
            },
            FileState {
                content: Arc::from(state.content),
                modified_at_ms: state.modified_at_ms,
                source: FileStateSource::Edit,
            },
        );
    }

    pub(super) fn read_is_unchanged(
        &self,
        environment_id: &str,
        path: &PathUri,
        modified_at_ms: i64,
        offset: usize,
        limit: Option<usize>,
        current_content: &str,
    ) -> bool {
        let key = FileStateKey {
            environment_id: environment_id.to_string(),
            path: path.clone(),
        };
        self.get(&key).is_some_and(|state| {
            state.modified_at_ms == modified_at_ms
                && state.source == FileStateSource::Read { offset, limit }
                && state.content.as_ref() == current_content
        })
    }

    pub(super) fn validate_edit(
        &self,
        environment_id: &str,
        path: &PathUri,
        current_modified_at_ms: i64,
        current_content: &str,
    ) -> Result<(), EditStateError> {
        let key = FileStateKey {
            environment_id: environment_id.to_string(),
            path: path.clone(),
        };
        let Some(state) = self.get(&key) else {
            return Err(EditStateError::NotRead);
        };
        let is_unchanged = match state.source {
            FileStateSource::Read { offset, limit } => {
                current_modified_at_ms == state.modified_at_ms
                    && select_read_content(current_content, offset, limit) == state.content.as_ref()
            }
            FileStateSource::Edit => current_content == state.content.as_ref(),
        };
        if !is_unchanged {
            return Err(EditStateError::Modified);
        }
        Ok(())
    }

    fn prepare_history_version(&self, history_version: u64) {
        let mut inner = self.inner();
        if inner.history_version == Some(history_version) {
            return;
        }
        inner.entries.clear();
        inner.lru.clear();
        inner.content_bytes = 0;
        inner.history_version = Some(history_version);
    }

    pub(super) fn remove(&self, environment_id: &str, path: &PathUri) {
        let key = FileStateKey {
            environment_id: environment_id.to_string(),
            path: path.clone(),
        };
        let mut inner = self.inner();
        if let Some(state) = inner.entries.remove(&key) {
            inner.content_bytes = inner
                .content_bytes
                .saturating_sub(state.content.len().max(1));
        }
        inner.lru.retain(|candidate| candidate != &key);
    }

    fn insert(&self, key: FileStateKey, state: FileState) {
        let state_size = state.content.len().max(1);
        let mut inner = self.inner();
        if let Some(previous) = inner.entries.remove(&key) {
            inner.content_bytes = inner
                .content_bytes
                .saturating_sub(previous.content.len().max(1));
        }
        inner.lru.retain(|candidate| candidate != &key);
        if state_size > MAX_FILE_STATE_BYTES {
            return;
        }
        while inner.entries.len() >= MAX_FILE_STATE_ENTRIES
            || inner.content_bytes.saturating_add(state_size) > MAX_FILE_STATE_BYTES
        {
            let Some(oldest) = inner.lru.pop_front() else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&oldest) {
                inner.content_bytes = inner
                    .content_bytes
                    .saturating_sub(evicted.content.len().max(1));
            }
        }
        inner.content_bytes = inner.content_bytes.saturating_add(state_size);
        inner.lru.push_back(key.clone());
        inner.entries.insert(key, state);
    }

    fn get(&self, key: &FileStateKey) -> Option<FileState> {
        let mut inner = self.inner();
        let state = inner.entries.get(key)?.clone();
        inner.lru.retain(|candidate| candidate != key);
        inner.lru.push_back(key.clone());
        Some(state)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, FileStateCacheInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) fn select_read_content(content: &str, offset: usize, limit: Option<usize>) -> String {
    content
        .split('\n')
        .skip(offset.saturating_sub(1))
        .take(limit.unwrap_or(usize::MAX))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn normalize_file_content(content: &str) -> String {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n")
}

#[cfg(test)]
#[path = "file_state_tests.rs"]
mod tests;
