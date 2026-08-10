use super::*;

impl WorkflowDelegate {
    pub(super) async fn invoke_agent(
        &self,
        mut input: AgentToolInput,
        tool_cancellation: CancellationToken,
    ) -> Result<JsonValue, String> {
        if let Some(label) = &mut input.options.label {
            *label = sanitize_progress_text(label);
        }
        if let Some(phase) = &mut input.options.phase {
            *phase = sanitize_progress_text(phase);
        }
        if let Some(phase_title) = &mut input.phase_title {
            *phase_title = sanitize_progress_text(phase_title);
        }
        let inputs_sha256 = workflow_agent_inputs_sha256(input.options.inputs.as_ref())?;
        if let Some(label) = input.options.label.as_deref() {
            ensure_progress_text_bound("workflow agent label", label)?;
        }
        if let Some(phase) = input.options.phase.as_deref() {
            ensure_progress_text_bound("workflow phase title", phase)?;
        }
        if let Some(phase_title) = input.phase_title.as_deref() {
            ensure_progress_text_bound("workflow phase title", phase_title)?;
        }
        if input
            .options
            .stall_ms
            .is_some_and(|stall_ms| stall_ms > MAX_WORKFLOW_AGENT_STALL_MS)
        {
            return Err(
                "choose stallMs within the supported workflow agent timeout range".to_string(),
            );
        }
        let journal_key = {
            let mut state = self
                .invocation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.agent_count = state.agent_count.saturating_add(1);
            input.index = match state.invocation_indices.get(&input.invocation_id) {
                Some(index) => *index,
                None => {
                    let requested = input.index;
                    let index = if state
                        .invocation_indices
                        .values()
                        .any(|index| *index == requested)
                    {
                        let index = state.next_index;
                        state.next_index = state.next_index.saturating_add(1);
                        index
                    } else {
                        state.next_index = state.next_index.max(requested.saturating_add(1));
                        requested
                    };
                    state
                        .invocation_indices
                        .insert(input.invocation_id.clone(), index);
                    index
                }
            };

            workflow_cache_key(
                &self.cache_root,
                &input.invocation_id,
                &input.prompt,
                &input.options,
                input.result_mode,
                inputs_sha256.as_deref(),
            )
        };
        let queued_at = unix_seconds();
        let label = input
            .options
            .label
            .clone()
            .unwrap_or_else(|| format!("agent-{}", input.index + 1));
        let prompt_preview = truncate_utf8(&input.prompt, PROMPT_PREVIEW_BYTES);
        let replay_allowed = self
            .invocation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rerun_from
            .is_none_or(|rerun_from| input.index < rerun_from);
        let cached = match self.config.journal.as_ref() {
            Some(journal) => journal.replay(&journal_key).await.map_err(|error| {
                format!("workflow journal replay failed for agent {label}: {error}")
            })?,
            None => None,
        };
        if let Some(cached) = cached.filter(|cached| {
            replay_allowed
                && !matches!(
                    &cached.outcome,
                    WorkflowAgentOutcome::Failure {
                        kind: WorkflowAgentFailureKind::Cancelled,
                        ..
                    }
                )
        }) {
            let failure = match &cached.outcome {
                WorkflowAgentOutcome::Success => None,
                WorkflowAgentOutcome::Failure { kind, message } => Some((*kind, message.clone())),
            };
            let progress_state = if failure.is_some() {
                WorkflowAgentState::Error
            } else {
                WorkflowAgentState::Done
            };
            if let Some((kind, message)) = &failure
                && *kind != WorkflowAgentFailureKind::Skipped
            {
                self.failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("{label}: {message}"));
            }
            self.emit_agent(
                &input,
                &label,
                progress_state,
                AgentEventDetails {
                    queued_at,
                    started_at: Some(queued_at),
                    agent_id: cached.result.agent_id.clone(),
                    model: cached.result.model.clone(),
                    fallback_model: cached.result.fallback_model.clone(),
                    cached: true,
                    blocked: failure
                        .as_ref()
                        .is_some_and(|(kind, _)| *kind == WorkflowAgentFailureKind::Blocked),
                    skipped: failure
                        .as_ref()
                        .is_some_and(|(kind, _)| *kind == WorkflowAgentFailureKind::Skipped),
                    error: failure.as_ref().map(|(_, message)| message.clone()),
                    result_preview: (failure.is_none()).then(|| preview_json(&cached.result.value)),
                    prompt_preview,
                    ..AgentEventDetails::default()
                },
            )
            .await;
            return match failure {
                None => {
                    self.workflow_tool_result_with_artifact(
                        agent_success_value(input.result_mode, cached.result.value),
                        0,
                    )
                    .await
                }
                Some((kind, message)) if input.result_mode == AgentResultMode::Settled => {
                    workflow_tool_result(
                        agent_failure_value(input.result_mode, kind, &message),
                        0,
                        None,
                    )
                }
                Some((_, message)) => Err(message),
            };
        }
        self.emit_agent(
            &input,
            &label,
            WorkflowAgentState::Queued,
            AgentEventDetails {
                queued_at,
                prompt_preview: prompt_preview.clone(),
                ..AgentEventDetails::default()
            },
        )
        .await;
        let mut concurrency_permit = Some(self.acquire_agent_permit(&tool_cancellation).await?);
        if let Some(journal) = self.config.journal.as_ref() {
            journal
                .append_started(journal_key.clone())
                .await
                .map_err(|error| {
                    format!("workflow journal could not durably start agent {label}: {error}")
                })?;
        }

        let started_at = unix_seconds();
        let started = Instant::now();
        let mut attempt = 0_u32;
        let mut stall_retries_used = 0_u32;
        let mut throttle_retried = false;
        let mut agent_usage = WorkflowTokenUsage::default();
        loop {
            let attempt_cancellation = CancellationToken::new();
            let action = Arc::new(AtomicUsize::new(AgentAction::None as usize));
            self.control
                .agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    input.index,
                    ActiveAgentControl {
                        action: Arc::clone(&action),
                        cancellation: attempt_cancellation.clone(),
                    },
                );
            self.emit_agent(
                &input,
                &label,
                WorkflowAgentState::Start,
                AgentEventDetails {
                    queued_at,
                    started_at: Some(started_at),
                    attempt,
                    tokens: Some(agent_usage.total_tokens),
                    tool_calls: Some(agent_usage.tool_uses),
                    prompt_preview: prompt_preview.clone(),
                    ..AgentEventDetails::default()
                },
            )
            .await;
            let request = WorkflowAgentRequest {
                index: input.index,
                invocation_id: input.invocation_id.clone(),
                prompt: input.prompt.clone(),
                options: WorkflowAgentOptions {
                    label: Some(label.clone()),
                    ..input.options.clone()
                },
                inputs: input.options.inputs.clone().map(|references| {
                    WorkflowAgentInputs::new(
                        references,
                        Arc::clone(&self.config.input_artifact_store),
                    )
                }),
                attempt,
            };
            let started_agent_id = Arc::new(Mutex::new(None::<String>));
            let started_agent_id_for_callback = Arc::clone(&started_agent_id);
            let previous_tokens = agent_usage.total_tokens;
            let previous_tool_calls = agent_usage.tool_uses;
            let on_started: WorkflowAgentStartedCallback<'_> = Box::new(move |agent_id| {
                *started_agent_id_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(agent_id);
            });
            let progress_input = input.clone();
            let progress_label = label.clone();
            let progress_prompt_preview = prompt_preview.clone();
            let last_tokens = Arc::new(AtomicU64::new(0));
            let last_tool_calls = Arc::new(AtomicU64::new(0));
            let progress_tokens = Arc::clone(&last_tokens);
            let progress_tool_calls = Arc::clone(&last_tool_calls);
            let progress_agent_id = Arc::clone(&started_agent_id);
            let last_activity = Arc::new(Mutex::new(None));
            let progress_activity = Arc::clone(&last_activity);
            let on_progress: WorkflowAgentProgressCallback<'_> = Box::new(move |update| {
                let progress_input = progress_input.clone();
                let progress_label = progress_label.clone();
                let progress_prompt_preview = progress_prompt_preview.clone();
                let progress_tokens = Arc::clone(&progress_tokens);
                let progress_tool_calls = Arc::clone(&progress_tool_calls);
                let progress_agent_id = Arc::clone(&progress_agent_id);
                let progress_activity = Arc::clone(&progress_activity);
                Box::pin(async move {
                    let usage = update.usage;
                    let tokens_changed = progress_tokens
                        .fetch_max(usage.total_tokens, Ordering::AcqRel)
                        < usage.total_tokens;
                    let tool_calls_changed = progress_tool_calls
                        .fetch_max(usage.tool_uses, Ordering::AcqRel)
                        < usage.tool_uses;
                    let activity_changed = {
                        let mut activity = progress_activity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let changed = *activity != update.activity;
                        *activity = update.activity;
                        changed
                    };
                    if tokens_changed || tool_calls_changed || activity_changed {
                        let total_tokens = progress_tokens.load(Ordering::Acquire);
                        let total_tool_calls = progress_tool_calls.load(Ordering::Acquire);
                        self.emit_agent(
                            &progress_input,
                            &progress_label,
                            WorkflowAgentState::Start,
                            AgentEventDetails {
                                queued_at,
                                started_at: Some(started_at),
                                attempt,
                                agent_id: clone_agent_id(&progress_agent_id),
                                activity: update.activity,
                                tokens: Some(previous_tokens.saturating_add(total_tokens)),
                                tool_calls: Some(
                                    previous_tool_calls.saturating_add(total_tool_calls),
                                ),
                                duration_ms: Some(duration_millis(started.elapsed())),
                                prompt_preview: progress_prompt_preview,
                                ..AgentEventDetails::default()
                            },
                        )
                        .await;
                    }
                })
            });
            let agent = self.agent_runtime.run_agent_with_progress(
                request,
                attempt_cancellation.clone(),
                on_started,
                on_progress,
            );
            tokio::pin!(agent);
            let (result, cancelled_message) = tokio::select! {
                result = &mut agent => (result, None),
                _ = self.control.cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    (agent.await, Some("workflow cancelled"))
                }
                _ = tool_cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    (agent.await, Some("workflow agent call cancelled"))
                }
                _ = attempt_cancellation.cancelled() => (agent.await, None),
            };
            let cancelled_message = cancelled_message.or_else(|| {
                if self.control.cancellation.is_cancelled() {
                    Some("workflow cancelled")
                } else if tool_cancellation.is_cancelled() {
                    Some("workflow agent call cancelled")
                } else {
                    None
                }
            });
            self.remove_agent_control(input.index);
            let mut attempt_usage = WorkflowTokenUsage {
                total_tokens: last_tokens.load(Ordering::Acquire),
                tool_uses: last_tool_calls.load(Ordering::Acquire),
            };
            let final_usage = match &result {
                Ok(success) => &success.usage,
                Err(failure) => &failure.usage,
            };
            attempt_usage.total_tokens = attempt_usage.total_tokens.max(final_usage.total_tokens);
            attempt_usage.tool_uses = attempt_usage.tool_uses.max(final_usage.tool_uses);
            self.total_tokens
                .fetch_add(attempt_usage.total_tokens, Ordering::AcqRel);
            self.total_tool_calls
                .fetch_add(attempt_usage.tool_uses, Ordering::AcqRel);
            agent_usage.total_tokens = agent_usage
                .total_tokens
                .saturating_add(attempt_usage.total_tokens);
            agent_usage.tool_uses = agent_usage
                .tool_uses
                .saturating_add(attempt_usage.tool_uses);
            if let Some(message) = cancelled_message {
                return Err(message.to_string());
            }
            let action = match action.load(Ordering::Acquire) {
                value if value == AgentAction::Skip as usize => AgentAction::Skip,
                value if value == AgentAction::Retry as usize => AgentAction::Retry,
                _ => AgentAction::None,
            };
            if action == AgentAction::Skip {
                let message = "skipped by user";
                let value = agent_failure_value(
                    input.result_mode,
                    WorkflowAgentFailureKind::Skipped,
                    message,
                );
                self.append_failure(
                    &journal_key,
                    WorkflowAgentFailureKind::Skipped,
                    message,
                    &agent_usage,
                )
                .await?;
                self.emit_agent(
                    &input,
                    &label,
                    WorkflowAgentState::Error,
                    AgentEventDetails {
                        queued_at,
                        started_at: Some(started_at),
                        attempt,
                        agent_id: clone_agent_id(&started_agent_id),
                        skipped: true,
                        error: Some(message.to_string()),
                        tokens: Some(agent_usage.total_tokens),
                        tool_calls: Some(agent_usage.tool_uses),
                        duration_ms: Some(duration_millis(started.elapsed())),
                        prompt_preview,
                        ..AgentEventDetails::default()
                    },
                )
                .await;
                return workflow_tool_result(value, agent_usage.total_tokens, None);
            }
            if action == AgentAction::Retry {
                attempt = attempt.saturating_add(1);
                continue;
            }
            match result {
                Ok(result) => {
                    let value = agent_success_value(input.result_mode, result.value.clone());
                    let tool_result = self
                        .workflow_tool_result_with_artifact(value, agent_usage.total_tokens)
                        .await?;
                    if let Some(journal) = self.config.journal.as_ref() {
                        journal
                            .append_result(
                                journal_key.clone(),
                                WorkflowJournalResult {
                                    result: WorkflowAgentResult {
                                        usage: agent_usage.clone(),
                                        ..result.clone()
                                    },
                                    outcome: WorkflowAgentOutcome::Success,
                                },
                            )
                            .await
                            .map_err(|error| {
                                format!(
                                    "workflow journal could not persist result for agent {label}: {error}"
                                )
                            })?;
                    }
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Done,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: result.agent_id.clone(),
                            model: result.model.clone(),
                            fallback_model: result.fallback_model.clone(),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            result_preview: Some(preview_json(&result.value)),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return Ok(tool_result);
                }
                Err(failure)
                    if failure.kind == WorkflowAgentFailureKind::Stalled
                        && stall_retries_used < self.config.stall_retries =>
                {
                    let backoff = stall_retry_backoff(
                        self.config.stall_retry_base_delay,
                        self.config.stall_retry_max_delay,
                        stall_retries_used,
                    );
                    self.record_log(
                        input.execution_generation,
                        format!(
                            "agent {label} made no progress; retrying in {}s (auto-retry {}/{})",
                            backoff.as_secs(),
                            stall_retries_used + 1,
                            self.config.stall_retries
                        ),
                    )
                    .await;
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.control.cancellation.cancelled() => {
                            return Err("workflow cancelled".to_string());
                        }
                        _ = tool_cancellation.cancelled() => {
                            return Err("workflow agent call cancelled".to_string());
                        }
                    }
                    stall_retries_used += 1;
                    attempt = attempt.saturating_add(1);
                }
                Err(failure)
                    if failure.kind == WorkflowAgentFailureKind::Throttled && !throttle_retried =>
                {
                    throttle_retried = true;
                    tokio::select! {
                        _ = tokio::time::sleep(self.config.throttle_retry_delay) => {}
                        _ = self.control.cancellation.cancelled() => {
                            return Err("workflow cancelled".to_string());
                        }
                    }
                    attempt = attempt.saturating_add(1);
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Stalled => {
                    // A user decision has no deadline. Release the concurrency slot while this
                    // attempt waits so queued invocations can make progress.
                    drop(concurrency_permit.take());
                    let decision = self
                        .await_user_decision(
                            &input,
                            &label,
                            AgentEventDetails {
                                queued_at,
                                started_at: Some(started_at),
                                attempt,
                                agent_id: clone_agent_id(&started_agent_id),
                                awaiting_decision: true,
                                error: Some(failure.message),
                                tokens: Some(agent_usage.total_tokens),
                                tool_calls: Some(agent_usage.tool_uses),
                                duration_ms: Some(duration_millis(started.elapsed())),
                                prompt_preview: prompt_preview.clone(),
                                ..AgentEventDetails::default()
                            },
                            &tool_cancellation,
                        )
                        .await;
                    match decision {
                        Some(AgentAction::Retry) => {
                            concurrency_permit =
                                Some(self.acquire_agent_permit(&tool_cancellation).await?);
                            attempt = attempt.saturating_add(1);
                        }
                        Some(AgentAction::Skip) => {
                            let value = agent_failure_value(
                                input.result_mode,
                                WorkflowAgentFailureKind::Skipped,
                                "skipped by user",
                            );
                            self.append_failure(
                                &journal_key,
                                WorkflowAgentFailureKind::Skipped,
                                "skipped by user",
                                &agent_usage,
                            )
                            .await?;
                            self.emit_agent(
                                &input,
                                &label,
                                WorkflowAgentState::Error,
                                AgentEventDetails {
                                    queued_at,
                                    started_at: Some(started_at),
                                    attempt,
                                    agent_id: clone_agent_id(&started_agent_id),
                                    skipped: true,
                                    error: Some("skipped by user".to_string()),
                                    tokens: Some(agent_usage.total_tokens),
                                    tool_calls: Some(agent_usage.tool_uses),
                                    duration_ms: Some(duration_millis(started.elapsed())),
                                    prompt_preview,
                                    ..AgentEventDetails::default()
                                },
                            )
                            .await;
                            return workflow_tool_result(value, agent_usage.total_tokens, None);
                        }
                        None => {
                            return Err("workflow cancelled".to_string());
                        }
                        Some(AgentAction::None) => {
                            unreachable!("pending user decision resolves to an action")
                        }
                    }
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Skipped => {
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    let value = agent_failure_value(
                        input.result_mode,
                        WorkflowAgentFailureKind::Skipped,
                        &message,
                    );
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            skipped: true,
                            error: Some(message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return if input.result_mode == AgentResultMode::Settled {
                        workflow_tool_result(value, agent_usage.total_tokens, None)
                    } else {
                        Err(message)
                    };
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Cancelled => {
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            error: Some(failure.message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return Err(failure.message);
                }
                Err(failure) => {
                    let blocked = failure.kind == WorkflowAgentFailureKind::Blocked;
                    let kind = failure.kind;
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    self.failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(format!("{label}: {message}"));
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            blocked,
                            error: Some(message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    self.append_failure(&journal_key, kind, &message, &agent_usage)
                        .await?;
                    if input.result_mode == AgentResultMode::Settled {
                        let value = agent_failure_value(input.result_mode, kind, &message);
                        return workflow_tool_result(value, agent_usage.total_tokens, None);
                    }
                    return Err(message);
                }
            }
        }
    }

    async fn acquire_agent_permit(
        &self,
        tool_cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, String> {
        tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow concurrency limiter closed".to_string())
            }
            _ = self.control.cancellation.cancelled() => Err("workflow cancelled".to_string()),
            _ = tool_cancellation.cancelled() => Err("workflow agent call cancelled".to_string()),
        }
    }

    fn remove_agent_control(&self, index: usize) {
        self.control
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&index);
    }

    /// Emits a stalled-agent failure and waits for the user to retry or skip it.
    ///
    /// The agent stays in the run's control map while waiting, so retry/skip actions
    /// from the host reach it. Returns the resolved [`AgentAction`], or `None` when the
    /// workflow was cancelled while waiting.
    async fn await_user_decision(
        &self,
        input: &AgentToolInput,
        label: &str,
        details: AgentEventDetails,
        tool_cancellation: &CancellationToken,
    ) -> Option<AgentAction> {
        let cancellation = CancellationToken::new();
        let action = Arc::new(AtomicUsize::new(AgentAction::None as usize));
        self.control
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                input.index,
                ActiveAgentControl {
                    action: Arc::clone(&action),
                    cancellation: cancellation.clone(),
                },
            );
        self.emit_agent(input, label, WorkflowAgentState::Error, details)
            .await;
        let decision = tokio::select! {
            _ = cancellation.cancelled() => action.load(Ordering::Acquire),
            _ = self.control.cancellation.cancelled() => AgentAction::None as usize,
            _ = tool_cancellation.cancelled() => AgentAction::None as usize,
        };
        self.remove_agent_control(input.index);
        match decision {
            value if value == AgentAction::Retry as usize => Some(AgentAction::Retry),
            value if value == AgentAction::Skip as usize => Some(AgentAction::Skip),
            _ => None,
        }
    }

    pub(super) async fn record_log(&self, execution_generation: u64, message: String) {
        let message = truncate_utf8(&sanitize_progress_text(&message), MAX_LOG_MESSAGE_BYTES);
        {
            let mut logs = self
                .logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            logs.push(message.clone());
        }
        self.emit(execution_generation, WorkflowEvent::WorkflowLog { message })
            .await;
    }

    async fn append_failure(
        &self,
        journal_key: &str,
        kind: WorkflowAgentFailureKind,
        message: &str,
        usage: &WorkflowTokenUsage,
    ) -> Result<(), String> {
        let Some(journal) = self.config.journal.as_ref() else {
            return Ok(());
        };
        let failed = WorkflowJournalResult {
            result: WorkflowAgentResult {
                value: JsonValue::Null,
                usage: usage.clone(),
                agent_id: None,
                model: None,
                fallback_model: None,
            },
            outcome: WorkflowAgentOutcome::Failure {
                kind,
                message: message.to_string(),
            },
        };
        journal
            .append_result(journal_key.to_string(), failed)
            .await
            .map_err(|error| format!("workflow journal could not persist agent failure: {error}"))
    }

    pub(super) async fn invoke_child(
        self: &Arc<Self>,
        input: ChildToolInput,
        tool_cancellation: CancellationToken,
        execution_generation: u64,
        invocation_prefix: String,
    ) -> Result<JsonValue, String> {
        let child_permit = tokio::select! {
            permit = Arc::clone(&self.child_semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow child concurrency limiter closed".to_string())?
            }
            _ = self.control.cancellation.cancelled() => {
                return Err("workflow cancelled".to_string());
            }
            _ = tool_cancellation.cancelled() => {
                return Err("child workflow call cancelled".to_string());
            }
        };
        validate_workflow_input_value(&input.args, "child workflow arguments")?;
        let resolver = self.config.child_resolver.as_ref().ok_or_else(|| {
            "nested workflow resolution is unavailable without a host workflow resolver".to_string()
        })?;
        let result_tool_name = {
            let mut digest = Sha256::new();
            digest.update(invocation_prefix.as_bytes());
            digest.update([0]);
            digest.update(input.invocation_id.as_bytes());
            let digest = format!("{:x}", digest.finalize());
            format!("{RESULT_TOOL_NAME}_{}", &digest[..32])
        };
        let request = WorkflowChildRequest {
            name_or_ref: input.name_or_ref,
            args: input.args,
        };
        let resolved = tokio::select! {
            resolved = resolver.resolve_child(request) => resolved?,
            _ = self.control.cancellation.cancelled() => {
                return Err("workflow cancelled".to_string());
            }
            _ = tool_cancellation.cancelled() => {
                return Err("child workflow call cancelled".to_string());
            }
        };
        validate_workflow_input_value(&resolved.args, "resolved child workflow arguments")?;
        let source = compile_workflow_source_with_context(
            &resolved.script,
            &resolved.args,
            WorkflowScriptContext {
                child_mode: true,
                phase_index: input.phase_index,
                phase_title: input.phase_title,
                result_tool_name: Some(result_tool_name.clone()),
            },
        )
        .map_err(|error| error.to_string())?;
        let result = run_workflow_source(WorkflowSourceExecution {
            source,
            delegate: Arc::clone(self),
            control: Arc::clone(&self.control),
            invocation_cancellation: tool_cancellation,
            synchronous_timeout: self.config.synchronous_timeout,
            result_tool_name,
            allow_child: false,
            rerun_receiver: None,
            execution_generation,
            invocation_prefix: format!("{invocation_prefix}/{}", input.invocation_id),
        })
        .await
        .map_err(|error| error.to_string())?;
        let input_artifact_store = Arc::clone(&self.config.input_artifact_store);
        tokio::spawn(async move {
            let _child_permit = child_permit;
            workflow_tool_result_with_artifact(input_artifact_store, result.value, result.tokens)
                .await
        })
        .await
        .map_err(|error| format!("child workflow result persistence task failed: {error}"))?
    }

    async fn workflow_tool_result_with_artifact(
        &self,
        value: JsonValue,
        tokens: u64,
    ) -> Result<JsonValue, String> {
        workflow_tool_result_with_artifact(
            Arc::clone(&self.config.input_artifact_store),
            value,
            tokens,
        )
        .await
    }

    async fn emit_agent(
        &self,
        input: &AgentToolInput,
        label: &str,
        state: WorkflowAgentState,
        details: AgentEventDetails,
    ) {
        let progress = WorkflowAgentProgress {
            invocation_id: input.invocation_id.clone(),
            index: input.index,
            label: label.to_string(),
            phase_index: input.phase_index,
            phase_title: input.phase_title.clone(),
            agent_id: details.agent_id,
            model: details.model.or_else(|| input.options.model.clone()),
            fallback_model: details.fallback_model,
            isolation: input.options.isolation,
            state,
            activity: details.activity,
            blocked: details.blocked,
            skipped: details.skipped,
            awaiting_decision: details.awaiting_decision,
            cached: details.cached,
            attempt: details.attempt,
            error: details.error.map(|error| {
                truncate_utf8(
                    &sanitize_progress_text(&error),
                    MAX_WORKFLOW_PROGRESS_TEXT_BYTES,
                )
            }),
            tokens: details.tokens,
            tool_calls: details.tool_calls,
            duration_ms: details.duration_ms,
            result_preview: details.result_preview,
            prompt_preview: details.prompt_preview,
            queued_at: details.queued_at,
            started_at: details.started_at,
            last_progress_at: unix_seconds(),
        };
        self.control
            .record_agent(input.execution_generation, progress.clone());
        self.emit(
            input.execution_generation,
            WorkflowEvent::WorkflowAgent(Box::new(progress)),
        )
        .await;
    }
}

async fn workflow_tool_result_with_artifact(
    input_artifact_store: Arc<dyn WorkflowInputArtifactStore>,
    value: JsonValue,
    tokens: u64,
) -> Result<JsonValue, String> {
    validate_workflow_input_value(&value, "workflow tool result")?;
    let artifact = input_artifact_store.put(value.clone()).await?;
    workflow_tool_result(value, tokens, Some(artifact))
}

fn clone_agent_id(agent_id: &Mutex<Option<String>>) -> Option<String> {
    agent_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn agent_success_value(result_mode: AgentResultMode, value: JsonValue) -> JsonValue {
    match result_mode {
        AgentResultMode::Value => value,
        AgentResultMode::Settled => serde_json::json!({
            "status": "fulfilled",
            "value": value,
        }),
    }
}

fn agent_failure_value(
    result_mode: AgentResultMode,
    kind: WorkflowAgentFailureKind,
    message: &str,
) -> JsonValue {
    match result_mode {
        AgentResultMode::Value => JsonValue::Null,
        AgentResultMode::Settled => serde_json::json!({
            "status": "rejected",
            "reason": {
                "kind": failure_kind_name(kind),
                "message": truncate_utf8(message, MAX_SETTLED_FAILURE_MESSAGE_BYTES),
            },
        }),
    }
}

fn failure_kind_name(kind: WorkflowAgentFailureKind) -> &'static str {
    match kind {
        WorkflowAgentFailureKind::Failed => "failed",
        WorkflowAgentFailureKind::TerminalApi => "terminalApi",
        WorkflowAgentFailureKind::Cancelled => "cancelled",
        WorkflowAgentFailureKind::Stalled => "stalled",
        WorkflowAgentFailureKind::Throttled => "throttled",
        WorkflowAgentFailureKind::Blocked => "blocked",
        WorkflowAgentFailureKind::Skipped => "skipped",
    }
}
