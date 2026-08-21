use std::fs::File;
use std::path::Path;
use std::process;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;

use super::InstallLayout;

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct PackageManifest {
    target: String,
    entrypoint: String,
}

pub(super) fn release_is_complete(release_dir: &Path, target: &str) -> Result<bool> {
    if !release_dir.join("codex-package.json").is_file() {
        return Ok(false);
    }
    Ok(validate_release(release_dir, target).is_ok())
}

pub(super) fn validate_release(release_dir: &Path, target: &str) -> Result<()> {
    let manifest_path = release_dir.join("codex-package.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: PackageManifest = serde_json::from_str(&manifest_text)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    if manifest.target != target {
        bail!(
            "package metadata target does not match {target}: found {}",
            manifest.target
        );
    }
    let entrypoint = release_dir.join(Path::new(&manifest.entrypoint));
    if !entrypoint.is_file() {
        bail!("package is missing entrypoint {}", manifest.entrypoint);
    }

    let executable_suffix = if cfg!(windows) { ".exe" } else { "" };
    for relative in [
        format!("bin/codex-code-mode-host{executable_suffix}"),
        format!("codex-path/rg{executable_suffix}"),
    ] {
        let path = release_dir.join(&relative);
        if !path.is_file() {
            bail!("package is missing {relative}");
        }
    }
    if cfg!(target_os = "linux") && !release_dir.join("codex-resources/bwrap").is_file() {
        bail!("package is missing codex-resources/bwrap");
    }
    Ok(())
}

pub(super) fn extract_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(destination)
        .with_context(|| format!("failed to extract {}", archive_path.display()))?;
    Ok(())
}

pub(super) fn replace_release_directory(staging_dir: &Path, release_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(
        release_dir
            .parent()
            .with_context(|| "release directory has no parent")?,
    )
    .with_context(|| format!("failed to create release parent {}", release_dir.display()))?;
    if release_dir.exists() {
        std::fs::remove_dir_all(release_dir).with_context(|| {
            format!(
                "failed to remove existing release {}",
                release_dir.display()
            )
        })?;
    }
    std::fs::rename(staging_dir, release_dir)
        .with_context(|| format!("failed to move staged release to {}", release_dir.display()))?;
    Ok(())
}

pub(super) fn activate_release(layout: &InstallLayout, release_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(&layout.bin_dir)
        .with_context(|| format!("failed to create {}", layout.bin_dir.display()))?;
    let current_link = layout.standalone_root.join("current");
    let executable_suffix = if cfg!(windows) { ".exe" } else { "" };

    #[cfg(unix)]
    {
        replace_symlink(release_dir, &current_link)?;
        let entrypoint = release_dir.join(format!("bin/codex++{executable_suffix}"));
        let entrypoint_link = layout.bin_dir.join(&layout.bin_name);
        replace_symlink(&entrypoint, &entrypoint_link)?;
        let host = release_dir.join(format!("bin/codex-code-mode-host{executable_suffix}"));
        replace_symlink(
            &host,
            &layout
                .bin_dir
                .join(format!("codex-code-mode-host{executable_suffix}")),
        )?;
    }

    #[cfg(windows)]
    {
        replace_junction(release_dir, &current_link)?;
        replace_windows_executable(
            &release_dir.join(format!("bin/codex++{executable_suffix}")),
            &layout
                .bin_dir
                .join(format!("{}{executable_suffix}", layout.bin_name)),
        )?;
        replace_windows_executable(
            &release_dir.join(format!("bin/codex-code-mode-host{executable_suffix}")),
            &layout
                .bin_dir
                .join(format!("codex-code-mode-host{executable_suffix}")),
        )?;
    }

    Ok(())
}

#[cfg(unix)]
fn replace_symlink(target: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temporary = destination.with_file_name(format!(
        ".{}.new.{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("link"),
        process::id()
    ));
    std::fs::remove_file(&temporary).ok();
    symlink(target, &temporary)
        .with_context(|| format!("failed to create symlink {}", temporary.display()))?;
    std::fs::rename(&temporary, destination)
        .with_context(|| format!("failed to replace symlink {}", destination.display()))?;
    Ok(())
}

#[cfg(windows)]
fn replace_junction(target: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        junction::delete(destination)
            .with_context(|| format!("failed to remove junction {}", destination.display()))?;
    }
    junction::create(target, destination)
        .with_context(|| format!("failed to create junction {}", destination.display()))?;
    Ok(())
}

#[cfg(windows)]
fn replace_windows_executable(source: &Path, destination: &Path) -> Result<()> {
    let backup = destination.with_file_name(format!(
        "{}.old.{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("codex"),
        process::id()
    ));
    let had_destination = destination.symlink_metadata().is_ok();
    if had_destination {
        std::fs::rename(destination, &backup)
            .with_context(|| format!("failed to move {} for replacement", destination.display()))?;
    }
    if let Err(error) = std::fs::copy(source, destination) {
        if had_destination && let Err(restore_error) = std::fs::rename(&backup, destination) {
            bail!(
                "failed to copy {} to {}: {error}; restoring the old file also failed: {restore_error}",
                source.display(),
                destination.display()
            );
        }
        return Err(error).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        });
    }
    if had_destination {
        std::fs::remove_file(&backup).ok();
    }
    Ok(())
}
