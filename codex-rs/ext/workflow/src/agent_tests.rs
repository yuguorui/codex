use super::worktree::WorktreeRemovalMode;
use super::worktree::cleanup_worktree;
use super::*;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::sync::Weak;
use tempfile::TempDir;
use tokio::process::Command;

#[test]
fn strict_schema_requires_nullable_optional_properties_recursively() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "details": {
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "state": { "enum": ["ready", "blocked"] }
                },
                "required": ["count"]
            }
        },
        "required": ["name"]
    });

    assert_eq!(
        strict_output_schema(&schema),
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "details": {
                    "type": ["object", "null"],
                    "properties": {
                        "count": { "type": "integer" },
                        "state": { "enum": ["ready", "blocked", null] }
                    },
                    "required": ["count", "state"],
                    "additionalProperties": false
                }
            },
            "required": ["details", "name"],
            "additionalProperties": false
        })
    );
}

#[test]
fn validation_preserves_optional_property_semantics_after_model_normalization() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "note": { "type": "string" }
        },
        "required": ["name"],
        "additionalProperties": false
    });

    assert_eq!(validate_schema(&json!({ "name": "run" }), &schema), Ok(()));
    assert_eq!(
        validate_schema(&json!({ "name": "run", "note": null }), &schema),
        Ok(())
    );
    assert!(validate_schema(&json!({ "name": "run", "note": 3 }), &schema).is_err());
}

#[test]
fn schema_validation_enforces_combinators_patterns_and_numeric_bounds() {
    let schema = json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "pattern": "^[A-Z]{2}$" },
            "score": { "type": "integer", "minimum": 1, "maximum": 3 },
            "kind": { "oneOf": [{ "const": "primary" }, { "const": "fallback" }] }
        },
        "required": ["code", "score", "kind"],
        "additionalProperties": false
    });

    assert_eq!(
        validate_schema(
            &json!({ "code": "OK", "score": 2, "kind": "primary" }),
            &schema
        ),
        Ok(())
    );
    for invalid in [
        json!({ "code": "bad", "score": 2, "kind": "primary" }),
        json!({ "code": "OK", "score": 4, "kind": "primary" }),
        json!({ "code": "OK", "score": 2, "kind": "unknown" }),
    ] {
        assert!(validate_schema(&invalid, &schema).is_err());
    }
}

#[test]
fn prompt_fallback_contains_schema_for_providers_without_native_structured_output() {
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"]
    });

    let contract = structured_output_contract(&schema, false).unwrap();

    assert!(contract.contains("Return only a JSON value matching this schema"));
    assert!(contract.contains(&serde_json::to_string(&schema).unwrap()));
}

#[test]
fn oversized_prompt_fallback_schema_requires_native_provider_support() {
    let schema = json!({
        "type": "string",
        "description": "x".repeat(MAX_PROMPT_SCHEMA_BYTES),
    });

    assert!(structured_output_contract(&schema, false).is_err());
    assert_eq!(
        structured_output_contract(&schema, true).unwrap(),
        "\n\nReturn only JSON matching the host-provided schema. Do not use Markdown fences or add prose."
    );
}

#[test]
fn oversized_schema_is_rejected_for_every_provider() {
    let schema = json!({
        "type": "string",
        "description": "x".repeat(MAX_OUTPUT_SCHEMA_BYTES),
    });

    assert!(structured_output_contract(&schema, false).is_err());
    assert!(structured_output_contract(&schema, true).is_err());
}

#[test]
fn structured_retry_prompt_is_a_bounded_in_conversation_nudge() {
    let error = "e".repeat(10_000);

    let prompt = structured_retry_prompt(&error);

    assert!(prompt.starts_with("Your previous final output did not satisfy"));
    assert!(prompt.ends_with("Return only corrected JSON."));
    assert!(!prompt.contains("Previous output:"));
    assert!(!prompt.contains(&"e".repeat(2_000)));
}

#[tokio::test]
async fn large_prompt_reaches_agent_runner_without_a_workflow_specific_limit() {
    let cwd = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("33333333-3333-4333-8333-333333333333").unwrap(),
        config,
        "wf_large-prompt".to_string(),
    );

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 0,
                prompt: "x".repeat(96 * 1024),
                options: codex_workflow::WorkflowAgentOptions::default(),
                attempt: 0,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("the dropped thread manager should fail after prompt forwarding");

    assert_eq!(error.kind, WorkflowAgentFailureKind::TerminalApi);
    assert!(error.message.contains("thread manager dropped"));
}

#[tokio::test]
async fn cleans_unchanged_worktree_when_agent_fails() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let parent_thread_id = ThreadId::from_string("44444444-4444-4444-8444-444444444444").unwrap();
    let run_id = "wf_cleanup-error";
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        parent_thread_id,
        config,
        run_id.to_string(),
    );

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 7,
                prompt: "fail after creating the worktree".to_string(),
                options: codex_workflow::WorkflowAgentOptions {
                    isolation: Some(WorkflowIsolation::Worktree),
                    ..Default::default()
                },
                attempt: 1,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("the dropped thread manager should fail the agent");

    assert!(error.message.contains("thread manager dropped"));
    let worktree_root = codex_home.path().join("worktrees").join(run_id);
    if worktree_root.exists() {
        let mut worktrees = tokio::fs::read_dir(worktree_root).await.unwrap();
        assert!(worktrees.next_entry().await.unwrap().is_none());
    }
    let branches = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args(["branch", "--list", "wf-cleanup-error-7-a1-*"])
        .output()
        .await
        .unwrap();
    assert!(branches.status.success());
    assert!(branches.stdout.is_empty());
}

#[tokio::test]
async fn uses_unique_worktrees_for_retried_or_resumed_attempts() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let run_id = "wf_retry-resume";

    let first = Worktree::create(&cwd, &home, run_id, /*index*/ 3, /*attempt*/ 0)
        .await
        .unwrap();
    let second = Worktree::create(&cwd, &home, run_id, /*index*/ 3, /*attempt*/ 0)
        .await
        .unwrap();

    assert_ne!(first.path, second.path);
    assert_ne!(first.branch, second.branch);
    assert!(first.path.exists());
    assert!(second.path.exists());
    assert!(first.cleanup_if_unchanged().await.is_none());
    assert!(second.cleanup_if_unchanged().await.is_none());
}

#[tokio::test]
async fn completed_workflow_reclaims_changed_worktrees_after_runtime_settles() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("55555555-5555-4555-8555-555555555555").unwrap(),
        config,
        "wf_changed-cleanup".to_string(),
    );
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_changed-cleanup",
        /*index*/ 2,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();

    let retained = worktree
        .cleanup_if_unchanged()
        .await
        .expect("changed worktree should remain available during the workflow");
    assert!(path.exists());
    runtime
        .retained_worktrees
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(retained);

    assert!(
        runtime
            .cleanup_worktrees(WorktreeCleanupMode::Completed)
            .await
            .is_empty()
    );

    assert_worktree_removed(repository.path(), &path, &branch).await;
}

#[tokio::test]
async fn interrupted_workflow_preserves_changed_worktree_and_reports_it() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("66666666-6666-4666-8666-666666666666").unwrap(),
        config,
        "wf_interrupted-cleanup".to_string(),
    );
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_interrupted-cleanup",
        /*index*/ 4,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();
    runtime
        .retained_worktrees
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(worktree);

    let messages = runtime
        .cleanup_worktrees(WorktreeCleanupMode::Interrupted)
        .await;

    assert_eq!(
        messages,
        vec![format!(
            "Retained changed workflow worktree after interruption: {} (branch {branch})",
            path.display()
        )]
    );
    assert_worktree_retained(repository.path(), &path, &branch).await;
    let cleanup_repository = repository.path().to_path_buf();
    let cleanup_path = path.clone();
    let cleanup_branch = branch.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_worktree(
            &cleanup_repository,
            &cleanup_path,
            &cleanup_branch,
            WorktreeRemovalMode::Force,
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn conservative_cleanup_preserves_commits_made_in_the_worktree() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_committed-cleanup",
        /*index*/ 3,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "committed change\n")
        .await
        .unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "workflow edit"]] {
        let output = Command::new("git")
            .arg("-C")
            .arg(path.as_path())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let retained = worktree
        .cleanup_if_unchanged()
        .await
        .expect("a committed workflow edit must be retained");

    assert_worktree_retained(repository.path(), &path, &branch).await;
    retained.cleanup().await;
    assert_worktree_removed(repository.path(), &path, &branch).await;
}

#[tokio::test]
async fn drop_preserves_a_changed_worktree_on_abnormal_exit() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_drop-cleanup",
        /*index*/ 4,
        /*attempt*/ 1,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();

    drop(worktree);

    assert_worktree_retained(repository.path(), &path, &branch).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_worktree_retained(repository.path(), &path, &branch).await;
    let cleanup_repository = repository.path().to_path_buf();
    let cleanup_path = path.clone();
    let cleanup_branch = branch.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_worktree(
            &cleanup_repository,
            &cleanup_path,
            &cleanup_branch,
            WorktreeRemovalMode::Force,
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn drop_reclaims_an_unchanged_worktree_in_the_background() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_drop-unchanged",
        /*index*/ 5,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();

    drop(worktree);

    assert_worktree_removed(repository.path(), &path, &branch).await;
}

async fn initialized_repository() -> TempDir {
    let repository = tempfile::tempdir().unwrap();
    for args in [
        &["init"][..],
        &["config", "user.email", "workflow-tests@example.invalid"],
        &["config", "user.name", "Workflow Tests"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::fs::write(repository.path().join("tracked.txt"), "tracked\n")
        .await
        .unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "initial"]] {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    repository
}

async fn assert_worktree_removed(repository: &Path, path: &Path, branch: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let branches = Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["branch", "--list"])
                .arg(branch)
                .output()
                .await
                .unwrap();
            assert!(branches.status.success());
            if !path.exists() && branches.stdout.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "worktree {} or branch {branch} was not removed",
            path.display()
        )
    });
}

async fn assert_worktree_retained(repository: &Path, path: &Path, branch: &str) {
    assert!(
        path.exists(),
        "worktree was removed from {}",
        path.display()
    );
    let branches = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["branch", "--list"])
        .arg(branch)
        .output()
        .await
        .unwrap();
    assert!(branches.status.success());
    assert!(!branches.stdout.is_empty(), "branch {branch} was removed");
}
