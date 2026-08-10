use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

const WORKFLOW_INPUT_RECURSION_GUARD: usize = 256;
const MAX_JAVASCRIPT_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
const MIN_JAVASCRIPT_UNSAFE_INTEGER: f64 = (1_u64 << 53) as f64;
const EXACT_INTEGER_GUIDANCE: &str =
    "represent exact integer identifiers as strings so JavaScript preserves their precision";
const INPUT_RECURSION_GUIDANCE: &str = "use a flatter structured value for workflow inputs";

pub type WorkflowInputArtifactFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowInputArtifactKind {
    #[default]
    Value,
    Descriptor,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowInputArtifactRef {
    pub sha256: String,
    pub kind: WorkflowInputArtifactKind,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WorkflowInputPathSegment {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowInputArtifactLocation {
    pub path: Vec<WorkflowInputPathSegment>,
    pub reference: WorkflowInputArtifactRef,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowInputDescriptor {
    pub value: JsonValue,
    #[serde(default)]
    pub artifacts: Vec<WorkflowInputArtifactLocation>,
    #[serde(default)]
    pub negative_zeros: Vec<Vec<WorkflowInputPathSegment>>,
}

/// Stores immutable workflow input values by their canonical content hash.
///
/// Implementations must verify content when loading a reference. A successful `put` must make the
/// artifact durable enough for a journal entry that subsequently records the returned hash.
pub trait WorkflowInputArtifactStore: Send + Sync {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef>;

    fn put_descriptor(
        &self,
        descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef>;

    fn get<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>>;

    fn get_descriptor<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedWorkflowInputs {
    references: BTreeMap<String, WorkflowInputArtifactRef>,
    values: HashMap<String, Arc<JsonValue>>,
    descriptors: HashMap<String, Arc<WorkflowInputDescriptor>>,
}

impl ResolvedWorkflowInputs {
    pub fn from_values(values: BTreeMap<String, Arc<JsonValue>>) -> Result<Self, String> {
        let mut references = BTreeMap::new();
        let mut artifacts = HashMap::new();
        for (alias, value) in values {
            let reference = workflow_input_artifact_ref(&value)?;
            artifacts.entry(reference.sha256.clone()).or_insert(value);
            references.insert(alias, reference);
        }
        validate_aliases(&references)?;
        Ok(Self {
            references,
            values: artifacts,
            descriptors: HashMap::new(),
        })
    }

    pub fn references(&self) -> &BTreeMap<String, WorkflowInputArtifactRef> {
        &self.references
    }

    pub fn value(&self, reference: &WorkflowInputArtifactRef) -> Option<&Arc<JsonValue>> {
        (reference.kind == WorkflowInputArtifactKind::Value)
            .then(|| self.values.get(&reference.sha256))
            .flatten()
    }

    pub fn descriptor(
        &self,
        reference: &WorkflowInputArtifactRef,
    ) -> Option<&Arc<WorkflowInputDescriptor>> {
        (reference.kind == WorkflowInputArtifactKind::Descriptor)
            .then(|| self.descriptors.get(&reference.sha256))
            .flatten()
    }
}

#[derive(Clone)]
pub struct WorkflowAgentInputs {
    references: BTreeMap<String, WorkflowInputArtifactRef>,
    store: Arc<dyn WorkflowInputArtifactStore>,
}

impl WorkflowAgentInputs {
    pub fn new(
        references: BTreeMap<String, WorkflowInputArtifactRef>,
        store: Arc<dyn WorkflowInputArtifactStore>,
    ) -> Self {
        Self { references, store }
    }

    pub fn references(&self) -> &BTreeMap<String, WorkflowInputArtifactRef> {
        &self.references
    }

    /// Loads each distinct artifact once while preserving descriptors for V8 expansion.
    pub async fn resolve_shared(&self) -> Result<ResolvedWorkflowInputs, String> {
        validate_aliases(&self.references)?;
        let mut values = HashMap::new();
        let mut descriptors = HashMap::new();
        let mut pending = self
            .references
            .values()
            .cloned()
            .map(|reference| (reference, 1_usize))
            .collect::<Vec<_>>();
        while let Some((reference, depth)) = pending.pop() {
            if depth > WORKFLOW_INPUT_RECURSION_GUARD {
                return Err(INPUT_RECURSION_GUIDANCE.to_string());
            }
            validate_artifact_sha256(&reference.sha256)?;
            match reference.kind {
                WorkflowInputArtifactKind::Value => {
                    if values.contains_key(&reference.sha256) {
                        continue;
                    }
                    let value = self.store.get(&reference).await?;
                    validate_workflow_input_value(&value, "workflow agent inputs")?;
                    values.insert(reference.sha256, value);
                }
                WorkflowInputArtifactKind::Descriptor => {
                    if descriptors.contains_key(&reference.sha256) {
                        continue;
                    }
                    let descriptor = self.store.get_descriptor(&reference).await?;
                    validate_workflow_input_descriptor(&descriptor)?;
                    pending.extend(
                        descriptor
                            .artifacts
                            .iter()
                            .map(|artifact| (artifact.reference.clone(), depth + 1)),
                    );
                    descriptors.insert(reference.sha256, descriptor);
                }
            }
        }
        Ok(ResolvedWorkflowInputs {
            references: self.references.clone(),
            values,
            descriptors,
        })
    }
}

impl fmt::Debug for WorkflowAgentInputs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowAgentInputs")
            .field("references", &self.references)
            .finish_non_exhaustive()
    }
}

impl PartialEq for WorkflowAgentInputs {
    fn eq(&self, other: &Self) -> bool {
        self.references == other.references
    }
}

#[derive(Default)]
pub struct MemoryWorkflowInputArtifactStore {
    values: Mutex<HashMap<String, Arc<JsonValue>>>,
    descriptors: Mutex<HashMap<String, Arc<WorkflowInputDescriptor>>>,
}

impl WorkflowInputArtifactStore for MemoryWorkflowInputArtifactStore {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move {
            let reference = workflow_input_artifact_ref(&value)?;
            self.values
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(reference.sha256.clone())
                .or_insert_with(|| Arc::new(value));
            Ok(reference)
        })
    }

    fn put_descriptor(
        &self,
        descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move {
            let reference = workflow_input_descriptor_ref(&descriptor)?;
            self.descriptors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(reference.sha256.clone())
                .or_insert_with(|| Arc::new(descriptor));
            Ok(reference)
        })
    }

    fn get<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>> {
        let sha256 = reference.sha256.clone();
        Box::pin(async move {
            self.values
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&sha256)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "restore workflow input artifact {sha256} in the configured store and retry"
                    )
                })
        })
    }

    fn get_descriptor<'a>(
        &'a self,
        reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>> {
        let sha256 = reference.sha256.clone();
        Box::pin(async move {
            self.descriptors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&sha256)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "restore workflow input descriptor {sha256} in the configured store and retry"
                    )
                })
        })
    }
}

pub async fn store_workflow_input_descriptor(
    descriptor: WorkflowInputDescriptor,
    store: &Arc<dyn WorkflowInputArtifactStore>,
) -> Result<WorkflowInputArtifactRef, String> {
    validate_workflow_input_descriptor(&descriptor)?;
    store.put_descriptor(descriptor).await
}

fn validate_workflow_input_descriptor(descriptor: &WorkflowInputDescriptor) -> Result<(), String> {
    validate_workflow_input_value(&descriptor.value, "workflow agent inputs")?;

    let mut paths = HashSet::with_capacity(descriptor.artifacts.len());
    for artifact in &descriptor.artifacts {
        validate_artifact_sha256(&artifact.reference.sha256)?;
        if !paths.insert(artifact.path.clone()) {
            return Err("provide each workflow input artifact at one distinct path".to_string());
        }

        workflow_input_descriptor_value(&descriptor.value, &artifact.path)?;
    }
    for path in &paths {
        if (0..path.len()).any(|length| paths.contains(&path[..length])) {
            return Err("provide non-overlapping workflow input artifact paths".to_string());
        }
    }
    let mut negative_zero_paths = HashSet::with_capacity(descriptor.negative_zeros.len());
    for path in &descriptor.negative_zeros {
        if !negative_zero_paths.insert(path) {
            return Err("provide each workflow negative zero at one distinct path".to_string());
        }
        let value = workflow_input_descriptor_value(&descriptor.value, path)?;
        if !value.as_f64().is_some_and(|value| value == 0.0) {
            return Err(
                "record workflow negative zero paths only for numeric zero values".to_string(),
            );
        }
        if (0..=path.len()).any(|length| paths.contains(&path[..length])) {
            return Err(
                "provide non-overlapping workflow input artifact and negative zero paths"
                    .to_string(),
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_workflow_input_value(
    value: &JsonValue,
    boundary: &str,
) -> Result<(), String> {
    validate_workflow_input_recursion(value)?;
    validate_v8_lossless_json_numbers(value, boundary)
}

fn validate_workflow_input_recursion(value: &JsonValue) -> Result<(), String> {
    let mut stack = vec![(value, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > WORKFLOW_INPUT_RECURSION_GUARD {
            return Err(INPUT_RECURSION_GUIDANCE.to_string());
        }
        match value {
            JsonValue::Array(items) => {
                stack.extend(items.iter().map(|item| (item, depth + 1)));
            }
            JsonValue::Object(entries) => {
                stack.extend(entries.values().map(|value| (value, depth + 1)));
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
        }
    }
    Ok(())
}

pub fn workflow_input_artifact_ref(value: &JsonValue) -> Result<WorkflowInputArtifactRef, String> {
    validate_workflow_input_recursion(value)?;
    validate_v8_lossless_json_numbers(value, "workflow agent inputs")?;
    let mut digest = Sha256::new();
    let mut writer = DigestWriter(&mut digest);
    write_canonical_value(&mut writer, value, 1)?;
    Ok(WorkflowInputArtifactRef {
        sha256: format!("{:x}", digest.finalize()),
        kind: WorkflowInputArtifactKind::Value,
    })
}

pub fn workflow_input_descriptor_ref(
    descriptor: &WorkflowInputDescriptor,
) -> Result<WorkflowInputArtifactRef, String> {
    let value = serde_json::to_value(descriptor).map_err(|error| error.to_string())?;
    let mut reference = workflow_input_artifact_ref(&value)?;
    reference.kind = WorkflowInputArtifactKind::Descriptor;
    Ok(reference)
}

pub fn canonical_workflow_input_bytes(value: &JsonValue) -> Result<Vec<u8>, String> {
    validate_workflow_input_recursion(value)?;
    validate_v8_lossless_json_numbers(value, "workflow agent inputs")?;
    let mut bytes = Vec::new();
    write_canonical_value(&mut bytes, value, 1)?;
    Ok(bytes)
}

pub fn validate_v8_lossless_json_numbers(value: &JsonValue, boundary: &str) -> Result<(), String> {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            JsonValue::Array(values) => stack.extend(values),
            JsonValue::Object(values) => stack.extend(values.values()),
            JsonValue::Number(number) => {
                let encoded = number.to_string();
                let integer_is_safe = number
                    .as_i64()
                    .is_some_and(|integer| integer.unsigned_abs() <= MAX_JAVASCRIPT_SAFE_INTEGER)
                    || number
                        .as_u64()
                        .is_some_and(|integer| integer <= MAX_JAVASCRIPT_SAFE_INTEGER);
                if integer_is_safe {
                    let Some(number) = number.as_f64() else {
                        return Err(format!("{boundary}: {EXACT_INTEGER_GUIDANCE}"));
                    };
                    if !javascript_number_round_trips(&encoded, number) {
                        return Err(format!("{boundary}: {EXACT_INTEGER_GUIDANCE}"));
                    }
                    continue;
                }

                if number.as_i64().is_none()
                    && number.as_u64().is_none()
                    && !encoded.contains('.')
                    && !encoded.contains('e')
                    && !encoded.contains('E')
                {
                    return Err(format!("{boundary}: {EXACT_INTEGER_GUIDANCE}"));
                }

                let Some(number) = number.as_f64() else {
                    return Err(format!("{boundary}: {EXACT_INTEGER_GUIDANCE}"));
                };
                if !number.is_finite()
                    || (number.fract() == 0.0 && number.abs() >= MIN_JAVASCRIPT_UNSAFE_INTEGER)
                    || !javascript_number_round_trips(&encoded, number)
                {
                    return Err(format!("{boundary}: {EXACT_INTEGER_GUIDANCE}"));
                }
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::String(_) => {}
        }
    }
    Ok(())
}

fn javascript_number_round_trips(encoded: &str, number: f64) -> bool {
    let Some(round_trip) = serde_json::Number::from_f64(number) else {
        return false;
    };
    canonical_decimal(encoded) == canonical_decimal(&round_trip.to_string())
}

fn canonical_decimal(encoded: &str) -> Option<(bool, String, i64)> {
    let (negative, unsigned) = match encoded.as_bytes().first() {
        Some(b'-') => (true, &encoded[1..]),
        Some(b'+') => (false, &encoded[1..]),
        Some(_) => (false, encoded),
        None => return None,
    };
    let (mantissa, explicit_exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (unsigned, 0),
    };
    let mut digits = String::with_capacity(mantissa.len());
    let mut fractional_digits = 0_i64;
    let mut decimal_seen = false;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => {
                digits.push(char::from(byte));
                fractional_digits += i64::from(decimal_seen);
            }
            b'.' if !decimal_seen => decimal_seen = true,
            _ => return None,
        }
    }
    if digits.is_empty() {
        return None;
    }
    let first_nonzero = digits.find(|digit| digit != '0');
    let Some(first_nonzero) = first_nonzero else {
        return Some((negative, "0".to_string(), 0));
    };
    digits.drain(..first_nonzero);
    let trailing_zeros = digits.len() - digits.trim_end_matches('0').len();
    digits.truncate(digits.len() - trailing_zeros);
    let exponent = explicit_exponent
        .checked_sub(fractional_digits)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    Some((negative, digits, exponent))
}

pub(crate) fn workflow_agent_inputs_sha256(
    inputs: Option<&BTreeMap<String, WorkflowInputArtifactRef>>,
) -> Result<Option<String>, String> {
    let Some(inputs) = inputs else {
        return Ok(None);
    };
    validate_aliases(inputs)?;

    let mut digest = Sha256::new();
    for (alias, reference) in inputs {
        validate_artifact_sha256(&reference.sha256)?;
        let alias_len = u64::try_from(alias.len()).map_err(|error| error.to_string())?;
        digest.update(alias_len.to_be_bytes());
        digest.update(alias.as_bytes());
        digest.update(reference.sha256.as_bytes());
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

pub fn validate_artifact_sha256(sha256: &str) -> Result<(), String> {
    if sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("provide a SHA-256 workflow input artifact reference".to_string())
    }
}

fn validate_aliases(inputs: &BTreeMap<String, WorkflowInputArtifactRef>) -> Result<(), String> {
    if inputs.is_empty() {
        return Err(
            "provide at least one named structured value through agent(..., { inputs })"
                .to_string(),
        );
    }
    for alias in inputs.keys() {
        if alias.trim().is_empty() {
            return Err("provide a short name for every workflow agent input".to_string());
        }
    }
    Ok(())
}

fn workflow_input_descriptor_value<'a>(
    mut value: &'a JsonValue,
    path: &[WorkflowInputPathSegment],
) -> Result<&'a JsonValue, String> {
    for segment in path {
        value = match (segment, value) {
            (WorkflowInputPathSegment::Key(key), JsonValue::Object(entries)) => entries.get(key),
            (WorkflowInputPathSegment::Index(index), JsonValue::Array(items)) => items.get(*index),
            (WorkflowInputPathSegment::Key(_), _) | (WorkflowInputPathSegment::Index(_), _) => None,
        }
        .ok_or_else(|| {
            "provide workflow input artifact paths that identify values in the descriptor"
                .to_string()
        })?;
    }
    Ok(value)
}

struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_canonical_value(
    writer: &mut impl Write,
    value: &JsonValue,
    depth: usize,
) -> Result<(), String> {
    if depth > WORKFLOW_INPUT_RECURSION_GUARD {
        return Err(INPUT_RECURSION_GUIDANCE.to_string());
    }

    match value {
        JsonValue::Array(values) => {
            writer.write_all(b"[").map_err(canonical_write_error)?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",").map_err(canonical_write_error)?;
                }
                write_canonical_value(writer, value, depth + 1)?;
            }
            writer.write_all(b"]").map_err(canonical_write_error)?;
        }
        JsonValue::Object(values) => {
            writer.write_all(b"{").map_err(canonical_write_error)?;
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(left, _)| *left);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",").map_err(canonical_write_error)?;
                }
                serde_json::to_writer(&mut *writer, key).map_err(|error| {
                    format!("failed to canonicalize workflow agent input: {error}")
                })?;
                writer.write_all(b":").map_err(canonical_write_error)?;
                write_canonical_value(writer, value, depth + 1)?;
            }
            writer.write_all(b"}").map_err(canonical_write_error)?;
        }
        value => serde_json::to_writer(&mut *writer, value)
            .map_err(|error| format!("failed to canonicalize workflow agent input: {error}"))?,
    }
    Ok(())
}

fn canonical_write_error(error: std::io::Error) -> String {
    format!("failed to canonicalize workflow agent input: {error}")
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
