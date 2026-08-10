use std::collections::BTreeMap;

use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use crate::context::WorkflowChildTask;
use crate::context::is_workflow_child_context_key;
use codex_protocol::models::ResponseItem;
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
    ) -> Vec<ResponseItem> {
        let fragments = values
            .iter()
            .filter(|(key, value)| self.values.get(*key) != Some(*value))
            .map(|(key, entry)| match entry.kind {
                AdditionalContextKind::Untrusted => workflow_child_task(key, entry).map_or_else(
                    || {
                        ContextualUserFragment::into(AdditionalContextUserFragment::new(
                            key.clone(),
                            entry.value.clone(),
                        ))
                    },
                    ContextualUserFragment::into,
                ),
                AdditionalContextKind::Application => ContextualUserFragment::into(
                    AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone()),
                ),
            })
            .collect();
        self.values = values;
        fragments
    }

    /// Rebuilds stable Workflow child context after compaction replaces the thread history.
    pub(crate) fn retained_workflow_child_context(&self) -> Vec<ResponseItem> {
        self.values
            .iter()
            .filter(|(key, _)| is_workflow_child_context_key(key))
            .map(|(key, entry)| match entry.kind {
                AdditionalContextKind::Untrusted => workflow_child_task(key, entry)
                    .map(ContextualUserFragment::into)
                    .unwrap_or_else(|| {
                        ContextualUserFragment::into(AdditionalContextUserFragment::new(
                            key.clone(),
                            entry.value.clone(),
                        ))
                    }),
                AdditionalContextKind::Application => ContextualUserFragment::into(
                    AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone()),
                ),
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
