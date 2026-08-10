use std::collections::BTreeMap;

use crate::context::WorkflowChildIsolation;
use crate::context::WorkflowChildOutputContract;
use crate::context::WorkflowChildPreamble;
use crate::context::WorkflowChildTask;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn retained_workflow_child_context_matches_initial_insertion() {
    let task = format!("start:{}:end", "界".repeat(300));
    let mut context =
        BTreeMap::from([WorkflowChildPreamble::new("workflow preamble").into_additional_context()]);
    for fragment in WorkflowChildIsolation::parts("workflow isolation") {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildOutputContract::parts("workflow output contract") {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    for fragment in WorkflowChildTask::parts(task) {
        let (key, entry) = fragment.into_additional_context();
        context.insert(key, entry);
    }
    let mut store = AdditionalContextStore::default();

    let initially_inserted = store.merge(context);
    let retained = store.retained_workflow_child_context();

    assert_eq!(retained, initially_inserted);
}
