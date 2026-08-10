use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_workflow::ResolvedWorkflowInputs;
use codex_workflow::WorkflowAgentFailure;
use codex_workflow::WorkflowAgentFailureKind;
use codex_workflow::WorkflowAgentFuture;
use codex_workflow::WorkflowAgentRequest;
use codex_workflow::WorkflowAgentResult;
use codex_workflow::WorkflowAgentRuntime;
use codex_workflow::WorkflowControl;
use codex_workflow::WorkflowEffort;
use codex_workflow::WorkflowExecutionError;
use codex_workflow::WorkflowInputArtifactKind;
use codex_workflow::WorkflowInputArtifactRef;
use codex_workflow::WorkflowInputPathSegment;
use codex_workflow::WorkflowRuntimeConfig;
use codex_workflow::execute_workflow;
use codex_workflow::validate_workflow_script;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;

const REVIEW_TARGET: &str = "REVIEW_TARGET_SENTINEL";
const REVIEW_DIFF_COMMAND: &str = "REVIEW_DIFF_COMMAND_SENTINEL";
const REVIEW_FILE: &str = "src/REVIEW_FILE_SENTINEL.rs";
const REVIEW_AGENTS_MD_FILE: &str = "REVIEW_AGENTS_MD_FILE_SENTINEL";
const REVIEW_SCOPE_SUMMARY: &str = "REVIEW_SCOPE_SENTINEL";
const REVIEW_CONVENTIONS: &str = "REVIEW_CONVENTIONS_SENTINEL";
const REVIEW_CANDIDATE: &str = "REVIEW_CANDIDATE_SENTINEL";
const REVIEW_FAILURE_SCENARIO: &str = "REVIEW_FAILURE_SCENARIO_SENTINEL";
const REVIEW_EVIDENCE: &str = "REVIEW_EVIDENCE_SENTINEL";
const RESEARCH_QUESTION: &str = "RESEARCH_QUESTION_SENTINEL";
const RESEARCH_SCOPE_SUMMARY: &str = "RESEARCH_SCOPE_SENTINEL";
const PRIMARY_QUERY: &str = "PRIMARY_QUERY_SENTINEL";
const TECHNICAL_QUERY: &str = "TECHNICAL_QUERY_SENTINEL";
const CONTRARIAN_QUERY: &str = "CONTRARIAN_QUERY_SENTINEL";
const RESEARCH_SOURCE_TITLE: &str = "RESEARCH_SOURCE_TITLE_SENTINEL";
const RESEARCH_SOURCE_SNIPPET: &str = "RESEARCH_SOURCE_SNIPPET_SENTINEL";
const RESEARCH_PUBLISH_DATE: &str = "RESEARCH_PUBLISH_DATE_SENTINEL";
const RESEARCH_CLAIM: &str = "RESEARCH_CLAIM_SENTINEL";
const RESEARCH_QUOTE: &str = "RESEARCH_QUOTE_SENTINEL";
const RESEARCH_VERDICT_EVIDENCE: &str = "RESEARCH_VERDICT_EVIDENCE_SENTINEL";
const RESEARCH_SOURCE_COUNT: usize = 3;
const RESEARCH_CLAIMS_PER_SOURCE: usize = 5;
const RESEARCH_CLAIM_COUNT: usize = RESEARCH_SOURCE_COUNT * RESEARCH_CLAIMS_PER_SOURCE;
const LARGE_FIELD_BYTES: usize = 2 * 1024;
const LARGE_STRUCTURED_INPUT_MIN_BYTES: usize = 16 * 1024;
const REVIEW_LEVELS: [ReviewLevel; 3] = [ReviewLevel::High, ReviewLevel::Xhigh, ReviewLevel::Max];
const FAILURE_KINDS: [WorkflowAgentFailureKind; 4] = [
    WorkflowAgentFailureKind::Failed,
    WorkflowAgentFailureKind::TerminalApi,
    WorkflowAgentFailureKind::Throttled,
    WorkflowAgentFailureKind::Skipped,
];

const PROMPT_SAFE_INPUT_STRINGS: &[&str] = &[
    "angle-A",
    "angle-B",
    "angle-C",
    "angle-D",
    "angle-E",
    "cleanup",
    "correctness",
    "CONFIRMED",
    "high",
    "xhigh",
    "max",
    "primary",
    "secondary",
    "blog",
    "technical",
    "contrarian",
    "central",
];

const DYNAMIC_INPUT_STEMS: &[&str] = &[
    REVIEW_TARGET,
    REVIEW_DIFF_COMMAND,
    "REVIEW_FILE_SENTINEL",
    REVIEW_AGENTS_MD_FILE,
    REVIEW_SCOPE_SUMMARY,
    REVIEW_CONVENTIONS,
    REVIEW_CANDIDATE,
    REVIEW_FAILURE_SCENARIO,
    REVIEW_EVIDENCE,
    RESEARCH_QUESTION,
    RESEARCH_SCOPE_SUMMARY,
    PRIMARY_QUERY,
    TECHNICAL_QUERY,
    CONTRARIAN_QUERY,
    RESEARCH_SOURCE_TITLE,
    RESEARCH_SOURCE_SNIPPET,
    RESEARCH_PUBLISH_DATE,
    RESEARCH_CLAIM,
    RESEARCH_QUOTE,
    RESEARCH_VERDICT_EVIDENCE,
    "research-source-",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewLevel {
    High,
    Xhigh,
    Max,
}

impl ReviewLevel {
    fn name(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn finder_cap(self) -> usize {
        match self {
            Self::High => 6,
            Self::Xhigh | Self::Max => 8,
        }
    }

    fn max_findings(self) -> usize {
        match self {
            Self::High => 10,
            Self::Xhigh | Self::Max => 15,
        }
    }

    fn candidate_count(self) -> usize {
        self.finder_labels().len()
    }

    fn effort(self) -> WorkflowEffort {
        match self {
            Self::High => WorkflowEffort::High,
            Self::Xhigh => WorkflowEffort::Xhigh,
            Self::Max => WorkflowEffort::Max,
        }
    }

    fn has_sweep(self) -> bool {
        self != Self::High
    }

    fn finder_labels(self) -> &'static [&'static str] {
        match self {
            Self::High => &["angle-A", "angle-B", "angle-C", "cleanup"],
            Self::Xhigh | Self::Max => &[
                "angle-A", "angle-B", "angle-C", "angle-D", "angle-E", "cleanup",
            ],
        }
    }
}

#[derive(Clone, Copy)]
enum Fixture {
    CodeReview(ReviewLevel),
    DeepResearch,
}

struct CapturingAgentRuntime {
    fixture: Fixture,
    failure: Option<InjectedFailure>,
    response_override: Option<InjectedResponse>,
    requests: Mutex<Vec<WorkflowAgentRequest>>,
}

#[derive(Clone, Copy)]
struct InjectedFailure {
    label: &'static str,
    kind: WorkflowAgentFailureKind,
}

struct InjectedResponse {
    label: &'static str,
    value: Value,
}

impl CapturingAgentRuntime {
    fn new(fixture: Fixture) -> Self {
        Self {
            fixture,
            failure: None,
            response_override: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn failing(
        fixture: Fixture,
        failed_label: &'static str,
        kind: WorkflowAgentFailureKind,
    ) -> Self {
        Self {
            fixture,
            failure: Some(InjectedFailure {
                label: failed_label,
                kind,
            }),
            response_override: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn returning_null(fixture: Fixture, label: &'static str) -> Self {
        Self::returning_value(fixture, label, Value::Null)
    }

    fn returning_value(fixture: Fixture, label: &'static str, value: Value) -> Self {
        Self {
            fixture,
            failure: None,
            response_override: Some(InjectedResponse { label, value }),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<WorkflowAgentRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn response(&self, request: &WorkflowAgentRequest, inputs: &Value) -> Value {
        match self.fixture {
            Fixture::CodeReview(level) => code_review_response(level, request, inputs),
            Fixture::DeepResearch => deep_research_response(request, inputs),
        }
    }
}

impl WorkflowAgentRuntime for CapturingAgentRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            let resolved = request
                .inputs
                .as_ref()
                .expect("agent inputs")
                .resolve_shared()
                .await
                .expect("resolve agent inputs");
            let inputs = materialize_fixture_inputs(&resolved);
            assert_request_contract(self.fixture, self.failure, &request, &inputs);
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            if self
                .failure
                .is_some_and(|failure| request.options.label.as_deref() == Some(failure.label))
            {
                let kind = self.failure.expect("injected failure").kind;
                return Err(WorkflowAgentFailure {
                    kind,
                    message: "fixture upstream failure".to_string(),
                    usage: Default::default(),
                });
            }
            let value = match self.response_override.as_ref() {
                Some(response) if request.options.label.as_deref() == Some(response.label) => {
                    response.value.clone()
                }
                Some(_) | None => self.response(&request, &inputs),
            };
            Ok(WorkflowAgentResult {
                value,
                usage: Default::default(),
                agent_id: None,
                model: None,
                fallback_model: None,
            })
        })
    }
}

fn materialize_fixture_inputs(inputs: &ResolvedWorkflowInputs) -> Value {
    let mut cache = HashMap::new();
    Value::Object(
        inputs
            .references()
            .iter()
            .map(|(alias, reference)| {
                (
                    alias.clone(),
                    materialize_fixture_artifact(inputs, reference, &mut cache),
                )
            })
            .collect(),
    )
}

fn materialize_fixture_artifact(
    inputs: &ResolvedWorkflowInputs,
    reference: &WorkflowInputArtifactRef,
    cache: &mut HashMap<WorkflowInputArtifactRef, Value>,
) -> Value {
    if let Some(value) = cache.get(reference) {
        return value.clone();
    }
    let mut value = match reference.kind {
        WorkflowInputArtifactKind::Value => inputs
            .value(reference)
            .expect("fixture value artifact")
            .as_ref()
            .clone(),
        WorkflowInputArtifactKind::Descriptor => {
            let descriptor = inputs
                .descriptor(reference)
                .expect("fixture descriptor artifact");
            let mut value = descriptor.value.clone();
            for artifact in &descriptor.artifacts {
                *fixture_value_at_path(&mut value, &artifact.path) =
                    materialize_fixture_artifact(inputs, &artifact.reference, cache);
            }
            for path in &descriptor.negative_zeros {
                *fixture_value_at_path(&mut value, path) =
                    serde_json::from_str("-0.0").expect("negative zero fixture");
            }
            value
        }
    };
    cache.insert(reference.clone(), value.clone());
    value.take()
}

fn fixture_value_at_path<'a>(
    mut value: &'a mut Value,
    path: &[WorkflowInputPathSegment],
) -> &'a mut Value {
    for segment in path {
        value = match (segment, value) {
            (WorkflowInputPathSegment::Key(key), Value::Object(entries)) => {
                entries.get_mut(key).expect("fixture descriptor key")
            }
            (WorkflowInputPathSegment::Index(index), Value::Array(items)) => {
                items.get_mut(*index).expect("fixture descriptor index")
            }
            (WorkflowInputPathSegment::Key(_), _) | (WorkflowInputPathSegment::Index(_), _) => {
                panic!("fixture descriptor path")
            }
        };
    }
    value
}

fn review_scope() -> Value {
    json!({
        "diffCommand": REVIEW_DIFF_COMMAND,
        "files": [REVIEW_FILE],
        "agentsMdFiles": [REVIEW_AGENTS_MD_FILE],
        "summary": REVIEW_SCOPE_SUMMARY,
        "conventions": REVIEW_CONVENTIONS,
    })
}

fn large_dynamic_string(prefix: &str, index: usize) -> String {
    format!("{prefix}_{index}_{}", "x".repeat(LARGE_FIELD_BYTES))
}

fn review_finder(level: ReviewLevel, label: &str) -> Value {
    let (kind, text) = match label {
        "angle-A" => (
            "correctness",
            concat!(
                "### Angle A \u{2014} line-by-line diff scan\n\n",
                "Read every hunk in the diff, line by line. Then read the enclosing function for\n",
                "each hunk \u{2014} bugs in unchanged lines of a touched function are in scope (the PR\n",
                "re-exposes or fails to fix them). For every line ask: what input, state, timing,\n",
                "or platform makes this line wrong? Look for inverted/wrong conditions,\n",
                "off-by-one, null/undefined deref, missing `await`, falsy-zero checks,\n",
                "wrong-variable copy-paste, error swallowed in catch, unescaped regex metachars.\n",
            ),
        ),
        "angle-B" => (
            "correctness",
            concat!(
                "### Angle B \u{2014} removed-behavior auditor\n\n",
                "For every line the diff DELETES or replaces, name the invariant or behavior it\n",
                "enforced, then search the new code for where that invariant is re-established.\n",
                "If you can't find it, that's a candidate: a removed guard, a dropped error\n",
                "path, a narrowed validation, a deleted test that was covering a real case.\n",
            ),
        ),
        "angle-C" => (
            "correctness",
            concat!(
                "### Angle C \u{2014} cross-file tracer\n\n",
                "For each function the diff changes, find its callers (search for the symbol) and\n",
                "check whether the change breaks any call site: a new precondition, a changed\n",
                "return shape, a new exception, a timing/ordering dependency. Also check callees:\n",
                "does a parallel change in the same PR make a call unsafe?\n",
            ),
        ),
        "angle-D" => (
            "correctness",
            concat!(
                "### Angle D \u{2014} language-pitfall specialist\n\n",
                "Scan for the classic pitfalls of the diff's language/framework \u{2014} for example:\n",
                "JS falsy-zero, `==` coercion, closure-captured loop var; Python mutable default\n",
                "args, late-binding closures; Go nil-map write, range-var capture; SQL injection;\n",
                "timezone/DST drift; float equality. Flag any instance the diff introduces.\n",
            ),
        ),
        "angle-E" => (
            "correctness",
            concat!(
                "### Angle E \u{2014} wrapper/proxy correctness\n\n",
                "When the PR adds or modifies a type that wraps another (cache, proxy, decorator,\n",
                "adapter): check that every method routes to the wrapped instance and not back\n",
                "through a registry/session/global \u{2014} e.g. a caching provider holding a\n",
                "`delegate` field that resolves IDs via `session.get(...)` instead of\n",
                "`delegate.get(...)` will re-enter the cache or recurse. Also check that the\n",
                "wrapper forwards all the methods the callers actually use.\n",
            ),
        ),
        "cleanup" => (
            "cleanup",
            concat!(
                "### Reuse\n\n",
                "Flag new code that re-implements something the codebase\n",
                "already has \u{2014} Search shared/utility modules and files adjacent to the change,\n",
                "and name the existing helper to call instead.\n\n\n",
                "### Simplification\n\n",
                "Flag unnecessary complexity the diff adds: redundant or derivable state,\n",
                "copy-paste with slight variation, deep nesting, dead code left behind. Name\n",
                "the simpler form that does the same job.\n\n\n",
                "### Efficiency\n\n",
                "Flag wasted work the diff introduces: redundant computation or repeated I/O,\n",
                "independent operations run sequentially, blocking work added to startup or\n",
                "hot paths. Also flag long-lived objects built from closures or captured\n",
                "environments \u{2014} they keep the entire enclosing scope alive for the object's\n",
                "lifetime (a memory leak when that scope holds large values); prefer a\n",
                "class/struct that copies only the fields it needs. Name the cheaper\n",
                "alternative.\n\n\n",
                "### Altitude\n\n",
                "Check that each change is implemented at the right depth, not as a fragile\n",
                "bandaid. Special cases layered on shared infrastructure are a sign the fix\n",
                "isn't deep enough \u{2014} prefer generalizing the underlying mechanism over adding\n",
                "special cases.\n\n\n",
                "### Conventions (AGENTS.md)\n\n",
                "Find the AGENTS.md files that govern the changed code: the user-level\n",
                "~/.codex/AGENTS.md, the repo-root AGENTS.md, plus any AGENTS.md or\n",
                "AGENTS.override.md in a directory that is an ancestor of a changed file (a\n",
                "directory's AGENTS.md only applies to files at or below it). Read each one\n",
                "that exists, then check the diff for clear violations of the rules they state.\n\n",
                "Only flag a violation when you can quote the exact rule and the exact line\n",
                "that breaks it \u{2014} no style preferences, no vague \"spirit of the doc\"\n",
                "inferences. In the finding, name the AGENTS.md path and quote the rule so the\n",
                "report can cite it. If no AGENTS.md applies, return nothing for this angle.\n",
            ),
        ),
        _ => panic!("unexpected finder label: {label}"),
    };
    let cap = if label == "cleanup" {
        5 * level.finder_cap()
    } else {
        level.finder_cap()
    };
    json!({ "label": label, "kind": kind, "cap": cap, "text": text })
}

fn review_candidate(index: usize, line: usize, kind: &str) -> Value {
    json!({
        "file": REVIEW_FILE,
        "line": line,
        "summary": format!("{REVIEW_CANDIDATE}_{index}"),
        "failure_scenario": large_dynamic_string(REVIEW_FAILURE_SCENARIO, index),
        "kind": kind,
    })
}

fn verified_review_candidate(index: usize, line: usize, kind: &str) -> Value {
    let mut candidate = review_candidate(index, line, kind);
    let object = candidate.as_object_mut().expect("review candidate object");
    object.insert("verdict".to_string(), json!("CONFIRMED"));
    object.insert(
        "evidence".to_string(),
        json!(large_dynamic_string(REVIEW_EVIDENCE, index)),
    );
    candidate
}

fn review_candidate_for_finder(level: ReviewLevel, label: &str) -> Value {
    let index = level
        .finder_labels()
        .iter()
        .position(|candidate_label| *candidate_label == label)
        .expect("finder label should belong to level");
    review_candidate(
        index,
        17 + index,
        if label == "cleanup" {
            "cleanup"
        } else {
            "correctness"
        },
    )
}

fn initial_review_candidates(level: ReviewLevel) -> Vec<Value> {
    level
        .finder_labels()
        .iter()
        .map(|label| review_candidate_for_finder(level, label))
        .collect()
}

fn verified_initial_review_candidates(level: ReviewLevel) -> Vec<Value> {
    initial_review_candidates(level)
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            verified_review_candidate(
                index,
                candidate["line"].as_u64().expect("candidate line") as usize,
                candidate["kind"].as_str().expect("candidate kind"),
            )
        })
        .collect()
}

fn sweep_candidate(level: ReviewLevel) -> Value {
    let index = level.candidate_count();
    review_candidate(index, 17 + index, "correctness")
}

fn verified_sweep_candidate(level: ReviewLevel) -> Value {
    let index = level.candidate_count();
    verified_review_candidate(index, 17 + index, "correctness")
}

fn ranked_review_candidates(level: ReviewLevel) -> Vec<Value> {
    let mut candidates = verified_initial_review_candidates(level);
    if level.has_sweep() {
        candidates.push(verified_sweep_candidate(level));
    }
    candidates.sort_by_key(|candidate| candidate["kind"] == "cleanup");
    candidates
}

fn research_source_url(index: usize) -> String {
    format!("https://research-source-{index}-sentinel.test/document-{index}")
}

fn research_source_host(index: usize) -> String {
    format!("research-source-{index}-sentinel.test")
}

fn research_source(index: usize) -> Value {
    json!({
        "url": research_source_url(index),
        "title": large_dynamic_string(RESEARCH_SOURCE_TITLE, index),
        "snippet": large_dynamic_string(RESEARCH_SOURCE_SNIPPET, index),
        "relevance": "high",
    })
}

fn research_claim(index: usize) -> Value {
    let source_index = index / RESEARCH_CLAIMS_PER_SOURCE;
    json!({
        "claim": format!("{RESEARCH_CLAIM}_{index}"),
        "quote": large_dynamic_string(RESEARCH_QUOTE, index),
        "importance": "central",
        "publishDate": format!("{RESEARCH_PUBLISH_DATE}_{source_index}"),
        "sourceUrl": research_source_url(source_index),
        "sourceQuality": research_source_quality(source_index),
    })
}

fn research_source_quality(index: usize) -> &'static str {
    match index {
        0 => "primary",
        1 => "secondary",
        2 => "blog",
        _ => panic!("unexpected research source index: {index}"),
    }
}

fn verifier_label(voter: usize, claim_index: usize) -> String {
    format!("v{voter}:{RESEARCH_CLAIM}_{claim_index}")
}

fn voted_research_claim(index: usize, failure: Option<InjectedFailure>) -> Value {
    let verdicts = (0..3)
        .filter(|voter| {
            failure.is_none_or(|failure| failure.label != verifier_label(*voter, index))
        })
        .map(|voter| {
            json!({
                "refuted": false,
                "evidence": large_dynamic_string(
                    RESEARCH_VERDICT_EVIDENCE,
                    index * 3 + voter,
                ),
                "confidence": "high",
            })
        })
        .collect::<Vec<_>>();
    let verdict_count = verdicts.len();
    let mut claim = research_claim(index);
    let object = claim.as_object_mut().expect("research claim object");
    object.insert("verdicts".to_string(), json!(verdicts));
    object.insert("refutedVotes".to_string(), json!(0));
    object.insert("erroredVotes".to_string(), json!(3 - verdict_count));
    object.insert("survives".to_string(), json!(true));
    object.insert("isRefuted".to_string(), json!(false));
    claim
}

fn active_research_claim_indices(failure: Option<InjectedFailure>) -> Vec<usize> {
    let failed_source = failure.and_then(|failure| {
        (0..RESEARCH_SOURCE_COUNT).find(|source_index| {
            failure.label == format!("search:{}", research_angle_label(*source_index))
                || failure.label == format!("fetch:{}", research_source_host(*source_index))
        })
    });
    (0..RESEARCH_CLAIM_COUNT)
        .filter(|claim_index| Some(claim_index / RESEARCH_CLAIMS_PER_SOURCE) != failed_source)
        .collect()
}

fn research_angle_label(index: usize) -> &'static str {
    match index {
        0 => "primary",
        1 => "technical",
        2 => "contrarian",
        _ => panic!("unexpected research source index: {index}"),
    }
}

fn collect_input_strings<'a>(value: &'a Value, strings: &mut Vec<&'a str>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_input_strings(value, strings);
            }
        }
        Value::Object(object) => {
            for value in object.values() {
                collect_input_strings(value, strings);
            }
        }
        Value::String(value) => strings.push(value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn normalized_prompt_fragment(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn assert_inputs_are_isolated_from_prompt(request: &WorkflowAgentRequest, inputs: &Value) {
    let normalized_prompt = normalized_prompt_fragment(&request.prompt);
    for stem in DYNAMIC_INPUT_STEMS {
        assert!(
            !request.prompt.contains(stem),
            "agent {:?} prompt contains dynamic sentinel stem {stem:?}",
            request.options.label,
        );
        let normalized_stem = normalized_prompt_fragment(stem);
        assert!(
            !normalized_prompt.contains(&normalized_stem),
            "agent {:?} prompt contains a transformed dynamic sentinel stem {stem:?}",
            request.options.label,
        );
    }

    let mut input_strings = Vec::new();
    collect_input_strings(inputs, &mut input_strings);
    for value in input_strings {
        if PROMPT_SAFE_INPUT_STRINGS.contains(&value) {
            continue;
        }
        assert!(
            !request.prompt.contains(value),
            "agent {:?} prompt contains dynamic input string beginning {:?}",
            request.options.label,
            value.chars().take(80).collect::<String>()
        );
        let preview = value.chars().take(24).collect::<String>();
        if preview.chars().count() == 24 {
            assert!(
                !request.prompt.contains(&preview),
                "agent {:?} prompt contains a dynamic input preview {preview:?}",
                request.options.label,
            );
            let normalized_preview = normalized_prompt_fragment(&preview);
            assert!(
                !normalized_prompt.contains(&normalized_preview),
                "agent {:?} prompt contains a transformed dynamic input preview {preview:?}",
                request.options.label,
            );
        }
    }
}

fn assert_large_structured_inputs(inputs: &Value) {
    let serialized = serde_json::to_vec(inputs).expect("serialize structured inputs");
    assert!(
        serialized.len() > LARGE_STRUCTURED_INPUT_MIN_BYTES,
        "structured synthesis fixture should exercise a large input"
    );
}

fn assert_request_contract(
    fixture: Fixture,
    failure: Option<InjectedFailure>,
    request: &WorkflowAgentRequest,
    inputs: &Value,
) {
    assert!(
        request.prompt.contains("AnalyzeWorkflowInputs"),
        "agent {:?} prompt should direct the agent to AnalyzeWorkflowInputs",
        request.options.label
    );
    assert_inputs_are_isolated_from_prompt(request, inputs);

    match fixture {
        Fixture::CodeReview(level) => {
            assert_code_review_inputs(level, request, inputs);
            if level == ReviewLevel::Max && request.options.phase.as_deref() == Some("Synthesize") {
                assert_large_structured_inputs(inputs);
            }
        }
        Fixture::DeepResearch => {
            assert_deep_research_inputs(request, failure, inputs);
            if request.options.phase.as_deref() == Some("Synthesize") {
                assert_large_structured_inputs(inputs);
            }
        }
    }
}

fn assert_code_review_inputs(level: ReviewLevel, request: &WorkflowAgentRequest, inputs: &Value) {
    let label = request.options.label.as_deref().expect("agent label");
    assert_eq!(request.options.effort, Some(level.effort()));
    let scope = review_scope();
    let (phase, expected) = match label {
        "scope" => ("Scope", json!({ "target": REVIEW_TARGET })),
        "angle-A" | "angle-B" | "angle-C" | "angle-D" | "angle-E" | "cleanup" => (
            "Find",
            json!({
                "scope": scope,
                "target": REVIEW_TARGET,
                "finder": review_finder(level, label),
            }),
        ),
        "sweep" => (
            "Sweep",
            json!({
                "scope": scope,
                "target": REVIEW_TARGET,
                "knownCandidates": verified_initial_review_candidates(level),
                "maxCandidates": 8,
            }),
        ),
        label if label.starts_with("verify:") => (
            "Verify",
            json!({
                "scope": scope,
                "target": REVIEW_TARGET,
                "group": vec![review_candidate_for_verifier_label(level, label)],
            }),
        ),
        "synthesize" => (
            "Synthesize",
            json!({
                "ranked": ranked_review_candidates(level),
                "level": level.name(),
                "maxFindings": level.max_findings(),
            }),
        ),
        _ => panic!("unexpected code-review agent label: {label}"),
    };
    assert_eq!(request.options.phase.as_deref(), Some(phase));
    assert_eq!(inputs, &expected);
}

fn review_candidate_for_verifier_label(level: ReviewLevel, label: &str) -> Value {
    let line = label
        .strip_suffix("(1)")
        .and_then(|label| label.rsplit_once(':'))
        .and_then(|(_, line)| line.parse::<usize>().ok())
        .expect("verifier label should identify one candidate location");
    let index = line
        .checked_sub(17)
        .expect("candidate line should be in range");
    if index == level.candidate_count() && level.has_sweep() {
        sweep_candidate(level)
    } else {
        let finder_label = level
            .finder_labels()
            .get(index)
            .expect("verifier line should identify a finder candidate");
        review_candidate_for_finder(level, finder_label)
    }
}

fn assert_deep_research_inputs(
    request: &WorkflowAgentRequest,
    failure: Option<InjectedFailure>,
    inputs: &Value,
) {
    let label = request.options.label.as_deref().expect("agent label");
    let (phase, expected) = match label {
        "scope" => ("Scope", json!({ "question": RESEARCH_QUESTION })),
        label if label.starts_with("search:") => {
            let angle_label = label.strip_prefix("search:").expect("search label");
            let query = match angle_label {
                "primary" => PRIMARY_QUERY,
                "technical" => TECHNICAL_QUERY,
                "contrarian" => CONTRARIAN_QUERY,
                _ => panic!("unexpected search label: {label}"),
            };
            (
                "Search",
                json!({
                    "question": RESEARCH_QUESTION,
                    "angle": { "label": angle_label, "query": query },
                }),
            )
        }
        label if label.starts_with("fetch:") => (
            "Fetch",
            json!({
                "question": RESEARCH_QUESTION,
                "source": (0..RESEARCH_SOURCE_COUNT)
                    .find(|index| label == format!("fetch:{}", research_source_host(*index)))
                    .map(research_source)
                    .expect("fetch label should identify its source"),
                "angle": (0..RESEARCH_SOURCE_COUNT)
                    .find(|index| label == format!("fetch:{}", research_source_host(*index)))
                    .map(research_angle_label)
                    .expect("fetch label should identify its angle"),
            }),
        ),
        label if label.starts_with('v') => {
            let (voter, claim_index) = label
                .strip_prefix('v')
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(voter, claim)| {
                    Some((
                        voter.parse::<usize>().ok()?,
                        claim
                            .strip_prefix(&format!("{RESEARCH_CLAIM}_"))?
                            .parse::<usize>()
                            .ok()?,
                    ))
                })
                .expect("verifier label should include its voter and claim indices");
            (
                "Verify",
                json!({
                    "question": RESEARCH_QUESTION,
                    "claim": research_claim(claim_index),
                    "voter": voter,
                    "votesPerClaim": 3,
                    "refutationsRequired": 2,
                }),
            )
        }
        "synthesize" => (
            "Synthesize",
            json!({
                "question": RESEARCH_QUESTION,
                "confirmed": active_research_claim_indices(failure)
                    .into_iter()
                    .map(|index| voted_research_claim(index, failure))
                    .collect::<Vec<_>>(),
                "refuted": [],
                "unverified": [],
                "votesPerClaim": 3,
            }),
        ),
        _ => panic!("unexpected deep-research agent label: {label}"),
    };
    assert_eq!(request.options.phase.as_deref(), Some(phase));
    assert_eq!(inputs, &expected);
}

fn review_candidate_index(candidate: &Value) -> usize {
    candidate["summary"]
        .as_str()
        .and_then(|summary| summary.rsplit_once('_'))
        .and_then(|(_, index)| index.parse::<usize>().ok())
        .expect("review candidate summary index")
}

fn code_review_response(
    level: ReviewLevel,
    request: &WorkflowAgentRequest,
    inputs: &Value,
) -> Value {
    let label = request.options.label.as_deref().expect("agent label");
    match label {
        "scope" => json!({
            "diffCommand": REVIEW_DIFF_COMMAND,
            "files": [REVIEW_FILE],
            "agentsMdFiles": [REVIEW_AGENTS_MD_FILE],
            "summary": REVIEW_SCOPE_SUMMARY,
            "conventions": REVIEW_CONVENTIONS,
        }),
        "angle-A" | "angle-B" | "angle-C" | "angle-D" | "angle-E" | "cleanup" => {
            let candidate = review_candidate_for_finder(level, label);
            let index = review_candidate_index(&candidate);
            json!({
                "candidates": [{
                    "file": format!("/repo/{REVIEW_FILE}"),
                    "line": candidate["line"],
                    "summary": format!("{REVIEW_CANDIDATE}_{index}"),
                    "failure_scenario": large_dynamic_string(REVIEW_FAILURE_SCENARIO, index),
                }],
            })
        }
        "sweep" => {
            let index = level.candidate_count();
            json!({
                "candidates": [{
                    "file": format!("/repo/{REVIEW_FILE}"),
                    "line": 17 + index,
                    "summary": format!("{REVIEW_CANDIDATE}_{index}"),
                    "failure_scenario": large_dynamic_string(REVIEW_FAILURE_SCENARIO, index),
                }],
            })
        }
        label if label.starts_with("verify:") => {
            let group = inputs["group"].as_array().expect("verifier group");
            json!({
                "verdicts": group.iter().enumerate().map(|(group_index, candidate)| json!({
                    "index": group_index,
                    "verdict": "CONFIRMED",
                    "evidence": large_dynamic_string(
                        REVIEW_EVIDENCE,
                        review_candidate_index(candidate),
                    ),
                })).collect::<Vec<_>>(),
            })
        }
        "synthesize" => json!({
            "summary": "REVIEW_SYNTHESIS_SENTINEL",
            "decisions": inputs["ranked"]
                .as_array()
                .expect("ranked findings")
                .iter()
                .enumerate()
                .map(|(index, _)| json!({ "index": index, "merge": [] }))
                .collect::<Vec<_>>(),
        }),
        _ => panic!("unexpected code-review agent label: {label}"),
    }
}

fn deep_research_response(request: &WorkflowAgentRequest, inputs: &Value) -> Value {
    let label = request.options.label.as_deref().expect("agent label");
    match label {
        "scope" => json!({
            "question": RESEARCH_QUESTION,
            "summary": RESEARCH_SCOPE_SUMMARY,
            "angles": [
                { "label": "primary", "query": PRIMARY_QUERY },
                { "label": "technical", "query": TECHNICAL_QUERY },
                { "label": "contrarian", "query": CONTRARIAN_QUERY },
            ],
        }),
        label if label.starts_with("search:") => {
            let source_index = (0..RESEARCH_SOURCE_COUNT)
                .find(|index| label == format!("search:{}", research_angle_label(*index)))
                .expect("search label should identify its source");
            json!({ "results": [research_source(source_index)] })
        }
        label if label.starts_with("fetch:") => {
            let source_index = (0..RESEARCH_SOURCE_COUNT)
                .find(|index| label == format!("fetch:{}", research_source_host(*index)))
                .expect("fetch label should identify its source");
            let first_claim = source_index * RESEARCH_CLAIMS_PER_SOURCE;
            json!({
                "sourceQuality": research_source_quality(source_index),
                "publishDate": format!("{RESEARCH_PUBLISH_DATE}_{source_index}"),
                "claims": (first_claim..first_claim + RESEARCH_CLAIMS_PER_SOURCE).map(|index| json!({
                "claim": format!("{RESEARCH_CLAIM}_{index}"),
                "quote": large_dynamic_string(RESEARCH_QUOTE, index),
                "importance": "central",
                "publishDate": format!("{RESEARCH_PUBLISH_DATE}_{source_index}"),
            })).collect::<Vec<_>>(),
            })
        }
        label if label.starts_with('v') => {
            let (voter, claim_index) = label
                .strip_prefix('v')
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(voter, claim)| {
                    Some((
                        voter.parse::<usize>().ok()?,
                        claim
                            .strip_prefix(&format!("{RESEARCH_CLAIM}_"))?
                            .parse::<usize>()
                            .ok()?,
                    ))
                })
                .expect("verifier label should identify its voter and claim");
            json!({
                "refuted": false,
                "evidence": large_dynamic_string(
                    RESEARCH_VERDICT_EVIDENCE,
                    claim_index * 3 + voter,
                ),
                "confidence": "high",
            })
        }
        "synthesize" => json!({
            "summary": "RESEARCH_SYNTHESIS_SENTINEL",
            "findings": inputs["confirmed"]
                .as_array()
                .expect("confirmed claims")
                .iter()
                .map(|claim| json!({
                    "claim": claim["claim"],
                    "confidence": "high",
                    "sources": [claim["sourceUrl"]],
                    "evidence": claim["quote"],
                }))
                .collect::<Vec<_>>(),
            "caveats": "none",
            "openQuestions": [],
        }),
        _ => panic!("unexpected deep-research agent label: {label}"),
    }
}

async fn execute_bundled(
    name: &str,
    args: Value,
    runtime: Arc<CapturingAgentRuntime>,
) -> Result<codex_workflow::WorkflowRunOutcome, WorkflowExecutionError> {
    let source = get(name).expect("bundled workflow should be registered");
    let script = validate_workflow_script(source).expect("bundled workflow should be valid");
    execute_workflow(
        &script,
        args,
        runtime,
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            concurrency: 8,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
}

fn synthesis_request(requests: &[WorkflowAgentRequest]) -> &WorkflowAgentRequest {
    let synthesis = synthesis_requests(requests);
    let [request] = synthesis.as_slice() else {
        panic!(
            "expected the Synthesize phase to contain exactly one agent, got {}",
            synthesis.len()
        );
    };
    request
}

fn synthesis_requests(requests: &[WorkflowAgentRequest]) -> Vec<&WorkflowAgentRequest> {
    let synthesis = requests
        .iter()
        .filter(|request| request.options.phase.as_deref() == Some("Synthesize"))
        .collect::<Vec<_>>();
    assert!(
        synthesis
            .iter()
            .all(|request| request.options.label.as_deref() == Some("synthesize"))
    );
    synthesis
}

fn assert_synthesis_failure_attempts(
    requests: &[WorkflowAgentRequest],
    kind: WorkflowAgentFailureKind,
) {
    let synthesis = synthesis_requests(requests);
    let expected_attempts = match kind {
        WorkflowAgentFailureKind::Throttled => vec![0, 1],
        WorkflowAgentFailureKind::Failed
        | WorkflowAgentFailureKind::TerminalApi
        | WorkflowAgentFailureKind::Skipped => vec![0],
        WorkflowAgentFailureKind::Cancelled
        | WorkflowAgentFailureKind::Stalled
        | WorkflowAgentFailureKind::Blocked => {
            panic!("unsupported final synthesis failure kind: {kind:?}")
        }
    };
    assert_eq!(
        synthesis
            .iter()
            .map(|request| request.attempt)
            .collect::<Vec<_>>(),
        expected_attempts
    );
    let first = synthesis.first().expect("final synthesis request");
    assert!(synthesis.iter().all(|request| {
        request.index == first.index
            && request.invocation_id == first.invocation_id
            && request.prompt == first.prompt
            && request.options == first.options
    }));
}

fn assert_request_plan(requests: &[WorkflowAgentRequest], expected: Vec<(String, String)>) {
    let mut actual = requests
        .iter()
        .map(|request| {
            (
                request
                    .options
                    .phase
                    .as_deref()
                    .expect("agent phase")
                    .to_string(),
                request
                    .options
                    .label
                    .as_deref()
                    .expect("agent label")
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected;
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

async fn assert_code_review_failure_stops_synthesis(kind: WorkflowAgentFailureKind) {
    let runtime = Arc::new(CapturingAgentRuntime::failing(
        Fixture::CodeReview(ReviewLevel::High),
        "angle-B",
        kind,
    ));
    let error = execute_bundled(
        "code-review",
        json!(format!("high {REVIEW_TARGET}")),
        runtime.clone(),
    )
    .await
    .expect_err("strict fan-in should fail after an upstream failure");
    assert!(
        error.to_string().contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );
    assert!(
        runtime.requests().iter().all(|request| !matches!(
            request.options.phase.as_deref(),
            Some("Verify" | "Synthesize")
        )),
        "partial finder results must not reach verification or synthesis"
    );
}

async fn assert_code_review_final_synthesis_is_required(kind: WorkflowAgentFailureKind) {
    let runtime = Arc::new(CapturingAgentRuntime::failing(
        Fixture::CodeReview(ReviewLevel::High),
        "synthesize",
        kind,
    ));
    let error = execute_bundled(
        "code-review",
        json!(format!("high {REVIEW_TARGET}")),
        runtime.clone(),
    )
    .await
    .expect_err("final synthesis must complete successfully");
    assert!(
        error.to_string().contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );

    let requests = runtime.requests();
    assert_synthesis_failure_attempts(&requests, kind);
}

#[test]
fn bundled_workflows_pass_runtime_validation() {
    for name in ["code-review", "deep-research"] {
        let source = get(name).expect("bundled workflow should be registered");
        validate_workflow_script(source)
            .unwrap_or_else(|error| panic!("bundled workflow {name} is invalid: {error}"));
    }
}

#[tokio::test]
async fn code_review_synthesizes_complete_structured_inputs_once() {
    for level in REVIEW_LEVELS {
        let runtime = Arc::new(CapturingAgentRuntime::new(Fixture::CodeReview(level)));
        let outcome = execute_bundled(
            "code-review",
            json!(format!("{} {REVIEW_TARGET}", level.name())),
            runtime.clone(),
        )
        .await
        .unwrap();

        let requests = runtime.requests();
        let mut expected = vec![("Scope".to_string(), "scope".to_string())];
        expected.extend(
            level
                .finder_labels()
                .iter()
                .map(|label| ("Find".to_string(), (*label).to_string())),
        );
        expected.extend((0..level.candidate_count()).map(|index| {
            (
                "Verify".to_string(),
                format!("verify:REVIEW_FILE_SENTINEL.rs:{}(1)", 17 + index),
            )
        }));
        if level.has_sweep() {
            expected.push(("Sweep".to_string(), "sweep".to_string()));
            expected.push((
                "Verify".to_string(),
                format!(
                    "verify:REVIEW_FILE_SENTINEL.rs:{}(1)",
                    17 + level.candidate_count()
                ),
            ));
        }
        expected.push(("Synthesize".to_string(), "synthesize".to_string()));
        assert_request_plan(&requests, expected);
        synthesis_request(&requests);
        assert_eq!(
            outcome.result["findings"].as_array().map(Vec::len),
            Some(level.candidate_count() + if level.has_sweep() { 1 } else { 0 })
        );
        assert_eq!(
            outcome.result["summary"],
            json!("REVIEW_SYNTHESIS_SENTINEL")
        );
    }
}

#[tokio::test]
async fn code_review_requires_every_finder_before_synthesis() {
    for kind in FAILURE_KINDS {
        assert_code_review_failure_stops_synthesis(kind).await;
    }
}

#[tokio::test]
async fn code_review_final_synthesis_user_skip_fails_workflow() {
    let runtime = Arc::new(CapturingAgentRuntime::returning_null(
        Fixture::CodeReview(ReviewLevel::High),
        "synthesize",
    ));
    let error = execute_bundled(
        "code-review",
        json!(format!("high {REVIEW_TARGET}")),
        runtime.clone(),
    )
    .await
    .expect_err("a fulfilled null synthesis result must fail the code-review workflow")
    .to_string();
    assert!(
        error.contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );
    assert!(
        error.contains("Final code-review synthesis must return a valid structured report."),
        "null synthesis should fail through explicit report validation: {error}"
    );

    let requests = runtime.requests();
    synthesis_request(&requests);
}

#[tokio::test]
async fn code_review_final_synthesis_failure_fails_workflow() {
    for kind in [
        WorkflowAgentFailureKind::Failed,
        WorkflowAgentFailureKind::TerminalApi,
        WorkflowAgentFailureKind::Throttled,
    ] {
        assert_code_review_final_synthesis_is_required(kind).await;
    }
}

#[tokio::test]
async fn code_review_final_synthesis_invalid_report_fails_workflow() {
    let runtime = Arc::new(CapturingAgentRuntime::returning_value(
        Fixture::CodeReview(ReviewLevel::High),
        "synthesize",
        json!({
            "summary": "INVALID_CODE_REVIEW_REPORT_SENTINEL",
            "decisions": "not-an-array",
        }),
    ));
    let error = execute_bundled(
        "code-review",
        json!(format!("high {REVIEW_TARGET}")),
        runtime.clone(),
    )
    .await
    .expect_err("a non-null invalid synthesis report must fail the code-review workflow")
    .to_string();
    assert!(
        error.contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );
    assert!(
        error.contains("Final code-review synthesis must return a valid structured report."),
        "invalid synthesis should fail through explicit report validation: {error}"
    );

    let requests = runtime.requests();
    synthesis_request(&requests);
}

#[tokio::test]
async fn deep_research_synthesizes_complete_structured_inputs_once() {
    let runtime = Arc::new(CapturingAgentRuntime::new(Fixture::DeepResearch));
    let outcome = execute_bundled("deep-research", json!(RESEARCH_QUESTION), runtime.clone())
        .await
        .unwrap();

    let requests = runtime.requests();
    let mut expected = vec![
        ("Scope".to_string(), "scope".to_string()),
        ("Search".to_string(), "search:primary".to_string()),
        ("Search".to_string(), "search:technical".to_string()),
        ("Search".to_string(), "search:contrarian".to_string()),
    ];
    for source_index in 0..RESEARCH_SOURCE_COUNT {
        expected.push((
            "Fetch".to_string(),
            format!("fetch:{}", research_source_host(source_index)),
        ));
    }
    for claim_index in 0..RESEARCH_CLAIM_COUNT {
        for voter in 0..3 {
            expected.push(("Verify".to_string(), verifier_label(voter, claim_index)));
        }
    }
    expected.push(("Synthesize".to_string(), "synthesize".to_string()));
    assert_request_plan(&requests, expected);
    synthesis_request(&requests);
    assert_eq!(
        outcome.result["findings"].as_array().map(Vec::len),
        Some(RESEARCH_CLAIM_COUNT)
    );
    assert_eq!(
        outcome.result["summary"],
        json!("RESEARCH_SYNTHESIS_SENTINEL")
    );
}

#[tokio::test]
async fn deep_research_keeps_search_fetch_and_verify_best_effort() {
    for kind in FAILURE_KINDS {
        let search_runtime = Arc::new(CapturingAgentRuntime::failing(
            Fixture::DeepResearch,
            "search:technical",
            kind,
        ));
        let search_outcome =
            execute_bundled("deep-research", json!(RESEARCH_QUESTION), search_runtime)
                .await
                .expect("one failed search should not fail the research workflow");
        assert_eq!(
            search_outcome.result["summary"],
            json!("RESEARCH_SYNTHESIS_SENTINEL")
        );

        let fetch_runtime = Arc::new(CapturingAgentRuntime::failing(
            Fixture::DeepResearch,
            "fetch:research-source-0-sentinel.test",
            kind,
        ));
        let fetch_outcome =
            execute_bundled("deep-research", json!(RESEARCH_QUESTION), fetch_runtime)
                .await
                .expect("one failed fetch should not fail the research workflow");
        assert_eq!(
            fetch_outcome.result["summary"],
            json!("RESEARCH_SYNTHESIS_SENTINEL")
        );
        assert_eq!(
            fetch_outcome.result["findings"]
                .as_array()
                .expect("research findings")
                .iter()
                .map(|finding| {
                    finding["claim"]
                        .as_str()
                        .expect("finding claim")
                        .to_string()
                })
                .collect::<Vec<_>>(),
            (RESEARCH_CLAIMS_PER_SOURCE..RESEARCH_CLAIM_COUNT)
                .map(|index| format!("{RESEARCH_CLAIM}_{index}"))
                .collect::<Vec<_>>()
        );

        let verifier_runtime = Arc::new(CapturingAgentRuntime::failing(
            Fixture::DeepResearch,
            "v0:RESEARCH_CLAIM_SENTINEL_0",
            kind,
        ));
        let verifier_outcome =
            execute_bundled("deep-research", json!(RESEARCH_QUESTION), verifier_runtime)
                .await
                .expect("one failed verifier should not fail the research workflow");
        assert_eq!(
            verifier_outcome.result["summary"],
            json!("RESEARCH_SYNTHESIS_SENTINEL")
        );
    }
}

#[tokio::test]
async fn deep_research_requires_final_synthesis() {
    for kind in FAILURE_KINDS {
        let runtime = Arc::new(CapturingAgentRuntime::failing(
            Fixture::DeepResearch,
            "synthesize",
            kind,
        ));
        let error = execute_bundled("deep-research", json!(RESEARCH_QUESTION), runtime.clone())
            .await
            .expect_err("final synthesis failure should fail the research workflow");
        assert!(
            error.to_string().contains("WorkflowParallelError"),
            "unexpected Workflow failure: {error}"
        );

        let requests = runtime.requests();
        assert_synthesis_failure_attempts(&requests, kind);
    }
}

#[tokio::test]
async fn deep_research_final_synthesis_fulfilled_null_fails_workflow() {
    let runtime = Arc::new(CapturingAgentRuntime::returning_null(
        Fixture::DeepResearch,
        "synthesize",
    ));
    let error = execute_bundled("deep-research", json!(RESEARCH_QUESTION), runtime.clone())
        .await
        .expect_err("a fulfilled null synthesis result must fail the research workflow");
    let error = error.to_string();
    assert!(
        error.contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );
    assert!(
        error.contains("Final deep-research synthesis must return a valid structured report."),
        "null synthesis should fail through explicit report validation: {error}"
    );

    let requests = runtime.requests();
    synthesis_request(&requests);
}

#[tokio::test]
async fn deep_research_final_synthesis_invalid_report_fails_workflow() {
    let runtime = Arc::new(CapturingAgentRuntime::returning_value(
        Fixture::DeepResearch,
        "synthesize",
        json!({
            "summary": "INVALID_RESEARCH_REPORT_SENTINEL",
            "findings": "not-an-array",
            "caveats": "fixture",
        }),
    ));
    let error = execute_bundled("deep-research", json!(RESEARCH_QUESTION), runtime.clone())
        .await
        .expect_err("a non-null invalid synthesis report must fail the research workflow")
        .to_string();
    assert!(
        error.contains("WorkflowParallelError"),
        "unexpected Workflow failure: {error}"
    );
    assert!(
        error.contains("Final deep-research synthesis must return a valid structured report."),
        "invalid synthesis should fail through explicit report validation: {error}"
    );

    let requests = runtime.requests();
    synthesis_request(&requests);
}
