use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;

use super::ContextualUserFragment;

const PREAMBLE_KEY: &str = "workflow_child_0_preamble";
const ISOLATION_KEY_PREFIX: &str = "workflow_child_1_isolation_part_";
const OUTPUT_CONTRACT_KEY_PREFIX: &str = "workflow_child_2_output_contract_part_";
const TASK_KEY_PREFIX: &str = "workflow_child_3_task_part_";
const PART_MARKER_END: &str = ">";
const MAX_PART_BYTES: usize = 768;

pub(crate) fn is_workflow_child_context_key(key: &str) -> bool {
    key == PREAMBLE_KEY
        || key.starts_with(ISOLATION_KEY_PREFIX)
        || key.starts_with(OUTPUT_CONTRACT_KEY_PREFIX)
        || key.starts_with(TASK_KEY_PREFIX)
}

/// Runtime-owned instructions that establish the role of a Workflow child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChildPreamble {
    body: String,
}

impl WorkflowChildPreamble {
    /// Creates the fixed role instructions for one Workflow child.
    pub fn new(body: impl Into<String>) -> Self {
        Self { body: body.into() }
    }

    /// Converts the fragment into Core's stable turn-level context representation.
    pub fn into_additional_context(self) -> (String, AdditionalContextEntry) {
        application_context(PREAMBLE_KEY, self.body)
    }
}

impl ContextualUserFragment for WorkflowChildPreamble {
    fn content_kind(&self) -> ContentItemKind {
        workflow_child_content_kind(PREAMBLE_KEY)
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<workflow_child_0_preamble>",
            "</workflow_child_0_preamble>",
        )
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}

/// Runtime-owned task instructions for a Workflow child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChildTask {
    key: String,
    body: String,
}

impl WorkflowChildTask {
    /// Splits task instructions into ordered context fragments.
    pub fn parts(body: impl Into<String>) -> Vec<Self> {
        numbered_parts(TASK_KEY_PREFIX, body.into())
            .into_iter()
            .map(|(key, body)| Self { key, body })
            .collect()
    }

    /// Converts one ordered fragment into Core's stable turn-level context representation.
    pub fn into_additional_context(self) -> (String, AdditionalContextEntry) {
        additional_context(&self.key, self.body, AdditionalContextKind::Untrusted)
    }

    pub(crate) fn from_additional_context(key: &str, body: &str) -> Option<Self> {
        key.starts_with(TASK_KEY_PREFIX).then(|| Self {
            key: key.to_string(),
            body: body.to_string(),
        })
    }
}

impl ContextualUserFragment for WorkflowChildTask {
    fn content_kind(&self) -> ContentItemKind {
        workflow_child_content_kind(&self.key)
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_child_3_task_part_", PART_MARKER_END)
    }

    fn matches_text(text: &str) -> bool {
        matches_numbered_part(text, TASK_KEY_PREFIX)
    }

    fn body(&self) -> String {
        numbered_part_body(&self.key, &self.body, TASK_KEY_PREFIX)
    }
}

/// Runtime-owned filesystem isolation instructions for a Workflow child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChildIsolation {
    key: String,
    body: String,
}

impl WorkflowChildIsolation {
    /// Splits filesystem isolation instructions into ordered context fragments.
    pub fn parts(body: impl Into<String>) -> Vec<Self> {
        numbered_parts(ISOLATION_KEY_PREFIX, body.into())
            .into_iter()
            .map(|(key, body)| Self { key, body })
            .collect()
    }

    /// Converts one ordered fragment into Core's stable turn-level context representation.
    pub fn into_additional_context(self) -> (String, AdditionalContextEntry) {
        application_context(&self.key, self.body)
    }
}

impl ContextualUserFragment for WorkflowChildIsolation {
    fn content_kind(&self) -> ContentItemKind {
        workflow_child_content_kind(&self.key)
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_child_1_isolation_part_", PART_MARKER_END)
    }

    fn matches_text(text: &str) -> bool {
        matches_numbered_part(text, ISOLATION_KEY_PREFIX)
    }

    fn body(&self) -> String {
        numbered_part_body(&self.key, &self.body, ISOLATION_KEY_PREFIX)
    }
}

/// Runtime-owned structured-output instructions for a Workflow child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChildOutputContract {
    key: String,
    body: String,
}

impl WorkflowChildOutputContract {
    /// Splits a structured-output contract into ordered context fragments.
    pub fn parts(body: impl Into<String>) -> Vec<Self> {
        numbered_parts(OUTPUT_CONTRACT_KEY_PREFIX, body.into())
            .into_iter()
            .map(|(key, body)| Self { key, body })
            .collect()
    }

    /// Converts one ordered fragment into Core's stable turn-level context representation.
    pub fn into_additional_context(self) -> (String, AdditionalContextEntry) {
        application_context(&self.key, self.body)
    }
}

impl ContextualUserFragment for WorkflowChildOutputContract {
    fn content_kind(&self) -> ContentItemKind {
        workflow_child_content_kind(&self.key)
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_child_2_output_contract_part_", PART_MARKER_END)
    }

    fn matches_text(text: &str) -> bool {
        matches_numbered_part(text, OUTPUT_CONTRACT_KEY_PREFIX)
    }

    fn body(&self) -> String {
        numbered_part_body(&self.key, &self.body, OUTPUT_CONTRACT_KEY_PREFIX)
    }
}

fn application_context(key: &str, value: String) -> (String, AdditionalContextEntry) {
    additional_context(key, value, AdditionalContextKind::Application)
}

fn workflow_child_content_kind(key: &str) -> ContentItemKind {
    ContentItemKind(format!("additional_content.{key}"))
}

fn additional_context(
    key: &str,
    value: String,
    kind: AdditionalContextKind,
) -> (String, AdditionalContextEntry) {
    (key.to_string(), AdditionalContextEntry { value, kind })
}

fn numbered_parts(key_prefix: &str, body: String) -> Vec<(String, String)> {
    if body.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < body.len() {
        let mut end = body.len().min(start.saturating_add(MAX_PART_BYTES));
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(body[start..end].to_string());
        start = end;
    }
    let total = chunks.len();
    let number_width = total.to_string().len().max(4);
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, body)| {
            let part = index + 1;
            (
                format!("{key_prefix}{part:0number_width$}_of_{total:0number_width$}"),
                body,
            )
        })
        .collect()
}

fn numbered_part_body(key: &str, body: &str, key_prefix: &str) -> String {
    let part = key
        .strip_prefix(key_prefix)
        .expect("workflow child context keys use their declared prefix");
    format!("{part}>{body}</{key}")
}

fn matches_numbered_part(text: &str, key_prefix: &str) -> bool {
    let text = text.trim();
    let Some(rest) = text.strip_prefix(&format!("<{key_prefix}")) else {
        return false;
    };
    let Some((part, _)) = rest.split_once(PART_MARKER_END) else {
        return false;
    };
    !part.is_empty() && text.ends_with(&format!("</{key_prefix}{part}>"))
}

#[cfg(test)]
#[path = "workflow_child_tests.rs"]
mod tests;
