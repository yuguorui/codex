use super::*;
use pretty_assertions::assert_eq;

#[test]
fn high_index_active_agent_remains_controllable() {
    let control = WorkflowControl::new();
    let cancellation = CancellationToken::new();
    let action = Arc::new(AtomicUsize::new(AgentAction::None as usize));
    control
        .state
        .agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            5_000,
            ActiveAgentControl {
                action: Arc::clone(&action),
                cancellation: cancellation.clone(),
            },
        );
    let active = active_agent(5_000);
    control
        .state
        .record_agent(/*execution_generation*/ 7, active.clone());

    assert!(control.skip_agent(5_000));
    assert_eq!(control.agent_progress(5_000), Some(active));
    assert!(cancellation.is_cancelled());
    assert_eq!(action.load(Ordering::Acquire), AgentAction::Skip as usize);
}

#[test]
fn high_index_settled_rerun_does_not_retain_history_in_runtime() {
    let control = WorkflowControl::new();
    for index in 0..10_000 {
        control
            .state
            .record_agent(/*execution_generation*/ 7, settled_agent(index));
    }

    assert_eq!(control.agent_progress(5_000), None);
    let invocations = control
        .state
        .invocations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(invocations.by_key.is_empty());
    assert!(invocations.latest_by_index.is_empty());
    drop(invocations);
    assert!(control.rerun_from(5_000));
    assert_eq!(control.state.take_rerun_from().0, Some(5_000));
}

#[test]
fn late_terminal_state_does_not_remove_the_current_execution() {
    let control = WorkflowControl::new();
    let cancellation = CancellationToken::new();
    control
        .state
        .agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            5_000,
            ActiveAgentControl {
                action: Arc::new(AtomicUsize::new(AgentAction::None as usize)),
                cancellation,
            },
        );
    let current = active_agent(5_000);
    control
        .state
        .record_agent(/*execution_generation*/ 8, current.clone());
    control
        .state
        .record_agent(/*execution_generation*/ 7, settled_agent(5_000));

    assert_eq!(control.agent_progress(5_000), Some(current));
}

#[test]
fn failure_summaries_remain_process_bounded() {
    let mut failures = WorkflowFailureBuffer::default();
    for index in 0..10_000 {
        failures.push(format!("failure-{index}"));
    }

    let snapshot = failures.snapshot();
    assert_eq!(snapshot.len(), MAX_WORKFLOW_FAILURES);
    assert_eq!(
        snapshot.first().map(String::as_str),
        Some("[dropped 9745 earlier workflow failures]")
    );
    assert_eq!(snapshot.last().map(String::as_str), Some("failure-9999"));
}

fn active_agent(index: usize) -> WorkflowAgentProgress {
    WorkflowAgentProgress {
        state: WorkflowAgentState::Start,
        started_at: Some(1),
        ..settled_agent(index)
    }
}

fn settled_agent(index: usize) -> WorkflowAgentProgress {
    WorkflowAgentProgress {
        invocation_id: format!("invocation-{index}"),
        index,
        label: format!("agent-{index}"),
        phase_index: None,
        phase_title: None,
        agent_id: None,
        model: None,
        fallback_model: None,
        isolation: None,
        state: WorkflowAgentState::Done,
        activity: None,
        blocked: false,
        skipped: false,
        awaiting_decision: false,
        cached: false,
        attempt: 0,
        error: None,
        tokens: None,
        tool_calls: None,
        duration_ms: None,
        result_preview: None,
        prompt_preview: "test".to_string(),
        queued_at: 1,
        started_at: Some(1),
        last_progress_at: 2,
    }
}
