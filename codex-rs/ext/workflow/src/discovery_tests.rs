use super::*;
use codex_workflow::WorkflowChildRequest;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn rejects_multiple_top_level_sources() {
    let fixture = DiscoveryFixture::new().await;
    let mut input = empty_input();
    input.script_path = Some("explicit.js".to_string());
    input.script = Some(script("inline"));
    input.name = Some("deep-research".to_string());

    let Err(error) = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    else {
        panic!("multiple Workflow sources must be rejected");
    };

    assert!(matches!(error, WorkflowResolveError::MultipleSources));
}

#[tokio::test]
async fn deepest_project_codex_workflow_wins_saved_workflow_precedence() {
    let fixture = DiscoveryFixture::new().await;
    let project = fixture.cwd.parent().unwrap();
    for (path, name) in [
        (
            fixture.codex_home.join("workflows/priority.js"),
            "user-codex",
        ),
        (
            project.join(".codex/workflows/priority.js"),
            "project-codex",
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

    let resolved = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    .unwrap();

    assert_eq!(resolved.script.meta.name, "nested-codex");
    assert_eq!(
        resolved.origin,
        WorkflowOrigin::File {
            path: fixture.cwd.join(".codex/workflows/priority.js")
        }
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

    let resolved = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    .unwrap();

    assert_eq!(resolved.script.meta.name, "project-research");
    assert_eq!(resolved.origin, WorkflowOrigin::File { path: project_path });
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

    let resolved = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &plugin_roots,
        ChildWorkflowPolicy::FreezeLocal,
    )
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
            ChildWorkflowPolicy::FreezeLocal,
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

    let error = match resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    {
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
        resolve_workflow(
            explicit,
            &fixture.cwd,
            &fixture.codex_home,
            &[],
            ChildWorkflowPolicy::FreezeLocal
        )
        .await,
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
        resolve_workflow(
            saved,
            &fixture.cwd,
            &fixture.codex_home,
            &[],
            ChildWorkflowPolicy::FreezeLocal
        )
        .await,
        Err(WorkflowResolveError::InvalidScript(
            WorkflowScriptError::TooLarge
        ))
    ));
}

#[tokio::test]
async fn static_named_child_is_frozen_and_ignores_later_source_changes() {
    let fixture = DiscoveryFixture::new().await;
    let child_path = fixture.cwd.join(".codex/workflows/child.js");
    write_script(&child_path, "child-before").await;
    let mut input = empty_input();
    input.script = Some(format!(
        "{}\nreturn workflow({{name: \"child\"}});",
        script("parent").lines().next().unwrap()
    ));

    let resolved = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    .unwrap();
    write_script(&child_path, "child-after").await;

    let resolver = resolved.composition.resolver().unwrap();
    let child = resolver
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "name": "child" }),
            args: JsonValue::Null,
        })
        .await
        .unwrap();
    assert_eq!(child.script.meta.name, "child-before");

    let error = resolver
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "name": "unapproved" }),
            args: JsonValue::Null,
        })
        .await
        .unwrap_err();
    assert!(error.contains("was not approved in the frozen composition"));
}

#[tokio::test]
async fn static_script_path_child_is_frozen_successfully() {
    let fixture = DiscoveryFixture::new().await;
    let child_path = fixture.cwd.join("children/explicit.js");
    write_script(&child_path, "explicit-child").await;
    let mut input = empty_input();
    input.script = Some(format!(
        "{}\nreturn workflow({{scriptPath: \"children/explicit.js\"}});",
        script("parent").lines().next().unwrap()
    ));

    let resolved = resolve_workflow(
        input,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    .unwrap();
    let child = resolved
        .composition
        .resolver()
        .unwrap()
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "scriptPath": "children/explicit.js" }),
            args: JsonValue::Null,
        })
        .await
        .unwrap();

    assert_eq!(child.script.meta.name, "explicit-child");
}

#[tokio::test]
async fn remote_and_nested_child_composition_fail_closed() {
    let fixture = DiscoveryFixture::new().await;
    let parent = format!(
        "{}\nreturn workflow({{name: \"child\"}});",
        script("parent").lines().next().unwrap()
    );
    let mut remote = empty_input();
    remote.script = Some(parent.clone());
    let remote_error = match resolve_workflow(
        remote,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::RejectRemote,
    )
    .await
    {
        Ok(_) => panic!("remote child composition must fail closed"),
        Err(error) => error,
    };
    let WorkflowResolveError::ChildComposition(remote_guidance) = remote_error else {
        panic!("remote composition should fail with the child-composition category");
    };
    assert_eq!(
        remote_guidance,
        "run child workflow composition with a local execution environment filesystem"
    );

    let child_path = fixture.cwd.join(".codex/workflows/child.js");
    let nested_child = format!(
        "{}\nreturn workflow(\"grandchild\");",
        script("child").lines().next().unwrap()
    );
    tokio::fs::create_dir_all(child_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(child_path, nested_child).await.unwrap();
    let mut nested = empty_input();
    nested.script = Some(parent);
    let nested_error = match resolve_workflow(
        nested,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
        ChildWorkflowPolicy::FreezeLocal,
    )
    .await
    {
        Ok(_) => panic!("nested child composition must fail closed"),
        Err(error) => error,
    };
    let WorkflowResolveError::ChildComposition(nested_guidance) = nested_error else {
        panic!("nested composition should fail with the child-composition category");
    };
    assert_eq!(
        nested_guidance,
        "call child workflow `child` directly from the root workflow"
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
        args: None,
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
