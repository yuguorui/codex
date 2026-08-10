use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::WorkflowMeta;
use crate::WorkflowPhase;
use crate::WorkflowTokenUsage;
use crate::validate_workflow_script;

#[derive(Default)]
struct FakeAgentRuntime {
    prompts: Mutex<Vec<String>>,
}

struct FakeJournal {
    replaying: Mutex<bool>,
    cached: Mutex<HashMap<String, WorkflowAgentResult>>,
    started: Mutex<Vec<String>>,
    written: Mutex<Vec<String>>,
}

struct FakeBudget {
    total: u64,
    spent: AtomicU64,
}

impl WorkflowBudget for FakeBudget {
    fn total(&self) -> u64 {
        self.total
    }

    fn spent(&self) -> u64 {
        self.spent.load(Ordering::Acquire)
    }
}

struct BudgetAgentRuntime {
    budget: Arc<FakeBudget>,
}

struct ConcurrentBudgetAgentRuntime {
    budget: Arc<FakeBudget>,
    barrier: Arc<Barrier>,
    prompts: Mutex<Vec<String>>,
}

impl WorkflowAgentRuntime for BudgetAgentRuntime {
    fn run_agent<'a>(
        &'a self,
        request: WorkflowAgentRequest,
        _cancellation: CancellationToken,
    ) -> WorkflowAgentFuture<'a> {
        Box::pin(async move {
            self.budget.spent.fetch_add(10, Ordering::AcqRel);
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
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

impl WorkflowAgentRuntime for ConcurrentBudgetAgentRuntime {
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
            self.barrier.wait().await;
            self.budget.spent.fetch_add(10, Ordering::AcqRel);
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
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

impl FakeJournal {
    fn new(cached: HashMap<String, WorkflowAgentResult>) -> Self {
        Self {
            replaying: Mutex::new(true),
            cached: Mutex::new(cached),
            started: Mutex::new(Vec::new()),
            written: Mutex::new(Vec::new()),
        }
    }
}

impl WorkflowJournal for FakeJournal {
    fn replay(&self, key: &str) -> Option<WorkflowAgentResult> {
        let mut replaying = self
            .replaying
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*replaying {
            return None;
        }
        let result = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned();
        if result.is_none() {
            *replaying = false;
        }
        result
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
        _result: WorkflowAgentResult,
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

impl FakeAgentRuntime {
    fn prompts(&self) -> Vec<String> {
        self.prompts
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
                });
            }
            if request.prompt.contains("stall") && request.attempt < 3 {
                return Err(WorkflowAgentFailure {
                    kind: WorkflowAgentFailureKind::Stalled,
                    message: "agent made no progress for 180s".to_string(),
                });
            }
            if request.prompt.contains("fail") {
                if request.prompt.contains("terminal-api") {
                    return Err(WorkflowAgentFailure {
                        kind: WorkflowAgentFailureKind::TerminalApi,
                        message: "terminal API failure".to_string(),
                    });
                }
                return Err(WorkflowAgentFailure::failed("requested failure"));
            }
            Ok(WorkflowAgentResult {
                value: json!(format!("result:{}", request.prompt)),
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
    let runtime = Arc::new(FakeAgentRuntime::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&events);
    let outcome = execute_workflow(
        &script(body),
        args,
        runtime.clone(),
        Arc::new(move |event| {
            event_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }),
        WorkflowRuntimeConfig {
            concurrency: 4,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
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
async fn syntax_errors_fail_before_any_agents_run() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let result = execute_workflow(
        &script("const first = agent('must not run'); text(]"),
        json!(null),
        runtime.clone(),
        Arc::new(|_| {}),
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
async fn direct_terminal_api_failure_returns_null_and_records_the_failure() {
    let (outcome, _, events) = run("return agent('terminal-api-fail')", json!(null)).await;

    assert_eq!(outcome.result, JsonValue::Null);
    assert_eq!(outcome.failures, vec!["agent-1: terminal API failure"]);
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::WorkflowAgent(agent)
            if agent.state == WorkflowAgentState::Error
                && agent.error.as_deref() == Some("terminal API failure")
    )));
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
async fn bounds_workflow_log_count_and_message_size() {
    let body = format!(
        r#"
log("x".repeat({}));
for (let index = 0; index < {}; index += 1) log(String(index));
return null;
"#,
        MAX_LOG_MESSAGE_BYTES + 1,
        MAX_WORKFLOW_LOGS,
    );

    let (outcome, _, events) = run(&body, json!(null)).await;

    assert_eq!(outcome.logs.len(), MAX_WORKFLOW_LOGS);
    assert_eq!(outcome.logs[0].len(), MAX_LOG_MESSAGE_BYTES);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, WorkflowEvent::WorkflowLog { .. }))
            .count(),
        MAX_WORKFLOW_LOGS
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
        json!([64, "workflow supports at most 64 active timers"])
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
            "Date.now() is nondeterministic in workflows",
            "Date.now() is nondeterministic in workflows",
            "Date() is nondeterministic in workflows",
            "Math.random() is nondeterministic in workflows",
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
            Arc::new(move |event| {
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
            Arc::new(move |event| {
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
async fn retry_agent_at_limit_fails_instead_of_settling_as_skipped() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script("return agent('slow-retry-limit', { label: 'worker' })");
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            runtime,
            Arc::new(move |event| {
                let _ = event_tx.send(event);
            }),
            WorkflowRuntimeConfig {
                max_agent_retries: 0,
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
                if agent.index == 0 && agent.state == WorkflowAgentState::Start
        ) {
            break;
        }
    }
    assert!(control.retry_agent(0));

    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("retry limit reached"));
    assert!(events_until_closed(event_rx).await.iter().any(|event| {
        matches!(
            event,
            WorkflowEvent::WorkflowAgent(agent)
                if agent.index == 0
                    && agent.state == WorkflowAgentState::Error
                    && !agent.skipped
                    && agent.error.as_deref().is_some_and(|error| error.contains("retry limit reached"))
        )
    }));
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
async fn enforces_the_agent_cap() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let outcome = execute_workflow(
        &script("return parallel([() => agent('one'), () => agent('two')])"),
        json!(null),
        runtime,
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            concurrency: 2,
            max_agents: 1,
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result.as_array().unwrap().len(), 2);
    assert!(
        outcome
            .result
            .as_array()
            .unwrap()
            .contains(&JsonValue::Null)
    );
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
            Arc::new(|_| {}),
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
        Arc::new(|_| {}),
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
            if message.contains("synchronous execution exceeded")
    ));
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
async fn journal_replays_only_the_unchanged_prefix() {
    let workflow = script(
        r#"
const first = await agent('cached-first');
const second = await agent('changed-second');
const third = await agent('cached-third');
return [first, second, third];
"#,
    );
    let options = WorkflowAgentOptions::default();
    let first_key = workflow_cache_key(&workflow_cache_root(&workflow), "cached-first", &options);
    let old_second_key = workflow_cache_key(&first_key, "old-second", &options);
    let old_third_key = workflow_cache_key(&old_second_key, "cached-third", &options);
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
        Arc::new(move |event| {
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
            "result:cached-third"
        ])
    );
    assert_eq!(runtime.prompts(), vec!["changed-second", "cached-third"]);
    assert_eq!(outcome.total_tokens, 20);
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
    let old_key = workflow_cache_key(&workflow_cache_root(&old_workflow), "same-prompt", &options);
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
        Arc::new(|_| {}),
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
async fn exposes_live_shared_budget_and_stops_calls_at_the_ceiling() {
    let budget = Arc::new(FakeBudget {
        total: 100,
        spent: AtomicU64::new(90),
    });
    let runtime = Arc::new(BudgetAgentRuntime {
        budget: Arc::clone(&budget),
    });
    let outcome = execute_workflow(
        &script(
            r#"
const first = await agent('one');
const second = await parallel([() => agent('two')]);
return [first, second[0], budget.total, budget.spent(), budget.remaining()];
"#,
        ),
        json!(null),
        runtime,
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            budget: Some(WorkflowBudgetSource::Shared(budget)),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!(["result:one", null, 100, 100, 0]));
    assert_eq!(outcome.agent_count, 1);
    assert_eq!(outcome.total_tokens, 10);
    assert!(
        outcome
            .logs
            .iter()
            .any(|log| log.contains("workflow token budget exceeded"))
    );
    assert!(
        outcome
            .logs
            .iter()
            .all(|log| !log.contains("read only property"))
    );
}

#[tokio::test]
async fn static_budget_tracks_workflow_tokens_without_a_host_budget() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let outcome = execute_workflow(
        &script(
            r#"
const first = await agent('one');
return [first, budget.total, budget.spent(), budget.remaining()];
"#,
        ),
        json!(null),
        runtime,
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            budget: Some(WorkflowBudgetSource::Fixed(25)),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, json!(["result:one", 25, 10, 15]));
}

#[tokio::test]
async fn budget_preserves_in_flight_parallel_results_and_blocks_later_agents() {
    let budget = Arc::new(FakeBudget {
        total: 25,
        spent: AtomicU64::new(0),
    });
    let runtime = Arc::new(ConcurrentBudgetAgentRuntime {
        budget: Arc::clone(&budget),
        barrier: Arc::new(Barrier::new(3)),
        prompts: Mutex::new(Vec::new()),
    });
    let outcome = execute_workflow(
        &script(
            r#"
const active = await parallel([
  () => agent('one'),
  () => agent('two'),
  () => agent('three'),
]);
const blocked = await parallel([() => agent('blocked')]);
return [...active, blocked[0], budget.spent(), budget.remaining()];
"#,
        ),
        json!(null),
        runtime.clone(),
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            budget: Some(WorkflowBudgetSource::Shared(budget)),
            throttle_retry_delay: Duration::ZERO,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result,
        json!(["result:one", "result:two", "result:three", null, 30, 0])
    );
    assert_eq!(outcome.agent_count, 3);
    assert_eq!(outcome.total_tokens, 30);
    let mut prompts = runtime
        .prompts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    prompts.sort_unstable();
    assert_eq!(prompts, vec!["one", "three", "two"]);
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
        Arc::new(move |event| {
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
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(nested_resolver),
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await;
    assert!(matches!(
        nested,
        Err(WorkflowExecutionError::Runtime(message)) if message.contains("nesting is limited to one level")
    ));
}

#[tokio::test]
async fn child_workflow_session_count_has_a_hard_configurable_limit() {
    let resolver = Arc::new(FakeChildResolver {
        script: validate_workflow_script(
            "export const meta = { name: 'child', description: 'child' }; return args",
        )
        .unwrap(),
        requests: Mutex::new(Vec::new()),
    });
    let outcome = execute_workflow(
        &script(
            r#"
const first = await workflow('child', 1);
try {
  await workflow('child', 2);
  return [first, 'missing cap error'];
} catch (error) {
  return [first, error.message];
}
"#,
        ),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_| {}),
        WorkflowRuntimeConfig {
            child_resolver: Some(resolver.clone()),
            max_child_sessions: 1,
            ..WorkflowRuntimeConfig::default()
        },
        WorkflowControl::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.result,
        json!([
            1,
            "WorkflowChildSessionCapError: workflow exceeds the 1 child session limit"
        ])
    );
    assert_eq!(resolver.requests.lock().unwrap().len(), 2);
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
            Arc::new(|_| {}),
            WorkflowRuntimeConfig::default(),
            WorkflowControl::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(WorkflowExecutionError::Runtime(message))
                if message.contains("exceeds the 256-byte limit")
        ));
    }

    let result = execute_workflow(
        &script(&format!(
            "return agent('bounded', {{ stallMs: {} }})",
            MAX_WORKFLOW_AGENT_STALL_MS + 1
        )),
        json!(null),
        Arc::new(FakeAgentRuntime::default()),
        Arc::new(|_| {}),
        WorkflowRuntimeConfig::default(),
        WorkflowControl::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(WorkflowExecutionError::Runtime(message))
            if message.contains("stallMs exceeds the 1800000ms limit")
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
            Arc::new(move |event| {
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
            Arc::new(move |event| {
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
            if let WorkflowEvent::WorkflowAgent(agent) = event {
                if agent.state == WorkflowAgentState::Error && agent.awaiting_decision {
                    break agent;
                }
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
            on_progress(WorkflowTokenUsage {
                total_tokens: 10,
                tool_uses: 1,
            });
            tokio::time::sleep(Duration::from_millis(5)).await;
            on_progress(WorkflowTokenUsage {
                total_tokens: 25,
                tool_uses: 2,
            });
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
            Arc::new(move |event| {
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
    results: Mutex<HashMap<String, WorkflowAgentResult>>,
}

impl WorkflowJournal for CachingJournal {
    fn replay(&self, key: &str) -> Option<WorkflowAgentResult> {
        self.results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn append_started(&self, _key: String) -> WorkflowJournalFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn append_result(&self, key: String, result: WorkflowAgentResult) -> WorkflowJournalFuture<'_> {
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
async fn rerun_from_re_executes_the_agent_and_recomputes_downstream() {
    let runtime = Arc::new(FakeAgentRuntime::default());
    let control = WorkflowControl::new();
    let task_control = control.clone();
    let task_runtime = runtime.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let workflow = script(
        "const a = await agent('chain-0'); \
         const b = await agent('chain-1-slow'); \
         return agent('chain-2')",
    );
    let task = tokio::spawn(async move {
        execute_workflow(
            &workflow,
            json!(null),
            task_runtime,
            Arc::new(move |event| {
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
