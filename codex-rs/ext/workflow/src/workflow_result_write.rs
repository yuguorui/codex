use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemSandboxContext;
use codex_file_system::WriteFileOptions;
use codex_tools::ToolExecutionEnvironment;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;

const RESULT_WRITE_PATH_MAX_BYTES: usize = 1_024;

pub(crate) struct WorkflowResultWriteTarget {
    file_system: Arc<dyn ExecutorFileSystem>,
    sandbox: Option<FileSystemSandboxContext>,
    path: PathUri,
}

pub(crate) struct WorkflowResultWrite {
    pub(crate) path: PathUri,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

pub(crate) fn resolve_result_write_target(
    environments: &[ToolExecutionEnvironment],
    requested_path: &str,
) -> Result<WorkflowResultWriteTarget, String> {
    if requested_path.is_empty() {
        return Err("provide a non-empty writePath".to_string());
    }
    if requested_path.len() > RESULT_WRITE_PATH_MAX_BYTES {
        return Err("choose a shorter writePath".to_string());
    }
    let environment = environments.first().ok_or_else(|| {
        "Workflow result writing requires a primary selected execution environment".to_string()
    })?;
    let path = environment
        .cwd
        .join(requested_path)
        .map_err(|error| format!("invalid Workflow result writePath: {error}"))?;
    let workspace_roots = environment
        .selection
        .as_ref()
        .map(|selection| selection.workspace_roots.clone())
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![environment.cwd.clone()]);
    if !workspace_roots
        .iter()
        .any(|workspace_root| path.starts_with(workspace_root))
    {
        return Err("choose a writePath inside a selected workspace root".to_string());
    }
    Ok(WorkflowResultWriteTarget {
        file_system: Arc::clone(&environment.file_system),
        sandbox: Some(environment.file_system_sandbox_context.clone()),
        path,
    })
}

/// Writes a verified Workflow result through the selected executor filesystem.
///
/// The preceding lexical workspace check keeps ordinary paths scoped, while the executor sandbox
/// remains authoritative for symlink-aware policy enforcement.
pub(crate) async fn write_workflow_result(
    target: &WorkflowResultWriteTarget,
    serialized: &str,
    sha256: &str,
) -> Result<WorkflowResultWrite, String> {
    let bytes = u64::try_from(serialized.len())
        .map_err(|error| format!("Workflow result length is not representable: {error}"))?;
    if let Some(parent) = target.path.parent() {
        target
            .file_system
            .create_directory(
                &parent,
                CreateDirectoryOptions {
                    recursive: true,
                    follow_symlinks: true,
                },
                target.sandbox.as_ref(),
            )
            .await
            .map_err(|error| {
                format!(
                    "failed to create the Workflow result directory at {}: {error}",
                    target.path
                )
            })?;
    }
    target
        .file_system
        .write_file(
            &target.path,
            serialized.as_bytes().to_vec(),
            WriteFileOptions {
                follow_symlinks: true,
            },
            target.sandbox.as_ref(),
        )
        .await
        .map_err(|error| {
            format!(
                "failed to write the Workflow result to {}: {error}",
                target.path
            )
        })?;
    Ok(WorkflowResultWrite {
        path: target.path.clone(),
        bytes,
        sha256: sha256.to_string(),
    })
}

#[cfg(test)]
#[path = "workflow_result_write_tests.rs"]
mod tests;
