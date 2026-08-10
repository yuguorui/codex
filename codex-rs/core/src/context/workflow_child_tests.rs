use codex_context_fragments::AdditionalContextDeveloperFragment;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AdditionalContextKind;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

use super::*;

#[test]
fn workflow_child_fragments_match_their_additional_context_representation() {
    let preamble = WorkflowChildPreamble::new("preamble");
    assert_context_representation(
        preamble.clone(),
        WorkflowChildPreamble::into_additional_context(preamble),
    );
    for task in WorkflowChildTask::parts("task") {
        assert!(WorkflowChildTask::matches_text(&task.render()));
        assert_eq!(task.role(), "user");
        assert!(task.requires_separate_message());
        let expected: ResponseItem = ContextualUserFragment::into(task.clone());
        let (key, entry) = task.into_additional_context();
        assert_eq!(entry.kind, AdditionalContextKind::Untrusted);
        let actual = WorkflowChildTask::from_additional_context(&key, &entry.value)
            .map(ContextualUserFragment::into)
            .expect("Workflow task context should round-trip");
        assert_eq!(actual, expected);
    }
    for isolation in WorkflowChildIsolation::parts("isolation") {
        assert!(WorkflowChildIsolation::matches_text(&isolation.render()));
        assert_context_representation(
            isolation.clone(),
            WorkflowChildIsolation::into_additional_context(isolation),
        );
    }
    for contract in WorkflowChildOutputContract::parts("contract") {
        assert!(WorkflowChildOutputContract::matches_text(
            &contract.render()
        ));
        assert_context_representation(
            contract.clone(),
            WorkflowChildOutputContract::into_additional_context(contract),
        );
    }
}

#[test]
fn workflow_child_task_keys_preserve_btree_order_above_9999_parts() {
    const PART_COUNT: usize = 10_001;
    let mut body = String::with_capacity(MAX_PART_BYTES * PART_COUNT);
    for index in 0..PART_COUNT {
        let marker = format!("{index:05}:");
        body.push_str(&marker);
        body.push_str(&"x".repeat(MAX_PART_BYTES - marker.len()));
    }

    let ordered = WorkflowChildTask::parts(body.clone())
        .into_iter()
        .map(|part| (part.key, part.body))
        .collect::<BTreeMap<_, _>>();
    let reconstructed = ordered.values().cloned().collect::<String>();

    assert_eq!(ordered.len(), PART_COUNT);
    assert_eq!(reconstructed, body);
    assert_eq!(
        ordered.first_key_value().map(|(key, _)| key.as_str()),
        Some("workflow_child_3_task_part_00001_of_10001")
    );
    assert_eq!(
        ordered.last_key_value().map(|(key, _)| key.as_str()),
        Some("workflow_child_3_task_part_10001_of_10001")
    );
}

#[test]
fn workflow_child_context_parts_preserve_complete_utf8_content() {
    let body = "界".repeat(MAX_PART_BYTES);
    let parts = WorkflowChildTask::parts(body.clone());
    let reconstructed = parts
        .iter()
        .map(|part| part.body.as_str())
        .collect::<String>();

    assert_eq!(reconstructed, body);
    assert!(parts.iter().all(|part| part.body.len() <= MAX_PART_BYTES));
}

fn assert_context_representation(
    fragment: impl ContextualUserFragment,
    (key, entry): (String, AdditionalContextEntry),
) {
    assert_eq!(fragment.role(), "developer");
    assert!(fragment.requires_separate_message());
    let expected: ResponseItem = ContextualUserFragment::into(fragment);
    let actual =
        ContextualUserFragment::into(AdditionalContextDeveloperFragment::new(key, entry.value));

    assert_eq!(actual, expected);
}
