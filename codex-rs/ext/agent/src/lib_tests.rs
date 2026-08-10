use super::*;
use codex_protocol::error::CodexErrorDetails;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Default)]
struct FutureProbe {
    active: AtomicUsize,
    completed: AtomicUsize,
    dropped: AtomicUsize,
    started: AtomicUsize,
}

impl FutureProbe {
    fn guard(&self) -> FutureProbeGuard<'_> {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.started.fetch_add(1, Ordering::Relaxed);
        FutureProbeGuard { probe: self }
    }

    fn assert_cancelled(&self) {
        assert_eq!(self.started.load(Ordering::Relaxed), 1);
        assert_eq!(self.completed.load(Ordering::Relaxed), 0);
        assert_eq!(self.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(self.active.load(Ordering::Relaxed), 0);
    }

    fn assert_completed(&self) {
        assert_eq!(self.started.load(Ordering::Relaxed), 1);
        assert_eq!(self.completed.load(Ordering::Relaxed), 1);
        assert_eq!(self.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(self.active.load(Ordering::Relaxed), 0);
    }

    async fn assert_stays_inactive(&self) {
        let started = self.started.load(Ordering::Relaxed);
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(self.started.load(Ordering::Relaxed), started);
        assert_eq!(self.active.load(Ordering::Relaxed), 0);
    }
}

struct FutureProbeGuard<'a> {
    probe: &'a FutureProbe,
}

impl FutureProbeGuard<'_> {
    fn complete(self) {
        self.probe.completed.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for FutureProbeGuard<'_> {
    fn drop(&mut self) {
        self.probe.active.fetch_sub(1, Ordering::Relaxed);
        self.probe.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct ScheduledEvent {
    cancel_on_ready: Option<CancellationToken>,
    delay: Duration,
    event: AgentCompletionEvent,
    probe: Arc<FutureProbe>,
}

struct ScheduledSample {
    delay: Duration,
    tokens: Option<u64>,
}

struct TestCompletionSource {
    actual_work: AgentActualWork,
    completion_usage_delay: Duration,
    completion_usage_probe: Arc<FutureProbe>,
    events: Mutex<VecDeque<ScheduledEvent>>,
    final_progress_delay: Duration,
    final_progress_probe: Arc<FutureProbe>,
    force_terminate_calls: AtomicUsize,
    force_terminate_delay: Duration,
    force_terminate_probe: Arc<FutureProbe>,
    force_termination: CancellationToken,
    interrupt_delay: Duration,
    interrupt_probe: Arc<FutureProbe>,
    sample_probe: Arc<FutureProbe>,
    samples: Mutex<VecDeque<ScheduledSample>>,
    status: AgentStatus,
    thread_id: ThreadId,
    tokens: AtomicU64,
}

impl TestCompletionSource {
    fn new(events: Vec<ScheduledEvent>) -> Self {
        Self {
            actual_work: AgentActualWork::None,
            events: Mutex::new(events.into()),
            completion_usage_delay: Duration::ZERO,
            completion_usage_probe: Arc::new(FutureProbe::default()),
            final_progress_delay: Duration::ZERO,
            final_progress_probe: Arc::new(FutureProbe::default()),
            force_terminate_calls: AtomicUsize::new(0),
            force_terminate_delay: Duration::ZERO,
            force_terminate_probe: Arc::new(FutureProbe::default()),
            force_termination: CancellationToken::new(),
            interrupt_delay: Duration::ZERO,
            interrupt_probe: Arc::new(FutureProbe::default()),
            sample_probe: Arc::new(FutureProbe::default()),
            samples: Mutex::new(VecDeque::new()),
            status: AgentStatus::Running,
            thread_id: ThreadId::new(),
            tokens: AtomicU64::new(0),
        }
    }

    fn with_actual_work(mut self, actual_work: AgentActualWork) -> Self {
        self.actual_work = actual_work;
        self
    }

    fn with_final_progress_delay(mut self, delay: Duration) -> Self {
        self.final_progress_delay = delay;
        self
    }

    fn with_completion_usage_delay(mut self, delay: Duration) -> Self {
        self.completion_usage_delay = delay;
        self
    }

    fn with_force_terminate_delay(mut self, delay: Duration) -> Self {
        self.force_terminate_delay = delay;
        self
    }

    fn with_interrupt_delay(mut self, delay: Duration) -> Self {
        self.interrupt_delay = delay;
        self
    }

    fn with_samples(mut self, samples: Vec<ScheduledSample>) -> Self {
        self.samples = Mutex::new(samples.into());
        self
    }

    fn with_status(mut self, status: AgentStatus) -> Self {
        self.status = status;
        self
    }
}

impl AgentCompletionSource for Arc<TestCompletionSource> {
    fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    async fn submit_interrupt(&self) {
        let guard = self.interrupt_probe.guard();
        tokio::time::sleep(self.interrupt_delay).await;
        guard.complete();
    }

    async fn force_terminate(&self, timeout: Duration) -> ThreadTeardownStatus {
        self.force_terminate_calls.fetch_add(1, Ordering::Relaxed);
        self.force_termination.cancel();
        let guard = self.force_terminate_probe.guard();
        if tokio::time::timeout(timeout, tokio::time::sleep(self.force_terminate_delay))
            .await
            .is_err()
        {
            return ThreadTeardownStatus::TimedOut;
        }
        guard.complete();
        ThreadTeardownStatus::Confirmed
    }

    async fn next_completion_event(&self) -> CodexResult<AgentCompletionEvent> {
        let scheduled = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or(CodexErr::InternalAgentDied)?;
        let guard = scheduled.probe.guard();
        tokio::time::sleep(scheduled.delay).await;
        if let Some(cancellation) = scheduled.cancel_on_ready {
            cancellation.cancel();
        }
        guard.complete();
        Ok(scheduled.event)
    }

    async fn sample_usage(&self) -> Option<TokenUsageInfo> {
        let guard = self.sample_probe.guard();
        let sample = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(ScheduledSample {
                delay: Duration::from_secs(60),
                tokens: None,
            });
        if !sample.delay.is_zero() {
            tokio::time::sleep(sample.delay).await;
        }
        if let Some(tokens) = sample.tokens {
            self.tokens.store(tokens, Ordering::Release);
        }
        let tokens = self.tokens.load(Ordering::Acquire);
        guard.complete();
        Some(token_usage(tokens))
    }

    async fn final_progress(&self, tool_uses: u64) -> AgentRunProgress {
        let guard = self.final_progress_probe.guard();
        tokio::time::sleep(self.final_progress_delay).await;
        let progress = AgentRunProgress {
            tokens: self.tokens.load(Ordering::Acquire),
            tool_uses,
            activity: None,
        };
        guard.complete();
        progress
    }

    async fn completion_token_usage(&self) -> Option<TokenUsageInfo> {
        let guard = self.completion_usage_probe.guard();
        tokio::time::sleep(self.completion_usage_delay).await;
        let usage = Some(token_usage(self.tokens.load(Ordering::Acquire)));
        guard.complete();
        usage
    }

    async fn agent_status(&self) -> AgentStatus {
        self.status.clone()
    }

    async fn actual_work(&self, active_tool_count: usize) -> AgentActualWork {
        if active_tool_count > 0 {
            AgentActualWork::ActiveTool
        } else {
            self.actual_work
        }
    }
}

fn token_usage(tokens: u64) -> TokenUsageInfo {
    TokenUsageInfo {
        total_token_usage: codex_protocol::protocol::TokenUsage {
            total_tokens: i64::try_from(tokens).expect("test token count should fit in i64"),
            ..Default::default()
        },
        last_token_usage: Default::default(),
        model_context_window: None,
    }
}

fn scheduled_event(
    delay: Duration,
    event: AgentCompletionEvent,
) -> (ScheduledEvent, Arc<FutureProbe>) {
    let probe = Arc::new(FutureProbe::default());
    (
        ScheduledEvent {
            cancel_on_ready: None,
            delay,
            event,
            probe: Arc::clone(&probe),
        },
        probe,
    )
}

fn runner() -> AgentRunner {
    AgentRunner::new(Weak::new())
}

async fn wait_for_probe(probe: &FutureProbe) {
    wait_for_probe_starts(probe, 1).await;
}

async fn wait_for_probe_starts(probe: &FutureProbe, starts: usize) {
    while probe.started.load(Ordering::Relaxed) < starts {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_tokens(source: &TestCompletionSource, tokens: u64) {
    while source.tokens.load(Ordering::Acquire) != tokens {
        tokio::task::yield_now().await;
    }
}

fn expect_wait_error(result: Result<AgentCompletion, AgentRunError>) -> AgentRunError {
    match result {
        Ok(_) => panic!("wait should return an error"),
        Err(error) => error,
    }
}

#[tokio::test(start_paused = true)]
async fn wait_for_completion_completes_without_interrupting() {
    let (tool, tool_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    let (completed, completed_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Completed {
            output: "done".to_string(),
            error: None,
        },
    );
    let source = Arc::new(TestCompletionSource::new(vec![tool, completed]));

    let completion = runner()
        .wait_for_completion(
            Arc::clone(&source),
            /*progress_timeout*/ None,
            CancellationToken::new(),
            /*on_progress*/ None,
        )
        .await
        .expect("normal completion should succeed");

    assert_eq!(completion.thread_id, source.thread_id);
    assert_eq!(completion.output, "done");
    assert_eq!(completion.tool_uses, 1);
    assert_eq!(completion.signal, AgentCompletionSignal::Event);
    assert_eq!(source.interrupt_probe.started.load(Ordering::Relaxed), 0);
    tool_probe.assert_completed();
    completed_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn terminal_status_recovers_when_completion_event_does_not_arrive() {
    let (activity, activity_probe) =
        scheduled_event(Duration::ZERO, AgentCompletionEvent::CurrentActivity);
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let source = Arc::new(
        TestCompletionSource::new(vec![activity, waiting])
            .with_samples(vec![ScheduledSample {
                delay: Duration::ZERO,
                tokens: Some(23),
            }])
            .with_status(AgentStatus::Completed(Some("recovered result".to_string()))),
    );
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = |_| Box::pin(async {}) as AgentProgressFuture<'_>;
            runner()
                .wait_for_completion(
                    source,
                    Some(Duration::from_secs(180)),
                    CancellationToken::new(),
                    Some(&on_progress),
                )
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;

    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    tokio::time::advance(TERMINAL_STATUS_EVENT_GRACE_PERIOD).await;
    let completion = task
        .await
        .expect("wait task should finish")
        .expect("terminal status should recover completion");

    assert_eq!(completion.thread_id, source.thread_id);
    assert_eq!(completion.output, "recovered result");
    assert_eq!(completion.token_usage, Some(token_usage(23)));
    assert_eq!(completion.tool_uses, 0);
    assert_eq!(completion.signal, AgentCompletionSignal::TerminalStatus);
    activity_probe.assert_completed();
    waiting_probe.assert_cancelled();
    assert_eq!(source.interrupt_probe.started.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn workflow_input_analysis_lifecycle_is_reported_from_turn_items() {
    let events = [
        AgentCompletionEvent::ToolStarted(Some(AgentRunActivity::AnalyzingWorkflowInputs)),
        AgentCompletionEvent::ToolCompleted(Some(AgentRunActivity::AnalyzingWorkflowInputs)),
        AgentCompletionEvent::Completed {
            output: "done".to_string(),
            error: None,
        },
    ]
    .into_iter()
    .map(|event| scheduled_event(Duration::ZERO, event).0)
    .collect();
    let source = Arc::new(TestCompletionSource::new(events));
    let progress = Arc::new(Mutex::new(Vec::new()));
    let callback_progress = Arc::clone(&progress);
    let callback = move |update| {
        callback_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(update);
        Box::pin(std::future::ready(())) as AgentProgressFuture<'_>
    };

    runner()
        .wait_for_completion(
            source,
            /*progress_timeout*/ None,
            CancellationToken::new(),
            Some(&callback),
        )
        .await
        .expect("agent completes");

    assert_eq!(
        *progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            AgentRunProgress {
                tokens: 0,
                tool_uses: 1,
                activity: Some(AgentRunActivity::AnalyzingWorkflowInputs),
            },
            AgentRunProgress {
                tokens: 0,
                tool_uses: 1,
                activity: None,
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn normal_completion_preserves_observed_usage_when_final_state_read_times_out() {
    let expected_usage = token_usage(47);
    let (usage, usage_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Usage(Some(expected_usage.clone())),
    );
    let (completed, completed_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Completed {
            output: "done".to_string(),
            error: None,
        },
    );
    let source = Arc::new(
        TestCompletionSource::new(vec![usage, completed])
            .with_completion_usage_delay(Duration::from_secs(60)),
    );
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, CancellationToken::new(), None)
                .await
        })
    };
    wait_for_probe(&source.completion_usage_probe).await;

    tokio::time::advance(LIVE_PROGRESS_STATE_READ_TIMEOUT).await;
    let completion = task
        .await
        .expect("wait task should finish")
        .expect("normal completion should succeed");

    assert_eq!(completion.token_usage, Some(expected_usage));
    usage_probe.assert_completed();
    completed_probe.assert_completed();
    source.completion_usage_probe.assert_cancelled();
}

#[tokio::test(start_paused = true)]
async fn wait_for_completion_cancellation_preserves_usage_without_double_counting_tools() {
    let (first_tool, first_tool_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (drained_tool, drained_tool_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    let (usage, usage_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Usage(Some(token_usage(43))),
    );
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![
        first_tool,
        waiting,
        drained_tool,
        usage,
        aborted,
    ]));
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;

    cancellation.cancel();
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Codex { error, .. }
            if matches!(error.details(), CodexErrorDetails::Interrupted)
    ));
    assert_eq!(
        error.progress(),
        AgentRunProgress {
            tokens: 43,
            tool_uses: 2,
            activity: None,
        }
    );
    first_tool_probe.assert_completed();
    waiting_probe.assert_cancelled();
    drained_tool_probe.assert_completed();
    usage_probe.assert_completed();
    aborted_probe.assert_completed();
    source.interrupt_probe.assert_completed();
    source.final_progress_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn wait_for_completion_stall_reports_stalled_after_bounded_shutdown() {
    let progress_timeout = Duration::from_secs(5);
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (shutdown, shutdown_probe) =
        scheduled_event(Duration::ZERO, AgentCompletionEvent::Shutdown);
    let source = Arc::new(TestCompletionSource::new(vec![waiting, shutdown]));
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;

    tokio::time::advance(progress_timeout).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Stalled { timeout, .. } if *timeout == progress_timeout
    ));
    waiting_probe.assert_cancelled();
    shutdown_probe.assert_completed();
    source.interrupt_probe.assert_completed();
    source.final_progress_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn stall_deadline_extends_while_a_tool_is_active() {
    let progress_timeout = Duration::from_secs(5);
    let (tool, tool_probe) = scheduled_event(
        Duration::from_secs(1),
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    let (dropped_after_deadline, _dropped_probe) = scheduled_event(
        Duration::from_secs(5),
        AgentCompletionEvent::Completed {
            output: "unused".to_string(),
            error: None,
        },
    );
    let (completed, completed_probe) = scheduled_event(
        Duration::from_secs(4),
        AgentCompletionEvent::Completed {
            output: "tool finished".to_string(),
            error: None,
        },
    );
    let source = Arc::new(TestCompletionSource::new(vec![
        tool,
        dropped_after_deadline,
        completed,
    ]));
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    wait_for_probe(&tool_probe).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    while tool_probe.completed.load(Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;

    tokio::time::advance(progress_timeout - Duration::from_secs(1)).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let completion = task
        .await
        .expect("wait task should finish")
        .expect("active tool should extend the stall deadline");

    assert_eq!(completion.output, "tool finished");
    assert_eq!(source.force_terminate_calls.load(Ordering::Relaxed), 0);
    completed_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn stall_deadline_extends_while_tracked_process_or_model_stream_is_active() {
    for actual_work in [
        AgentActualWork::TrackedProcess,
        AgentActualWork::ModelStream,
    ] {
        let progress_timeout = Duration::from_secs(5);
        let (dropped_after_deadline, _dropped_probe) = scheduled_event(
            Duration::from_secs(6),
            AgentCompletionEvent::Completed {
                output: "unused".to_string(),
                error: None,
            },
        );
        let (completed, completed_probe) = scheduled_event(
            Duration::from_secs(4),
            AgentCompletionEvent::Completed {
                output: "work finished".to_string(),
                error: None,
            },
        );
        let source = Arc::new(
            TestCompletionSource::new(vec![dropped_after_deadline, completed])
                .with_actual_work(actual_work),
        );
        let task = {
            let source = Arc::clone(&source);
            tokio::spawn(async move {
                runner()
                    .wait_for_completion(
                        source,
                        Some(progress_timeout),
                        CancellationToken::new(),
                        None,
                    )
                    .await
            })
        };

        tokio::time::advance(progress_timeout).await;
        tokio::time::advance(progress_timeout).await;
        let completion = task
            .await
            .expect("wait task should finish")
            .expect("concrete host work should extend the stall deadline");

        assert_eq!(completion.output, "work finished");
        assert_eq!(source.force_terminate_calls.load(Ordering::Relaxed), 0);
        completed_probe.assert_completed();
    }
}

#[tokio::test(start_paused = true)]
async fn simultaneous_cancellation_terminal_and_progress_tick_choose_cancellation() {
    let cancellation = CancellationToken::new();
    let (mut completed, completed_probe) = scheduled_event(
        LIVE_PROGRESS_REPORT_INTERVAL,
        AgentCompletionEvent::Completed {
            output: "late completion".to_string(),
            error: None,
        },
    );
    completed.cancel_on_ready = Some(cancellation.clone());
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![completed, aborted]).with_samples(vec![ScheduledSample {
            delay: Duration::ZERO,
            tokens: Some(19),
        }]),
    );
    let callback_called = Arc::new(AtomicUsize::new(0));
    let task = {
        let callback_called = Arc::clone(&callback_called);
        let cancellation = cancellation.clone();
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = move |_progress| -> AgentProgressFuture<'static> {
                callback_called.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {})
            };
            runner()
                .wait_for_completion(source, None, cancellation, Some(&on_progress))
                .await
        })
    };
    wait_for_probe(&completed_probe).await;

    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Codex { error, .. }
            if matches!(error.details(), CodexErrorDetails::Interrupted)
    ));
    assert_eq!(callback_called.load(Ordering::Relaxed), 0);
    completed_probe.assert_completed();
    aborted_probe.assert_completed();
    source.interrupt_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn cancellation_drops_an_in_flight_progress_callback() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, aborted]).with_samples(vec![ScheduledSample {
            delay: Duration::ZERO,
            tokens: Some(13),
        }]),
    );
    let callback_probe = Arc::new(FutureProbe::default());
    let cancellation = CancellationToken::new();
    let task = {
        let callback_probe = Arc::clone(&callback_probe);
        let cancellation = cancellation.clone();
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = move |_progress| -> AgentProgressFuture<'static> {
                let callback_probe = Arc::clone(&callback_probe);
                Box::pin(async move {
                    let _guard = callback_probe.guard();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                })
            };
            runner()
                .wait_for_completion(source, None, cancellation, Some(&on_progress))
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    wait_for_probe(&callback_probe).await;

    cancellation.cancel();
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Codex { error, .. }
            if matches!(error.details(), CodexErrorDetails::Interrupted)
    ));
    callback_probe.assert_cancelled();
    waiting_probe.assert_cancelled();
    aborted_probe.assert_completed();
    callback_probe.assert_stays_inactive().await;
}

#[tokio::test(start_paused = true)]
async fn stall_drops_an_in_flight_progress_callback() {
    let progress_timeout = Duration::from_secs(5);
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, aborted]).with_samples(vec![ScheduledSample {
            delay: Duration::ZERO,
            tokens: Some(17),
        }]),
    );
    let callback_probe = Arc::new(FutureProbe::default());
    let task = {
        let callback_probe = Arc::clone(&callback_probe);
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = move |_progress| -> AgentProgressFuture<'static> {
                let callback_probe = Arc::clone(&callback_probe);
                Box::pin(async move {
                    let _guard = callback_probe.guard();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                })
            };
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    Some(&on_progress),
                )
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    wait_for_probe(&callback_probe).await;

    tokio::time::advance(progress_timeout - LIVE_PROGRESS_REPORT_INTERVAL).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        error,
        AgentRunError::Stalled { timeout, .. } if timeout == progress_timeout
    ));
    callback_probe.assert_cancelled();
    waiting_probe.assert_cancelled();
    aborted_probe.assert_completed();
    callback_probe.assert_stays_inactive().await;
}

#[tokio::test(start_paused = true)]
async fn simultaneous_stall_and_terminal_choose_stall() {
    let progress_timeout = Duration::from_secs(5);
    let (completed, completed_probe) = scheduled_event(
        progress_timeout,
        AgentCompletionEvent::Completed {
            output: "late completion".to_string(),
            error: None,
        },
    );
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![completed, aborted]));
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    wait_for_probe(&completed_probe).await;

    tokio::time::advance(progress_timeout).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Stalled { timeout, .. } if *timeout == progress_timeout
    ));
    completed_probe.assert_cancelled();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn event_winner_accounts_tool_before_secondary_cancellation_check() {
    let cancellation = CancellationToken::new();
    let (mut tool, tool_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    tool.cancel_on_ready = Some(cancellation.clone());
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![tool, aborted]));

    let error = expect_wait_error(
        runner()
            .wait_for_completion(source, None, cancellation, None)
            .await,
    );

    assert_eq!(
        error.progress(),
        AgentRunProgress {
            tokens: 0,
            tool_uses: 1,
            activity: None,
        }
    );
    tool_probe.assert_completed();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn event_winner_accounts_usage_before_secondary_cancellation_check() {
    let cancellation = CancellationToken::new();
    let (mut usage, usage_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Usage(Some(token_usage(61))),
    );
    usage.cancel_on_ready = Some(cancellation.clone());
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![usage, aborted]));

    let error = expect_wait_error(
        runner()
            .wait_for_completion(source, None, cancellation, None)
            .await,
    );

    assert_eq!(
        error.progress(),
        AgentRunProgress {
            tokens: 61,
            tool_uses: 0,
            activity: None,
        }
    );
    usage_probe.assert_completed();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn cancellation_preempts_blocked_progress_state_read() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, aborted]).with_samples(vec![ScheduledSample {
            delay: Duration::from_secs(60),
            tokens: Some(29),
        }]),
    );
    let callback_called = Arc::new(AtomicUsize::new(0));
    let cancellation = CancellationToken::new();
    let task = {
        let callback_called = Arc::clone(&callback_called);
        let cancellation = cancellation.clone();
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = move |_progress| -> AgentProgressFuture<'static> {
                callback_called.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {})
            };
            runner()
                .wait_for_completion(source, None, cancellation, Some(&on_progress))
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    wait_for_probe(&source.sample_probe).await;

    cancellation.cancel();
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Codex { error, .. }
            if matches!(error.details(), CodexErrorDetails::Interrupted)
    ));
    assert_eq!(callback_called.load(Ordering::Relaxed), 0);
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
    waiting_probe.assert_cancelled();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn blocked_progress_state_read_times_out_without_callback() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (waiting_after_timeout, waiting_after_timeout_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, waiting_after_timeout, aborted]).with_samples(
            vec![ScheduledSample {
                delay: Duration::from_secs(60),
                tokens: Some(29),
            }],
        ),
    );
    let callback_called = Arc::new(AtomicUsize::new(0));
    let cancellation = CancellationToken::new();
    let task = {
        let callback_called = Arc::clone(&callback_called);
        let cancellation = cancellation.clone();
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let on_progress = move |_progress| -> AgentProgressFuture<'static> {
                callback_called.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {})
            };
            runner()
                .wait_for_completion(source, None, cancellation, Some(&on_progress))
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(LIVE_PROGRESS_REPORT_INTERVAL).await;
    wait_for_probe(&source.sample_probe).await;

    tokio::time::advance(LIVE_PROGRESS_STATE_READ_TIMEOUT).await;
    wait_for_probe(&waiting_after_timeout_probe).await;
    assert_eq!(callback_called.load(Ordering::Relaxed), 0);
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
    assert_eq!(source.sample_probe.completed.load(Ordering::Relaxed), 0);
    assert_eq!(source.sample_probe.dropped.load(Ordering::Relaxed), 1);

    cancellation.cancel();
    let _ = expect_wait_error(task.await.expect("wait task should finish"));
    waiting_probe.assert_cancelled();
    waiting_after_timeout_probe.assert_cancelled();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn other_turn_events_do_not_reset_stall_deadline() {
    let progress_timeout = Duration::from_secs(5);
    let (other_one, other_one_probe) =
        scheduled_event(Duration::from_secs(1), AgentCompletionEvent::OtherTurn);
    let (other_two, other_two_probe) =
        scheduled_event(Duration::from_secs(1), AgentCompletionEvent::OtherTurn);
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![
        other_one, other_two, waiting, aborted,
    ]));
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    wait_for_probe(&other_one_probe).await;

    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_probe(&other_two_probe).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Stalled { timeout, .. } if *timeout == progress_timeout
    ));
    other_one_probe.assert_completed();
    other_two_probe.assert_completed();
    waiting_probe.assert_cancelled();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn current_turn_activity_resets_stall_deadline() {
    let progress_timeout = Duration::from_secs(5);
    let (activity, activity_probe) = scheduled_event(
        Duration::from_secs(4),
        AgentCompletionEvent::CurrentActivity,
    );
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (aborted, aborted_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(TestCompletionSource::new(vec![activity, waiting, aborted]));
    let task = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            runner()
                .wait_for_completion(
                    source,
                    Some(progress_timeout),
                    CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    wait_for_probe(&activity_probe).await;

    tokio::time::advance(Duration::from_secs(4)).await;
    wait_for_probe(&waiting_probe).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert!(matches!(
        &error,
        AgentRunError::Stalled { timeout, .. } if *timeout == progress_timeout
    ));
    activity_probe.assert_completed();
    waiting_probe.assert_cancelled();
    aborted_probe.assert_completed();
}

#[tokio::test(start_paused = true)]
async fn shutdown_deadline_cancels_blocked_interrupt_submission() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting])
            .with_interrupt_delay(Duration::from_secs(60))
            .with_force_terminate_delay(Duration::from_secs(60)),
    );
    let cancellation = CancellationToken::new();
    let child_probe = Arc::new(FutureProbe::default());
    let child = {
        let child_probe = Arc::clone(&child_probe);
        let force_termination = source.force_termination.clone();
        tokio::spawn(async move {
            let guard = child_probe.guard();
            force_termination.cancelled().await;
            guard.complete();
        })
    };
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&child_probe).await;
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&source.interrupt_probe).await;
    wait_for_probe(&source.sample_probe).await;

    tokio::time::advance(TERMINAL_SHUTDOWN_TIMEOUT - FORCE_CLOSE_TEARDOWN_RESERVE).await;
    wait_for_probe(&source.force_terminate_probe).await;
    tokio::task::yield_now().await;
    assert!(child.is_finished());
    assert!(!task.is_finished());
    tokio::time::advance(FORCE_CLOSE_TEARDOWN_RESERVE).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));
    child.await.expect("forced child termination should finish");

    assert!(matches!(&error, AgentRunError::TeardownTimedOut { .. }));
    waiting_probe.assert_cancelled();
    source.interrupt_probe.assert_cancelled();
    assert_eq!(source.force_terminate_calls.load(Ordering::Relaxed), 1);
    source.force_terminate_probe.assert_cancelled();
    child_probe.assert_completed();
    assert_eq!(
        source.final_progress_probe.started.load(Ordering::Relaxed),
        0
    );
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
    assert_eq!(source.sample_probe.completed.load(Ordering::Relaxed), 0);
    assert_eq!(
        source.sample_probe.started.load(Ordering::Relaxed),
        source.sample_probe.dropped.load(Ordering::Relaxed)
    );
    source.interrupt_probe.assert_stays_inactive().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_deadline_cancels_blocked_terminal_drain() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (draining, draining_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let source = Arc::new(TestCompletionSource::new(vec![waiting, draining]));
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&draining_probe).await;
    wait_for_probe(&source.sample_probe).await;

    tokio::time::advance(TERMINAL_SHUTDOWN_TIMEOUT).await;
    let _ = expect_wait_error(task.await.expect("wait task should finish"));

    waiting_probe.assert_cancelled();
    draining_probe.assert_cancelled();
    source.interrupt_probe.assert_completed();
    assert_eq!(
        source.final_progress_probe.started.load(Ordering::Relaxed),
        0
    );
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
    assert_eq!(
        source.sample_probe.started.load(Ordering::Relaxed),
        source.sample_probe.dropped.load(Ordering::Relaxed)
    );
    draining_probe.assert_stays_inactive().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_deadline_cancels_blocked_final_state_and_usage_snapshots() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (terminal, terminal_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, terminal])
            .with_final_progress_delay(Duration::from_secs(60))
            .with_samples(vec![ScheduledSample {
                delay: Duration::ZERO,
                tokens: Some(37),
            }]),
    );
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&source.final_progress_probe).await;
    wait_for_probe(&source.sample_probe).await;
    wait_for_tokens(&source, 37).await;

    tokio::time::advance(TERMINAL_SHUTDOWN_TIMEOUT).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert_eq!(error.progress().tokens, 37);
    waiting_probe.assert_cancelled();
    terminal_probe.assert_completed();
    source.final_progress_probe.assert_cancelled();
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
    assert_eq!(
        source.sample_probe.started.load(Ordering::Relaxed),
        source.sample_probe.dropped.load(Ordering::Relaxed)
    );
    source.final_progress_probe.assert_stays_inactive().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_keeps_freshest_concurrent_usage_before_deadline() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (draining, draining_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, draining]).with_samples(vec![
            ScheduledSample {
                delay: Duration::from_millis(500),
                tokens: Some(17),
            },
            ScheduledSample {
                delay: Duration::from_millis(500),
                tokens: Some(31),
            },
        ]),
    );
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&draining_probe).await;
    wait_for_probe(&source.sample_probe).await;

    tokio::time::advance(Duration::from_millis(500)).await;
    wait_for_tokens(&source, 17).await;
    tokio::time::advance(SHUTDOWN_USAGE_SAMPLE_INTERVAL).await;
    wait_for_probe_starts(&source.sample_probe, 2).await;
    tokio::time::advance(Duration::from_millis(500)).await;
    wait_for_tokens(&source, 31).await;
    tokio::time::advance(Duration::from_millis(950)).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert_eq!(error.progress().tokens, 31);
    waiting_probe.assert_cancelled();
    draining_probe.assert_cancelled();
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_force_close_boundary_accepts_completed_final_snapshot() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (terminal, terminal_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, terminal])
            .with_final_progress_delay(TERMINAL_SHUTDOWN_TIMEOUT - FORCE_CLOSE_TEARDOWN_RESERVE),
    );
    source.tokens.store(53, Ordering::Release);
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&source.final_progress_probe).await;

    tokio::time::advance(TERMINAL_SHUTDOWN_TIMEOUT).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert_eq!(
        error.progress(),
        AgentRunProgress {
            tokens: 53,
            tool_uses: 0,
            activity: None,
        }
    );
    waiting_probe.assert_cancelled();
    terminal_probe.assert_completed();
    source.final_progress_probe.assert_completed();
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_force_close_boundary_keeps_completed_usage_snapshot() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (draining, draining_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, draining]).with_samples(vec![ScheduledSample {
            delay: TERMINAL_SHUTDOWN_TIMEOUT - FORCE_CLOSE_TEARDOWN_RESERVE,
            tokens: Some(71),
        }]),
    );
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&draining_probe).await;
    wait_for_probe(&source.sample_probe).await;

    tokio::time::advance(TERMINAL_SHUTDOWN_TIMEOUT).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert_eq!(error.progress().tokens, 71);
    waiting_probe.assert_cancelled();
    draining_probe.assert_cancelled();
    assert_eq!(source.sample_probe.completed.load(Ordering::Relaxed), 1);
    assert_eq!(source.sample_probe.active.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_keeps_usage_and_tools_published_during_force_close_reserve() {
    let (waiting, waiting_probe) =
        scheduled_event(Duration::from_secs(60), AgentCompletionEvent::OtherTurn);
    let (tool, tool_probe) = scheduled_event(
        TERMINAL_SHUTDOWN_TIMEOUT - FORCE_CLOSE_TEARDOWN_RESERVE + Duration::from_millis(50),
        AgentCompletionEvent::ToolStarted(/*activity*/ None),
    );
    let (usage, usage_probe) = scheduled_event(
        Duration::ZERO,
        AgentCompletionEvent::Usage(Some(token_usage(37))),
    );
    let (terminal, terminal_probe) = scheduled_event(Duration::ZERO, AgentCompletionEvent::Aborted);
    let source = Arc::new(
        TestCompletionSource::new(vec![waiting, tool, usage, terminal])
            .with_final_progress_delay(Duration::from_secs(60))
            .with_force_terminate_delay(FORCE_CLOSE_TEARDOWN_RESERVE - Duration::from_millis(50)),
    );
    let cancellation = CancellationToken::new();
    let task = {
        let source = Arc::clone(&source);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runner()
                .wait_for_completion(source, None, cancellation, None)
                .await
        })
    };
    wait_for_probe(&waiting_probe).await;
    cancellation.cancel();
    wait_for_probe(&tool_probe).await;

    tokio::time::advance(
        TERMINAL_SHUTDOWN_TIMEOUT - FORCE_CLOSE_TEARDOWN_RESERVE + Duration::from_millis(50),
    )
    .await;
    wait_for_probe(&source.final_progress_probe).await;
    tokio::time::advance(FORCE_CLOSE_TEARDOWN_RESERVE - Duration::from_millis(50)).await;
    let error = expect_wait_error(task.await.expect("wait task should finish"));

    assert_eq!(
        error.progress(),
        AgentRunProgress {
            tokens: 37,
            tool_uses: 1,
            activity: None,
        }
    );
    waiting_probe.assert_cancelled();
    tool_probe.assert_completed();
    usage_probe.assert_completed();
    terminal_probe.assert_completed();
    source.final_progress_probe.assert_cancelled();
    source.force_terminate_probe.assert_completed();
}
