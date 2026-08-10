use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;

use super::*;

#[test]
fn canonical_bytes_are_stable_for_nested_object_order() {
    let left = json!({"report": {"b": 2, "a": 1}});
    let right = json!({"report": {"a": 1, "b": 2}});

    assert_eq!(
        canonical_workflow_input_bytes(&left).unwrap(),
        canonical_workflow_input_bytes(&right).unwrap(),
    );
    assert_eq!(
        workflow_input_artifact_ref(&left).unwrap(),
        workflow_input_artifact_ref(&right).unwrap(),
    );
}

#[test]
fn agent_input_hash_tracks_aliases_and_artifact_content() {
    let report = workflow_input_artifact_ref(&json!([1, 2, 3])).unwrap();
    let changed = workflow_input_artifact_ref(&json!([1, 2, 4])).unwrap();
    let baseline = BTreeMap::from([("report".to_string(), report.clone())]);
    let changed_alias = BTreeMap::from([("other".to_string(), report)]);
    let changed_value = BTreeMap::from([("report".to_string(), changed)]);

    let baseline_hash = workflow_agent_inputs_sha256(Some(&baseline)).unwrap();
    assert_ne!(
        baseline_hash,
        workflow_agent_inputs_sha256(Some(&changed_alias)).unwrap()
    );
    assert_ne!(
        baseline_hash,
        workflow_agent_inputs_sha256(Some(&changed_value)).unwrap()
    );
}

#[test]
fn long_and_many_aliases_only_require_non_empty_names() {
    let reference = workflow_input_artifact_ref(&json!(1)).unwrap();
    let empty = BTreeMap::new();
    let unnamed = BTreeMap::from([(String::new(), reference.clone())]);
    let long = BTreeMap::from([("x".repeat(1024), reference.clone())]);
    let many = (0..256)
        .map(|index| (format!("input-{index}"), reference.clone()))
        .collect::<BTreeMap<_, _>>();

    assert!(
        workflow_agent_inputs_sha256(Some(&empty))
            .unwrap_err()
            .contains("provide at least one named structured value")
    );
    assert!(
        workflow_agent_inputs_sha256(Some(&unnamed))
            .unwrap_err()
            .contains("provide a short name")
    );
    assert!(workflow_agent_inputs_sha256(Some(&long)).is_ok());
    assert!(workflow_agent_inputs_sha256(Some(&many)).is_ok());
}

#[test]
fn large_input_values_have_no_aggregate_byte_limit() {
    let value = json!({"report": "x".repeat(2 * 1024 * 1024)});

    let canonical = canonical_workflow_input_bytes(&value).unwrap();

    assert!(canonical.len() > 2 * 1024 * 1024);
    assert_eq!(
        workflow_input_artifact_ref(&value).unwrap().sha256.len(),
        64
    );
}

#[test]
fn wide_inputs_are_accepted_with_a_recursion_guard() {
    let wide = JsonValue::Array(vec![JsonValue::Null; 300_000]);
    assert!(canonical_workflow_input_bytes(&wide).is_ok());

    let mut deep = JsonValue::Null;
    for _ in 0..WORKFLOW_INPUT_RECURSION_GUARD {
        deep = JsonValue::Array(vec![deep]);
    }
    assert!(
        canonical_workflow_input_bytes(&deep)
            .unwrap_err()
            .contains("flatter structured value")
    );
}

#[tokio::test]
async fn descriptors_preserve_nested_artifact_references() {
    let store: Arc<dyn WorkflowInputArtifactStore> =
        Arc::new(MemoryWorkflowInputArtifactStore::default());
    let cached = store.put(json!({"source": "cached"})).await.unwrap();
    let descriptor = WorkflowInputDescriptor {
        value: json!({
            "cached": null,
            "fresh": [true, "value"],
        }),
        artifacts: vec![WorkflowInputArtifactLocation {
            path: vec![WorkflowInputPathSegment::Key("cached".to_string())],
            reference: cached.clone(),
        }],
        negative_zeros: Vec::new(),
    };

    let stored = store_workflow_input_descriptor(descriptor.clone(), &store)
        .await
        .unwrap();

    assert_eq!(
        store.get_descriptor(&stored).await.unwrap().as_ref(),
        &descriptor
    );
}

#[tokio::test]
async fn descriptor_shape_is_checked_before_recursive_resolution() {
    let store: Arc<dyn WorkflowInputArtifactStore> =
        Arc::new(MemoryWorkflowInputArtifactStore::default());
    let mut value = JsonValue::Null;
    for _ in 0..WORKFLOW_INPUT_RECURSION_GUARD {
        value = JsonValue::Array(vec![value]);
    }
    let descriptor = WorkflowInputDescriptor {
        value,
        artifacts: Vec::new(),
        negative_zeros: Vec::new(),
    };

    assert!(
        store_workflow_input_descriptor(descriptor, &store)
            .await
            .unwrap_err()
            .contains("flatter structured value")
    );
}

#[tokio::test]
async fn descriptor_artifact_paths_are_validated_before_resolution() {
    let store: Arc<dyn WorkflowInputArtifactStore> =
        Arc::new(MemoryWorkflowInputArtifactStore::default());
    let cached = store.put(json!({"source": "cached"})).await.unwrap();
    let cases = [
        WorkflowInputDescriptor {
            value: json!({"present": null}),
            artifacts: vec![WorkflowInputArtifactLocation {
                path: vec![WorkflowInputPathSegment::Key("missing".to_string())],
                reference: cached.clone(),
            }],
            negative_zeros: Vec::new(),
        },
        WorkflowInputDescriptor {
            value: json!({"nested": [null]}),
            artifacts: vec![
                WorkflowInputArtifactLocation {
                    path: vec![WorkflowInputPathSegment::Key("nested".to_string())],
                    reference: cached.clone(),
                },
                WorkflowInputArtifactLocation {
                    path: vec![
                        WorkflowInputPathSegment::Key("nested".to_string()),
                        WorkflowInputPathSegment::Index(0),
                    ],
                    reference: cached.clone(),
                },
            ],
            negative_zeros: Vec::new(),
        },
    ];

    for descriptor in cases {
        assert!(
            store_workflow_input_descriptor(descriptor, &store)
                .await
                .is_err()
        );
    }
}

#[test]
fn many_negative_zero_and_artifact_paths_validate_without_pairwise_scanning() {
    let path_count = 10_000;
    let reference = workflow_input_artifact_ref(&json!({"shared": true})).unwrap();
    let descriptor = WorkflowInputDescriptor {
        value: json!({
            "artifacts": vec![JsonValue::Null; path_count],
            "zeros": vec![json!(0); path_count],
        }),
        artifacts: (0..path_count)
            .map(|index| WorkflowInputArtifactLocation {
                path: vec![
                    WorkflowInputPathSegment::Key("artifacts".to_string()),
                    WorkflowInputPathSegment::Index(index),
                ],
                reference: reference.clone(),
            })
            .collect(),
        negative_zeros: (0..path_count)
            .map(|index| {
                vec![
                    WorkflowInputPathSegment::Key("zeros".to_string()),
                    WorkflowInputPathSegment::Index(index),
                ]
            })
            .collect(),
    };

    validate_workflow_input_descriptor(&descriptor).unwrap();
}

#[test]
fn numbers_cross_into_v8_without_integer_precision_loss() {
    for value in [
        json!(-9_007_199_254_740_991_i64),
        json!(9_007_199_254_740_991_u64),
        json!(42),
        json!(0.5),
        json!(-12.25),
        serde_json::from_str("-0").unwrap(),
        serde_json::from_str("-0.0").unwrap(),
        serde_json::from_str("1.25e-3").unwrap(),
        serde_json::from_str("2.5e-20").unwrap(),
        serde_json::from_str("9.007199254740991e15").unwrap(),
    ] {
        validate_v8_lossless_json_numbers(&value, "test input").unwrap();
    }

    for value in [
        json!(-9_007_199_254_740_992_i64),
        json!(9_007_199_254_740_992_u64),
        serde_json::from_str("18446744073709551616").unwrap(),
        serde_json::from_str("-9223372036854775809").unwrap(),
        serde_json::from_str("1e-400").unwrap(),
        serde_json::from_str("9007199254740991.1").unwrap(),
        serde_json::from_str("1.0000000000000000001").unwrap(),
    ] {
        assert!(
            validate_v8_lossless_json_numbers(&value, "test input")
                .unwrap_err()
                .contains("represent exact integer identifiers as strings")
        );
    }
}

struct UnsafeIntegerArtifactStore;

impl WorkflowInputArtifactStore for UnsafeIntegerArtifactStore {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move { workflow_input_artifact_ref(&value) })
    }

    fn put_descriptor(
        &self,
        _descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async { Err("descriptors are unavailable in this test store".to_string()) })
    }

    fn get<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>> {
        Box::pin(async {
            Ok(Arc::new(
                serde_json::from_str("18446744073709551616").unwrap(),
            ))
        })
    }

    fn get_descriptor<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>> {
        Box::pin(async { Err("descriptors are unavailable in this test store".to_string()) })
    }
}

#[tokio::test]
async fn resolved_agent_inputs_are_revalidated_before_v8_injection() {
    let inputs = WorkflowAgentInputs::new(
        BTreeMap::from([(
            "identifier".to_string(),
            WorkflowInputArtifactRef {
                sha256: "a".repeat(64),
                kind: WorkflowInputArtifactKind::Value,
            },
        )]),
        Arc::new(UnsafeIntegerArtifactStore),
    );

    let error = inputs.resolve_shared().await.unwrap_err();

    assert!(error.contains("represent exact integer identifiers as strings"));
}

struct CountingArtifactStore {
    value: Arc<JsonValue>,
    reads: AtomicUsize,
}

impl WorkflowInputArtifactStore for CountingArtifactStore {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move { workflow_input_artifact_ref(&value) })
    }

    fn put_descriptor(
        &self,
        _descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async { Err("descriptors are unavailable in this test store".to_string()) })
    }

    fn get<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::AcqRel);
            Ok(Arc::clone(&self.value))
        })
    }

    fn get_descriptor<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>> {
        Box::pin(async { Err("descriptors are unavailable in this test store".to_string()) })
    }
}

#[tokio::test]
async fn duplicate_aliases_share_one_artifact_materialization() {
    let reference = WorkflowInputArtifactRef {
        sha256: "a".repeat(64),
        kind: WorkflowInputArtifactKind::Value,
    };
    let store = Arc::new(CountingArtifactStore {
        value: Arc::new(json!({"report": [1, 2, 3]})),
        reads: AtomicUsize::new(0),
    });
    let inputs = WorkflowAgentInputs::new(
        BTreeMap::from([
            ("first".to_string(), reference.clone()),
            ("second".to_string(), reference.clone()),
        ]),
        store.clone(),
    );

    let resolved = inputs.resolve_shared().await.unwrap();

    assert_eq!(resolved.values.len(), 1);
    assert_eq!(store.reads.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn large_distinct_artifacts_are_materialized() {
    let store = Arc::new(CountingArtifactStore {
        value: Arc::new(json!("x".repeat(1024 * 1024))),
        reads: AtomicUsize::new(0),
    });
    let inputs = WorkflowAgentInputs::new(
        BTreeMap::from([
            (
                "first".to_string(),
                WorkflowInputArtifactRef {
                    sha256: "a".repeat(64),
                    kind: WorkflowInputArtifactKind::Value,
                },
            ),
            (
                "second".to_string(),
                WorkflowInputArtifactRef {
                    sha256: "b".repeat(64),
                    kind: WorkflowInputArtifactKind::Value,
                },
            ),
        ]),
        store.clone(),
    );

    let resolved = inputs.resolve_shared().await.unwrap();

    assert_eq!(resolved.values.len(), 2);
    assert_eq!(store.reads.load(Ordering::Acquire), 2);
}
