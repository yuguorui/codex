use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn explicit_sources_take_precedence_and_preserve_invocation_fields() {
    let fixture = DiscoveryFixture::new().await;
    let explicit_path = fixture.cwd.join("explicit.js");
    write_script(&explicit_path, "explicit").await;
    let args = json!({ "topic": "budgets" });
    let mut input = empty_input();
    input.script_path = Some("explicit.js".to_string());
    input.script = Some(script("inline"));
    input.name = Some("deep-research".to_string());
    input.args = args.clone();
    input.resume_from_run_id = Some("wf_abc123".to_string());

    let resolved = resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &[])
        .await
        .unwrap();

    assert_eq!(
        resolved.script,
        validate_workflow_script(script("explicit")).unwrap()
    );
    assert_eq!(
        (resolved.args, resolved.resume_from_run_id),
        (args, Some("wf_abc123".to_string()))
    );
    assert_eq!(resolved.origin, WorkflowOrigin::File(explicit_path));
    assert!(!resolved.shadows_existing);

    let mut input = empty_input();
    input.script = Some(script("inline"));
    input.name = Some("deep-research".to_string());
    let resolved = resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &[])
        .await
        .unwrap();
    assert_eq!(
        resolved.script,
        validate_workflow_script(script("inline")).unwrap()
    );
}

#[tokio::test]
async fn deepest_project_codex_workflow_wins_saved_workflow_precedence() {
    let fixture = DiscoveryFixture::new().await;
    let project = fixture.cwd.parent().unwrap();
    for (path, name) in [
        (
            fixture
                .codex_home
                .parent()
                .unwrap()
                .join(".claude/workflows/priority.js"),
            "user-claude",
        ),
        (
            fixture.codex_home.join("workflows/priority.js"),
            "user-codex",
        ),
        (
            project.join(".claude/workflows/priority.js"),
            "project-claude",
        ),
        (
            project.join(".codex/workflows/priority.js"),
            "project-codex",
        ),
        (
            fixture.cwd.join(".claude/workflows/priority.js"),
            "nested-claude",
        ),
        (
            fixture.cwd.join(".codex/workflows/priority.js"),
            "nested-codex",
        ),
    ] {
        write_script(&path, name).await;
    }
    let mut input = empty_input();
    input.name = Some("priority".to_string());

    let resolved = resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &[])
        .await
        .unwrap();

    assert_eq!(resolved.script.meta.name, "nested-codex");
    assert_eq!(
        resolved.origin,
        WorkflowOrigin::File(fixture.cwd.join(".codex/workflows/priority.js"))
    );
    assert!(resolved.shadows_existing);
}

#[tokio::test]
async fn project_workflow_can_shadow_a_bundled_workflow_and_reports_it() {
    let fixture = DiscoveryFixture::new().await;
    let project_path = fixture.cwd.join(".codex/workflows/deep-research.js");
    write_script(&project_path, "project-research").await;
    let mut input = empty_input();
    input.name = Some("deep-research".to_string());

    let resolved = resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &[])
        .await
        .unwrap();

    assert_eq!(resolved.script.meta.name, "project-research");
    assert_eq!(resolved.origin, WorkflowOrigin::File(project_path));
    assert!(resolved.shadows_existing);
}

#[tokio::test]
async fn namespaced_workflow_resolves_only_from_the_matching_active_plugin_root() {
    let fixture = DiscoveryFixture::new().await;
    let plugin_path = fixture.cwd.join("plugins/acme/workflows/research.js");
    write_script(&plugin_path, "plugin-research").await;
    let plugin_roots = vec![PluginWorkflowRoot {
        namespace: "acme".to_string(),
        workflows_dir: fixture.cwd.join("plugins/acme/workflows"),
    }];
    let mut input = empty_input();
    input.name = Some("acme:research".to_string());

    let resolved = resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &plugin_roots)
        .await
        .unwrap();

    assert_eq!(resolved.script.meta.name, "plugin-research");
    assert_eq!(
        resolved.origin,
        WorkflowOrigin::Plugin {
            namespace: "acme".to_string(),
            path: plugin_path,
        }
    );
    assert!(!resolved.shadows_existing);

    let mut unqualified = empty_input();
    unqualified.name = Some("research".to_string());
    assert!(matches!(
        resolve_workflow(
            unqualified,
            &fixture.cwd,
            &fixture.codex_home,
            &plugin_roots,
        )
        .await,
        Err(WorkflowResolveError::NotFound(name)) if name == "research"
    ));
}

#[tokio::test]
async fn saved_workflow_near_miss_reports_the_unsupported_extension() {
    let fixture = DiscoveryFixture::new().await;
    let near_miss = fixture.codex_home.join("workflows/research.ts");
    write_script(&near_miss, "research").await;
    let mut input = empty_input();
    input.name = Some("research".to_string());

    let error = match resolve_workflow(input, &fixture.cwd, &fixture.codex_home, &[]).await {
        Ok(_) => panic!("TypeScript workflows should be rejected as a near miss"),
        Err(error) => error,
    };

    match error {
        WorkflowResolveError::NearMiss(path) => assert_eq!(path, near_miss.display().to_string()),
        other => panic!("expected near-miss error, got {other}"),
    }
}

#[tokio::test]
async fn rejects_oversized_explicit_and_saved_workflows_during_bounded_read() {
    let fixture = DiscoveryFixture::new().await;
    let oversized = "x".repeat(MAX_WORKFLOW_SCRIPT_BYTES + 1);
    let explicit_path = fixture.cwd.join("oversized.js");
    tokio::fs::write(&explicit_path, &oversized).await.unwrap();
    let mut explicit = empty_input();
    explicit.script_path = Some(explicit_path.display().to_string());

    assert!(matches!(
        resolve_workflow(explicit, &fixture.cwd, &fixture.codex_home, &[]).await,
        Err(WorkflowResolveError::InvalidScript(
            WorkflowScriptError::TooLarge
        ))
    ));

    let saved_path = fixture.codex_home.join("workflows/oversized.js");
    tokio::fs::create_dir_all(saved_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&saved_path, oversized).await.unwrap();
    let mut saved = empty_input();
    saved.name = Some("oversized".to_string());

    assert!(matches!(
        resolve_workflow(saved, &fixture.cwd, &fixture.codex_home, &[]).await,
        Err(WorkflowResolveError::InvalidScript(
            WorkflowScriptError::TooLarge
        ))
    ));
}

#[tokio::test]
async fn child_resolver_accepts_name_references_and_rejects_invalid_objects() {
    let fixture = DiscoveryFixture::new().await;
    let child_path = fixture.cwd.join(".codex/workflows/child.js");
    write_script(&child_path, "child").await;
    let resolver = SavedWorkflowChildResolver::new(
        fixture.cwd.clone(),
        fixture.codex_home.clone(),
        Vec::new(),
    );
    let args = json!(["one", 2]);

    let child = resolver
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "name": "child" }),
            args: args.clone(),
        })
        .await
        .unwrap();

    assert_eq!(
        (child.script, child.args),
        (validate_workflow_script(script("child")).unwrap(), args)
    );

    let path_child = resolver
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "scriptPath": child_path }),
            args: json!({ "source": "path" }),
        })
        .await
        .unwrap();
    assert_eq!(
        (path_child.script.meta.name, path_child.args),
        ("child".to_string(), json!({ "source": "path" }))
    );
    assert!(
        resolver
            .resolve_child(WorkflowChildRequest {
                name_or_ref: json!({ "path": "child" }),
                args: JsonValue::Null,
            })
            .await
            .is_err()
    );
    assert!(
        resolver
            .resolve_child(WorkflowChildRequest {
                name_or_ref: json!({ "name": "child", "scriptPath": "child.js" }),
                args: JsonValue::Null,
            })
            .await
            .is_err()
    );
}

#[tokio::test]
async fn child_resolver_inherits_active_plugin_workflow_roots() {
    let fixture = DiscoveryFixture::new().await;
    let workflows_dir = fixture.cwd.join("plugins/acme/workflows");
    write_script(&workflows_dir.join("child.js"), "plugin-child").await;
    let resolver = SavedWorkflowChildResolver::new(
        fixture.cwd.clone(),
        fixture.codex_home.clone(),
        vec![PluginWorkflowRoot {
            namespace: "acme".to_string(),
            workflows_dir,
        }],
    );

    let child = resolver
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!("acme:child"),
            args: json!({ "source": "plugin" }),
        })
        .await
        .unwrap();

    assert_eq!(
        (child.script.meta.name, child.args),
        ("plugin-child".to_string(), json!({ "source": "plugin" }))
    );
}

#[test]
fn rejects_unsafe_names_and_invalid_resume_ids() {
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "nested/name",
        "nested\\name",
        "bad\u{0}",
        ":workflow",
        "plugin:",
        "plugin:workflow:extra",
    ] {
        assert!(matches!(
            validate_name(name),
            Err(WorkflowResolveError::InvalidName)
        ));
    }
    for run_id in ["wf_short", "wf_ABC123", "run_abc123", "wf_abc_123"] {
        assert!(matches!(
            validate_resume_id(Some(run_id)),
            Err(WorkflowResolveError::InvalidRunId)
        ));
    }
    assert!(validate_name("review-v2").is_ok());
    assert!(validate_name("plugin:review-v2").is_ok());
    assert!(validate_resume_id(Some("wf_abc-123")).is_ok());
}

#[test]
fn explicit_paths_require_the_javascript_extension() {
    for extension in ["mjs", "cjs", "ts"] {
        let path = PathBuf::from(format!("workflow.{extension}"));
        assert!(matches!(
            ensure_js_extension(&path),
            Err(WorkflowResolveError::NearMiss(_))
        ));
    }
    assert!(matches!(
        ensure_js_extension(Path::new("workflow.json")),
        Err(WorkflowResolveError::NotFound(_))
    ));
    assert!(ensure_js_extension(Path::new("workflow.js")).is_ok());
}

struct DiscoveryFixture {
    _root: TempDir,
    cwd: AbsolutePathBuf,
    codex_home: AbsolutePathBuf,
}

impl DiscoveryFixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project/nested");
        let codex_home = root.path().join("home/.codex");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&codex_home).await.unwrap();
        Self {
            cwd: AbsolutePathBuf::try_from(cwd).unwrap(),
            codex_home: AbsolutePathBuf::try_from(codex_home).unwrap(),
            _root: root,
        }
    }
}

fn empty_input() -> WorkflowInput {
    WorkflowInput {
        script: None,
        name: None,
        description: None,
        title: None,
        args: JsonValue::Null,
        script_path: None,
        resume_from_run_id: None,
    }
}

fn script(name: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: 'discovery test' }};\nreturn '{name}';\n"
    )
}

async fn write_script(path: &Path, name: &str) {
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(path, script(name)).await.unwrap();
}
