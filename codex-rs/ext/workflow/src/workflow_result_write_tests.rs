use super::*;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sha2::Digest;
use std::sync::Arc;

#[test]
fn relative_write_path_resolves_against_the_selected_environment() {
    let root = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let root = PathUri::from_abs_path(&root);
    let environments = vec![execution_environment(root.clone(), vec![root.clone()])];

    let target = resolve_result_write_target(&environments, "reports/result.json").unwrap();

    assert_eq!(target.path, root.join("reports/result.json").unwrap());
}

#[test]
fn write_path_cannot_escape_the_selected_workspace_roots() {
    let root = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let root = PathUri::from_abs_path(&root);
    let environments = vec![execution_environment(root, Vec::new())];

    let error = match resolve_result_write_target(&environments, "../outside.json") {
        Err(error) => error,
        Ok(_) => panic!("writePath escaping the workspace should be rejected"),
    };

    assert_eq!(
        error,
        "choose a writePath inside a selected workspace root".to_string()
    );
}

#[test]
fn foreign_remote_path_is_resolved_without_host_native_conversion() {
    #[cfg(unix)]
    let root = PathUri::parse("file:///C:/workspace").unwrap();
    #[cfg(windows)]
    let root = PathUri::parse("file:///workspace").unwrap();
    let environments = vec![execution_environment(root.clone(), vec![root])];

    let target = resolve_result_write_target(&environments, "reports/result.json").unwrap();

    #[cfg(unix)]
    let expected = "file:///C:/workspace/reports/result.json";
    #[cfg(windows)]
    let expected = "file:///workspace/reports/result.json";
    assert_eq!(target.path.to_string(), expected);
}

#[tokio::test]
async fn write_result_creates_parent_directories_and_writes_verified_content() {
    let directory = tempfile::tempdir().unwrap();
    let root = AbsolutePathBuf::try_from(directory.path().to_path_buf()).unwrap();
    let root_uri = PathUri::from_abs_path(&root);
    let environments = vec![execution_environment(
        root_uri.clone(),
        vec![root_uri.clone()],
    )];
    let target = resolve_result_write_target(&environments, "reports/result.json").unwrap();
    let serialized = r#"{"answer":42}"#;
    let sha256 = format!("{:x}", sha2::Sha256::digest(serialized.as_bytes()));

    let write = write_workflow_result(&target, serialized, &sha256)
        .await
        .unwrap();

    assert_eq!(write.path, root_uri.join("reports/result.json").unwrap());
    assert_eq!(write.bytes, u64::try_from(serialized.len()).unwrap());
    assert_eq!(write.sha256, sha256);
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("reports/result.json"))
            .await
            .unwrap(),
        serialized
    );
}

fn execution_environment(cwd: PathUri, workspace_roots: Vec<PathUri>) -> ToolExecutionEnvironment {
    let sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
        PermissionProfile::Disabled,
        cwd.clone(),
    );
    ToolExecutionEnvironment::new(
        "selected".to_string(),
        cwd.clone(),
        Some(TurnEnvironmentSelection {
            environment_id: "selected".to_string(),
            cwd,
            workspace_roots,
            config: codex_protocol::protocol::EnvironmentConfigState::FromThread,
        }),
        /*is_remote*/ true,
        "selected-executor".to_string(),
        Arc::clone(&codex_exec_server::LOCAL_FS),
        sandbox,
        Arc::new(()),
    )
}
