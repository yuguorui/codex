use std::collections::BTreeMap;

use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use crate::context::WorkflowChildTask;
use crate::context::is_workflow_child_context_key;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: BTreeMap<String, AdditionalContextEntry>,
}

impl AdditionalContextStore {
    pub(crate) fn merge(
        &mut self,
        values: BTreeMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        let fragments = values
            .iter()
            .filter(|(key, value)| self.values.get(*key) != Some(*value))
            .map(|(key, entry)| match entry.kind {
                AdditionalContextKind::Untrusted => workflow_child_task(key, entry).map_or_else(
                    || {
                        AdditionalContextUserFragment::new(key.clone(), entry.value.clone())
                            .into_response_input_item()
                    },
                    ContextualUserFragment::into_response_input_item,
                ),
                AdditionalContextKind::Application => {
                    AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
                        .into_response_input_item()
                }
            })
            .collect();
        self.values = values;
        fragments
    }

    /// Rebuilds stable Workflow child context after compaction replaces the thread history.
    pub(crate) fn retained_workflow_child_context(&self) -> Vec<ResponseInputItem> {
        self.values
            .iter()
            .filter(|(key, _)| is_workflow_child_context_key(key))
            .map(|(key, entry)| match entry.kind {
                AdditionalContextKind::Untrusted => workflow_child_task(key, entry)
                    .map(ContextualUserFragment::into_response_input_item)
                    .unwrap_or_else(|| {
                        AdditionalContextUserFragment::new(key.clone(), entry.value.clone())
                            .into_response_input_item()
                    }),
                AdditionalContextKind::Application => {
                    AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
                        .into_response_input_item()
                }
            })
            .collect()
    }
}

fn workflow_child_task(key: &str, entry: &AdditionalContextEntry) -> Option<WorkflowChildTask> {
    WorkflowChildTask::from_additional_context(key, &entry.value)
}

#[cfg(test)]
#[path = "additional_context_tests.rs"]
mod tests;
