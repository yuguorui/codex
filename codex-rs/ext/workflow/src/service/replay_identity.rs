use sha2::Digest;
use sha2::Sha256;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use super::PersistedTurnEnvironmentSelection;
use super::PersistedWorkflowEnvironmentLocation;

pub(super) async fn workspace_fingerprint(
    location: PersistedWorkflowEnvironmentLocation,
    selections: &[PersistedTurnEnvironmentSelection],
    excluded_path: PathBuf,
) -> Option<String> {
    if location != PersistedWorkflowEnvironmentLocation::Local {
        return None;
    }
    let mut roots = Vec::new();
    for selection in selections {
        roots.push(selection.cwd.to_abs_path().ok()?.into());
        roots.extend(
            selection
                .workspace_roots
                .iter()
                .map(|root| root.to_abs_path().map(Into::into))
                .collect::<Result<Vec<PathBuf>, _>>()
                .ok()?,
        );
    }
    roots.sort();
    roots.dedup();
    roots.retain(|root| !root.starts_with(&excluded_path));
    if roots.is_empty() {
        return None;
    }

    tokio::task::spawn_blocking(move || fingerprint_workspace(&roots, &excluded_path))
        .await
        .ok()?
        .ok()
}

fn fingerprint_workspace(
    roots: &[PathBuf],
    excluded_path: &Path,
) -> Result<String, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut git_roots = Vec::new();
    let mut filesystem_roots = Vec::new();
    for root in roots {
        match git_root(root) {
            Some(git_root) => git_roots.push(git_root),
            None => filesystem_roots.push(root.clone()),
        }
    }
    git_roots.sort();
    git_roots.dedup();
    filesystem_roots.retain(|root| !git_roots.iter().any(|git_root| root.starts_with(git_root)));
    filesystem_roots.sort();
    filesystem_roots.dedup();

    for root in git_roots {
        hash_git_workspace(&mut hasher, &root, excluded_path)?;
    }
    for root in filesystem_roots {
        hash_path(&mut hasher, &root, &root, excluded_path)?;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn git_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = PathBuf::from(root.trim_end_matches(['\r', '\n']));
    root.is_absolute().then_some(root)
}

fn hash_git_workspace(
    hasher: &mut Sha256,
    root: &Path,
    excluded_path: &Path,
) -> Result<(), std::io::Error> {
    hasher.update(b"git-workspace");
    update_os_str(hasher, root.as_os_str());
    let identity_commands = [
        &["rev-parse", "HEAD"] as &[&str],
        &["symbolic-ref", "-q", "HEAD"],
        &["ls-files", "-s", "-z"],
        &["config", "--local", "--null", "--list"],
    ];
    let identity = identity_commands
        .iter()
        .map(|args| git_output(root, args).map(git_output_identity))
        .collect::<Result<Vec<_>, _>>()?;
    for (status, stdout, stderr) in &identity {
        hasher.update(status.to_le_bytes());
        hasher.update(stdout);
        hasher.update(stderr);
    }

    hash_git_worktree(hasher, root, excluded_path)?;
    let identity_after = identity_commands
        .iter()
        .map(|args| git_output(root, args).map(git_output_identity))
        .collect::<Result<Vec<_>, _>>()?;
    if identity != identity_after {
        return Err(std::io::Error::other(format!(
            "Git workspace changed while hashing: {}",
            root.display()
        )));
    }
    Ok(())
}

fn hash_git_worktree(
    hasher: &mut Sha256,
    root: &Path,
    excluded_path: &Path,
) -> Result<(), std::io::Error> {
    let entries = read_directory(root)?;
    for entry in &entries {
        if entry.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        hash_path(hasher, root, entry, excluded_path)?;
    }
    if entries != read_directory(root)? {
        return Err(std::io::Error::other(format!(
            "Git worktree changed while hashing: {}",
            root.display()
        )));
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    Command::new("git").arg("-C").arg(root).args(args).output()
}

fn git_output_identity(output: std::process::Output) -> (i32, Vec<u8>, Vec<u8>) {
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

fn hash_path(
    hasher: &mut Sha256,
    root: &Path,
    path: &Path,
    excluded_path: &Path,
) -> Result<(), std::io::Error> {
    if path.starts_with(excluded_path) {
        return Ok(());
    }

    let before = std::fs::symlink_metadata(path)?;
    let permissions = metadata_permissions(&before);
    let file_type = before.file_type();
    let excluded_ancestor = excluded_path.starts_with(path);
    if !excluded_ancestor {
        let relative = path.strip_prefix(root).unwrap_or(path);
        update_os_str(hasher, relative.as_os_str());
        hasher.update(permissions.to_le_bytes());
    }
    if file_type.is_symlink() {
        hasher.update(b"symlink");
        let target = std::fs::read_link(path)?;
        update_os_str(hasher, target.as_os_str());
        let resolved_target = path.canonicalize()?;
        let canonical_root = root.canonicalize()?;
        if !resolved_target.starts_with(&canonical_root) {
            return Err(std::io::Error::other(format!(
                "workspace symlink resolves outside its captured root: {}",
                path.display()
            )));
        }
        let after = std::fs::symlink_metadata(path)?;
        if permissions != metadata_permissions(&after)
            || !after.file_type().is_symlink()
            || target != std::fs::read_link(path)?
        {
            return Err(std::io::Error::other(format!(
                "workspace symlink changed while hashing: {}",
                path.display()
            )));
        }
        return Ok(());
    }
    if file_type.is_file() {
        hasher.update(b"file");
        let mut file = File::open(path)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let after = std::fs::symlink_metadata(path)?;
        if before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
            || permissions != metadata_permissions(&after)
            || !after.file_type().is_file()
        {
            return Err(std::io::Error::other(format!(
                "workspace file changed while hashing: {}",
                path.display()
            )));
        }
        return Ok(());
    }
    if file_type.is_dir() {
        if !excluded_ancestor {
            hasher.update(b"directory");
        }
        let entries = read_directory(path)?;
        for entry in &entries {
            hash_path(hasher, root, entry, excluded_path)?;
        }
        let after = std::fs::symlink_metadata(path)?;
        if entries != read_directory(path)?
            || permissions != metadata_permissions(&after)
            || !after.file_type().is_dir()
        {
            return Err(std::io::Error::other(format!(
                "workspace directory changed while hashing: {}",
                path.display()
            )));
        }
        return Ok(());
    }

    hasher.update(b"special");
    hasher.update(before.len().to_le_bytes());
    Ok(())
}

fn read_directory(path: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut entries = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

#[cfg(unix)]
fn metadata_permissions(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    u64::from(metadata.mode())
}

#[cfg(windows)]
fn metadata_permissions(metadata: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;

    u64::from(metadata.file_attributes())
}

#[cfg(unix)]
fn update_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = value.as_bytes();
    hasher.update(bytes.len().to_le_bytes());
    hasher.update(bytes);
}

#[cfg(windows)]
fn update_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let value = value.encode_wide().collect::<Vec<_>>();
    hasher.update(value.len().to_le_bytes());
    for unit in value {
        hasher.update(unit.to_le_bytes());
    }
}
