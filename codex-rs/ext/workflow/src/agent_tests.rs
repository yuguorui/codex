use super::worktree::WorktreeRemovalMode;
use super::worktree::cleanup_worktree;
use super::*;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::Weak;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Notify;

struct BlockingWorkflowAgentStop {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

struct BlockWorkflowAgentStop;

impl codex_extension_api::ThreadLifecycleContributor<codex_core::config::Config>
    for BlockingWorkflowAgentStop
{
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, codex_core::config::Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(input.session_source, SessionSource::SubAgent(_)) {
                input.thread_store.insert(BlockWorkflowAgentStop);
            }
        })
    }

    fn on_thread_stop<'a>(
        &'a self,
        input: codex_extension_api::ThreadStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<BlockWorkflowAgentStop>().is_some() {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

async fn wait_for_workflow_agent_running(thread: &codex_core::CodexThread) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if thread.agent_status().await == AgentStatus::Running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("workflow agent should enter the running state");
}

#[test]
fn interrupted_agent_runs_map_to_non_replayable_cancellation() {
    let failure = map_agent_error(
        AgentRunError::Codex {
            error: CodexErr::Interrupted,
            progress: AgentRunProgress {
                tokens: 17,
                tool_uses: 2,
                activity: None,
            },
        },
        3,
    );

    assert_eq!(
        failure,
        WorkflowAgentFailure {
            kind: WorkflowAgentFailureKind::Cancelled,
            message: CodexErr::Interrupted.to_string(),
            usage: WorkflowTokenUsage {
                total_tokens: 17,
                tool_uses: 5,
            },
        }
    );
}

#[test]
fn teardown_timeout_maps_to_a_clear_terminal_failure() {
    let failure = map_agent_error(
        AgentRunError::TeardownTimedOut {
            progress: AgentRunProgress {
                tokens: 19,
                tool_uses: 3,
                activity: None,
            },
        },
        2,
    );

    assert_eq!(
        failure,
        WorkflowAgentFailure {
            kind: WorkflowAgentFailureKind::Failed,
            message: "workflow agent teardown did not complete before the shutdown deadline"
                .to_string(),
            usage: WorkflowTokenUsage {
                total_tokens: 19,
                tool_uses: 5,
            },
        }
    );
}

#[test]
fn strict_schema_requires_nullable_optional_properties_recursively() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "details": {
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "state": { "enum": ["ready", "blocked"] }
                },
                "required": ["count"]
            }
        },
        "required": ["name"]
    });

    assert_eq!(
        strict_output_schema(&schema).unwrap(),
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "details": {
                    "type": ["object", "null"],
                    "properties": {
                        "count": { "type": "integer" },
                        "state": { "enum": ["ready", "blocked", null] }
                    },
                    "required": ["count", "state"],
                    "additionalProperties": false
                }
            },
            "required": ["details", "name"],
            "additionalProperties": false
        })
    );
}

#[test]
fn validation_preserves_optional_property_semantics_after_model_normalization() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "note": { "type": "string" }
        },
        "required": ["name"],
        "additionalProperties": false
    });

    assert_eq!(validate_schema(&json!({ "name": "run" }), &schema), Ok(()));
    assert_eq!(
        validate_schema(&json!({ "name": "run", "note": null }), &schema),
        Ok(())
    );
    assert!(validate_schema(&json!({ "name": "run", "note": 3 }), &schema).is_err());
}

#[test]
fn optional_boolean_schemas_accept_their_required_null_representation() {
    let schema = json!({
        "type": "object",
        "properties": {
            "anything": true,
            "nothing": false
        }
    });

    assert_eq!(
        strict_output_schema(&schema).unwrap(),
        json!({
            "type": "object",
            "properties": {
                "anything": true,
                "nothing": { "type": "null" }
            },
            "required": ["anything", "nothing"],
            "additionalProperties": false
        })
    );
    assert_eq!(validate_schema(&json!({}), &schema), Ok(()));
    assert_eq!(
        validate_schema(&json!({ "anything": null, "nothing": null }), &schema),
        Ok(())
    );
    assert!(validate_schema(&json!({ "nothing": "value" }), &schema).is_err());
}

#[test]
fn definitions_and_local_ref_targets_share_strict_optional_semantics() {
    let schema = json!({
        "type": "object",
        "properties": {
            "profile": { "$ref": "#/$defs/profile" }
        },
        "$defs": {
            "profile": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "note": { "type": "string" }
                },
                "required": ["name"]
            }
        }
    });

    let normalized = strict_output_schema(&schema).unwrap();

    assert_eq!(
        normalized,
        json!({
            "type": "object",
            "properties": {
                "profile": {
                    "anyOf": [
                        { "$ref": "#/$defs/profile" },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["profile"],
            "additionalProperties": false,
            "$defs": {
                "profile": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "note": { "type": ["string", "null"] }
                    },
                    "required": ["name", "note"],
                    "additionalProperties": false
                }
            }
        })
    );
    assert_eq!(validate_schema(&json!({}), &schema), Ok(()));
    assert_eq!(
        validate_schema(&json!({ "profile": { "name": "Ada" } }), &schema),
        Ok(())
    );
    assert_eq!(
        validate_schema(
            &json!({ "profile": { "name": "Ada", "note": null } }),
            &schema
        ),
        Ok(())
    );
    assert!(
        validate_schema(
            &json!({ "profile": { "name": "Ada", "unexpected": true } }),
            &schema
        )
        .is_err()
    );
}

#[test]
fn schema_normalization_visits_every_supported_subschema_location() {
    let optional_object = json!({
        "type": "object",
        "properties": { "value": { "type": "string" } }
    });
    let strict_object = json!({
        "type": "object",
        "properties": { "value": { "type": ["string", "null"] } },
        "required": ["value"],
        "additionalProperties": false
    });
    let schema = json!({
        "$defs": { "entry": optional_object },
        "definitions": { "entry": optional_object },
        "properties": { "entry": optional_object },
        "required": ["entry"],
        "patternProperties": { "^entry": optional_object },
        "dependentSchemas": { "entry": optional_object },
        "dependencies": { "entry": optional_object },
        "allOf": [optional_object],
        "anyOf": [optional_object],
        "oneOf": [optional_object],
        "prefixItems": [optional_object],
        "additionalItems": optional_object,
        "additionalProperties": optional_object,
        "contains": optional_object,
        "contentSchema": optional_object,
        "else": optional_object,
        "if": optional_object,
        "items": optional_object,
        "not": optional_object,
        "propertyNames": optional_object,
        "then": optional_object,
        "unevaluatedItems": optional_object,
        "unevaluatedProperties": optional_object
    });

    let normalized = strict_output_schema(&schema).unwrap();

    assert_eq!(
        normalized,
        json!({
            "$defs": { "entry": strict_object },
            "definitions": { "entry": strict_object },
            "properties": { "entry": strict_object },
            "patternProperties": { "^entry": strict_object },
            "dependentSchemas": { "entry": strict_object },
            "dependencies": { "entry": strict_object },
            "allOf": [strict_object],
            "anyOf": [strict_object],
            "oneOf": [strict_object],
            "prefixItems": [strict_object],
            "additionalItems": strict_object,
            "additionalProperties": strict_object,
            "contains": strict_object,
            "contentSchema": strict_object,
            "else": strict_object,
            "if": strict_object,
            "items": strict_object,
            "not": strict_object,
            "propertyNames": strict_object,
            "then": strict_object,
            "unevaluatedItems": strict_object,
            "unevaluatedProperties": strict_object,
            "required": ["entry"]
        })
    );

    let validation_object = json!({
        "type": "object",
        "properties": { "value": { "type": ["string", "null"] } },
        "additionalProperties": false
    });
    let mut validation_schema = schema;
    make_optional_properties_nullable(&mut validation_schema);
    assert_eq!(
        validation_schema,
        json!({
            "$defs": { "entry": validation_object },
            "definitions": { "entry": validation_object },
            "properties": { "entry": validation_object },
            "required": ["entry"],
            "patternProperties": { "^entry": validation_object },
            "dependentSchemas": { "entry": validation_object },
            "dependencies": { "entry": validation_object },
            "allOf": [validation_object],
            "anyOf": [validation_object],
            "oneOf": [validation_object],
            "prefixItems": [validation_object],
            "additionalItems": validation_object,
            "additionalProperties": validation_object,
            "contains": validation_object,
            "contentSchema": validation_object,
            "else": validation_object,
            "if": validation_object,
            "items": validation_object,
            "not": validation_object,
            "propertyNames": validation_object,
            "then": validation_object,
            "unevaluatedItems": validation_object,
            "unevaluatedProperties": validation_object
        })
    );
}

#[tokio::test]
async fn worktree_context_replaces_the_captured_filesystem_with_the_worktree() {
    let original = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let original = AbsolutePathBuf::try_from(original.path().to_path_buf()).unwrap();
    let worktree = AbsolutePathBuf::try_from(worktree.path().to_path_buf()).unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(original.to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let original_uri = PathUri::from_abs_path(&original);
    let selection = TurnEnvironmentSelection {
        environment_id: "local".to_string(),
        cwd: original_uri.clone(),
        workspace_roots: vec![original_uri],
        config: EnvironmentConfigState::FromThread,
    };
    let secondary = TurnEnvironmentSelection {
        environment_id: "secondary".to_string(),
        cwd: selection.cwd.clone(),
        workspace_roots: selection.workspace_roots.clone(),
        config: EnvironmentConfigState::Pending,
    };

    let environments = isolated_worktree_context(
        &mut config,
        Some(&[selection.clone(), secondary.clone()]),
        &worktree,
    )
    .unwrap()
    .unwrap();

    let worktree_uri = PathUri::from_abs_path(&worktree);
    assert_eq!(
        environments,
        vec![
            TurnEnvironmentSelection {
                environment_id: selection.environment_id,
                cwd: worktree_uri.clone(),
                workspace_roots: vec![worktree_uri],
                config: EnvironmentConfigState::FromThread,
            },
            secondary
        ]
    );
    assert_eq!(config.cwd, worktree);
    assert_eq!(config.workspace_roots, vec![worktree.clone()]);
    assert_eq!(config.permissions.workspace_roots(), &[worktree]);
}

#[tokio::test]
async fn worktree_context_rejects_owner_supplied_environment_configuration() {
    let root = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let root = AbsolutePathBuf::try_from(root.path().to_path_buf()).unwrap();
    let worktree = AbsolutePathBuf::try_from(worktree.path().to_path_buf()).unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(root.to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let root_uri = PathUri::from_abs_path(&root);
    let selection = TurnEnvironmentSelection {
        environment_id: "remote".to_string(),
        cwd: root_uri.clone(),
        workspace_roots: vec![root_uri],
        config: EnvironmentConfigState::Pending,
    };

    let error = isolated_worktree_context(&mut config, Some(&[selection]), &worktree)
        .expect_err("owner-supplied environment configuration must not be replaced");

    assert_eq!(error.kind, WorkflowAgentFailureKind::Blocked);
    assert_eq!(
        error.message,
        "use thread-derived configuration for worktree isolation in environment `remote`"
    );
}

#[test]
fn schema_validation_enforces_combinators_patterns_and_numeric_bounds() {
    let schema = json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "pattern": "^[A-Z]{2}$" },
            "score": { "type": "integer", "minimum": 1, "maximum": 3 },
            "kind": { "oneOf": [{ "const": "primary" }, { "const": "fallback" }] }
        },
        "required": ["code", "score", "kind"],
        "additionalProperties": false
    });

    assert_eq!(
        validate_schema(
            &json!({ "code": "OK", "score": 2, "kind": "primary" }),
            &schema
        ),
        Ok(())
    );
    for invalid in [
        json!({ "code": "bad", "score": 2, "kind": "primary" }),
        json!({ "code": "OK", "score": 4, "kind": "primary" }),
        json!({ "code": "OK", "score": 2, "kind": "unknown" }),
    ] {
        assert!(validate_schema(&invalid, &schema).is_err());
    }
}

#[test]
fn prompt_fallback_contains_schema_for_providers_without_native_structured_output() {
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"]
    });

    let contract = structured_output_contract(&schema, false).unwrap();
    let normalized = strict_output_schema(&schema).unwrap();

    assert_eq!(
        contract,
        format!(
            "{WORKFLOW_AGENT_SCHEMA_CONTRACT_PREFIX}{}",
            serde_json::to_string(&normalized).unwrap()
        )
    );
}

#[test]
fn large_schema_uses_native_delivery_or_fragmented_fallback() {
    let schema = json!({
        "type": "string",
        "description": "x".repeat(256 * 1024),
    });

    let fallback = structured_output_contract(&schema, false).unwrap();
    assert!(fallback.starts_with(WORKFLOW_AGENT_SCHEMA_CONTRACT_PREFIX));
    assert!(fallback.len() > 256 * 1024);
    assert_eq!(
        structured_output_contract(&schema, true).unwrap(),
        "\n\nReturn only JSON matching the host-provided schema."
    );

    let context = workflow_agent_context("task", "", &fallback).unwrap();
    assert!(
        context
            .keys()
            .filter(|key| key.starts_with("workflow_child_2_output_contract_part_"))
            .count()
            > 1
    );
}

#[test]
fn schema_complexity_is_bounded_before_recursive_processing() {
    let mut accepted_depth = JsonValue::Bool(true);
    for _ in 1..MAX_OUTPUT_SCHEMA_DEPTH {
        accepted_depth = json!({ "not": accepted_depth });
    }
    assert!(serialize_bounded_schema(&accepted_depth, "workflow agent schema").is_ok());

    let rejected_depth = json!({ "not": accepted_depth });
    let error = structured_output_contract(&rejected_depth, true)
        .expect_err("schema nesting beyond the limit must fail before validation");
    assert_eq!(error.message, "use a focused workflow agent schema");

    let accepted_nodes = JsonValue::Array(vec![json!(0); MAX_OUTPUT_SCHEMA_NODES - 1]);
    assert!(serialize_bounded_schema(&accepted_nodes, "workflow agent schema").is_ok());

    let rejected_nodes = JsonValue::Array(vec![json!(0); MAX_OUTPUT_SCHEMA_NODES]);
    let error = structured_output_contract(&rejected_nodes, true)
        .expect_err("schema node count beyond the limit must fail before validation");
    assert_eq!(error.message, "use a focused workflow agent schema");
}

#[test]
fn workflow_context_fragments_preserve_large_runtime_owned_values() {
    let schema = json!({
        "type": "string",
        "description": "s".repeat(256 * 1024),
    });
    let output_contract = structured_output_contract(&schema, false).unwrap();
    let isolation = format!(
        "You are working in an isolated git worktree at /tmp/{}. Keep all edits there.",
        "i".repeat(256 * 1024)
    );
    let task_prompt = "p".repeat(256 * 1024);

    let context = workflow_agent_context(&task_prompt, &isolation, &output_contract).unwrap();

    let mut expected_context =
        BTreeMap::from([
            WorkflowChildPreamble::new(WORKFLOW_SUBAGENT_PREAMBLE).into_additional_context()
        ]);
    for fragment in WorkflowChildTask::parts(&task_prompt) {
        let (key, entry) = fragment.into_additional_context();
        expected_context.insert(key, entry);
    }
    for fragment in WorkflowChildIsolation::parts(&isolation) {
        let (key, entry) = fragment.into_additional_context();
        expected_context.insert(key, entry);
    }
    for fragment in WorkflowChildOutputContract::parts(output_contract.trim_start()) {
        let (key, entry) = fragment.into_additional_context();
        expected_context.insert(key, entry);
    }
    assert_eq!(context, expected_context);
    let reconstructed_prompt = context
        .iter()
        .filter(|(key, _)| key.starts_with("workflow_child_3_task_part_"))
        .map(|(_, entry)| entry.value.as_str())
        .collect::<String>();
    assert_eq!(reconstructed_prompt, task_prompt);
}

#[test]
fn structured_retry_prompt_is_a_bounded_in_conversation_nudge() {
    let error = "e".repeat(10_000);

    let prompt = structured_retry_prompt(&error);

    assert!(prompt.starts_with("Your previous final output did not satisfy"));
    assert!(prompt.ends_with("Return only corrected JSON."));
    assert!(!prompt.contains("Previous output:"));
    assert!(!prompt.contains(&"e".repeat(2_000)));
    assert!(prompt.len() < 768);
}

#[tokio::test]
async fn large_prompt_is_fragmented_and_recoverable_in_the_model_request() {
    skip_if_no_network!();

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("large-workflow-prompt-response"),
            responses::ev_assistant_message("large-workflow-prompt-message", "done"),
            responses::ev_completed("large-workflow-prompt-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await.unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Arc::downgrade(&test.thread_manager)),
        test.session_configured.session_id.into(),
        test.config.clone(),
        "wf_large-prompt".to_string(),
    );

    let prompt = format!("task-start:{}:task-end", "界".repeat(90_000));
    assert!(prompt.len() >= 256 * 1024);
    let result = runtime
        .run(
            WorkflowAgentRequest {
                index: 0,
                invocation_id: "test-agent".to_string(),
                prompt: prompt.clone(),
                options: codex_workflow::WorkflowAgentOptions::default(),
                inputs: None,
                attempt: 0,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.value, json!("done"));
    let request = response_mock.single_request();
    let task_fragments = request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<workflow_child_3_task_part_"))
        .collect::<Vec<_>>();
    assert!(task_fragments.len() > 1);
    assert!(task_fragments.iter().all(|fragment| fragment.len() < 1024));
    let reconstructed = task_fragments
        .iter()
        .map(|fragment| {
            fragment
                .split_once('>')
                .and_then(|(_, body)| body.rsplit_once("</").map(|(body, _)| body))
                .expect("task context fragment should have matching markers")
        })
        .collect::<String>();
    assert_eq!(reconstructed, prompt);

    assert_eq!(
        request
            .message_input_texts("user")
            .into_iter()
            .filter(|text| !text.starts_with("<workflow_child_3_task_part_"))
            .collect::<Vec<_>>(),
        vec![WORKFLOW_AGENT_TASK_INSTRUCTION.to_string()]
    );
}

#[tokio::test]
async fn large_native_schema_reaches_the_agent_runner() {
    let cwd = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("33333333-3333-4333-8333-333333333334").unwrap(),
        config,
        "wf_large-schema".to_string(),
    );
    let schema = json!({
        "type": "string",
        "description": "x".repeat(256 * 1024),
    });

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 0,
                invocation_id: "test-agent".to_string(),
                prompt: "return a string".to_string(),
                options: codex_workflow::WorkflowAgentOptions {
                    schema: Some(schema),
                    ..Default::default()
                },
                inputs: None,
                attempt: 0,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("the agent runner should receive the large native schema");

    assert_eq!(error.kind, WorkflowAgentFailureKind::TerminalApi);
    assert!(error.message.contains("thread manager dropped"));
}

#[tokio::test]
async fn missing_captured_environment_blocks_agent_before_local_fallback() {
    let cwd = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new_with_environments(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("77777777-7777-4777-8777-777777777777").unwrap(),
        config,
        "wf_missing-environment".to_string(),
        Some(Vec::new()),
        None,
        WorkflowEnvironmentLocation::Local,
    );

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 0,
                invocation_id: "test-agent".to_string(),
                prompt: "must not run locally".to_string(),
                options: codex_workflow::WorkflowAgentOptions::default(),
                inputs: None,
                attempt: 0,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("an absent captured environment must block execution");

    assert_eq!(error.kind, WorkflowAgentFailureKind::Blocked);
    assert_eq!(
        error.message,
        "capture the workflow agent execution environment before starting the agent"
    );
}

#[tokio::test]
async fn remote_environment_rejects_host_worktree_isolation() {
    let cwd = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new_with_environments(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("88888888-8888-4888-8888-888888888888").unwrap(),
        config,
        "wf_remote-worktree".to_string(),
        None,
        None,
        WorkflowEnvironmentLocation::Remote,
    );

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 0,
                invocation_id: "test-agent".to_string(),
                prompt: "must not create a host worktree".to_string(),
                options: codex_workflow::WorkflowAgentOptions {
                    isolation: Some(WorkflowIsolation::Worktree),
                    ..Default::default()
                },
                inputs: None,
                attempt: 0,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("remote worktree isolation must fail closed");

    assert_eq!(error.kind, WorkflowAgentFailureKind::Blocked);
    assert_eq!(
        error.message,
        "use worktree isolation with a local workflow execution environment"
    );
}

#[tokio::test]
async fn cleans_unchanged_worktree_when_agent_fails() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let parent_thread_id = ThreadId::from_string("44444444-4444-4444-8444-444444444444").unwrap();
    let run_id = "wf_cleanup-error";
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        parent_thread_id,
        config,
        run_id.to_string(),
    );

    let error = runtime
        .run(
            WorkflowAgentRequest {
                index: 7,
                invocation_id: "test-agent".to_string(),
                prompt: "fail after creating the worktree".to_string(),
                options: codex_workflow::WorkflowAgentOptions {
                    isolation: Some(WorkflowIsolation::Worktree),
                    ..Default::default()
                },
                inputs: None,
                attempt: 1,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("the dropped thread manager should fail the agent");

    assert!(error.message.contains("thread manager dropped"));
    let worktree_root = codex_home.path().join("worktrees").join(run_id);
    if worktree_root.exists() {
        let mut worktrees = tokio::fs::read_dir(worktree_root).await.unwrap();
        assert!(worktrees.next_entry().await.unwrap().is_none());
    }
    let branches = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args(["branch", "--list", "wf-cleanup-error-7-a1-*"])
        .output()
        .await
        .unwrap();
    assert!(branches.status.success());
    assert!(branches.stdout.is_empty());
}

#[tokio::test]
async fn teardown_timeout_retains_an_unchanged_worktree_without_inspecting_it() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(&cwd, &home, "wf_teardown-timeout", 4, 0)
        .await
        .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    let mut result = Err(failure(
        WorkflowAgentFailureKind::Failed,
        "workflow agent teardown did not complete before the shutdown deadline",
    ));

    let retained = worktree.preserve_after_teardown_timeout();
    apply_teardown_failure(
        &mut result,
        format!(
            "workflow agent teardown did not complete before the shutdown deadline; {retained}"
        ),
    );

    assert_worktree_retained(repository.path(), &path, &branch).await;
    assert!(
        result
            .unwrap_err()
            .message
            .contains("Retained workflow worktree because agent teardown was not confirmed")
    );
    cleanup_worktree(
        repository.path(),
        &path,
        &branch,
        WorktreeRemovalMode::Force,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_teardown_timeout_skips_worktree_cleanup() {
    skip_if_no_network!();
    skip_if_remote!("git worktree isolation uses the host filesystem");

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(BlockingWorkflowAgentStop {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));
    let server = responses::start_mock_server().await;
    let request_args = json!({
        "questions": [{
            "id": "confirm",
            "header": "Confirm",
            "question": "Continue?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the task."
            }, {
                "label": "No",
                "description": "Stop the task."
            }]
        }]
    });
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("workflow-teardown-timeout-response"),
            responses::ev_function_call(
                "workflow-teardown-timeout-input",
                "request_user_input",
                &request_args.to_string(),
            ),
            responses::ev_completed("workflow-teardown-timeout-response"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::DefaultModeRequestUserInput)
                .expect("test config should allow request_user_input");
        })
        .build_with_auto_env(&server)
        .await
        .unwrap();
    initialize_repository(test.cwd_path()).await;

    let parent_thread_id = test.session_configured.session_id.into();
    let run_id = "wf_real-teardown-timeout";
    let runtime = Arc::new(CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Arc::downgrade(&test.thread_manager)),
        parent_thread_id,
        test.config.clone(),
        run_id.to_string(),
    ));
    let cancellation = CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let run = {
        let runtime = Arc::clone(&runtime);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            runtime
                .run_with_started(
                    WorkflowAgentRequest {
                        index: 2,
                        invocation_id: "real-timeout-agent".to_string(),
                        prompt: "Wait for confirmation.".to_string(),
                        options: codex_workflow::WorkflowAgentOptions {
                            isolation: Some(WorkflowIsolation::Worktree),
                            ..Default::default()
                        },
                        inputs: None,
                        attempt: 0,
                    },
                    cancellation,
                    Box::new(move |thread_id| {
                        started_tx.send(thread_id).expect("child should start");
                    }),
                )
                .await
        })
    };
    let child_id = ThreadId::from_string(&started_rx.await.expect("child should start")).unwrap();
    let child = test.thread_manager.get_thread(child_id).await.unwrap();
    wait_for_workflow_agent_running(&child).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if response_mock.requests().len() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial workflow agent request should reach the mock server");

    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .expect("real session teardown should reach the blocking lifecycle hook");
    let error = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("workflow agent should return after the teardown deadline")
        .expect("workflow agent task should not panic")
        .expect_err("unconfirmed teardown must fail the agent");

    let worktree_root = test.config.codex_home.join("worktrees").join(run_id);
    let mut entries = tokio::fs::read_dir(&worktree_root).await.unwrap();
    let worktree_path = entries
        .next_entry()
        .await
        .unwrap()
        .expect("timed-out teardown must retain the worktree")
        .path();
    assert!(entries.next_entry().await.unwrap().is_none());
    let branches = Command::new("git")
        .arg("-C")
        .arg(test.cwd_path())
        .args(["branch", "--list", "wf-real-teardown-timeout-2-a0-*"])
        .output()
        .await
        .unwrap();
    assert!(branches.status.success());
    let branch = String::from_utf8(branches.stdout)
        .unwrap()
        .split_whitespace()
        .last()
        .expect("timed-out teardown must retain the worktree branch")
        .to_string();
    assert!(error.message.contains("teardown did not complete"));
    assert!(error.message.contains(&worktree_path.display().to_string()));
    assert_worktree_retained(test.cwd_path(), &worktree_path, &branch).await;

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), child.wait_until_terminated())
        .await
        .expect("session should finish after teardown is released");
    cleanup_worktree(
        test.cwd_path(),
        &worktree_path,
        &branch,
        WorktreeRemovalMode::Force,
    );
}

#[tokio::test]
async fn uses_unique_worktrees_for_retried_or_resumed_attempts() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let run_id = "wf_retry-resume";

    let first = Worktree::create(&cwd, &home, run_id, /*index*/ 3, /*attempt*/ 0)
        .await
        .unwrap();
    let second = Worktree::create(&cwd, &home, run_id, /*index*/ 3, /*attempt*/ 0)
        .await
        .unwrap();

    assert_ne!(first.path, second.path);
    assert_ne!(first.branch, second.branch);
    assert!(first.path.exists());
    assert!(second.path.exists());
    assert!(first.cleanup_if_unchanged().await.is_none());
    assert!(second.cleanup_if_unchanged().await.is_none());
}

#[tokio::test]
async fn completed_workflow_reclaims_changed_worktrees_after_runtime_settles() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("55555555-5555-4555-8555-555555555555").unwrap(),
        config,
        "wf_changed-cleanup".to_string(),
    );
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_changed-cleanup",
        /*index*/ 2,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();

    let retained = worktree
        .cleanup_if_unchanged()
        .await
        .expect("changed worktree should remain available during the workflow");
    assert!(path.exists());
    runtime
        .retained_worktrees
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(retained);

    assert!(
        runtime
            .cleanup_worktrees(WorktreeCleanupMode::Completed)
            .await
            .is_empty()
    );

    assert_worktree_removed(repository.path(), &path, &branch).await;
}

#[tokio::test]
async fn interrupted_workflow_preserves_changed_worktree_and_reports_it() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(repository.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let runtime = CodexWorkflowAgentRuntime::new(
        AgentRunner::new(Weak::<ThreadManager>::new()),
        ThreadId::from_string("66666666-6666-4666-8666-666666666666").unwrap(),
        config,
        "wf_interrupted-cleanup".to_string(),
    );
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_interrupted-cleanup",
        /*index*/ 4,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();
    runtime
        .retained_worktrees
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(worktree);

    let messages = runtime
        .cleanup_worktrees(WorktreeCleanupMode::Interrupted)
        .await;

    assert_eq!(
        messages,
        vec![format!(
            "Retained changed workflow worktree after interruption: {} (branch {branch})",
            path.display()
        )]
    );
    assert_worktree_retained(repository.path(), &path, &branch).await;
    let cleanup_repository = repository.path().to_path_buf();
    let cleanup_path = path.clone();
    let cleanup_branch = branch.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_worktree(
            &cleanup_repository,
            &cleanup_path,
            &cleanup_branch,
            WorktreeRemovalMode::Force,
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn conservative_cleanup_preserves_commits_made_in_the_worktree() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_committed-cleanup",
        /*index*/ 3,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "committed change\n")
        .await
        .unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "workflow edit"]] {
        let output = Command::new("git")
            .arg("-C")
            .arg(path.as_path())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let retained = worktree
        .cleanup_if_unchanged()
        .await
        .expect("a committed workflow edit must be retained");

    assert_worktree_retained(repository.path(), &path, &branch).await;
    retained.cleanup().await;
    assert_worktree_removed(repository.path(), &path, &branch).await;
}

#[tokio::test]
async fn drop_preserves_a_changed_worktree_on_abnormal_exit() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_drop-cleanup",
        /*index*/ 4,
        /*attempt*/ 1,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();
    tokio::fs::write(path.join("tracked.txt"), "changed\n")
        .await
        .unwrap();

    drop(worktree);

    assert_worktree_retained(repository.path(), &path, &branch).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_worktree_retained(repository.path(), &path, &branch).await;
    let cleanup_repository = repository.path().to_path_buf();
    let cleanup_path = path.clone();
    let cleanup_branch = branch.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_worktree(
            &cleanup_repository,
            &cleanup_path,
            &cleanup_branch,
            WorktreeRemovalMode::Force,
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn drop_reclaims_an_unchanged_worktree_in_the_background() {
    let repository = initialized_repository().await;
    let codex_home = tempfile::tempdir().unwrap();
    let cwd = AbsolutePathBuf::try_from(repository.path().to_path_buf()).unwrap();
    let home = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let worktree = Worktree::create(
        &cwd,
        &home,
        "wf_drop-unchanged",
        /*index*/ 5,
        /*attempt*/ 0,
    )
    .await
    .unwrap();
    let path = worktree.path.clone();
    let branch = worktree.branch.clone();

    drop(worktree);

    assert_worktree_removed(repository.path(), &path, &branch).await;
}

async fn initialized_repository() -> TempDir {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path()).await;
    repository
}

async fn initialize_repository(path: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.email", "workflow-tests@example.invalid"],
        &["config", "user.name", "Workflow Tests"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::fs::write(path.join("tracked.txt"), "tracked\n")
        .await
        .unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "initial"]] {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

async fn assert_worktree_removed(repository: &Path, path: &Path, branch: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let branches = Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["branch", "--list"])
                .arg(branch)
                .output()
                .await
                .unwrap();
            assert!(branches.status.success());
            if !path.exists() && branches.stdout.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "worktree {} or branch {branch} was not removed",
            path.display()
        )
    });
}

async fn assert_worktree_retained(repository: &Path, path: &Path, branch: &str) {
    assert!(
        path.exists(),
        "worktree was removed from {}",
        path.display()
    );
    let branches = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["branch", "--list"])
        .arg(branch)
        .output()
        .await
        .unwrap();
    assert!(branches.status.success());
    assert!(!branches.stdout.is_empty(), "branch {branch} was removed");
}
