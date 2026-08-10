use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde::de::IgnoredAny;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowResultArtifact {
    pub sha256: String,
    pub bytes: u64,
    pub storage_id: String,
}

impl WorkflowResultArtifact {
    pub(crate) fn file_name(&self) -> String {
        format!("{}.{}.json", self.sha256, self.storage_id)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedWorkflowResult {
    artifact: WorkflowResultArtifact,
    serialized: Arc<str>,
}

impl VerifiedWorkflowResult {
    pub(crate) fn artifact(&self) -> &WorkflowResultArtifact {
        &self.artifact
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowResultChunk {
    pub(crate) text: String,
    pub(crate) offset: u64,
    pub(crate) next_offset: u64,
    pub(crate) total_bytes: u64,
}

impl WorkflowResultChunk {
    pub(crate) fn complete(&self) -> bool {
        self.next_offset == self.total_bytes
    }
}

pub(crate) async fn persist_result_artifact(
    snapshot_path: &AbsolutePathBuf,
    serialized: Arc<str>,
) -> Result<WorkflowResultArtifact, String> {
    let artifact = WorkflowResultArtifact {
        sha256: format!("{:x}", Sha256::digest(serialized.as_bytes())),
        bytes: u64::try_from(serialized.len())
            .map_err(|error| format!("workflow result length is not representable: {error}"))?,
        storage_id: uuid::Uuid::new_v4().simple().to_string(),
    };
    let path = result_artifact_path(snapshot_path, &artifact)?;
    let blocking_artifact = artifact.clone();
    tokio::task::spawn_blocking(move || {
        write_artifact_atomically(&path, serialized.as_bytes(), &blocking_artifact)
    })
    .await
    .map_err(|error| format!("workflow result artifact writer failed: {error}"))??;
    Ok(artifact)
}

#[cfg(test)]
pub(crate) async fn validate_result_artifact(
    snapshot_path: &AbsolutePathBuf,
    artifact: &WorkflowResultArtifact,
) -> Result<(), String> {
    let path = result_artifact_path(snapshot_path, artifact)?;
    let artifact = artifact.clone();
    tokio::task::spawn_blocking(move || validate_artifact_file(&path, &artifact))
        .await
        .map_err(|error| format!("workflow result artifact validator failed: {error}"))?
}

pub(crate) async fn load_verified_result_artifact(
    snapshot_path: &AbsolutePathBuf,
    artifact: &WorkflowResultArtifact,
) -> Result<VerifiedWorkflowResult, String> {
    let path = result_artifact_path(snapshot_path, artifact)?;
    let artifact = artifact.clone();
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("failed to read workflow result artifact: {error}"))?;
        validate_artifact_contents(&bytes, &artifact)?;
        let serialized = String::from_utf8(bytes)
            .map_err(|error| format!("workflow result artifact is not valid UTF-8: {error}"))?;
        Ok(VerifiedWorkflowResult {
            artifact,
            serialized: Arc::from(serialized),
        })
    })
    .await
    .map_err(|error| format!("workflow result artifact loader failed: {error}"))?
}

#[cfg(test)]
pub(crate) async fn read_result_artifact_chunk(
    snapshot_path: &AbsolutePathBuf,
    artifact: &WorkflowResultArtifact,
    offset: u64,
    max_bytes: usize,
) -> Result<WorkflowResultChunk, String> {
    let verified = load_verified_result_artifact(snapshot_path, artifact).await?;
    read_verified_result_chunk(&verified, offset, max_bytes)
}

pub(crate) fn read_verified_result_chunk(
    verified: &VerifiedWorkflowResult,
    offset: u64,
    max_bytes: usize,
) -> Result<WorkflowResultChunk, String> {
    if max_bytes == 0 {
        return Err("choose a positive maxBytes value".to_string());
    }
    let artifact = &verified.artifact;
    if offset > artifact.bytes {
        return Err(
            "start with offset 0 or continue from a nextOffset returned by ReadWorkflowResult"
                .to_string(),
        );
    }
    let start = usize::try_from(offset)
        .map_err(|error| format!("workflow result page offset is not representable: {error}"))?;
    let end = start
        .saturating_add(max_bytes.saturating_add(3))
        .min(verified.serialized.len());
    let bytes = &verified.serialized.as_bytes()[start..end];

    let valid_len = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => {
            return Err("continue with a nextOffset returned by ReadWorkflowResult".to_string());
        }
    };
    let valid = std::str::from_utf8(&bytes[..valid_len]).unwrap();
    let mut chunk_len = max_bytes.min(valid_len);
    while chunk_len > 0 && !valid.is_char_boundary(chunk_len) {
        chunk_len -= 1;
    }
    if chunk_len == 0 && offset < artifact.bytes {
        chunk_len = valid.chars().next().map(char::len_utf8).ok_or_else(|| {
            "workflow result page did not contain a complete character".to_string()
        })?;
    }
    let text = String::from_utf8(bytes[..chunk_len].to_vec())
        .map_err(|error| format!("workflow result artifact is not valid UTF-8: {error}"))?;
    let next_offset =
        offset.saturating_add(u64::try_from(chunk_len).map_err(|error| {
            format!("workflow result page offset is not representable: {error}")
        })?);
    Ok(WorkflowResultChunk {
        text,
        offset,
        next_offset,
        total_bytes: artifact.bytes,
    })
}

pub(crate) async fn cleanup_result_artifacts(
    snapshots_directory: &AbsolutePathBuf,
    referenced: HashSet<String>,
) -> Result<(), String> {
    const MINIMUM_STALE_AGE: Duration = Duration::from_secs(60 * 60);

    let results_directory = snapshots_directory.join("results").to_path_buf();
    tokio::task::spawn_blocking(move || {
        let entries = match std::fs::read_dir(&results_directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let stale = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= MINIMUM_STALE_AGE);
            if !stale {
                continue;
            }
            let remove = if name.starts_with('.') && name.ends_with(".tmp") {
                true
            } else {
                path.extension().and_then(|extension| extension.to_str()) == Some("json")
                    && !referenced.contains(name)
            };
            if remove
                && let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %error, "failed to clean stale workflow result artifact");
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("workflow result artifact cleanup failed: {error}"))?
}

fn result_artifact_path(
    snapshot_path: &AbsolutePathBuf,
    artifact: &WorkflowResultArtifact,
) -> Result<AbsolutePathBuf, String> {
    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("provide a lowercase SHA-256 workflow result artifact descriptor".to_string());
    }
    if artifact.storage_id.len() != 32
        || !artifact
            .storage_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("provide a lowercase workflow result artifact storage id".to_string());
    }
    let parent = snapshot_path
        .parent()
        .ok_or_else(|| "workflow snapshot path has no parent".to_string())?;
    Ok(parent.join("results").join(artifact.file_name()))
}

fn write_artifact_atomically(
    path: &Path,
    contents: &[u8],
    artifact: &WorkflowResultArtifact,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "workflow result artifact path has no parent".to_string())?;
    let parent_existed = parent.exists();
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create workflow result artifact directory: {error}"))?;
    if !parent_existed && let Some(snapshot_directory) = parent.parent() {
        sync_directory(snapshot_directory).map_err(|error| {
            format!("failed to sync workflow result artifact directory: {error}")
        })?;
    }
    let temporary_path = parent.join(format!(
        ".{}.{}.tmp",
        artifact.sha256,
        uuid::Uuid::new_v4().simple()
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        match std::fs::rename(&temporary_path, path) {
            Ok(()) => {}
            Err(error) => return Err(error),
        }
        sync_directory(parent)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(format!(
            "failed to persist workflow result artifact: {error}"
        ));
    }
    validate_artifact_file(path, artifact)
}

fn validate_artifact_file(path: &Path, artifact: &WorkflowResultArtifact) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to open workflow result artifact: {error}"))?;
    validate_artifact_contents(&bytes, artifact)
}

fn validate_artifact_contents(
    contents: &[u8],
    artifact: &WorkflowResultArtifact,
) -> Result<(), String> {
    let mut reader = HashingReader::new(contents);
    {
        let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
        IgnoredAny::deserialize(&mut deserializer)
            .map_err(|error| format!("workflow result artifact is not valid JSON: {error}"))?;
        deserializer
            .end()
            .map_err(|error| format!("workflow result artifact is not valid JSON: {error}"))?;
    }
    let (bytes, sha256) = reader.finish();
    if bytes != artifact.bytes {
        return Err("workflow result artifact length does not match its descriptor".to_string());
    }
    if sha256 != artifact.sha256 {
        return Err("workflow result artifact digest does not match its descriptor".to_string());
    }
    Ok(())
}

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes, format!("{:x}", self.hasher.finalize()))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.bytes = self.bytes.saturating_add(read as u64);
        Ok(read)
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "result_artifact_tests.rs"]
mod tests;
