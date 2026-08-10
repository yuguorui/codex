use codex_workflow::WorkflowAgentResult;
use codex_workflow::WorkflowJournal;
use codex_workflow::WorkflowJournalFuture;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Semaphore;

const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOURNAL_ENTRIES: usize = 4_096;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum JournalEntry {
    Started {
        key: String,
    },
    Result {
        key: String,
        #[serde(flatten)]
        result: WorkflowAgentResult,
    },
}

struct ReplayState {
    enabled: bool,
    results: HashMap<String, WorkflowAgentResult>,
}

struct LoadedJournal {
    results: HashMap<String, WorkflowAgentResult>,
    entry_count: usize,
}

pub(crate) struct FileWorkflowJournal {
    path: PathBuf,
    replay: Mutex<ReplayState>,
    write_lock: Semaphore,
    entry_count: AtomicUsize,
}

impl FileWorkflowJournal {
    pub(crate) async fn open(path: PathBuf, replay_path: Option<&Path>) -> Result<Self, String> {
        let destination = load_entries(&path).await?;
        let destination_count = destination.entry_count;
        let results = match replay_path {
            Some(replay_path) if replay_path == path.as_path() => destination.results,
            Some(replay_path) => load_entries(replay_path).await?.results,
            None => HashMap::new(),
        };
        Ok(Self {
            path,
            replay: Mutex::new(ReplayState {
                enabled: replay_path.is_some(),
                results,
            }),
            write_lock: Semaphore::new(1),
            entry_count: AtomicUsize::new(destination_count),
        })
    }

    async fn append(&self, entry: JournalEntry) -> Result<(), String> {
        let _permit = self
            .write_lock
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        if self.entry_count.load(Ordering::Acquire) >= MAX_JOURNAL_ENTRIES {
            return Err(format!(
                "workflow journal exceeds the {MAX_JOURNAL_ENTRIES}-entry limit"
            ));
        }
        let mut bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let parent = path
                .parent()
                .ok_or_else(|| "workflow journal path has no parent".to_string())?;
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| error.to_string())?;
            let append_bytes = u64::try_from(bytes.len()).map_err(|error| error.to_string())?;
            let current_bytes = file.metadata().map_err(|error| error.to_string())?.len();
            if current_bytes.saturating_add(append_bytes) > MAX_JOURNAL_BYTES {
                return Err(format!(
                    "workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit"
                ));
            }
            file.write_all(&bytes).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())??;
        self.entry_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

impl WorkflowJournal for FileWorkflowJournal {
    fn replay(&self, key: &str) -> Option<WorkflowAgentResult> {
        let mut replay = self
            .replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !replay.enabled {
            return None;
        }
        let result = replay.results.get(key).cloned();
        if result.is_none() {
            replay.enabled = false;
        }
        result
    }

    fn append_started(&self, key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(self.append(JournalEntry::Started { key }))
    }

    fn append_result(&self, key: String, result: WorkflowAgentResult) -> WorkflowJournalFuture<'_> {
        Box::pin(self.append(JournalEntry::Result { key, result }))
    }
}

async fn load_entries(path: &Path) -> Result<LoadedJournal, String> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedJournal {
                results: HashMap::new(),
                entry_count: 0,
            });
        }
        Err(error) => return Err(error.to_string()),
    };
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(format!(
            "workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit"
        ));
    }
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedJournal {
                results: HashMap::new(),
                entry_count: 0,
            });
        }
        Err(error) => return Err(error.to_string()),
    };
    let mut results = HashMap::new();
    let mut entry_count = 0_usize;
    for (line_number, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        entry_count += 1;
        if entry_count > MAX_JOURNAL_ENTRIES {
            return Err(format!(
                "workflow journal exceeds the {MAX_JOURNAL_ENTRIES}-entry limit"
            ));
        }
        match serde_json::from_str::<JournalEntry>(line) {
            Ok(JournalEntry::Result { key, result }) => {
                results.insert(key, result);
            }
            Ok(JournalEntry::Started { .. }) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    line = line_number + 1,
                    %error,
                    "ignoring malformed workflow journal line"
                );
            }
        }
    }
    Ok(LoadedJournal {
        results,
        entry_count,
    })
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
