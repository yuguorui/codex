use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_workflow::WorkflowInputArtifactFuture;
use codex_workflow::WorkflowInputArtifactKind;
use codex_workflow::WorkflowInputArtifactRef;
use codex_workflow::WorkflowInputArtifactStore;
use codex_workflow::WorkflowInputDescriptor;
use codex_workflow::canonical_workflow_input_bytes;
use codex_workflow::validate_artifact_sha256;
use codex_workflow::workflow_input_artifact_ref;
use codex_workflow::workflow_input_descriptor_ref;
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;

pub(crate) struct FileWorkflowInputArtifactStore {
    directory: PathBuf,
    replay_directory: Option<PathBuf>,
    write_lock: Mutex<()>,
}

impl FileWorkflowInputArtifactStore {
    pub(crate) fn new(directory: PathBuf, replay_directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            replay_directory,
            write_lock: Mutex::new(()),
        }
    }

    fn path(&self, reference: &WorkflowInputArtifactRef) -> Result<PathBuf, String> {
        validate_artifact_sha256(&reference.sha256)?;
        Ok(self.directory.join(format!("{}.json", reference.sha256)))
    }

    async fn read_from(
        directory: &Path,
        reference: &WorkflowInputArtifactRef,
    ) -> Result<Option<Arc<JsonValue>>, String> {
        let path = directory.join(format!("{}.json", reference.sha256));
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "failed to read workflow input artifact {}: {error}",
                    path.display()
                ));
            }
        };
        let display_path = path.display().to_string();
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            let value: JsonValue = serde_json::from_slice(&bytes).map_err(|error| {
                format!("failed to parse workflow input artifact {display_path}: {error}")
            })?;
            let actual = workflow_input_artifact_ref(&value)?;
            if actual != reference {
                return Err(format!(
                    "workflow input artifact {display_path} does not match its content hash"
                ));
            }
            Ok(Some(Arc::new(value)))
        })
        .await
        .map_err(|error| format!("failed to materialize workflow input artifact: {error}"))?
    }

    async fn read_descriptor_from(
        directory: &Path,
        reference: &WorkflowInputArtifactRef,
    ) -> Result<Option<Arc<WorkflowInputDescriptor>>, String> {
        let path = directory.join(format!("{}.json", reference.sha256));
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "failed to read workflow input descriptor {}: {error}",
                    path.display()
                ));
            }
        };
        let display_path = path.display().to_string();
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            let descriptor: WorkflowInputDescriptor =
                serde_json::from_slice(&bytes).map_err(|error| {
                    format!("failed to parse workflow input descriptor {display_path}: {error}")
                })?;
            let actual = workflow_input_descriptor_ref(&descriptor)?;
            if actual != reference {
                return Err(format!(
                    "workflow input descriptor {display_path} does not match its content hash"
                ));
            }
            Ok(Some(Arc::new(descriptor)))
        })
        .await
        .map_err(|error| format!("failed to materialize workflow input descriptor: {error}"))?
    }

    async fn write(
        &self,
        reference: &WorkflowInputArtifactRef,
        contents: String,
    ) -> Result<(), String> {
        let path = self.path(reference)?;
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            let directory_existed = directory.exists();
            std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            if !directory_existed && let Some(parent) = directory.parent() {
                sync_parent_directory(parent)?;
            }
            codex_utils_path::write_atomically(&path, &contents)
                .map_err(|error| error.to_string())?;
            std::fs::File::open(&path)
                .and_then(|file| file.sync_data())
                .map_err(|error| error.to_string())?;
            sync_parent_directory(&directory)
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

impl WorkflowInputArtifactStore for FileWorkflowInputArtifactStore {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move {
            let reference = workflow_input_artifact_ref(&value)?;
            let _write = self.write_lock.lock().await;
            if Self::read_from(&self.directory, &reference)
                .await?
                .is_some()
            {
                return Ok(reference);
            }
            let bytes = canonical_workflow_input_bytes(&value)?;
            let contents = String::from_utf8(bytes).map_err(|error| error.to_string())?;
            self.write(&reference, contents).await?;
            Ok(reference)
        })
    }

    fn put_descriptor(
        &self,
        descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move {
            let reference = workflow_input_descriptor_ref(&descriptor)?;
            let _write = self.write_lock.lock().await;
            if Self::read_descriptor_from(&self.directory, &reference)
                .await?
                .is_some()
            {
                return Ok(reference);
            }
            let value = serde_json::to_value(&descriptor).map_err(|error| error.to_string())?;
            let bytes = canonical_workflow_input_bytes(&value)?;
            let contents = String::from_utf8(bytes).map_err(|error| error.to_string())?;
            self.write(&reference, contents).await?;
            Ok(reference)
        })
    }

    fn get<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>> {
        let reference = reference.clone();
        Box::pin(async move {
            if reference.kind != WorkflowInputArtifactKind::Value {
                return Err(
                    "load workflow input descriptors through the descriptor path".to_string(),
                );
            }
            validate_artifact_sha256(&reference.sha256)?;
            if let Some(value) = Self::read_from(&self.directory, &reference).await? {
                return Ok(value);
            }
            if let Some(replay_directory) = &self.replay_directory
                && let Some(value) = Self::read_from(replay_directory, &reference).await?
            {
                return Ok(value);
            }
            Err(format!(
                "restore workflow input artifact {} in the configured store and retry",
                reference.sha256
            ))
        })
    }

    fn get_descriptor<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>> {
        let reference = reference.clone();
        Box::pin(async move {
            if reference.kind != WorkflowInputArtifactKind::Descriptor {
                return Err("load workflow input values through the value path".to_string());
            }
            validate_artifact_sha256(&reference.sha256)?;
            if let Some(descriptor) =
                Self::read_descriptor_from(&self.directory, &reference).await?
            {
                return Ok(descriptor);
            }
            if let Some(replay_directory) = &self.replay_directory
                && let Some(descriptor) =
                    Self::read_descriptor_from(replay_directory, &reference).await?
            {
                return Ok(descriptor);
            }
            Err(format!(
                "restore workflow input descriptor {} in the configured store and retry",
                reference.sha256
            ))
        })
    }
}

#[cfg(unix)]
fn sync_parent_directory(directory: &Path) -> Result<(), String> {
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())
}

#[cfg(any(windows, not(any(unix, windows))))]
fn sync_parent_directory(_directory: &Path) -> Result<(), String> {
    Ok(())
}
