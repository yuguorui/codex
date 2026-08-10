use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::MAX_WORKFLOW_RESULT_DEPTH;
use crate::WorkflowDeclaredInputFile;
use crate::WorkflowDeclaredInputs;
use crate::WorkflowInputArtifactFuture;
use crate::WorkflowMeta;
use crate::WorkflowPhase;
use crate::WorkflowTokenUsage;
use crate::validate_workflow_script;
use crate::workflow_input_artifact_ref;

#[derive(Default)]
struct FakeAgentRuntime {
    prompts: Mutex<Vec<String>>,
    requests: Mutex<Vec<WorkflowAgentRequest>>,
}

struct FakeJournal {
    cached: Mutex<HashMap<String, WorkflowJournalResult>>,
    started: Mutex<Vec<String>>,
    written: Mutex<Vec<String>>,
}

impl FakeJournal {
    fn new(cached: HashMap<String, WorkflowAgentResult>) -> Self {
        Self {
            cached: Mutex::new(
                cached
                    .into_iter()
                    .map(|(key, result)| {
                        (
                            key,
                            WorkflowJournalResult {
                                result,
                                outcome: WorkflowAgentOutcome::Success,
                            },
                        )
                    })
                    .collect(),
            ),
            started: Mutex::new(Vec::new()),
            written: Mutex::new(Vec::new()),
        }
    }
}

impl WorkflowJournal for FakeJournal {
    fn replay<'a>(&'a self, key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        Box::pin(async move {
            Ok(self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .cloned())
        })
    }

    fn append_started(&self, key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async move {
            self.started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key);
            Ok(())
        })
    }

    fn append_result(
        &self,
        key: String,
        _result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(async move {
            self.written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key);
            Ok(())
        })
    }
}

struct FakeChildResolver {
    script: ValidatedWorkflowScript,
    requests: Mutex<Vec<WorkflowChildRequest>>,
}

impl WorkflowChildResolver for FakeChildResolver {
    fn resolve_child<'a>(&'a self, request: WorkflowChildRequest) -> WorkflowChildFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            Ok(ResolvedWorkflowChild {
                script: self.script.clone(),
                args: request.args,
            })
        })
    }
}

struct FixedArgsChildResolver {
    script: ValidatedWorkflowScript,
    args: JsonValue,
}

struct BlockingPutArtifactStore {
    started: AtomicUsize,
    started_notify: Notify,
    releases: Semaphore,
}

impl BlockingPutArtifactStore {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            started_notify: Notify::new(),
            releases: Semaphore::new(0),
        }
    }

    async fn wait_for_puts(&self, expected: usize) {
        loop {
            let notified = self.started_notify.notified();
            if self.started.load(Ordering::Acquire) >= expected {
                return;
            }
            notified.await;
        }
    }

    fn release_one(&self) {
        self.releases.add_permits(1);
    }
}

impl WorkflowInputArtifactStore for BlockingPutArtifactStore {
    fn put(&self, value: JsonValue) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::AcqRel);
            self.started_notify.notify_waiters();
            self.releases
                .acquire()
                .await
                .map_err(|_| "artifact release semaphore closed".to_string())?
                .forget();
            workflow_input_artifact_ref(&value)
        })
    }

    fn put_descriptor(
        &self,
        _descriptor: WorkflowInputDescriptor,
    ) -> WorkflowInputArtifactFuture<'_, WorkflowInputArtifactRef> {
        Box::pin(async { Err("descriptor writes are unused in this test".to_string()) })
    }

    fn get<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<JsonValue>> {
        Box::pin(async { Err("artifact reads are unused in this test".to_string()) })
    }

    fn get_descriptor<'a>(
        &'a self,
        _reference: &WorkflowInputArtifactRef,
    ) -> WorkflowInputArtifactFuture<'a, Arc<WorkflowInputDescriptor>> {
        Box::pin(async { Err("descriptor reads are unused in this test".to_string()) })
    }
}

impl WorkflowChildResolver for FixedArgsChildResolver {
    fn resolve_child<'a>(&'a self, _request: WorkflowChildRequest) -> WorkflowChildFuture<'a> {
        Box::pin(async move {
            Ok(ResolvedWorkflowChild {
                script: self.script.clone(),
                args: self.args.clone(),
            })
        })
    }
}

impl FakeAgentRuntime {
    fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn requests(&self) -> Vec<WorkflowAgentRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl WorkflowAgentRuntime for FakeAgentRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            self.prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.prompt.clone());
            let delay = if request.prompt.contains("slow") {
                80
            } else {
                1
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = cancellation.cancelled() => {
                    return Err(WorkflowAgentFailure::failed("cancelled"));
                }
            }
            if request.prompt.contains("always-stall") {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "agent made no progress for 180s".to_string(),
                    usage: WorkflowTokenUsage::default(),
                });
            }
            if request.prompt.contains("stall") && request.attempt < 3 {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "agent made no progress for 180s".to_string(),
                    usage: WorkflowTokenUsage::default(),
                });
            }
            if request.prompt.contains("fail") {
                if request.prompt.contains("long") {
                    return Err(WorkflowAgentFailure::failed("界".repeat(500)));
                }
                if request.prompt.contains("terminal-api") {
                    return Err(WorkflowAgentFailure {
                        kind: WorkflowAgentFailureKind::TerminalApi,
                        message: "terminal API failure".to_string(),
                        usage: WorkflowTokenUsage::default(),
                    });
                }
                return Err(WorkflowAgentFailure::failed("requested failure"));
            }
            if request.prompt.contains("throttled") {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Throttled,
                    message: "request throttled".to_string(),
                    usage: WorkflowTokenUsage::default(),
                });
            }
            if request.prompt.contains("host-skipped") {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Skipped,
                    message: "agent skipped by host".to_string(),
                    usage: WorkflowTokenUsage::default(),
                });
            }
            let value = if request.prompt == "envelope-output" {
                json!({
                    "status": "rejected",
                    "reason": { "kind": "skipped", "message": "model value" },
                })
            } else if request.prompt == "negative-zero" {
                serde_json::from_str("-0.0").unwrap()
            } else if request.prompt == "positive-zero" {
                serde_json::from_str("0.0").unwrap()
            } else if request.prompt == "unsafe-host-result" {
                serde_json::from_str("18446744073709551616").unwrap()
            } else if let Some(index) = request.prompt.strip_prefix("large-report-") {
                json!({
                    "index": index.parse::<usize>().unwrap(),
                    "body": format!("report-{index}:{}", "x".repeat(768 * 1024)),
                })
            } else {
                json!(format!("result:{}", request.prompt))
            };
            Ok(WorkflowAgentResult {
                value,
                usage: WorkflowTokenUsage {
                    total_tokens: 10,
                    tool_uses: 1,
                },
                agent_id: Some(format!("agent-{}", request.index)),
                model: Some("fake-model".to_string()),
                fallback_model: None,
            })
        })
    }
}

fn script(body: &str) -> ValidatedWorkflowScript {
    validate_workflow_script(format!(
        "export const meta = {{ name: 'test', description: 'test workflow', phases: [{{ title: 'Run' }}] }};\n{body}"
    ))
    .unwrap()
}

async fn run(
    body: &str,
    args: serde_json::Value,
) -> (
    WorkflowRunOutcome,
    Arc<FakeAgentRuntime>,
    Vec<WorkflowEvent>,
) {
    run_with_config(
        body,
        args,
        WorkflowRuntimeConfig {
            concurrency: 4,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
    )
    .await
}

async fn run_with_config(
    body: &str,
    args: serde_json::Value,
    config: WorkflowRuntimeConfig,
) -> (
    WorkflowRunOutcome,
    Arc<FakeAgentRuntime>,
    Vec<WorkflowEvent>,
) {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);
    let outcome = execute_workflow(
        &script(body),
        args,
        runtime.clone(),
        Arc::new(move |_, event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        config,
        WorkflowControl::new(),
    )
    .await
    .unwrap();
    let events = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (outcome, runtime, events)
}

#[tokio::test]
async fn declared_inputs_are_frozen_and_available_through_read_only_runtime_apis() {
    let declared_inputs = WorkflowDeclaredInputs {
        patterns: vec!["src/input.txt".to_string()],
        files: BTreeMap::from([(
            "src/input.txt".to_string(),
            WorkflowDeclaredInputFile {
                sha256: format!("{:x}", Sha256::digest(b"frozen contents")),
                bytes: "frozen contents".len(),
                content: "frozen contents".to_string(),
            },
        )]),
    };
    let (outcome, _, _) = run_with_config(
        r#"
const files = await listInputs();
const content = await readInput("src/input.txt");
let undeclared;
try {
  await readInput("src/other.txt");
  undeclared = "unexpected success";
} catch (error) {
  undeclared = String(error);
}
return { files, content, undeclared };
"#,
        json!(null),
        WorkflowRuntimeConfig {
            declared_inputs: Arc::new(declared_inputs),
            ..WorkflowRuntimeConfig::default()
        },
    )
    .await;

    assert_eq!(
        outcome.result,
        json!({
            "files": [{
                "path": "src/input.txt",
                "bytes": 15,
                "sha256": format!("{:x}", Sha256::digest(b"frozen contents")),
            }],
            "content": "frozen contents",
            "undeclared": "readInput may only read files frozen by meta.inputs; `src/other.txt` is unavailable",
        })
    );
}

#[tokio::test]
async fn passes_args_to_agents_and_returns_the_workflow_result() {
    let (outcome, runtime, _) = run(
        "return agent(`inspect:${args.target}`, { label: 'inspect' })",
        json!({ "target": "src/lib.rs" }),
    )
    .await;

    assert_eq!(outcome.result, json!("result:inspect:src/lib.rs"));
    assert_eq!(outcome.agent_count, 1);
    assert_eq!(outcome.total_tokens, 10);
    assert_eq!(outcome.total_tool_calls, 1);
    assert_eq!(runtime.prompts(), vec!["inspect:src/lib.rs"]);
}

#[tokio::test]
async fn rejects_unsafe_root_args_before_starting_the_workflow_isolate() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let args = json!({
        "identifier": serde_json::from_str::<JsonValue>("18446744073709551616").unwrap(),
    });

    let error = execute_workflow(
        &script("return agent(String(args.identifier))"),
        args,
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
    assert_eq!(runtime.requests(), Vec::new());
}

#[tokio::test]
async fn rejects_unsafe_agent_host_results_before_returning_them_to_v8() {
    let runtime = Arc::new(FakeAgentRuntime::default());

    let error = execute_workflow(
        &script("return agent('unsafe-host-result')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
    assert_eq!(runtime.prompts(), vec!["unsafe-host-result"]);
}

#[tokio::test]
async fn agent_inputs_are_sanitized_independently_and_reach_rust_without_prompt_concatenation() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let outcome = execute_workflow(
        &script(
            r#"
const large = "界😀\"\\".repeat(40_000);
return agent("Analyze the named reports without embedding them", {
  label: "analyze",
  inputs: {
    reports: args.reports,
    large,
  },
});
"#,
        ),
        json!({
            "reports": [
                {"area": "核心", "score": 7},
                {"area": "TUI 😀", "score": 3},
            ],
        }),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result,
        json!("result:Analyze the named reports without embedding them")
    );
    let requests = runtime.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one Workflow agent request");
    };
    assert_eq!(
        request.prompt,
        "Analyze the named reports without embedding them"
    );
    let inputs = request
        .inputs
        .as_ref()
        .expect("structured inputs")
        .resolve_shared()
        .await
        .unwrap();
    let reports = inputs
        .descriptor(&inputs.references()["reports"])
        .expect("reports descriptor");
    assert_eq!(
        reports.value,
        json!([
            {"area": "核心", "score": 7},
            {"area": "TUI 😀", "score": 3},
        ])
    );
    let large = inputs
        .descriptor(&inputs.references()["large"])
        .expect("large descriptor");
    assert!(
        large
            .value
            .as_str()
            .is_some_and(|value| value.len() > 256 * 1024)
    );
    assert!(!request.prompt.contains("核心"));
    assert!(!request.prompt.contains("TUI"));
}

#[tokio::test]
async fn synthesis_reads_all_large_reports_through_one_artifact_reference() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let outcome = execute_workflow(
        &script(
            r#"
const reports = await parallel(
  Array.from({ length: 8 }, (_, index) => () => agent("large-report-" + index)),
  { requireAll: true },
);
return agent("synthesize-large-reports", { inputs: { reports } });
"#,
        ),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!("result:synthesize-large-reports"));
    let request = runtime
        .requests()
        .into_iter()
        .find(|request| request.prompt == "synthesize-large-reports")
        .expect("synthesis request");
    let references = request
        .options
        .inputs
        .as_ref()
        .expect("synthesis input references");
    assert_eq!(references.len(), 1);
    let inputs = request
        .inputs
        .expect("synthesis input capability")
        .resolve_shared()
        .await
        .unwrap();
    let reports = inputs
        .descriptor(&inputs.references()["reports"])
        .expect("reports descriptor");
    assert_eq!(reports.artifacts.len(), 8);
    assert!(reports.artifacts.iter().all(|location| {
        let report = inputs
            .value(&location.reference)
            .expect("upstream report artifact");
        report["body"]
            .as_str()
            .is_some_and(|body| body.len() > 700 * 1024)
    }));
}

#[tokio::test]
async fn primitive_upstream_results_do_not_reuse_artifacts_or_lose_signed_zero() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    execute_workflow(
        &script(
            r#"
const negative = await agent('negative-zero');
const positive = await agent('positive-zero');
return agent('inspect-zeros', { inputs: { negative, positive } });
"#,
        ),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    let request = runtime
        .requests()
        .into_iter()
        .find(|request| request.prompt == "inspect-zeros")
        .unwrap();
    let inputs = request.inputs.unwrap().resolve_shared().await.unwrap();
    let negative = inputs
        .descriptor(&inputs.references()["negative"])
        .expect("negative zero descriptor");
    let positive = inputs
        .descriptor(&inputs.references()["positive"])
        .expect("positive zero descriptor");
    assert_eq!(negative.negative_zeros, vec![Vec::new()]);
    assert!(positive.negative_zeros.is_empty());
}

#[tokio::test]
async fn large_agent_inputs_share_duplicate_aliases() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    execute_workflow(
        &script(
            "const shared = Array(5000).fill(null); const inputs = Object.fromEntries(Array.from({ length: 100 }, (_, i) => [`input-${i}`, shared])); return agent('x', { inputs });",
        ),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    let request = runtime.requests().pop().unwrap();
    let references = request.inputs.unwrap();
    assert_eq!(references.references().len(), 100);
    assert_eq!(
        references
            .references()
            .values()
            .map(|reference| &reference.sha256)
            .collect::<HashSet<_>>()
            .len(),
        1
    );
}

#[tokio::test]
async fn root_args_preserve_json_types() {
    let outcome = execute_workflow(
        &script(
            "return { isArray: Array.isArray(args.items), items: args.items, label: args.label }",
        ),
        json!({ "label": "manifest", "items": ["one", "two"] }),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result,
        json!({
            "isArray": true,
            "items": ["one", "two"],
            "label": "manifest",
        })
    );
}

#[tokio::test]
async fn wide_root_args_are_accepted_and_numbers_stay_lossless() {
    let outcome = execute_workflow(
        &script("return args.length"),
        JsonValue::Array(vec![JsonValue::Null; 5000]),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.result, json!(5000));

    let mut deep = JsonValue::Null;
    for _ in 0..300 {
        deep = JsonValue::Array(vec![deep]);
    }
    let error = execute_workflow(
        &script("return agent('unreachable')"),
        deep,
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("flatter structured value"));

    let error = execute_workflow(
        &script("return args"),
        serde_json::from_str("1.0000000000000000001").unwrap(),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
}

#[tokio::test]
async fn agent_inputs_reject_unsafe_integers_before_hashing_or_delegation() {
    let runtime = Arc::new(FakeAgentRuntime::default());

    let error = execute_workflow(
        &script("return agent('inspect', { inputs: { identifier: 9007199254740992 } })"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
    assert_eq!(runtime.requests(), Vec::new());
}

#[tokio::test]
async fn large_agent_prompt_is_delegated() {
    let prompt = "界a".repeat(128 * 1024);

    let (_, runtime, _) = run(
        "return agent(args.prompt)",
        json!({ "prompt": prompt.clone() }),
    )
    .await;

    assert_eq!(runtime.prompts(), vec![prompt]);
}

#[tokio::test]
async fn large_agent_schema_is_delegated() {
    let description = "s".repeat(256 * 1024);
    let schema = json!({
        "type": "string",
        "description": description,
    });

    let (_, runtime, _) = run(
        "return agent('large-schema', { schema: args.schema })",
        json!({ "schema": schema.clone() }),
    )
    .await;

    assert_eq!(runtime.requests()[0].options.schema, Some(schema));
}

#[tokio::test]
async fn final_results_are_rejected_before_the_runtime_retains_them() {
    let body = format!(
        "let value = null; for (let index = 0; index < {MAX_WORKFLOW_RESULT_DEPTH}; index++) value = [value]; return value"
    );
    let result = execute_workflow(
        &script(&body),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await;

    let Err(WorkflowExecutionError::Runtime(message)) = result else {
        panic!("deep workflow result must fail at the runtime boundary");
    };
    assert!(message.contains("WorkflowResultLimitError: return a shallower workflow result"));
    assert!(message.len() < 512);
}

#[tokio::test]
async fn collection_limit_errors_describe_the_continuation_path_without_capacity_numbers() {
    let cases = [
        (
            "return parallel(Array.from({ length: 4097 }, () => () => null))",
            "pass a focused work set to parallel(); split larger work across additional calls",
        ),
        (
            "return pipeline(Array.from({ length: 4097 }, () => null), (value) => value)",
            "pass a focused work set to pipeline(); split larger work across additional calls",
        ),
    ];

    for (body, expected) in cases {
        let result = execute_workflow(
            &script(body),
            json!(null),
            Arc::new(FakeAgentRuntime::default()),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig::default(),
            WorkflowControl::new(),
        )
        .await;

        let Err(WorkflowExecutionError::Runtime(message)) = result else {
            panic!("large workflow collection must fail at the runtime boundary");
        };
        assert!(
            message.contains(expected),
            "expected `{expected}` in `{message}`"
        );
        assert!(!message.contains("4096"));
    }
}

#[tokio::test]
async fn wide_results_are_not_governed_by_a_node_quota() {
    let outcome = execute_workflow(
        &script("return Array.from({ length: 40000 }, (_, index) => index)"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result.as_array().map(Vec::len), Some(40_000));
}

#[tokio::test]
async fn structurally_complex_schemas_fail_before_agent_delegation() {
    let cases = [
        (
            r#"
let schema = { type: "null" };
for (let depth = 0; depth < 64; depth += 1) schema = { items: schema };
return agent("must not run", { schema });
"#,
            "WorkflowSchemaLimitError: use a focused workflow agent schema; split larger material across additional calls or artifacts",
        ),
        (
            r#"
let schema = { type: "null" };
for (let depth = 0; depth < 12; depth += 1) schema = { anyOf: [schema, schema] };
return agent("must not run", { schema });
"#,
            "WorkflowSchemaLimitError: use a focused workflow agent schema; split larger material across additional calls or artifacts",
        ),
    ];

    for (body, expected) in cases {
        let runtime = Arc::new(FakeAgentRuntime::default());
        let result = execute_workflow(
            &script(body),
            json!(null),
            runtime.clone(),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig::default(),
            WorkflowControl::new(),
        )
        .await;

        let Err(WorkflowExecutionError::Runtime(message)) = result else {
            panic!("structurally complex schema must fail at the JavaScript boundary");
        };
        assert!(
            message.contains(expected),
            "expected `{expected}` in runtime error `{message}`"
        );
        assert_eq!(runtime.prompts(), Vec::<String>::new());
    }
}

#[tokio::test]
async fn deep_results_fail_at_the_javascript_boundary() {
    let result = execute_workflow(
        &script(
            r#"
let result = null;
for (let depth = 0; depth < 64; depth += 1) result = [result];
return result;
"#,
        ),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await;

    assert!(matches!(
        result,
        Err(WorkflowExecutionError::Runtime(message))
            if message.contains("WorkflowResultLimitError: return a shallower workflow result")
    ));
}

#[tokio::test]
async fn large_result_crosses_the_runtime_boundary_without_an_aggregate_byte_limit() {
    let outcome = execute_workflow(
        &script(r#"return { payload: "\0你好".repeat(200_000) };"#),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result["payload"].as_str().unwrap().chars().count(),
        600_000
    );
}

#[tokio::test]
async fn shared_dag_results_preserve_json_value_semantics_with_bounded_copying() {
    let (outcome, _, _) = run(
        r#"
const leaf = { value: "shared" };
const branch = [leaf, leaf];
return { left: branch, right: branch };
"#,
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!({
            "left": [{ "value": "shared" }, { "value": "shared" }],
            "right": [{ "value": "shared" }, { "value": "shared" }],
        })
    );
}

#[tokio::test]
async fn syntax_errors_fail_before_any_agents_run() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let result = execute_workflow(
        &script("const first = agent('must not run'); text(]"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await;

    assert!(matches!(result, Err(WorkflowExecutionError::Runtime(_))));
    assert_eq!(runtime.prompts(), Vec::<String>::new());
}

#[tokio::test]
async fn agent_progress_timestamps_use_unix_seconds_like_task_timestamps() {
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (_, _, events) = run("return agent('timestamp')", json!(null)).await;
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let (queued_at, started_at, last_progress_at) = events
        .iter()
        .find_map(|event| match event {
            WorkflowEvent::WorkflowAgent(agent) if agent.state == WorkflowAgentState::Done => agent
                .started_at
                .map(|started_at| (agent.queued_at, started_at, agent.last_progress_at)),
            _ => None,
        })
        .expect("completed agent progress event");
    for timestamp in [queued_at, started_at, last_progress_at] {
        assert!((before..=after).contains(&timestamp));
    }
}

#[tokio::test]
async fn parallel_is_all_settled_and_reports_failures_as_null() {
    let (outcome, _, events) = run(
        "return parallel([() => agent('one'), () => agent('fail'), () => agent('three')])",
        json!(null),
    )
    .await;

    assert_eq!(outcome.result, json!(["result:one", null, "result:three"]));
    assert_eq!(outcome.agent_count, 3);
    assert!(
        outcome
            .logs
            .iter()
            .any(|log| log.contains("parallel[1] failed"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent) if agent.state == WorkflowAgentState::Error
    )));
}

#[tokio::test]
async fn direct_terminal_api_failure_rejects_and_records_the_failure() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);
    let result = execute_workflow(
        &script("return agent('terminal-api-fail')"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(move |_, event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        WorkflowRuntimeConfig {
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await;

    let Err(WorkflowExecutionError::Runtime(message)) = result else {
        panic!("direct host failure must reject the workflow");
    };
    assert!(message.contains("terminal API failure"));
    let events = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Error
                && agent.error.as_deref() == Some("terminal API failure")
    )));
}

#[tokio::test]
async fn strict_parallel_never_runs_synthesis_with_host_failures() {
    for failure in ["terminal-api-fail", "throttled", "host-skipped"] {
        let runtime = Arc::new(FakeAgentRuntime::default());
        let result = execute_workflow(
            &script(
                "const reports = await parallel([() => agent('ok'), () => agent(args.failure)], { requireAll: true }); return agent('synthesize', { inputs: { reports } })",
            ),
            json!({ "failure": failure }),
            runtime.clone(),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig {
                throttle_retry_delay: Duration::ZERO,
                ..WorkflowRuntimeConfig::default()
            },
            WorkflowControl::new(),
        )
        .await;

        let Err(WorkflowExecutionError::Runtime(message)) = result else {
            panic!("strict fan-in must fail for {failure}");
        };
        assert!(message.contains("WorkflowParallelError"));
        assert!(
            !runtime
                .prompts()
                .iter()
                .any(|prompt| prompt == "synthesize")
        );
    }
}

#[tokio::test]
async fn agent_settled_distinguishes_success_and_failure_kinds() {
    let (outcome, _, _) = run(
        "return parallel([() => agentSettled('ok'), () => agentSettled('fail'), () => agentSettled('terminal-api-fail')])",
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!([
            { "status": "fulfilled", "value": "result:ok" },
            {
                "status": "rejected",
                "reason": { "kind": "failed", "message": "requested failure" },
            },
            {
                "status": "rejected",
                "reason": { "kind": "terminalApi", "message": "terminal API failure" },
            },
        ])
    );
}

#[tokio::test]
async fn agent_settled_bounds_failure_metadata_by_utf8_bytes() {
    let (outcome, _, _) = run("return agentSettled('long-fail')", json!(null)).await;

    let message = outcome.result["reason"]["message"].as_str().unwrap();
    assert_eq!(message.len(), 510);
    assert!(message.chars().all(|character| character == '界'));
}

#[tokio::test]
async fn pipeline_advances_each_item_without_a_cross_item_barrier() {
    let (outcome, runtime, _) = run(
        r#"
return pipeline(
  ["slow", "fast"],
  item => agent(`first:${item}`),
  (_previous, original) => agent(`second:${original}`),
)
"#,
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!(["result:second:slow", "result:second:fast"])
    );
    let prompts = runtime.prompts();
    let second_fast = prompts
        .iter()
        .position(|prompt| prompt == "second:fast")
        .unwrap();
    let second_slow = prompts
        .iter()
        .position(|prompt| prompt == "second:slow")
        .unwrap();
    assert!(second_fast < second_slow, "prompts were {prompts:?}");
}

#[tokio::test]
async fn pipeline_invocation_identities_survive_reversed_branch_completion() {
    let workflow = script(
        r#"
return pipeline(
  args.items,
  item => agent(`first:${item}`),
  (_previous, original) => agent(`second:${original}`),
)
"#,
    );
    let runtime = Arc::new(FakeAgentRuntime::default());
    for items in [json!(["slow-0", "fast-1"]), json!(["fast-0", "slow-1"])] {
        execute_workflow(
            &workflow,
            json!({ "items": items }),
            runtime.clone(),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig {
                concurrency: 4,
                ..WorkflowRuntimeConfig::default()
            },
            WorkflowControl::new(),
        )
        .await
        .unwrap();
    }

    let requests = runtime.requests();
    let mappings = requests
        .chunks_exact(4)
        .map(|run| {
            run.iter()
                .map(|request| (request.invocation_id.clone(), request.index))
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0], mappings[1]);
    assert_eq!(
        mappings[0],
        std::collections::BTreeMap::from([
            ("root/pipeline:0/stage:0/item:0/agent:0".to_string(), 0),
            ("root/pipeline:0/stage:0/item:1/agent:0".to_string(), 1),
            ("root/pipeline:0/stage:1/item:0/agent:0".to_string(), 2),
            ("root/pipeline:0/stage:1/item:1/agent:0".to_string(), 3),
        ])
    );
}

#[tokio::test]
async fn parallel_callbacks_keep_scope_across_multiple_awaited_agent_and_child_calls() {
    let child = validate_workflow_script(
        "export const meta = { name: 'child', description: 'child' }; return agent(`child:${args.item}:${args.step}`)",
    )
    .unwrap();
    let resolver = Arc::new(FakeChildResolver {
        script: child,
        requests: Mutex::new(Vec::new()),
    });
    let runtime = Arc::new(FakeAgentRuntime::default());
    execute_workflow(
        &script(
            r#"
return parallel(args.items.map(item => async () => {
  await Promise.resolve();
  await new Promise(resolve => setTimeout(resolve, 0));
  await agent(`first:${item}`);
  await workflow('child', { item, step: 0 });
  await agent(`second:${item}`);
  return workflow('child', { item, step: 1 });
}), { requireAll: true });
"#,
        ),
        json!({"items": ["slow", "fast"]}),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            concurrency: 4,
            child_resolver: Some(resolver),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    let identities = runtime
        .requests()
        .into_iter()
        .map(|request| (request.prompt, request.invocation_id))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        identities,
        std::collections::BTreeMap::from([
            (
                "child:fast:0".to_string(),
                "root/parallel:0/item:1/workflow:0/agent:0".to_string()
            ),
            (
                "child:fast:1".to_string(),
                "root/parallel:0/item:1/workflow:1/agent:0".to_string()
            ),
            (
                "child:slow:0".to_string(),
                "root/parallel:0/item:0/workflow:0/agent:0".to_string()
            ),
            (
                "child:slow:1".to_string(),
                "root/parallel:0/item:0/workflow:1/agent:0".to_string()
            ),
            (
                "first:fast".to_string(),
                "root/parallel:0/item:1/agent:0".to_string()
            ),
            (
                "first:slow".to_string(),
                "root/parallel:0/item:0/agent:0".to_string()
            ),
            (
                "second:fast".to_string(),
                "root/parallel:0/item:1/agent:1".to_string()
            ),
            (
                "second:slow".to_string(),
                "root/parallel:0/item:0/agent:1".to_string()
            ),
        ])
    );
}

#[tokio::test]
async fn pipeline_callbacks_keep_scope_across_multiple_awaited_agent_calls() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    execute_workflow(
        &script(
            r#"
return pipeline(args.items, async item => {
  await Promise.resolve();
  await new Promise(resolve => setTimeout(resolve, 0));
  await agent(`first:${item}`);
  await agent(`second:${item}`);
  return agent(`third:${item}`);
});
"#,
        ),
        json!({"items": ["slow", "fast"]}),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            concurrency: 4,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    let identities = runtime
        .requests()
        .into_iter()
        .map(|request| (request.prompt, request.invocation_id))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        identities,
        std::collections::BTreeMap::from([
            (
                "first:fast".to_string(),
                "root/pipeline:0/stage:0/item:1/agent:0".to_string()
            ),
            (
                "first:slow".to_string(),
                "root/pipeline:0/stage:0/item:0/agent:0".to_string()
            ),
            (
                "second:fast".to_string(),
                "root/pipeline:0/stage:0/item:1/agent:1".to_string()
            ),
            (
                "second:slow".to_string(),
                "root/pipeline:0/stage:0/item:0/agent:1".to_string()
            ),
            (
                "third:fast".to_string(),
                "root/pipeline:0/stage:0/item:1/agent:2".to_string()
            ),
            (
                "third:slow".to_string(),
                "root/pipeline:0/stage:0/item:0/agent:2".to_string()
            ),
        ])
    );
}

#[tokio::test]
async fn progress_text_removes_control_and_bidi_spoofing() {
    let (outcome, _, events) = run(
        "phase(args.phase); log(args.log); return agent('work', { label: args.label })",
        json!({
            "phase": "phase\nvisible\u{202e}",
            "log": "claim\rverified\u{2066}",
            "label": "review\tcomplete\u{200f}",
        }),
    )
    .await;

    assert_eq!(outcome.logs, vec!["claim verified"]);
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowPhase { title, .. } if title == "phase visible"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent) if agent.label == "review complete"
    )));
}

#[tokio::test]
async fn emits_declared_and_active_phases_logs_and_agent_states() {
    let (outcome, _, events) = run(
        "phase('Run'); console.log('starting'); return agent('work', { label: 'worker' })",
        json!(null),
    )
    .await;

    assert_eq!(outcome.logs, vec!["starting"]);
    assert!(events.contains(&WorkflowEvent::WorkflowPhase {
        index: 0,
        title: "Run".to_string(),
        kind: WorkflowProgressKind::Declared,
    }));
    assert!(events.contains(&WorkflowEvent::WorkflowPhase {
        index: 0,
        title: "Run".to_string(),
        kind: WorkflowProgressKind::Active,
    }));
    let states = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::WorkflowAgent(agent) => Some(agent.state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            WorkflowAgentState::Queued,
            WorkflowAgentState::Start,
            WorkflowAgentState::Done,
        ]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.phase_index == Some(0) && agent.phase_title.as_deref() == Some("Run")
    )));
}

#[tokio::test]
async fn bounds_workflow_logs_while_retaining_the_start_and_latest_diagnostics() {
    let additional_logs = 10;
    let body = format!(
        r#"
log("x".repeat({}));
for (let index = 0; index < {}; index += 1) log(String(index));
return null;
"#,
        MAX_LOG_MESSAGE_BYTES + 1,
        MAX_WORKFLOW_LOGS + additional_logs,
    );

    let (outcome, _, events) = run(&body, json!(null)).await;

    assert_eq!(outcome.logs.len(), MAX_WORKFLOW_LOGS);
    assert_eq!(outcome.logs[0].len(), MAX_LOG_MESSAGE_BYTES);
    assert_eq!(
        outcome.logs[WORKFLOW_LOG_HEAD_LEN],
        format!(
            "[dropped {} earlier workflow log messages]",
            additional_logs + 2
        )
    );
    assert_eq!(
        outcome.logs.last(),
        Some(&(MAX_WORKFLOW_LOGS + additional_logs - 1).to_string())
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, WorkflowEvent::WorkflowLog { .. }))
            .count(),
        MAX_WORKFLOW_LOGS + additional_logs + 1
    );
}

#[tokio::test]
async fn bounds_active_workflow_timers() {
    let (outcome, _, _) = run(
        r#"
const timers = [];
let error = null;
for (let index = 0; index <= 64; index += 1) {
  try {
    timers.push(setTimeout(() => {}, 0));
  } catch (caught) {
    error = caught.message;
  }
}
for (const timer of timers) clearTimeout(timer);
return [timers.length, error];
"#,
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!([
            64,
            "clear or await existing workflow timers before creating more"
        ])
    );
}

#[tokio::test]
async fn runtime_shims_block_aliased_nondeterministic_apis() {
    let (outcome, _, _) = run(
        r#"
const deterministic = new Date(0).toISOString();
const attempts = [
  () => { const clock = Date; return clock.now(); },
  () => Date.prototype.constructor.now(),
  () => Date(0),
  () => { const random = Math["ran" + "dom"]; return random(); },
];
return [deterministic, ...attempts.map(attempt => {
  try { return attempt(); } catch (error) { return error.message; }
})];
"#,
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!([
            "1970-01-01T00:00:00.000Z",
            "provide the current time through workflow args",
            "provide the current time through workflow args",
            "construct dates with `new Date(explicitValue)`",
            "provide random values through workflow args",
        ])
    );
}

#[tokio::test]
async fn skip_agent_cancels_the_active_attempt_and_returns_null() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script("return agent('slow-skip', { label: 'worker' })");
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig::default(),
            task_control,
        )
        .await
    });

    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 0 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.skip_agent(0));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, JsonValue::Null);
    assert!(
        events_until_closed(event_rx)
            .await
            .iter()
            .any(|event| matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent)
                    if agent.index == 0
                        && agent.state == WorkflowAgentState::Error
                        && agent.skipped
            ))
    );
    assert!(!control.skip_agent(0));
}

#[tokio::test]
async fn retry_agent_cancels_the_active_attempt_and_starts_the_next_attempt() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let task_runtime = runtime.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script("return agent('slow-retry', { label: 'worker' })");
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig::default(),
            task_control,
        )
        .await
    });

    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 0
                    && agent.state == WorkflowAgentState::Start
                    && agent.attempt == 0
        ) {
            break;
        }
    }
    assert!(control.retry_agent(0));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("result:slow-retry"));
    assert_eq!(runtime.prompts(), vec!["slow-retry", "slow-retry"]);
    let remaining_events = events_until_closed(event_rx).await;
    assert!(remaining_events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.index == 0
                && agent.state == WorkflowAgentState::Start
                && agent.attempt == 1
    )));
    assert!(remaining_events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.index == 0
                && agent.state == WorkflowAgentState::Done
                && agent.attempt == 1
    )));
    assert!(!control.retry_agent(0));
}

#[tokio::test]
async fn repeated_active_retries_remain_cancellable_and_eventually_complete() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let task_runtime = runtime.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script("return agent('slow-retry-many', { label: 'worker' })");
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig::default(),
            task_control,
        )
        .await
    });

    for expected_attempt in 0..6 {
        loop {
            let event = event_rx.recv().await.unwrap();
            if matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent)
                    if agent.index == 0
                        && agent.state == WorkflowAgentState::Start
                        && agent.attempt == expected_attempt
            ) {
                break;
            }
        }
        assert!(control.retry_agent(0));
    }

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("result:slow-retry-many"));
    assert_eq!(runtime.prompts().len(), 7);
    assert!(
        events_until_closed(event_rx)
            .await
            .iter()
            .any(|event| matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent)
                    if agent.index == 0
                        && agent.state == WorkflowAgentState::Done
                        && agent.attempt == 6
            ))
    );
}

async fn events_until_closed(
    mut events: tokio::sync::mpsc::UnboundedReceiver<WorkflowEvent>,
) -> Vec<WorkflowEvent> {
    let mut collected = Vec::new();
    while let Some(event) = events.recv().await {
        collected.push(event);
    }
    collected
}

#[tokio::test]
async fn agent_fan_out_continues_past_the_previous_total_limit() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let task_runtime = runtime.clone();
    let outcome = execute_workflow(
        &script(
            "const tasks = Array.from({ length: 1001 }, (_, index) => \
             () => agent(`worker-${index}`)); return parallel(tasks)",
        ),
        json!(null),
        task_runtime,
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            concurrency: 16,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result.as_array().unwrap().len(), 1001);
    assert!(
        outcome
            .result
            .as_array()
            .unwrap()
            .iter()
            .all(JsonValue::is_string)
    );
    assert_eq!(outcome.agent_count, 1001);
    assert_eq!(runtime.prompts().len(), 1001);
}

#[tokio::test]
async fn cancellation_terminates_cpu_bound_scripts() {
    let control = WorkflowControl::new();
    let stop = control.clone();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("while (true) {}"),
            json!(null),
            Arc::new(FakeAgentRuntime::default()),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig::default(),
            control,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    stop.stop();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap(),
        Err(WorkflowExecutionError::Cancelled)
    );
}

#[tokio::test]
async fn synchronous_watchdog_terminates_cpu_bound_scripts() {
    let result = execute_workflow(
        &script("while (true) {}"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            synchronous_timeout: Duration::from_millis(25),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await;

    assert!(matches!(
        result,
        Err(WorkflowExecutionError::Runtime(message))
            if message.contains("await workflow APIs between bounded computation steps")
    ));
}

#[tokio::test]
async fn terminal_runtime_errors_are_bounded_before_leaving_the_runtime() {
    let error = execute_workflow(
        &script(&format!(
            "throw new Error({:?})",
            "terminal-sentinel:".to_string() + &"界".repeat(MAX_TERMINAL_RUNTIME_ERROR_BYTES)
        )),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    let WorkflowExecutionError::Runtime(message) = error else {
        panic!("expected a runtime error");
    };
    assert!(message.contains("terminal-sentinel"));
    assert!(message.len() <= MAX_TERMINAL_RUNTIME_ERROR_BYTES);
}

#[tokio::test]
async fn hides_non_workflow_code_mode_globals() {
    let (outcome, _, _) = run(
        "return [typeof tools, typeof notify, typeof store, typeof text]",
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!(["undefined", "undefined", "undefined", "undefined"])
    );
}

#[tokio::test]
async fn sandbox_blocks_dynamic_code_imports_and_modern_nondeterminism() {
    let (outcome, _, _) = run(
        r#"
const blocked = [];
for (const generate of [
  () => eval("1 + 1"),
  () => Function("return 2")(),
  () => (async function () {}).constructor("return 3")(),
]) {
  try {
    generate();
    blocked.push(false);
  } catch (error) {
    blocked.push(error instanceof EvalError);
  }
}
try {
  await import("node:fs");
  blocked.push(false);
} catch (error) {
  blocked.push(String(error).toLowerCase().includes("unsupported import"));
}
return {
  blocked,
  temporal: typeof Temporal,
  frozen: [
    AggregateError,
    SuppressedError,
    DisposableStack,
    AsyncDisposableStack,
    Iterator,
    Float16Array,
  ].every(value => Object.isFrozen(value) && Object.isFrozen(value.prototype)),
};
"#,
        json!(null),
    )
    .await;

    assert_eq!(
        outcome.result,
        json!({
            "blocked": [true, true, true, true],
            "temporal": "undefined",
            "frozen": true,
        })
    );
}

#[tokio::test]
async fn journal_replays_matching_logical_invocations_independently() {
    let workflow = script(
        r#"
const first = await agent('cached-first');
const second = await agent('changed-second');
const third = await agent('cached-third');
return [first, second, third];
"#,
    );
    let options = WorkflowAgentOptions::default();
    let first_key = workflow_cache_key(
        &workflow_cache_root(&workflow, None),
        "root/agent:0",
        "cached-first",
        &options,
        AgentResultMode::Value,
        None,
    );
    let old_second_key = workflow_cache_key(
        &workflow_cache_root(&workflow, None),
        "root/agent:1",
        "old-second",
        &options,
        AgentResultMode::Value,
        None,
    );
    let old_third_key = workflow_cache_key(
        &workflow_cache_root(&workflow, None),
        "root/agent:2",
        "cached-third",
        &options,
        AgentResultMode::Value,
        None,
    );
    let cached = [
        (first_key, "cached-first"),
        (old_second_key, "old-second"),
        (old_third_key, "cached-third"),
    ]
    .into_iter()
    .map(|(key, prompt)| {
        (
            key,
            WorkflowAgentResult {
                value: json!(format!("replayed:{prompt}")),
                usage: WorkflowTokenUsage {
                    total_tokens: 99,
                    tool_uses: 4,
                },
                agent_id: Some(format!("cached-{prompt}")),
                model: Some("cached-model".to_string()),
                fallback_model: None,
            },
        )
    })
    .collect();
    let journal = Arc::new(FakeJournal::new(cached));
    let runtime = Arc::new(FakeAgentRuntime::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);

    let outcome = execute_workflow(
        &workflow,
        json!(null),
        runtime.clone(),
        Arc::new(move |_, event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        WorkflowRuntimeConfig {
            journal: Some(journal),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result,
        json!([
            "replayed:cached-first",
            "result:changed-second",
            "replayed:cached-third"
        ])
    );
    assert_eq!(runtime.prompts(), vec!["changed-second"]);
    assert_eq!(outcome.total_tokens, 10);
    assert!(
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|event| matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent) if agent.cached
            ))
    );
}

#[tokio::test]
async fn journal_rejects_cached_results_when_the_approved_script_changes() {
    let old_workflow = script("return agent('same-prompt')");
    let new_workflow = script(
        r#"
const result = await agent('same-prompt');
return { result };
"#,
    );
    let options = WorkflowAgentOptions::default();
    let old_key = workflow_cache_key(
        &workflow_cache_root(&old_workflow, None),
        "root/agent:0",
        "same-prompt",
        &options,
        AgentResultMode::Value,
        None,
    );
    let journal = Arc::new(FakeJournal::new(HashMap::from([(
        old_key,
        WorkflowAgentResult {
            value: json!("stale-result"),
            usage: WorkflowTokenUsage::default(),
            agent_id: None,
            model: None,
            fallback_model: None,
        },
    )])));
    let runtime = Arc::new(FakeAgentRuntime::default());

    let outcome = execute_workflow(
        &new_workflow,
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(journal),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!({ "result": "result:same-prompt" }));
    assert_eq!(runtime.prompts(), vec!["same-prompt"]);
}

#[tokio::test]
async fn child_workflow_inherits_phase_and_cannot_nest_again() {
    let child = validate_workflow_script(
        r#"export const meta = { name: 'child', description: 'child', phases: [{ title: 'Ignored' }] };
phase('Ignored');
return agent(`child:${args.target}`);
"#,
    )
    .unwrap();
    let resolver = Arc::new(FakeChildResolver {
        script: child,
        requests: Mutex::new(Vec::new()),
    });
    let runtime = Arc::new(FakeAgentRuntime::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);
    let outcome = execute_workflow(
        &script("phase('Run'); return workflow('child', { target: 'item' })"),
        json!(null),
        runtime.clone(),
        Arc::new(move |_, event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        WorkflowRuntimeConfig {
            child_resolver: Some(resolver.clone()),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!("result:child:item"));
    assert_eq!(outcome.agent_count, 1);
    assert_eq!(runtime.prompts(), vec!["child:item"]);
    assert!(
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|event| matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent)
                    if agent.phase_index == Some(0)
                        && agent.phase_title.as_deref() == Some("Run")
            ))
    );
    assert_eq!(
        resolver
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[WorkflowChildRequest {
            name_or_ref: json!("child"),
            args: json!({ "target": "item" }),
        }]
    );

    let nested_resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'nested', description: 'nested' }; return workflow('grandchild')",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });
    let nested = execute_workflow(
        &script("return workflow('child')"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(nested_resolver),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await;
    assert!(matches!(
        nested,
        Err(WorkflowExecutionError::Runtime(message)) if message.contains("call workflow() from the root workflow")
    ));
}

#[tokio::test]
async fn child_workflow_args_are_validated_before_resolution_and_child_startup() {
    let child = validate_workflow_script(
        "export const meta = { name: 'child', description: 'child' }; return args",
    )
    .unwrap();
    let resolver = Arc::new(FakeChildResolver {
        script: child.clone(),
        requests: Mutex::new(Vec::new()),
    });
    let request_error = execute_workflow(
        &script("return workflow('child', 9007199254740992)"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(resolver.clone()),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        request_error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
    assert_eq!(resolver.requests.lock().unwrap().as_slice(), &[]);

    let resolved_error = execute_workflow(
        &script("return workflow('child', null)"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(Arc::new(FixedArgsChildResolver {
                script: child,
                args: serde_json::from_str("18446744073709551616").unwrap(),
            })),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        resolved_error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );

    let child_script = validate_workflow_script(
        "export const meta = { name: 'child', description: 'child' }; return args",
    )
    .unwrap();
    let wide = JsonValue::Array(vec![JsonValue::Null; 5000]);
    let outcome = execute_workflow(
        &script("return workflow('child', null)"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(Arc::new(FixedArgsChildResolver {
                script: child_script.clone(),
                args: wide.clone(),
            })),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.result, wide);

    let mut deep = JsonValue::Null;
    for _ in 0..300 {
        deep = JsonValue::Array(vec![deep]);
    }
    let error = execute_workflow(
        &script("return workflow('child', null)"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(Arc::new(FixedArgsChildResolver {
                script: child_script,
                args: deep,
            })),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("flatter structured value"));
}

#[tokio::test]
async fn child_results_are_validated_before_returning_to_the_parent_isolate() {
    let resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'child', description: 'child' }; return 9007199254740992",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });

    let error = execute_workflow(
        &script("return workflow('child')"),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(resolver),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("represent exact integer identifiers as strings")
    );
}

#[tokio::test]
async fn parallel_child_sessions_use_independent_admission_past_old_cumulative_totals() {
    let resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'child', description: 'child' }; await agent('child:' + args); return args",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });
    let outcome = execute_workflow(
        &script(
            r#"
return parallel(
  Array.from({ length: 17 }, (_, index) => () => workflow('child', index)),
  { requireAll: true },
);
"#,
        ),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            concurrency: 4,
            child_resolver: Some(resolver.clone()),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!((0..17).collect::<Vec<_>>()));
    assert_eq!(resolver.requests.lock().unwrap().len(), 17);
    assert_eq!(outcome.agent_count, 17);
}

#[tokio::test]
async fn child_admission_is_held_through_result_persistence() {
    let resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'child', description: 'child' }; return { index: args }",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });
    let store = Arc::new(BlockingPutArtifactStore::new());
    let task_store = store.clone();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("return parallel([() => workflow('child', 0), () => workflow('child', 1)]);"),
            json!(null),
            Arc::new(FakeAgentRuntime::default()),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig {
                concurrency: 1,
                child_resolver: Some(resolver),
                input_artifact_store: task_store,
                ..WorkflowRuntimeConfig::default()
            },
            WorkflowControl::new(),
        )
        .await
    });

    store.wait_for_puts(1).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), store.wait_for_puts(2))
            .await
            .is_err()
    );
    store.release_one();
    store.wait_for_puts(2).await;
    store.release_one();

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!([{"index": 0}, {"index": 1}]));
    assert_eq!(store.started.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn cancelled_child_caller_keeps_admission_until_persistence_stops() {
    let resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'child', description: 'child' }; return { index: args }",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });
    let store = Arc::new(BlockingPutArtifactStore::new());
    let config = WorkflowRuntimeConfig {
        concurrency: 1,
        child_resolver: Some(resolver),
        input_artifact_store: store.clone(),
        ..WorkflowRuntimeConfig::default()
    };
    let control = WorkflowControl::new();
    let delegate = Arc::new(WorkflowDelegate::new(
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        config,
        Arc::clone(&control.state),
        "cancelled-child-persistence".to_string(),
    ));
    let child_input = |invocation_id: &str, index: usize| ChildToolInput {
        invocation_id: invocation_id.to_string(),
        name_or_ref: json!("child"),
        args: json!(index),
        phase_index: None,
        phase_title: None,
    };

    let first_delegate = Arc::clone(&delegate);
    let first = tokio::spawn(async move {
        first_delegate
            .invoke_child(
                child_input("first", 0),
                CancellationToken::new(),
                0,
                "root".to_string(),
            )
            .await
    });
    store.wait_for_puts(1).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let second_delegate = Arc::clone(&delegate);
    let second = tokio::spawn(async move {
        second_delegate
            .invoke_child(
                child_input("second", 1),
                CancellationToken::new(),
                0,
                "root".to_string(),
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), store.wait_for_puts(2))
            .await
            .is_err()
    );
    store.release_one();
    store.wait_for_puts(2).await;
    store.release_one();

    assert_eq!(second.await.unwrap().unwrap()["value"], json!({"index": 1}));
}

#[tokio::test]
async fn dynamic_progress_text_and_stall_timeout_have_host_side_limits() {
    let oversized_unicode = "界".repeat(100);
    for body in [
        format!("phase({oversized_unicode:?}); return null"),
        format!("return agent('bounded', {{ label: {oversized_unicode:?} }})"),
    ] {
        let result = execute_workflow(
            &script(&body),
            json!(null),
            Arc::new(FakeAgentRuntime::default()),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig::default(),
            WorkflowControl::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(WorkflowExecutionError::Runtime(message))
                if message.contains("use a concise")
        ));
    }

    let result = execute_workflow(
        &script(&format!(
            "return agent('bounded', {{ stallMs: {} }})",
            MAX_WORKFLOW_AGENT_STALL_MS + 1
        )),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(WorkflowExecutionError::Runtime(message))
            if message.contains("choose stallMs within the supported workflow agent timeout range")
    ));
}

#[test]
fn workflow_metadata_type_remains_stable() {
    let parsed = script("return null");

    assert_eq!(
        parsed.meta,
        WorkflowMeta {
            name: "test".to_string(),
            description: "test workflow".to_string(),
            title: None,
            when_to_use: None,
            phases: vec![WorkflowPhase {
                title: "Run".to_string(),
                detail: None,
                model: None,
            }],
            inputs: Vec::new(),
        }
    );
}

#[tokio::test]
async fn stalled_agents_auto_retry_exponentially_and_then_recover() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("return agent('stall-recover', { label: 'worker' })"),
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                stall_retries: 3,
                stall_retry_base_delay: Duration::from_millis(5),
                stall_retry_max_delay: Duration::from_millis(40),
                throttle_retry_delay: Duration::ZERO,
                ..WorkflowRuntimeConfig::default()
            },
            WorkflowControl::new(),
        )
        .await
    });

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("result:stall-recover"));
    assert_eq!(runtime.prompts().len(), 4);
    assert!(
        outcome
            .logs
            .iter()
            .any(|log| log.contains("made no progress") && log.contains("auto-retry"))
    );
    let events = events_until_closed(event_rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Done && agent.attempt == 3
    )));
}

#[tokio::test]
async fn stalled_agents_suspend_for_user_retry_and_skip() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_runtime = runtime.clone();
    let workflow = script("return agent('always-stall', { label: 'worker' })");
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                stall_retries: 3,
                stall_retry_base_delay: Duration::from_millis(1),
                stall_retry_max_delay: Duration::from_millis(4),
                throttle_retry_delay: Duration::ZERO,
                ..WorkflowRuntimeConfig::default()
            },
            task_control,
        )
        .await
    });

    for expected_attempts in [4, 5] {
        let awaiting = loop {
            let event = event_rx.recv().await.unwrap();
            if let WorkflowEvent::WorkflowAgent(agent) = event
                && agent.state == WorkflowAgentState::Error
                && agent.awaiting_decision
            {
                break agent;
            }
        };
        assert_eq!(runtime.prompts().len(), expected_attempts);
        assert_eq!(awaiting.label, "worker");
        if expected_attempts == 4 {
            assert!(control.retry_agent(0));
        } else {
            assert!(control.skip_agent(0));
        }
    }

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, JsonValue::Null);
    assert_eq!(runtime.prompts().len(), 5);
    let events = events_until_closed(event_rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Error
                && !agent.awaiting_decision
                && agent.skipped
    )));
}

#[test]
fn stall_retry_backoff_grows_exponentially_and_is_capped() {
    let base = Duration::from_secs(10);
    let unbounded = Duration::from_secs(1_000);
    assert_eq!(
        stall_retry_backoff(base, unbounded, 0),
        Duration::from_secs(10)
    );
    assert_eq!(
        stall_retry_backoff(base, unbounded, 1),
        Duration::from_secs(20)
    );
    assert_eq!(
        stall_retry_backoff(base, unbounded, 2),
        Duration::from_secs(40)
    );
    assert_eq!(
        stall_retry_backoff(base, Duration::from_secs(25), 3),
        Duration::from_secs(25)
    );
}

#[derive(Default)]
struct ProgressReportingRuntime {
    prompts: Mutex<Vec<String>>,
}

struct TerminalFailureUsageRuntime;

impl WorkflowAgentRuntime for TerminalFailureUsageRuntime {
    fn run_agent<'a>(
        &'a self,
        _request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async {
            Err(
                WorkflowAgentFailure::failed("failed after completion").with_usage(
                    WorkflowTokenUsage {
                        total_tokens: 9,
                        tool_uses: 2,
                    },
                ),
            )
        })
    }
}

struct CancellationUsageRuntime;

impl WorkflowAgentRuntime for CancellationUsageRuntime {
    fn run_agent<'a>(
        &'a self,
        _request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(
                WorkflowAgentFailure::failed("cancelled with final usage").with_usage(
                    WorkflowTokenUsage {
                        total_tokens: 13,
                        tool_uses: 3,
                    },
                ),
            )
        })
    }
}

struct ThrottleThenStallRuntime;

impl WorkflowAgentRuntime for ThrottleThenStallRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            let usage = WorkflowTokenUsage {
                total_tokens: u64::from(request.attempt + 1),
                tool_uses: 1,
            };
            match request.attempt {
                0 => Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Throttled,
                    message: "throttled".to_string(),
                    usage,
                }),
                1 | 2 => Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "stalled".to_string(),
                    usage,
                }),
                _ => Ok(WorkflowAgentResult {
                    value: json!("recovered"),
                    usage,
                    agent_id: None,
                    model: None,
                    fallback_model: None,
                }),
            }
        })
    }
}

struct UserRetryThenStallRuntime;

impl WorkflowAgentRuntime for UserRetryThenStallRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            let usage = WorkflowTokenUsage {
                total_tokens: u64::from(request.attempt + 1),
                tool_uses: 1,
            };
            match request.attempt {
                0 => {
                    cancellation.cancelled().await;
                    Err(WorkflowAgentFailure::failed("cancelled for retry").with_usage(usage))
                }
                1 | 2 => Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "stalled".to_string(),
                    usage,
                }),
                _ => Ok(WorkflowAgentResult {
                    value: json!("recovered"),
                    usage,
                    agent_id: None,
                    model: None,
                    fallback_model: None,
                }),
            }
        })
    }
}

#[tokio::test]
async fn failed_attempt_uses_terminal_usage_without_progress_callbacks() {
    let outcome = execute_workflow(
        &script("return agentSettled('failure-usage')"),
        json!(null),
        Arc::new(TerminalFailureUsageRuntime),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.total_tokens, 9);
    assert_eq!(outcome.total_tool_calls, 2);
}

#[tokio::test]
async fn cancelled_attempt_is_awaited_for_terminal_usage() {
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("return agentSettled('cancel-usage')"),
            json!(null),
            Arc::new(CancellationUsageRuntime),
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig::default(),
            task_control,
        )
        .await
    });
    loop {
        if matches!(
            event_rx.recv().await.unwrap(),
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 0 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.skip_agent(0));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.total_tokens, 13);
    assert_eq!(outcome.total_tool_calls, 3);
}

#[tokio::test]
async fn stall_retry_count_is_independent_from_throttle_attempts() {
    let outcome = execute_workflow(
        &script("return agent('throttle-then-stall')"),
        json!(null),
        Arc::new(ThrottleThenStallRuntime),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            stall_retries: 2,
            stall_retry_base_delay: Duration::ZERO,
            stall_retry_max_delay: Duration::ZERO,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!("recovered"));
    assert_eq!(outcome.total_tokens, 10);
    assert_eq!(outcome.total_tool_calls, 4);
}

#[tokio::test]
async fn stall_retry_count_is_independent_from_user_retry_attempts() {
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("return agent('retry-then-stall')"),
            json!(null),
            Arc::new(UserRetryThenStallRuntime),
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                stall_retries: 2,
                stall_retry_base_delay: Duration::ZERO,
                stall_retry_max_delay: Duration::ZERO,
                ..WorkflowRuntimeConfig::default()
            },
            task_control,
        )
        .await
    });
    loop {
        if matches!(
            event_rx.recv().await.unwrap(),
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 0 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.retry_agent(0));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("recovered"));
    assert_eq!(outcome.total_tokens, 10);
    assert_eq!(outcome.total_tool_calls, 4);
}

struct RetryUsageRuntime;

impl WorkflowAgentRuntime for RetryUsageRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
                usage: WorkflowTokenUsage {
                    total_tokens: 7,
                    tool_uses: 2,
                },
                agent_id: None,
                model: None,
                fallback_model: None,
            })
        })
    }

    fn run_agent_with_progress<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
        _on_started: WorkflowAgentStartedCallback<'a>,
        _on_progress: WorkflowAgentProgressCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            if request.attempt == 0 {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "stalled with terminal usage".to_string(),
                    usage: WorkflowTokenUsage {
                        total_tokens: 5,
                        tool_uses: 1,
                    },
                });
            }
            Ok(WorkflowAgentResult {
                value: json!("recovered"),
                usage: WorkflowTokenUsage {
                    total_tokens: 7,
                    tool_uses: 2,
                },
                agent_id: None,
                model: None,
                fallback_model: None,
            })
        })
    }
}

#[tokio::test]
async fn failed_attempt_usage_is_counted_once_before_automatic_retry() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);
    let outcome = execute_workflow(
        &script("return agent('usage-retry', { label: 'worker' })"),
        json!(null),
        Arc::new(RetryUsageRuntime),
        Arc::new(move |_, event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        WorkflowRuntimeConfig {
            stall_retries: 1,
            stall_retry_base_delay: Duration::ZERO,
            stall_retry_max_delay: Duration::ZERO,
            initial_usage: WorkflowTokenUsage {
                total_tokens: 20,
                tool_uses: 3,
            },
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!("recovered"));
    assert_eq!(outcome.total_tokens, 32);
    assert_eq!(outcome.total_tool_calls, 6);
    assert!(
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|event| matches!(
                event,
                WorkflowEvent::WorkflowAgent(agent)
                    if agent.state == WorkflowAgentState::Done
                        && agent.tokens == Some(12)
                        && agent.tool_calls == Some(3)
            ))
    );
}

impl WorkflowAgentRuntime for ProgressReportingRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.prompt.clone());
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
                usage: WorkflowTokenUsage {
                    total_tokens: 25,
                    tool_uses: 2,
                },
                agent_id: None,
                model: None,
                fallback_model: None,
            })
        })
    }

    fn run_agent_with_progress<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
        on_started: WorkflowAgentStartedCallback<'a>,
        on_progress: WorkflowAgentProgressCallback<'a>,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.prompt.clone());
            on_started(format!("agent-{}", request.index));
            on_progress(WorkflowAgentProgressUpdate {
                usage: WorkflowTokenUsage {
                    total_tokens: 10,
                    tool_uses: 1,
                },
                activity: None,
            })
            .await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            on_progress(WorkflowAgentProgressUpdate {
                usage: WorkflowTokenUsage {
                    total_tokens: 25,
                    tool_uses: 2,
                },
                activity: None,
            })
            .await;
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
                usage: WorkflowTokenUsage {
                    total_tokens: 25,
                    tool_uses: 2,
                },
                agent_id: None,
                model: None,
                fallback_model: None,
            })
        })
    }
}

#[tokio::test]
async fn agent_live_progress_reports_token_and_tool_usage() {
    let runtime = Arc::new(ProgressReportingRuntime::default());
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        execute_workflow(
            &script("return agent('progress', { label: 'worker' })"),
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig::default(),
            WorkflowControl::new(),
        )
        .await
    });

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("result:progress"));
    assert_eq!(outcome.total_tokens, 25);
    let events = events_until_closed(event_rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Start
                && agent.tokens == Some(10)
                && agent.tool_calls == Some(1)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Start
                && agent.tokens == Some(25)
                && agent.tool_calls == Some(2)
    )));
}

#[derive(Default)]
struct CachingJournal {
    results: Mutex<HashMap<String, WorkflowJournalResult>>,
}

struct FailingStartedJournal;

struct FailingReplayJournal;

struct FailingResultJournal;

impl WorkflowJournal for FailingStartedJournal {
    fn replay<'a>(&'a self, _key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        Box::pin(async { Ok(None) })
    }

    fn append_started(&self, _key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Err("durability unavailable".to_string()) })
    }

    fn append_result(
        &self,
        _key: String,
        _result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

impl WorkflowJournal for FailingReplayJournal {
    fn replay<'a>(&'a self, _key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        Box::pin(async { Err("source journal is corrupt".to_string()) })
    }

    fn append_started(&self, _key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn append_result(
        &self,
        _key: String,
        _result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

impl WorkflowJournal for FailingResultJournal {
    fn replay<'a>(&'a self, _key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        Box::pin(async { Ok(None) })
    }

    fn append_started(&self, _key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn append_result(
        &self,
        _key: String,
        _result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Err("result storage unavailable".to_string()) })
    }
}

impl WorkflowJournal for CachingJournal {
    fn replay<'a>(&'a self, key: &'a str) -> WorkflowJournalReplayFuture<'a> {
        Box::pin(async move {
            Ok(self
                .results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .cloned())
        })
    }

    fn append_started(&self, _key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn append_result(
        &self,
        key: String,
        result: WorkflowJournalResult,
    ) -> WorkflowJournalFuture<'_> {
        Box::pin(async move {
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, result);
            Ok(())
        })
    }
}

#[tokio::test]
async fn journal_started_failure_prevents_agent_execution() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let error = execute_workflow(
        &script("return agent('must-not-run')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(Arc::new(FailingStartedJournal)),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("could not durably start agent agent-1")
    );
    assert!(runtime.requests().is_empty());
}

#[tokio::test]
async fn journal_replay_failure_prevents_agent_execution() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let error = execute_workflow(
        &script("return agent('must-not-run')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(Arc::new(FailingReplayJournal)),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow journal replay failed for agent agent-1")
    );
    assert!(runtime.requests().is_empty());
}

#[tokio::test]
async fn journal_result_failure_prevents_successful_workflow_completion() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let error = execute_workflow(
        &script("return agent('completed-once')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(Arc::new(FailingResultJournal)),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow journal could not persist result for agent agent-1")
    );
    assert_eq!(runtime.requests().len(), 1);
}

#[tokio::test]
async fn journal_result_failure_prevents_settled_failure_completion() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let error = execute_workflow(
        &script("return agentSettled('fail')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(Arc::new(FailingResultJournal)),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow journal could not persist agent failure")
    );
    assert_eq!(runtime.requests().len(), 1);
}

#[tokio::test]
async fn workflow_cancellation_is_not_replayed_on_resume() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let journal = Arc::new(CachingJournal::default());
    let control = WorkflowControl::new();
    let stop = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_runtime = runtime.clone();
    let first_journal = journal.clone();
    let first = tokio::spawn(async move {
        execute_workflow(
            &script("return agent('slow-cancel-resume')"),
            json!(null),
            first_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                journal: Some(first_journal),
                ..WorkflowRuntimeConfig::default()
            },
            control,
        )
        .await
    });
    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent) if agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    stop.stop();
    assert_eq!(first.await.unwrap(), Err(WorkflowExecutionError::Cancelled));

    let resumed = execute_workflow(
        &script("return agent('slow-cancel-resume')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(journal),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(resumed.result, json!("result:slow-cancel-resume"));
    assert_eq!(
        runtime.prompts(),
        vec!["slow-cancel-resume", "slow-cancel-resume"]
    );
}

#[tokio::test]
async fn explicit_user_skip_remains_replayable() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let journal = Arc::new(CachingJournal::default());
    let control = WorkflowControl::new();
    let skip = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_runtime = runtime.clone();
    let first_journal = journal.clone();
    let first = tokio::spawn(async move {
        execute_workflow(
            &script("return agentSettled('slow-explicit-skip')"),
            json!(null),
            first_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                journal: Some(first_journal),
                ..WorkflowRuntimeConfig::default()
            },
            control,
        )
        .await
    });
    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent) if agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(skip.skip_agent(0));
    let expected = json!({
        "status": "rejected",
        "reason": {"kind": "skipped", "message": "skipped by user"},
    });
    assert_eq!(first.await.unwrap().unwrap().result, expected);

    let replayed = execute_workflow(
        &script("return agentSettled('slow-explicit-skip')"),
        json!(null),
        runtime.clone(),
        Arc::new(|_, _| {}),
        WorkflowRuntimeConfig {
            journal: Some(journal),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(replayed.result, expected);
    assert_eq!(runtime.prompts(), vec!["slow-explicit-skip"]);
}

#[tokio::test]
async fn journal_success_metadata_cannot_collide_with_model_values() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let journal = Arc::new(CachingJournal::default());
    let mut outcomes = Vec::new();
    for _ in 0..2 {
        outcomes.push(
            execute_workflow(
                &script("return agentSettled('envelope-output')"),
                json!(null),
                runtime.clone(),
                Arc::new(|_, _| {}),
                WorkflowRuntimeConfig {
                    journal: Some(journal.clone()),
                    ..WorkflowRuntimeConfig::default()
                },
                WorkflowControl::new(),
            )
            .await
            .unwrap()
            .result,
        );
    }

    let expected = json!({
        "status": "fulfilled",
        "value": {
            "status": "rejected",
            "reason": { "kind": "skipped", "message": "model value" },
        },
    });
    assert_eq!(outcomes, vec![expected.clone(), expected]);
    assert_eq!(runtime.prompts(), vec!["envelope-output"]);
}

#[tokio::test]
async fn journal_identity_replays_equal_inputs_and_changes_with_aliases_or_values() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let journal = Arc::new(CachingJournal::default());
    let workflow = script("return agent('analyze', { inputs: { [args.alias]: args.value } })");

    for args in [
        json!({"alias": "report", "value": {"a": 1, "b": [2, 3]}}),
        json!({"alias": "report", "value": {"b": [2, 3], "a": 1}}),
        json!({"alias": "other", "value": {"a": 1, "b": [2, 3]}}),
        json!({"alias": "report", "value": {"a": 1, "b": [2, 4]}}),
    ] {
        execute_workflow(
            &workflow,
            args,
            runtime.clone(),
            Arc::new(|_, _| {}),
            WorkflowRuntimeConfig {
                journal: Some(journal.clone()),
                ..WorkflowRuntimeConfig::default()
            },
            WorkflowControl::new(),
        )
        .await
        .unwrap();
    }

    let requests = runtime.requests();
    assert_eq!(requests.len(), 3);
    let mut resolved_inputs = Vec::new();
    for request in &requests {
        let resolved = request
            .inputs
            .as_ref()
            .expect("structured inputs")
            .resolve_shared()
            .await
            .unwrap();
        resolved_inputs.push(resolved.references().values().next().unwrap().clone());
    }
    assert_eq!(resolved_inputs[0], resolved_inputs[1]);
    assert_ne!(resolved_inputs[0], resolved_inputs[2]);
}

fn rerun_boundary_harness(
    body: &str,
) -> (
    String,
    Arc<WorkflowDelegate>,
    Arc<FakeAgentRuntime>,
    WorkflowControl,
) {
    let workflow = script(body);
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let config = WorkflowRuntimeConfig {
        journal: Some(Arc::new(CachingJournal::default())),
        ..WorkflowRuntimeConfig::default()
    };
    let source = compile_workflow_source_with_context(
        &workflow,
        &json!(null),
        WorkflowScriptContext {
            result_tool_name: Some(RESULT_TOOL_NAME.to_string()),
            ..WorkflowScriptContext::default()
        },
    )
    .unwrap();
    let delegate = Arc::new(WorkflowDelegate::new(
        runtime.clone(),
        Arc::new(|_, _| {}),
        config.clone(),
        Arc::clone(&control.state),
        workflow_cache_root(&workflow, config.definition_sha256.as_deref()),
    ));
    (source, delegate, runtime, control)
}

async fn run_rerun_boundary_attempt(
    source: &str,
    delegate: &Arc<WorkflowDelegate>,
    control: &WorkflowControl,
) -> (
    Option<usize>,
    Result<WorkflowSourceResult, WorkflowExecutionError>,
) {
    let (rerun_from, rerun_receiver) = control.state.take_rerun_from();
    let execution_generation = delegate.begin_session(rerun_from);
    let result = run_workflow_source(
        source.to_string(),
        Arc::clone(delegate),
        Arc::clone(&control.state),
        CancellationToken::new(),
        Duration::from_secs(30),
        RESULT_TOOL_NAME.to_string(),
        /*allow_child*/ false,
        Some(rerun_receiver),
        execution_generation,
        "root".to_string(),
    )
    .await;
    (rerun_from, result)
}

#[tokio::test]
async fn agent_settled_failure_replays_from_the_journal() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let journal = Arc::new(CachingJournal::default());
    let workflow = script("return agentSettled('long-fail')");
    let mut outcomes = Vec::new();

    for _ in 0..2 {
        outcomes.push(
            execute_workflow(
                &workflow,
                json!(null),
                runtime.clone(),
                Arc::new(|_, _| {}),
                WorkflowRuntimeConfig {
                    journal: Some(journal.clone()),
                    throttle_retry_delay: Duration::ZERO,
                    ..WorkflowRuntimeConfig::default()
                },
                WorkflowControl::new(),
            )
            .await
            .unwrap(),
        );
    }

    assert_eq!(outcomes[0].result, outcomes[1].result);
    assert_eq!(outcomes[0].failures, outcomes[1].failures);
    assert_eq!(runtime.prompts(), vec!["long-fail"]);
}

#[test]
fn settled_mode_has_a_distinct_cache_identity() {
    let options = WorkflowAgentOptions::default();
    let root = "script";

    assert_ne!(
        workflow_cache_key(
            root,
            "root/agent:0",
            "prompt",
            &options,
            AgentResultMode::Value,
            None,
        ),
        workflow_cache_key(
            root,
            "root/agent:0",
            "prompt",
            &options,
            AgentResultMode::Settled,
            None,
        ),
    );
}

#[tokio::test]
async fn rerun_from_replayed_agent_settled_failure_is_not_duplicated() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let task_runtime = runtime.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script(
        "const failure = await agentSettled('fail'); \
         await agent('slow'); \
         return failure",
    );
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                journal: Some(Arc::new(CachingJournal::default())),
                ..WorkflowRuntimeConfig::default()
            },
            task_control,
        )
        .await
    });

    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 1 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.rerun_from(1));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.failures, vec!["agent-1: requested failure"]);
    assert_eq!(
        runtime.prompts(),
        vec!["fail".to_string(), "slow".to_string(), "slow".to_string()]
    );
    let events = events_until_closed(event_rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.index == 0
                && agent.state == WorkflowAgentState::Error
                && agent.cached
    )));
}

#[tokio::test]
async fn rerun_from_re_executes_the_agent_and_recomputes_downstream() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let task_runtime = runtime.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script(
        "const a = await agent('chain-0'); \
         const b = await agent('chain-1-slow', { inputs: { upstream: a } }); \
         return agent('chain-2', { inputs: { upstream: b } })",
    );
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |_, event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                journal: Some(Arc::new(CachingJournal::default())),
                ..WorkflowRuntimeConfig::default()
            },
            task_control,
        )
        .await
    });

    // Wait for the downstream agent to start before requesting the rerun, so
    // chain-1 has already settled and chain-2 already ran downstream of it.
    loop {
        let event = event_rx.recv().await.unwrap();
        if matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 2 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.rerun_from(1));

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.result, json!("result:chain-2"));
    assert_eq!(
        runtime.prompts(),
        vec![
            "chain-0".to_string(),
            "chain-1-slow".to_string(),
            "chain-2".to_string(),
            "chain-1-slow".to_string(),
            "chain-2".to_string(),
        ]
    );
    assert!(
        outcome
            .logs
            .iter()
            .any(|log| log.contains("re-executing from") && log.contains("recomputed"))
    );
    let events = events_until_closed(event_rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.index == 0 && agent.state == WorkflowAgentState::Done && agent.cached
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.index == 2 && agent.state == WorkflowAgentState::Done
    )));
}

#[tokio::test]
async fn success_completion_reruns_from_the_earliest_pending_agent() {
    let (source, delegate, runtime, control) = rerun_boundary_harness(
        "await agent('success-0'); \
         await agent('success-1'); \
         return agent('success-2')",
    );

    let (rerun_from, first) = run_rerun_boundary_attempt(&source, &delegate, &control).await;
    assert_eq!(rerun_from, None);
    assert_eq!(first.unwrap().value, json!("result:success-2"));

    assert!(control.rerun_from(0));
    assert!(control.rerun_from(2));
    assert!(matches!(
        control.state.finish_success(),
        WorkflowCompletionDecision::Rerun
    ));

    let (rerun_from, second) = run_rerun_boundary_attempt(&source, &delegate, &control).await;
    assert_eq!(rerun_from, Some(0));
    assert_eq!(second.unwrap().value, json!("result:success-2"));
    assert!(matches!(
        control.state.finish_success(),
        WorkflowCompletionDecision::Complete
    ));
    assert_eq!(
        runtime.prompts(),
        vec![
            "success-0".to_string(),
            "success-1".to_string(),
            "success-2".to_string(),
            "success-0".to_string(),
            "success-1".to_string(),
            "success-2".to_string(),
        ]
    );
}

#[tokio::test]
async fn error_completion_rerun_is_not_consumed_by_a_buffered_wake() {
    let (source, delegate, runtime, control) = rerun_boundary_harness(
        "await agent('error-boundary'); \
         return JSON.parse('{')",
    );

    let (rerun_from, first) = run_rerun_boundary_attempt(&source, &delegate, &control).await;
    assert_eq!(rerun_from, None);
    assert!(matches!(first, Err(WorkflowExecutionError::Runtime(_))));

    assert!(control.rerun_from(0));
    assert!(matches!(
        control.state.finish_error(),
        WorkflowCompletionDecision::Rerun
    ));

    let (rerun_from, second) = run_rerun_boundary_attempt(&source, &delegate, &control).await;
    assert_eq!(rerun_from, Some(0));
    assert!(matches!(second, Err(WorkflowExecutionError::Runtime(_))));
    assert!(matches!(
        control.state.finish_error(),
        WorkflowCompletionDecision::Complete
    ));
    assert_eq!(
        runtime.prompts(),
        vec!["error-boundary".to_string(), "error-boundary".to_string()]
    );
    assert!(!control.rerun_from(0));
}
