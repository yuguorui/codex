use super::*;

impl WorkflowDelegate {
    pub(super) async fn invoke_agent(
        &self,
        mut input: AgentToolInput,
        tool_cancellation: CancellationToken,
    ) -> Result<JsonValue, String> {
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
            return Err(format!(
                "workflow agent stallMs exceeds the {MAX_WORKFLOW_AGENT_STALL_MS}ms limit"
            ));
        }
        if self.remaining_budget() == Some(0) {
            return Err("WorkflowBudgetExceededError: workflow token budget exceeded".to_string());
        }
        let journal_key = {
            let mut state = self
                .invocation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.agent_count >= self.config.max_agents {
                return Err(format!(
                    "WorkflowAgentCapError: workflow exceeds the {} agent limit",
                    self.config.max_agents
                ));
            }
            input.index = state.agent_count;
            state.agent_count += 1;
            let key = workflow_cache_key(&state.previous_cache_key, &input.prompt, &input.options);
            state.previous_cache_key.clone_from(&key);
            key
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
        if let Some(cached) = self
            .config
            .journal
            .as_ref()
            .and_then(|journal| journal.replay(&journal_key))
            .filter(|_| replay_allowed)
        {
            self.emit_agent(
                &input,
                &label,
                WorkflowAgentState::Done,
                AgentEventDetails {
                    queued_at,
                    started_at: Some(queued_at),
                    agent_id: cached.agent_id.clone(),
                    model: cached.model.clone(),
                    fallback_model: cached.fallback_model.clone(),
                    cached: true,
                    result_preview: Some(preview_json(&cached.value)),
                    prompt_preview,
                    ..AgentEventDetails::default()
                },
            );
            return Ok(agent_tool_result(cached.value, 0, self.budget_spent()));
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
        );
        let permit = tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow concurrency limiter closed".to_string())?
            }
            _ = self.control.cancellation.cancelled() => return Err("workflow cancelled".to_string()),
            _ = tool_cancellation.cancelled() => return Err("workflow agent call cancelled".to_string()),
        };
        let _permit = permit;
        if let Some(journal) = self.config.journal.as_ref()
            && let Err(error) = journal.append_started(journal_key.clone()).await
        {
            self.record_log(format!("workflow journal started append failed: {error}"));
        }

        let started_at = unix_seconds();
        let started = Instant::now();
        let mut attempt = 0_u32;
        let mut throttle_retried = false;
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
                    prompt_preview: prompt_preview.clone(),
                    ..AgentEventDetails::default()
                },
            );
            let request = WorkflowAgentRequest {
                index: input.index,
                prompt: input.prompt.clone(),
                options: WorkflowAgentOptions {
                    label: Some(label.clone()),
                    ..input.options.clone()
                },
                attempt,
            };
            let started_agent_id = Arc::new(Mutex::new(None::<String>));
            let started_agent_id_for_callback = Arc::clone(&started_agent_id);
            let started_input = input.clone();
            let started_label = label.clone();
            let started_prompt_preview = prompt_preview.clone();
            let on_started: WorkflowAgentStartedCallback<'_> = Box::new(move |agent_id| {
                *started_agent_id_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(agent_id.clone());
                self.emit_agent(
                    &started_input,
                    &started_label,
                    WorkflowAgentState::Start,
                    AgentEventDetails {
                        queued_at,
                        started_at: Some(started_at),
                        attempt,
                        agent_id: Some(agent_id),
                        prompt_preview: started_prompt_preview.clone(),
                        ..AgentEventDetails::default()
                    },
                );
            });
            let progress_input = input.clone();
            let progress_label = label.clone();
            let progress_prompt_preview = prompt_preview.clone();
            let last_tokens = Arc::new(AtomicU64::new(0));
            let last_tool_calls = Arc::new(AtomicU64::new(0));
            let on_progress: WorkflowAgentProgressCallback<'_> = Box::new(move |usage| {
                let tokens_changed =
                    last_tokens.swap(usage.total_tokens, Ordering::AcqRel) != usage.total_tokens;
                let tool_calls_changed =
                    last_tool_calls.swap(usage.tool_uses, Ordering::AcqRel) != usage.tool_uses;
                if tokens_changed || tool_calls_changed {
                    self.emit_agent(
                        &progress_input,
                        &progress_label,
                        WorkflowAgentState::Start,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            tokens: Some(usage.total_tokens),
                            tool_calls: Some(usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview: progress_prompt_preview.clone(),
                            ..AgentEventDetails::default()
                        },
                    );
                }
            });
            let mut result = tokio::select! {
                result = self.agent_runtime.run_agent_with_progress(
                    request,
                    attempt_cancellation.clone(),
                    on_started,
                    on_progress,
                ) => result,
                _ = self.control.cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    Err(WorkflowAgentFailure::failed("workflow cancelled"))
                }
                _ = tool_cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    Err(WorkflowAgentFailure::failed("workflow agent call cancelled"))
                }
                _ = attempt_cancellation.cancelled() => {
                    Err(WorkflowAgentFailure::failed("workflow agent attempt cancelled"))
                }
            };
            self.remove_agent_control(input.index);
            let action = match action.load(Ordering::Acquire) {
                value if value == AgentAction::Skip as usize => AgentAction::Skip,
                value if value == AgentAction::Retry as usize => AgentAction::Retry,
                _ => AgentAction::None,
            };
            if action == AgentAction::Skip {
                if let Some(journal) = self.config.journal.as_ref() {
                    let skipped = WorkflowAgentResult {
                        value: JsonValue::Null,
                        usage: WorkflowTokenUsage::default(),
                        agent_id: None,
                        model: None,
                        fallback_model: None,
                    };
                    if let Err(error) = journal.append_result(journal_key.clone(), skipped).await {
                        self.record_log(format!("workflow journal skip append failed: {error}"));
                    }
                }
                self.emit_agent(
                    &input,
                    &label,
                    WorkflowAgentState::Error,
                    AgentEventDetails {
                        queued_at,
                        started_at: Some(started_at),
                        attempt,
                        agent_id: started_agent_id
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone(),
                        skipped: true,
                        error: Some("skipped by user".to_string()),
                        duration_ms: Some(duration_millis(started.elapsed())),
                        prompt_preview,
                        ..AgentEventDetails::default()
                    },
                );
                return Ok(agent_tool_result(JsonValue::Null, 0, self.budget_spent()));
            }
            if action == AgentAction::Retry {
                if attempt < self.config.max_agent_retries {
                    attempt += 1;
                    continue;
                }
                result = Err(WorkflowAgentFailure::failed(format!(
                    "workflow agent retry limit reached after {} retries",
                    self.config.max_agent_retries
                )));
            }
            match result {
                Ok(result) => {
                    self.total_tokens
                        .fetch_add(result.usage.total_tokens, Ordering::AcqRel);
                    self.total_tool_calls
                        .fetch_add(result.usage.tool_uses, Ordering::AcqRel);
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
                            tokens: Some(result.usage.total_tokens),
                            tool_calls: Some(result.usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            result_preview: Some(preview_json(&result.value)),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    );
                    if let Some(journal) = self.config.journal.as_ref()
                        && let Err(error) = journal
                            .append_result(journal_key.clone(), result.clone())
                            .await
                    {
                        self.record_log(format!("workflow journal result append failed: {error}"));
                    }
                    return Ok(agent_tool_result(
                        result.value,
                        result.usage.total_tokens,
                        self.budget_spent(),
                    ));
                }
                Err(failure)
                    if failure.kind == WorkflowAgentFailureKind::Stalled
                        && attempt < self.config.stall_retries =>
                {
                    let backoff = stall_retry_backoff(
                        self.config.stall_retry_base_delay,
                        self.config.stall_retry_max_delay,
                        attempt,
                    );
                    self.record_log(format!(
                        "agent {label} made no progress; retrying in {}s (auto-retry {}/{})",
                        backoff.as_secs(),
                        attempt + 1,
                        self.config.stall_retries
                    ));
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.control.cancellation.cancelled() => {
                            return Err("workflow cancelled".to_string());
                        }
                        _ = tool_cancellation.cancelled() => {
                            return Err("workflow agent call cancelled".to_string());
                        }
                    }
                    attempt += 1;
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
                    attempt += 1;
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Stalled => {
                    let decision = self
                        .await_user_decision(
                            &input,
                            &label,
                            attempt,
                            queued_at,
                            started_at,
                            &started_agent_id,
                            failure.message,
                            prompt_preview.clone(),
                            Some(duration_millis(started.elapsed())),
                            &tool_cancellation,
                        )
                        .await;
                    match decision {
                        Some(AgentAction::Retry) => {
                            attempt += 1;
                        }
                        Some(AgentAction::Skip) => {
                            self.emit_agent(
                                &input,
                                &label,
                                WorkflowAgentState::Error,
                                AgentEventDetails {
                                    queued_at,
                                    started_at: Some(started_at),
                                    attempt,
                                    agent_id: started_agent_id
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .clone(),
                                    skipped: true,
                                    error: Some("skipped by user".to_string()),
                                    duration_ms: Some(duration_millis(started.elapsed())),
                                    prompt_preview,
                                    ..AgentEventDetails::default()
                                },
                            );
                            return Ok(agent_tool_result(JsonValue::Null, 0, self.budget_spent()));
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
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: started_agent_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone(),
                            skipped: true,
                            error: Some(failure.message),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    );
                    return Ok(agent_tool_result(JsonValue::Null, 0, self.budget_spent()));
                }
                Err(failure) => {
                    let blocked = failure.kind == WorkflowAgentFailureKind::Blocked;
                    let returns_null = matches!(
                        failure.kind,
                        WorkflowAgentFailureKind::TerminalApi | WorkflowAgentFailureKind::Throttled
                    );
                    let message = failure.message;
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
                            agent_id: started_agent_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone(),
                            blocked,
                            error: Some(message.clone()),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    );
                    return if returns_null {
                        Ok(agent_tool_result(JsonValue::Null, 0, self.budget_spent()))
                    } else {
                        Err(message)
                    };
                }
            }
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
        attempt: u32,
        queued_at: u64,
        started_at: u64,
        started_agent_id: &Mutex<Option<String>>,
        error_message: String,
        prompt_preview: String,
        duration_ms: Option<u64>,
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
        self.emit_agent(
            input,
            label,
            WorkflowAgentState::Error,
            AgentEventDetails {
                queued_at,
                started_at: Some(started_at),
                attempt,
                agent_id: started_agent_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                awaiting_decision: true,
                error: Some(error_message),
                duration_ms,
                prompt_preview,
                ..AgentEventDetails::default()
            },
        );
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

    pub(super) fn record_log(&self, message: String) {
        let message = truncate_utf8(&message, MAX_LOG_MESSAGE_BYTES);
        let mut logs = self
            .logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if logs.len() >= MAX_WORKFLOW_LOGS {
            return;
        }
        logs.push(message.clone());
        drop(logs);
        self.emit(WorkflowEvent::WorkflowLog { message });
    }

    pub(super) async fn invoke_child(
        self: &Arc<Self>,
        input: ChildToolInput,
        tool_cancellation: CancellationToken,
    ) -> Result<JsonValue, String> {
        let resolver = self.config.child_resolver.as_ref().ok_or_else(|| {
            "nested workflow resolution is unavailable without a host workflow resolver".to_string()
        })?;
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
        let result_number = self
            .child_session_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.config.max_child_sessions).then_some(count + 1)
            })
            .map_err(|_| {
                format!(
                    "WorkflowChildSessionCapError: workflow exceeds the {} child session limit",
                    self.config.max_child_sessions
                )
            })?;
        let result_tool_name = format!("{RESULT_TOOL_NAME}_{result_number}");
        let source = compile_workflow_source_with_context(
            &resolved.script,
            &resolved.args,
            self.budget_total(),
            WorkflowScriptContext {
                child_mode: true,
                phase_index: input.phase_index,
                phase_title: input.phase_title,
                result_tool_name: Some(result_tool_name.clone()),
                initial_spent_tokens: self.budget_spent(),
            },
        )
        .map_err(|error| error.to_string())?;
        let result = run_workflow_source(
            source,
            Arc::clone(self),
            Arc::clone(&self.control),
            tool_cancellation,
            self.config.synchronous_timeout,
            result_tool_name,
            /*allow_child*/ false,
            /*observe_rerun*/ false,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({
            "value": result.value,
            "tokens": result.tokens,
            "spent": self.budget_spent(),
        }))
    }

    fn emit_agent(
        &self,
        input: &AgentToolInput,
        label: &str,
        state: WorkflowAgentState,
        details: AgentEventDetails,
    ) {
        self.emit(WorkflowEvent::WorkflowAgent(Box::new(
            WorkflowAgentProgress {
                index: input.index,
                label: label.to_string(),
                phase_index: input.phase_index,
                phase_title: input.phase_title.clone(),
                agent_id: details.agent_id,
                model: details.model.or_else(|| input.options.model.clone()),
                fallback_model: details.fallback_model,
                isolation: input.options.isolation,
                state,
                blocked: details.blocked,
                skipped: details.skipped,
                awaiting_decision: details.awaiting_decision,
                cached: details.cached,
                attempt: details.attempt,
                error: details.error,
                tokens: details.tokens,
                tool_calls: details.tool_calls,
                duration_ms: details.duration_ms,
                result_preview: details.result_preview,
                prompt_preview: details.prompt_preview,
                queued_at: details.queued_at,
                started_at: details.started_at,
                last_progress_at: unix_seconds(),
            },
        )));
    }
}
