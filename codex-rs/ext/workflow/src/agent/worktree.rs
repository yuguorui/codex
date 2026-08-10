use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow::WorkflowAgentFailure;
use codex_workflow::WorkflowAgentFailureKind;
use std::path::Path;
use tokio::process::Command;
use tracing::warn;

use super::failure;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeCleanupMode {
    Completed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorktreeRemovalMode<'a> {
    Force,
    PreserveChanges { base_commit: &'a str },
}

pub(super) struct Worktree {
    pub(super) repository: AbsolutePathBuf,
    pub(super) path: AbsolutePathBuf,
    pub(super) branch: String,
    base_commit: String,
    cleanup_on_drop: bool,
}

impl Worktree {
    pub(super) async fn create(
        cwd: &AbsolutePathBuf,
        codex_home: &AbsolutePathBuf,
        run_id: &str,
        index: usize,
        attempt: u32,
    ) -> Result<Self, WorkflowAgentFailure> {
        let repository_output = Command::new("git")
            .arg("-C")
            .arg(cwd.as_path())
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .await
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        if !repository_output.status.success() {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                "worktree isolation requires a git repository",
            ));
        }
        let repository = String::from_utf8_lossy(&repository_output.stdout)
            .trim()
            .to_string();
        let repository = AbsolutePathBuf::try_from(std::path::PathBuf::from(repository))
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        let base_commit_output = Command::new("git")
            .arg("-C")
            .arg(repository.as_path())
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        if !base_commit_output.status.success() {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                "worktree isolation requires a repository with a current commit",
            ));
        }
        let base_commit = String::from_utf8_lossy(&base_commit_output.stdout)
            .trim()
            .to_string();
        let run_slug = run_id.trim_start_matches("wf_").replace('_', "-");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let nonce = &nonce[..8];
        let branch = format!("wf-{run_slug}-{index}-a{attempt}-{nonce}");
        let path = codex_home
            .join("worktrees")
            .join(run_id)
            .join(format!("{index}-{attempt}-{nonce}"));
        let parent = path.parent().ok_or_else(|| {
            failure(
                WorkflowAgentFailureKind::Failed,
                "workflow worktree path has no parent",
            )
        })?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        let output = Command::new("git")
            .arg("-C")
            .arg(repository.as_path())
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(path.as_path())
            .arg(&base_commit)
            .output()
            .await
            .map_err(|error| failure(WorkflowAgentFailureKind::Failed, error.to_string()))?;
        if !output.status.success() {
            return Err(failure(
                WorkflowAgentFailureKind::Failed,
                format!(
                    "failed to create workflow worktree: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(Self {
            repository,
            path,
            branch,
            base_commit,
            cleanup_on_drop: true,
        })
    }

    pub(super) async fn cleanup_if_unchanged(mut self) -> Option<Self> {
        let repository = self.repository.clone();
        let path = self.path.clone();
        let branch = self.branch.clone();
        let base_commit = self.base_commit.clone();
        match tokio::task::spawn_blocking(move || {
            cleanup_worktree(
                &repository,
                &path,
                &branch,
                WorktreeRemovalMode::PreserveChanges {
                    base_commit: &base_commit,
                },
            )
        })
        .await
        {
            Ok(true) => {
                self.cleanup_on_drop = false;
                None
            }
            Ok(false) => Some(self),
            Err(error) => {
                warn!("workflow worktree cleanup task failed: {error}");
                Some(self)
            }
        }
    }

    pub(super) async fn cleanup(mut self) {
        let repository = self.repository.clone();
        let path = self.path.clone();
        let branch = self.branch.clone();
        match tokio::task::spawn_blocking(move || {
            cleanup_worktree(&repository, &path, &branch, WorktreeRemovalMode::Force)
        })
        .await
        {
            Ok(true) => self.cleanup_on_drop = false,
            Ok(false) => {}
            Err(error) => warn!("workflow worktree cleanup task failed: {error}"),
        }
    }

    pub(super) fn preserve_after_interruption(mut self) -> String {
        self.cleanup_on_drop = false;
        format!(
            "Retained changed workflow worktree after interruption: {} (branch {})",
            self.path.display(),
            self.branch
        )
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        let cleanup = DroppedWorktreeCleanup {
            repository: self.repository.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                drop(handle.spawn_blocking(move || cleanup.run()));
            }
            Err(_) => {
                if let Err(error) = std::thread::Builder::new()
                    .name("workflow-worktree-cleanup".to_string())
                    .spawn(move || cleanup.run())
                {
                    warn!("failed to schedule dropped workflow worktree cleanup: {error}");
                }
            }
        }
    }
}

struct DroppedWorktreeCleanup {
    repository: AbsolutePathBuf,
    path: AbsolutePathBuf,
    branch: String,
    base_commit: String,
}

impl DroppedWorktreeCleanup {
    fn run(self) {
        if !cleanup_worktree(
            &self.repository,
            &self.path,
            &self.branch,
            WorktreeRemovalMode::PreserveChanges {
                base_commit: &self.base_commit,
            },
        ) {
            warn!(
                path = %self.path.display(),
                branch = %self.branch,
                "retaining changed workflow worktree after abnormal exit"
            );
        }
    }
}

pub(super) fn cleanup_worktree(
    repository: &Path,
    path: &Path,
    branch: &str,
    mode: WorktreeRemovalMode<'_>,
) -> bool {
    if let WorktreeRemovalMode::PreserveChanges { base_commit } = mode
        && worktree_has_changes(path, base_commit)
    {
        return false;
    }
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .args(["worktree", "remove"]);
    if mode == WorktreeRemovalMode::Force {
        command.arg("--force");
    }
    let remove = command.arg(path).output();
    match remove {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            warn!(
                path = %path.display(),
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "failed to remove workflow worktree"
            );
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["worktree", "prune"])
                .status();
            return false;
        }
        Err(error) => {
            warn!(path = %path.display(), "failed to run workflow worktree cleanup: {error}");
            return false;
        }
    }

    if let Some(run_dir) = path.parent() {
        let _ = std::fs::remove_dir(run_dir);
    }

    if let WorktreeRemovalMode::PreserveChanges { base_commit } = mode
        && !branch_is_at_base(repository, branch, base_commit)
    {
        warn!(
            branch,
            "retaining workflow branch changed during conservative cleanup"
        );
        return true;
    }

    let delete_branch = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["branch", "-D"])
        .arg(branch)
        .output();
    match delete_branch {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            branch,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "failed to delete workflow worktree branch"
        ),
        Err(error) => warn!(branch, "failed to run workflow branch cleanup: {error}"),
    }

    true
}

fn worktree_has_changes(path: &Path, base_commit: &str) -> bool {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output();
    if status.map_or(true, |output| {
        !output.status.success() || !output.stdout.is_empty()
    }) {
        return true;
    }
    !branch_is_at_base(path, "HEAD", base_commit)
}

fn branch_is_at_base(repository: &Path, branch: &str, base_commit: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["rev-parse", branch])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == base_commit
        })
}
