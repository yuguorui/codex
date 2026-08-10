use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn workflow_source(name: &str, body: &str) -> String {
    format!("export const meta = {{ name: '{name}', description: 'composition test' }};\n{body}")
}

#[test]
fn persisted_composition_rejects_unknown_fields() {
    let error = serde_json::from_value::<PersistedWorkflowComposition>(json!({
        "definitionSha256": "a".repeat(64),
        "children": [],
        "unexpected": true,
    }))
    .unwrap_err();

    assert!(error.to_string().contains("unknown field"));
}

fn persisted_child_with(
    reference: serde_json::Value,
    origin: serde_json::Value,
) -> serde_json::Value {
    json!({
        "definitionSha256": "b".repeat(64),
        "children": [{
            "reference": reference,
            "origin": origin,
            "shadowsExisting": false,
            "artifactFile": "child.js",
            "scriptSha256": "a".repeat(64),
        }],
    })
}

fn assert_unknown_nested_field_is_rejected(value: serde_json::Value) {
    let error = serde_json::from_value::<PersistedWorkflowComposition>(value).unwrap_err();
    assert!(error.to_string().contains("unknown field"));
}

fn test_absolute_child_path() -> String {
    std::env::current_dir()
        .unwrap()
        .join("child.js")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn persisted_name_reference_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "name", "name": "child", "extra": true}),
        json!({"kind": "inline"}),
    ));
}

#[test]
fn persisted_script_path_reference_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "scriptPath", "scriptPath": "child.js", "extra": true}),
        json!({"kind": "file", "path": test_absolute_child_path()}),
    ));
}

#[test]
fn persisted_inline_origin_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "name", "name": "child"}),
        json!({"kind": "inline", "extra": true}),
    ));
}

#[test]
fn persisted_bundled_origin_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "name", "name": "child"}),
        json!({"kind": "bundled", "extra": true}),
    ));
}

#[test]
fn persisted_file_origin_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "scriptPath", "scriptPath": "child.js"}),
        json!({"kind": "file", "path": test_absolute_child_path(), "extra": true}),
    ));
}

#[test]
fn persisted_plugin_origin_rejects_unknown_fields() {
    assert_unknown_nested_field_is_rejected(persisted_child_with(
        json!({"kind": "name", "name": "child"}),
        json!({
            "kind": "plugin",
            "namespace": "example",
            "path": test_absolute_child_path(),
            "extra": true,
        }),
    ));
}

#[test]
fn persisted_nested_variants_accept_their_exact_fields() {
    let child = json!({
        "reference": {"kind": "name", "name": "child"},
        "origin": {"kind": "inline"},
        "shadowsExisting": false,
        "artifactFile": "child.js",
        "scriptSha256": "a".repeat(64),
    });
    serde_json::from_value::<PersistedWorkflowComposition>(json!({
        "definitionSha256": "b".repeat(64),
        "children": [child],
    }))
    .unwrap();
}

struct CompositionFixture {
    _root: tempfile::TempDir,
    cwd: AbsolutePathBuf,
    codex_home: AbsolutePathBuf,
    parent: ValidatedWorkflowScript,
    frozen: FrozenWorkflowComposition,
    children_dir: AbsolutePathBuf,
    persisted: PersistedWorkflowComposition,
}

impl CompositionFixture {
    async fn new(parent_body: &str, children: &[(&str, &str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = AbsolutePathBuf::try_from(root.path().join("project")).unwrap();
        let codex_home = AbsolutePathBuf::try_from(root.path().join("home/.codex")).unwrap();
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&codex_home).await.unwrap();
        for (path, name, body) in children {
            tokio::fs::write(cwd.join(path), workflow_source(name, body))
                .await
                .unwrap();
        }
        let parent = validate_workflow_script(workflow_source("parent", parent_body)).unwrap();
        let frozen = freeze_workflow_composition(
            &parent,
            ChildWorkflowPolicy::FreezeLocal,
            &cwd,
            &codex_home,
            &[],
        )
        .await
        .unwrap();
        let children_dir = cwd.join("persisted-children");
        let persisted = persist_workflow_composition(&frozen, &children_dir)
            .await
            .unwrap();
        Self {
            _root: root,
            cwd,
            codex_home,
            parent,
            frozen,
            children_dir,
            persisted,
        }
    }

    async fn one_path_child(child_body: &str) -> Self {
        Self::new(
            "return workflow({ scriptPath: 'child.js' });",
            &[("child.js", "child", child_body)],
        )
        .await
    }

    async fn restore(
        &self,
        persisted: &PersistedWorkflowComposition,
    ) -> Result<FrozenWorkflowComposition, String> {
        restore_workflow_composition(&self.parent, persisted, &self.children_dir).await
    }

    async fn replace_first_artifact(
        &self,
        persisted: &mut PersistedWorkflowComposition,
        source: &str,
    ) {
        let script_sha256 = sha256(source);
        let artifact_file = artifact_file_name(&script_sha256);
        tokio::fs::write(self.children_dir.join(&artifact_file), source)
            .await
            .unwrap();
        persisted.children[0].script_sha256 = script_sha256;
        persisted.children[0].artifact_file = artifact_file;
    }
}

#[tokio::test]
async fn freezes_and_restores_multiple_children_beyond_one_script_size_in_total() {
    let large_body = format!("/* {} */\nreturn 'done';", "x".repeat(300 * 1024));
    let fixture = CompositionFixture::new(
        "await workflow({ scriptPath: 'child-a.js' });\nreturn workflow({ scriptPath: 'child-b.js' });",
        &[
            ("child-a.js", "child-a", &large_body),
            ("child-b.js", "child-b", &large_body),
        ],
    )
    .await;

    let restored = fixture.restore(&fixture.persisted).await.unwrap();

    assert_eq!(restored.child_count(), 2);
}

#[tokio::test]
async fn persisted_manifest_integrity_is_verified_during_restore() {
    let fixture = CompositionFixture::one_path_child("return 'approved';").await;

    let mut binding_count = fixture.persisted.clone();
    binding_count.children.clear();

    let mut binding_reference = fixture.persisted.clone();
    binding_reference.children[0].reference = WorkflowChildReference::Name {
        name: "different-child".to_string(),
    };

    let mut composition_hash_format = fixture.persisted.clone();
    composition_hash_format.definition_sha256 = "not-a-sha256".to_string();

    let mut artifact_hash_format = fixture.persisted.clone();
    artifact_hash_format.children[0].script_sha256 = "A".repeat(64);

    let mut artifact_path = fixture.persisted.clone();
    artifact_path.children[0].artifact_file =
        format!("../{}", fixture.persisted.children[0].artifact_file);

    let mut incompatible_origin = fixture.persisted.clone();
    incompatible_origin.children[0].origin = WorkflowOrigin::Bundled;

    let mut definition_hash = fixture.persisted.clone();
    definition_hash.definition_sha256 = "0".repeat(64);

    let cases = [
        (
            "binding count",
            binding_count,
            "does not match the analyzed binding count",
        ),
        (
            "binding reference",
            binding_reference,
            "exactly match the statically analyzed workflow references",
        ),
        (
            "composition SHA-256 format",
            composition_hash_format,
            "composition hash is invalid",
        ),
        (
            "artifact SHA-256 format",
            artifact_hash_format,
            "artifact hash is invalid",
        ),
        (
            "artifact path traversal",
            artifact_path,
            "artifact path is invalid",
        ),
        (
            "origin/reference compatibility",
            incompatible_origin,
            "origin is incompatible with its binding",
        ),
        (
            "definition hash",
            definition_hash,
            "composition hash does not match its manifest",
        ),
    ];

    for (case, persisted, expected_error) in cases {
        let error = fixture.restore(&persisted).await.unwrap_err();
        assert!(
            error.contains(expected_error),
            "{case} returned an unexpected error: {error}"
        );
    }
}

#[tokio::test]
async fn duplicate_persisted_bindings_are_rejected() {
    let fixture = CompositionFixture::new(
        "const first = await workflow({ scriptPath: 'child-a.js' });\nreturn [first, await workflow({ scriptPath: 'child-b.js' })];",
        &[
            ("child-a.js", "child-a", "return 'a';"),
            ("child-b.js", "child-b", "return 'b';"),
        ],
    )
    .await;
    let mut persisted = fixture.persisted.clone();
    persisted.children[1].reference = persisted.children[0].reference.clone();

    let error = fixture.restore(&persisted).await.unwrap_err();
    assert!(error.contains("manifest contains duplicate bindings"));
}

#[tokio::test]
async fn persisted_child_artifacts_are_validated_during_restore() {
    let missing = CompositionFixture::one_path_child("return 'approved';").await;
    tokio::fs::remove_file(
        missing
            .children_dir
            .join(&missing.persisted.children[0].artifact_file),
    )
    .await
    .unwrap();
    assert!(missing.restore(&missing.persisted).await.is_err());

    let invalid = CompositionFixture::one_path_child("return 'approved';").await;
    let mut invalid_persisted = invalid.persisted.clone();
    invalid
        .replace_first_artifact(&mut invalid_persisted, "not a workflow script")
        .await;
    let error = invalid.restore(&invalid_persisted).await.unwrap_err();
    assert!(error.contains("first statement must be"));

    let nested = CompositionFixture::one_path_child("return 'approved';").await;
    let mut nested_persisted = nested.persisted.clone();
    nested
        .replace_first_artifact(
            &mut nested_persisted,
            &workflow_source("child", "return workflow({ scriptPath: 'grandchild.js' });"),
        )
        .await;
    let error = nested.restore(&nested_persisted).await.unwrap_err();
    assert!(error.contains("persist child workflow calls only in the root workflow"));
}

#[tokio::test]
async fn persisted_child_artifact_hash_is_verified_during_restore() {
    let fixture = CompositionFixture::one_path_child("return 'approved';").await;
    let restored = fixture.restore(&fixture.persisted).await.unwrap();
    assert_eq!(
        restored.definition_sha256(),
        fixture.frozen.definition_sha256()
    );

    tokio::fs::write(
        fixture
            .children_dir
            .join(&fixture.persisted.children[0].artifact_file),
        workflow_source("child", "return 'tampered';"),
    )
    .await
    .unwrap();
    let error = fixture.restore(&fixture.persisted).await.unwrap_err();
    assert!(error.contains("failed SHA-256 verification"));
}

#[tokio::test]
async fn persistence_atomically_repairs_a_corrupted_child_artifact() {
    let fixture = CompositionFixture::one_path_child("return 'approved';").await;
    let artifact_path = fixture
        .children_dir
        .join(&fixture.persisted.children[0].artifact_file);
    tokio::fs::write(&artifact_path, "corrupted").await.unwrap();

    let persisted = persist_workflow_composition(&fixture.frozen, &fixture.children_dir)
        .await
        .unwrap();

    assert_eq!(persisted, fixture.persisted);
    assert_eq!(
        tokio::fs::read_to_string(artifact_path).await.unwrap(),
        fixture
            .frozen
            .children
            .values()
            .next()
            .unwrap()
            .script
            .source
    );
}

#[tokio::test]
async fn restored_resolver_uses_frozen_source_after_original_file_changes() {
    let approved_source = workflow_source("child", "return 'approved';");
    let fixture = CompositionFixture::one_path_child("return 'approved';").await;
    tokio::fs::write(
        fixture.cwd.join("child.js"),
        workflow_source("child", "return 'modified after persistence';"),
    )
    .await
    .unwrap();

    let restored = fixture.restore(&fixture.persisted).await.unwrap();
    let args = json!({ "request": "preserved" });
    let child = restored
        .resolver()
        .unwrap()
        .resolve_child(WorkflowChildRequest {
            name_or_ref: json!({ "scriptPath": "child.js" }),
            args: args.clone(),
        })
        .await
        .unwrap();

    assert_eq!((child.script.source, child.args), (approved_source, args));
}

#[tokio::test]
async fn composition_identity_changes_when_a_frozen_child_changes() {
    let fixture = CompositionFixture::one_path_child("return 'first';").await;
    tokio::fs::write(
        fixture.cwd.join("child.js"),
        workflow_source("child", "return 'second';"),
    )
    .await
    .unwrap();
    let changed = freeze_workflow_composition(
        &fixture.parent,
        ChildWorkflowPolicy::FreezeLocal,
        &fixture.cwd,
        &fixture.codex_home,
        &[],
    )
    .await
    .unwrap();

    assert_ne!(
        fixture.frozen.definition_sha256(),
        changed.definition_sha256()
    );
}
