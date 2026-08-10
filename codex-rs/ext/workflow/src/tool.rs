use codex_agent_extension::AgentExecutionEnvironmentSnapshot;
use codex_agent_extension::AgentModelOverrides;
use codex_agent_extension::AgentRunner;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolApprovalArtifact;
use codex_extension_api::ToolApprovalDenialSource;
use codex_extension_api::ToolApprovalOutcome;
use codex_extension_api::ToolApprovalRequest;
use codex_extension_api::ToolApprovalReviewMode;
use codex_extension_api::ToolApprovalReviewRequest;
use codex_extension_api::ToolAvailability;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutionEnvironment;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::TurnItemEmitter;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_tools::ToolExposure;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Weak;

use crate::agent::WorkflowEnvironmentLocation;
use crate::composition::ChildWorkflowPolicy;
use crate::composition::FrozenWorkflowComposition;
use crate::discovery::WorkflowInput;
use crate::discovery::WorkflowOrigin;
use crate::discovery::active_plugin_workflow_roots;
use crate::discovery::resolve_workflow;
use crate::persistence::workflow_session_dir;
use crate::service::WorkflowLaunch;
use crate::service::WorkflowLaunchRequest;
use crate::service::WorkflowService;
use crate::spec::WORKFLOW_TOOL_NAME;
use crate::spec::workflow_tool_spec;
use crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES;
use crate::workflow_result_tool::model_bounded_error;
use crate::workflow_result_tool::model_bounded_json_value;
use crate::workflow_result_tool::truncate_model_text;

const APPROVAL_PREVIEW_SEGMENT_BYTES: usize = 2_000;
const APPROVAL_CHILD_SUMMARY_BYTES: usize = 8_000;
const GENERATED_WORKFLOW_RUN_ID: &str = "wf_00000000000000000000000000000000";
const GENERATED_WORKFLOW_TASK_ID: &str = "w00000000";

pub(crate) struct WorkflowToolExecutor {
    thread_id: ThreadId,
    service: WorkflowService,
    agent_runner: AgentRunner,
    thread_manager: Weak<ThreadManager>,
}

impl WorkflowToolExecutor {
    pub(crate) fn new(
        thread_id: ThreadId,
        service: WorkflowService,
        agent_runner: AgentRunner,
        thread_manager: Weak<ThreadManager>,
    ) -> Self {
        Self {
            thread_id,
            service,
            agent_runner,
            thread_manager,
        }
    }
}

async fn request_workflow_approval(
    emitter: &dyn TurnItemEmitter,
    mut request: ToolApprovalReviewRequest,
    artifact: &WorkflowApprovalArtifact,
) -> Result<ToolApprovalOutcome, FunctionCallError> {
    request.prompt.question.push_str(&format!(
        "\n\nComplete structured Workflow action: {}\nRead this reference with workflowApprovalArtifact/read.\nAction SHA-256: {}",
        artifact.reference, artifact.sha256
    ));
    Ok(emitter.request_approval_detailed(request).await)
}

struct WorkflowApprovalArtifact {
    path: codex_utils_absolute_path::AbsolutePathBuf,
    reference: String,
    sha256: String,
    contents: String,
}

const WORKFLOW_APPROVAL_ARTIFACT_PAGE_BYTES: usize = 512;

async fn persist_workflow_approval_action(
    approval_artifact_dir: &codex_utils_absolute_path::AbsolutePathBuf,
    thread_id: ThreadId,
    action: &serde_json::Value,
) -> Result<WorkflowApprovalArtifact, FunctionCallError> {
    let action = canonical_json_value(action.clone());
    let contents = serde_json::to_string_pretty(&action).map_err(|error| {
        model_bounded_error(format_args!(
            "failed to serialize the complete Workflow approval action: {error}"
        ))
    })?;
    let sha256 = format!("{:x}", Sha256::digest(contents.as_bytes()));
    let path = approval_artifact_dir.join(format!("{sha256}.json"));
    let write_path = path.to_path_buf();
    let write_contents = contents.clone();
    tokio::task::spawn_blocking(move || {
        codex_utils_path::write_atomically(&write_path, &write_contents)
    })
    .await
    .map_err(|error| {
        model_bounded_error(format_args!(
            "failed to persist the complete Workflow approval action: {error}"
        ))
    })?
    .map_err(|error| {
        model_bounded_error(format_args!(
            "failed to persist the complete Workflow approval action: {error}"
        ))
    })?;
    Ok(WorkflowApprovalArtifact {
        path,
        reference: workflow_approval_artifact_reference(thread_id, &sha256),
        sha256,
        contents,
    })
}

async fn verify_workflow_approval_artifact(
    artifact: &WorkflowApprovalArtifact,
) -> Result<(), FunctionCallError> {
    let bytes = tokio::fs::read(&artifact.path).await.map_err(|error| {
        model_bounded_error(format_args!(
            "failed to verify the approved Workflow action: {error}"
        ))
    })?;
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if bytes != artifact.contents.as_bytes() || actual_sha256 != artifact.sha256 {
        return Err(model_bounded_error(
            "the persisted Workflow approval action changed before execution",
        ));
    }
    Ok(())
}

/// Returns the stable app-server reference for a Workflow approval artifact.
pub fn workflow_approval_artifact_reference(thread_id: ThreadId, sha256: &str) -> String {
    format!("codex://workflow-approval/{thread_id}/{sha256}")
}

/// Verified contents of a content-addressed Workflow approval action.
pub struct WorkflowApprovalArtifactData {
    /// SHA-256 content identifier.
    pub sha256: String,
    /// Byte offset of this page in the verified artifact.
    pub offset: usize,
    /// One bounded UTF-8 page from the verified artifact.
    pub contents: String,
    /// Byte offset to request next, or `None` after the final page.
    pub next_offset: Option<usize>,
}

/// Reads and verifies one page of a content-addressed Workflow approval artifact.
pub async fn read_workflow_approval_artifact(
    codex_home: &codex_utils_absolute_path::AbsolutePathBuf,
    thread_id: ThreadId,
    sha256: &str,
    offset: usize,
) -> Result<WorkflowApprovalArtifactData, String> {
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid Workflow approval artifact id".to_string());
    }
    let path = workflow_session_dir(codex_home, thread_id)
        .join("workflows/approvals")
        .join(format!("{sha256}.json"));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("failed to read Workflow approval artifact: {error}"))?;
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if actual_sha256 != sha256 {
        return Err("Workflow approval artifact failed SHA-256 verification".to_string());
    }
    let _: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse Workflow approval artifact: {error}"))?;
    let contents = String::from_utf8(bytes)
        .map_err(|error| format!("Workflow approval artifact is not valid UTF-8: {error}"))?;
    if offset >= contents.len() || !contents.is_char_boundary(offset) {
        return Err("invalid Workflow approval artifact offset".to_string());
    }
    let mut end = offset
        .saturating_add(WORKFLOW_APPROVAL_ARTIFACT_PAGE_BYTES)
        .min(contents.len());
    while end > offset && !contents.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset {
        return Err("Workflow approval artifact page has no UTF-8 boundary".to_string());
    }
    Ok(WorkflowApprovalArtifactData {
        sha256: sha256.to_string(),
        offset,
        contents: contents[offset..end].to_string(),
        next_offset: (end < contents.len()).then_some(end),
    })
}

impl ToolExecutor<ToolCall> for WorkflowToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        workflow_tool_spec(WORKFLOW_TOOL_NAME)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn availability(&self) -> ToolAvailability {
        ToolAvailability::RootSessionOnly
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let input = serde_json::from_str::<WorkflowInput>(invocation.function_arguments()?)
                .map_err(|error| {
                    model_bounded_error(format_args!("invalid Workflow input: {error}"))
                })?;
            let context = self.execution_context(&invocation).await?;
            self.handle_with_context(invocation, input, context).await
        })
    }
}

impl WorkflowToolExecutor {
    async fn handle_with_context(
        &self,
        invocation: ToolCall,
        input: WorkflowInput,
        context: WorkflowExecutionContext,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let WorkflowExecutionContext {
            config,
            environments,
            captured_environments,
            execution_environment_action,
            location,
            script_access,
        } = context;
        validate_script_access(&input, script_access)?;
        let plugin_roots = active_plugin_workflow_roots(&self.thread_manager, &config).await;
        let child_policy = match location {
            WorkflowEnvironmentLocation::Local => ChildWorkflowPolicy::FreezeLocal,
            WorkflowEnvironmentLocation::Remote => ChildWorkflowPolicy::RejectRemote,
        };
        let resolved = resolve_workflow(
            input,
            &config.cwd,
            &config.codex_home,
            &plugin_roots,
            child_policy,
        )
        .await
        .map_err(model_bounded_error)?;
        let (agent_runner, frozen_agent_configurations) = self
            .agent_runner
            .freeze_workflow_agent_configs(
                self.thread_id,
                &config,
                AgentModelOverrides {
                    model: config.agent_default_subagent_model.clone(),
                    reasoning_effort: config.agent_default_subagent_reasoning_effort.clone(),
                },
            )
            .await
            .map_err(model_bounded_error)?;
        let approval_review_mode = invocation.turn_item_emitter.approval_review_mode();
        if config.permissions.approval_policy.value() != AskForApproval::Never
            || approval_review_mode == ToolApprovalReviewMode::StrictAutomatic
        {
            let meta = &resolved.script.meta;
            let title = meta.title.as_deref().unwrap_or(&meta.name);
            let phases = if meta.phases.is_empty() {
                "(none)".to_string()
            } else {
                bounded_text(
                    &meta
                        .phases
                        .iter()
                        .map(|phase| phase.title.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    1_000,
                )
            };
            let script_hash = format!("{:x}", Sha256::digest(resolved.script.source.as_bytes()));
            let preview = approval_script_preview(&resolved.script.source);
            let source = resolved.origin.approval_label();
            let canonical_args = canonical_json_value(resolved.args.clone());
            let canonical_args_text = serde_json::to_string(&canonical_args).map_err(|error| {
                model_bounded_error(format_args!(
                    "failed to serialize canonical Workflow arguments: {error}"
                ))
            })?;
            let args_hash = format!("{:x}", Sha256::digest(canonical_args_text.as_bytes()));
            let args_preview = approval_arguments_preview(&canonical_args_text);
            let shadow_notice = if resolved.shadows_existing {
                "\nWarning: this file shadows a lower-priority workflow with the same name."
            } else {
                ""
            };
            let reviewed_script = match &resolved.origin {
                WorkflowOrigin::Bundled => serde_json::json!({
                    "kind": "trustedBundled",
                    "sha256": script_hash,
                }),
                WorkflowOrigin::Inline
                | WorkflowOrigin::File { .. }
                | WorkflowOrigin::Plugin { .. } => serde_json::json!({
                    "kind": "completeSource",
                    "source": resolved.script.source,
                    "sha256": script_hash,
                }),
            };
            let reviewed_children = resolved
                .composition
                .children()
                .map(|child| {
                    let reviewed_script = match &child.origin {
                        WorkflowOrigin::Bundled => serde_json::json!({
                            "kind": "trustedBundled",
                            "sha256": child.script_sha256,
                        }),
                        WorkflowOrigin::Inline
                        | WorkflowOrigin::File { .. }
                        | WorkflowOrigin::Plugin { .. } => serde_json::json!({
                            "kind": "completeSource",
                            "source": child.script.source,
                            "sha256": child.script_sha256,
                        }),
                    };
                    serde_json::json!({
                        "binding": child.reference,
                        "origin": child.origin.approval_label(),
                        "reviewedScript": reviewed_script,
                        "scriptSha256": child.script_sha256,
                        "shadowsExisting": child.shadows_existing,
                    })
                })
                .collect::<Vec<_>>();
            let definition_sha256 = resolved.composition.definition_sha256();
            let child_summary = approval_children_summary(&resolved.composition);
            let action = serde_json::json!({
                "tool": WORKFLOW_TOOL_NAME,
                "definitionSha256": definition_sha256,
                "origin": source,
                "name": meta.name,
                "title": title,
                "description": meta.description,
                "phases": meta
                    .phases
                    .iter()
                    .map(|phase| phase.title.as_str())
                    .collect::<Vec<_>>(),
                "reviewedScript": reviewed_script,
                "reviewedChildren": reviewed_children,
                "arguments": canonical_args,
                "argumentsSha256": args_hash,
                "resumeFromRunId": resolved.resume_from_run_id,
                "shadowsExisting": resolved.shadows_existing,
                "execution": execution_environment_action,
                "frozenAgentConfigurations": frozen_agent_configurations,
            });
            let artifact = persist_workflow_approval_action(
                &workflow_session_dir(&config.codex_home, self.thread_id)
                    .join("workflows/approvals"),
                self.thread_id,
                &action,
            )
            .await?;
            let decision = request_workflow_approval(
                    invocation.turn_item_emitter.as_ref(),
                    ToolApprovalReviewRequest {
                        prompt: ToolApprovalRequest {
                            call_id: invocation.call_id.clone(),
                            id: format!("workflow_approval_{}", invocation.call_id),
                            header: "Workflow".to_string(),
                            question: format!(
                                "Review dynamic workflow before running\n\nName: {}\nTitle: {}\nDescription: {}\nPhases: {phases}\nSource: {source}{shadow_notice}\nScript size: {} bytes\nScript SHA-256: {script_hash}\nFrozen definition SHA-256: {definition_sha256}\n\n{child_summary}\n\nArguments SHA-256: {args_hash}\n\n{args_preview}\n\n{preview}",
                                bounded_text(&meta.name, 200),
                                bounded_text(title, 200),
                                bounded_text(&meta.description, 500),
                                resolved.script.source.len(),
                            ),
                            approve_label: "Run workflow".to_string(),
                            deny_label: "Cancel".to_string(),
                        },
                        action,
                        artifact: Some(ToolApprovalArtifact::new(
                            artifact.sha256.clone(),
                            artifact.contents.clone(),
                        )),
                    },
                    &artifact,
                )
                .await?;
            match decision {
                ToolApprovalOutcome::Approved => {}
                ToolApprovalOutcome::Denied { rejection, source } => {
                    let prefix = match source {
                        ToolApprovalDenialSource::User => "the user denied the dynamic workflow",
                        ToolApprovalDenialSource::AutomaticReviewer => {
                            "automatic approval review denied the dynamic workflow"
                        }
                        ToolApprovalDenialSource::Configuration => {
                            "configuration denied the dynamic workflow"
                        }
                        ToolApprovalDenialSource::Unknown => "dynamic workflow was not approved",
                        _ => "dynamic workflow was not approved",
                    };
                    let message = if source == ToolApprovalDenialSource::Unknown
                        && rejection == "approval was denied"
                    {
                        prefix.to_string()
                    } else {
                        bounded_approval_failure(prefix, &rejection)
                    };
                    return Err(model_bounded_error(message));
                }
                ToolApprovalOutcome::TimedOut { rejection } => {
                    return Err(model_bounded_error(bounded_approval_failure(
                        "automatic approval review timed out",
                        &rejection,
                    )));
                }
                ToolApprovalOutcome::Cancelled { reason } => {
                    return Err(model_bounded_error(bounded_approval_failure(
                        "dynamic workflow approval was cancelled",
                        &reason,
                    )));
                }
                ToolApprovalOutcome::Unavailable => {
                    return Err(model_bounded_error(
                        "dynamic workflow approval is required but unavailable in this client"
                            .to_string(),
                    ));
                }
                _ => {
                    return Err(model_bounded_error(
                        "dynamic workflow approval returned an unsupported outcome",
                    ));
                }
            }
            verify_workflow_approval_artifact(&artifact).await?;
        }
        preflight_model_launch_response(self.thread_id, &config, &resolved)?;
        let launch = self
            .service
            .launch(WorkflowLaunchRequest {
                thread_id: self.thread_id,
                turn_id: invocation.turn_id,
                config,
                resolved,
                agent_runner,
                environments,
                captured_environments,
                environment_location: location,
            })
            .await
            .map_err(model_bounded_error)?;
        let value = model_launch_response(&launch);
        Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
    }
}

impl WorkflowToolExecutor {
    async fn execution_context(
        &self,
        invocation: &ToolCall,
    ) -> Result<WorkflowExecutionContext, FunctionCallError> {
        let Some(thread_manager) = self.thread_manager.upgrade() else {
            return Err(workflow_diagnostic(
                "Workflow cannot establish an authoritative execution context because the owning thread manager is unavailable",
            ));
        };
        let _thread = thread_manager
            .get_thread(self.thread_id)
            .await
            .map_err(model_bounded_error)?;
        let mut config = projected_agent_config(invocation)?;
        let tool_environments = invocation.execution_environments();
        let captured_environments = self
            .agent_runner
            .capture_execution_environments(self.thread_id, &tool_environments)
            .await
            .map_err(model_bounded_error)?;
        let environments =
            captured_environment_selections(tool_environments.iter().map(|environment| {
                (
                    environment.environment_id.as_str(),
                    &environment.cwd,
                    environment.selection.as_ref(),
                )
            }))?;
        let primary = environments.first().ok_or_else(|| {
            model_bounded_error("Workflow requires a selected execution environment")
        })?;
        let tool_environment = &tool_environments[0];
        let permission_profile = tool_environment
            .file_system_sandbox_context
            .permissions
            .clone()
            .try_into()
            .map_err(|error: std::io::Error| {
                workflow_diagnostic(format_args!(
                    "Workflow cannot apply the selected execution permissions: {error}"
                ))
            })?;
        config
            .permissions
            .set_permission_profile(permission_profile)
            .map_err(|error| {
                workflow_diagnostic(format_args!(
                    "Workflow execution permission profile is invalid: {error}"
                ))
            })?;
        let location = if tool_environments
            .iter()
            .any(|environment| environment.is_remote)
        {
            WorkflowEnvironmentLocation::Remote
        } else {
            WorkflowEnvironmentLocation::Local
        };
        let script_access = match workflow_host_paths(location, primary)? {
            Some((cwd, workspace_roots)) => {
                config.permissions.set_workspace_roots(workspace_roots);
                config.cwd = cwd;
                WorkflowScriptAccess::HostFilesystem
            }
            None => WorkflowScriptAccess::InlineOnly,
        };
        let execution_environment_action = approval_execution_environment_action(
            &tool_environments,
            &config,
            invocation.turn_item_emitter.approval_review_mode(),
        );
        Ok(WorkflowExecutionContext {
            config,
            environments,
            captured_environments: Some(captured_environments),
            execution_environment_action,
            location,
            script_access,
        })
    }
}

fn approval_execution_environment_action(
    environments: &[ToolExecutionEnvironment],
    config: &Config,
    review_mode: ToolApprovalReviewMode,
) -> serde_json::Value {
    let environments = environments
        .iter()
        .map(|environment| {
            let selection = environment.selection.as_ref();
            serde_json::json!({
                "environmentId": environment.environment_id,
                "location": if environment.is_remote { "remote" } else { "local" },
                "cwd": environment.cwd,
                "workspaceRoots": selection
                    .map(|selection| selection.workspace_roots.clone())
                    .unwrap_or_default(),
                "environmentConfig": selection
                    .map(|selection| approval_environment_config(&selection.config)),
                "sandboxContext": environment.file_system_sandbox_context,
                "executorId": environment.executor_id,
            })
        })
        .collect::<Vec<_>>();
    let review_mode = match review_mode {
        ToolApprovalReviewMode::User => "user",
        ToolApprovalReviewMode::Automatic => "automatic",
        ToolApprovalReviewMode::StrictAutomatic => "strictAutomatic",
    };
    serde_json::json!({
        "environments": environments,
        "effectivePermissions": {
            "approvalPolicy": config.permissions.approval_policy.value(),
            "approvalReviewMode": review_mode,
            "approvalsReviewer": config.approvals_reviewer,
            "permissionProfile": config.permissions.permission_profile(),
            "activePermissionProfile": config.permissions.active_permission_profile(),
            "profileWorkspaceRoots": config.permissions.profile_workspace_roots(),
            "workspaceRoots": config.permissions.workspace_roots(),
            "allowLoginShell": config.permissions.allow_login_shell,
        },
    })
}

fn approval_environment_config(
    state: &codex_protocol::protocol::EnvironmentConfigState,
) -> serde_json::Value {
    match state {
        codex_protocol::protocol::EnvironmentConfigState::FromThread => {
            serde_json::json!({ "state": "fromThread" })
        }
        codex_protocol::protocol::EnvironmentConfigState::Pending => {
            serde_json::json!({ "state": "pending" })
        }
        codex_protocol::protocol::EnvironmentConfigState::Ready(config) => serde_json::json!({
            "state": "ready",
            "allowLoginShell": config.allow_login_shell,
            "permissionProfile": config.permission_profile.permission_profile(),
            "activePermissionProfile": config.permission_profile.active_permission_profile(),
            "profileWorkspaceRoots": config.permission_profile.profile_workspace_roots(),
            "selectedCapabilityRoots": config.selected_capability_roots,
        }),
        codex_protocol::protocol::EnvironmentConfigState::Failed(error) => {
            serde_json::json!({ "state": "failed", "error": error })
        }
    }
}

fn captured_environment_selections<'a, I>(
    environments: I,
) -> Result<Vec<TurnEnvironmentSelection>, FunctionCallError>
where
    I: IntoIterator<
        Item = (
            &'a str,
            &'a codex_utils_path_uri::PathUri,
            Option<&'a TurnEnvironmentSelection>,
        ),
    >,
{
    environments
        .into_iter()
        .map(|(environment_id, cwd, selection)| {
            let selection = selection.cloned().ok_or_else(|| {
                workflow_diagnostic(format_args!(
                    "Workflow requires environment `{environment_id}` to include the exact turn selection captured for this tool call"
                ))
            })?;
            if selection.environment_id != environment_id || &selection.cwd != cwd {
                return Err(workflow_diagnostic(format_args!(
                    "Workflow requires captured selection metadata for environment `{environment_id}` to agree with its executor binding"
                )));
            }
            Ok(selection)
        })
        .collect()
}

struct WorkflowExecutionContext {
    config: Config,
    environments: Vec<TurnEnvironmentSelection>,
    captured_environments: Option<AgentExecutionEnvironmentSnapshot>,
    execution_environment_action: serde_json::Value,
    location: WorkflowEnvironmentLocation,
    script_access: WorkflowScriptAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowScriptAccess {
    HostFilesystem,
    InlineOnly,
}

fn validate_script_access(
    input: &WorkflowInput,
    script_access: WorkflowScriptAccess,
) -> Result<(), FunctionCallError> {
    if script_access == WorkflowScriptAccess::HostFilesystem {
        return Ok(());
    }
    if input.name.is_some() || input.script_path.is_some() {
        return Err(model_bounded_error(
            "Workflow accepts only the `script` source when the selected execution environment filesystem is foreign to the app-server host; `name` and `scriptPath` are not allowed",
        ));
    }
    if input.script.is_none() {
        return Err(model_bounded_error(
            "Workflow requires an inline `script` when the selected execution environment filesystem is foreign to the app-server host",
        ));
    }
    Ok(())
}

fn host_native_workflow_paths(
    environment: &TurnEnvironmentSelection,
) -> Result<
    Option<(
        codex_utils_absolute_path::AbsolutePathBuf,
        Vec<codex_utils_absolute_path::AbsolutePathBuf>,
    )>,
    FunctionCallError,
> {
    let Ok(cwd) = environment.cwd.to_abs_path() else {
        return Ok(None);
    };
    #[allow(clippy::redundant_closure_for_method_calls)]
    let workspace_roots = environment
        .workspace_roots
        .iter()
        .map(|root| root.to_abs_path())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            workflow_diagnostic(format_args!(
                "Workflow cannot map the selected workspace roots to this host: {error}"
            ))
        })?;
    Ok(Some((cwd, workspace_roots)))
}

fn workflow_host_paths(
    location: WorkflowEnvironmentLocation,
    environment: &TurnEnvironmentSelection,
) -> Result<
    Option<(
        codex_utils_absolute_path::AbsolutePathBuf,
        Vec<codex_utils_absolute_path::AbsolutePathBuf>,
    )>,
    FunctionCallError,
> {
    match location {
        WorkflowEnvironmentLocation::Local => host_native_workflow_paths(environment),
        WorkflowEnvironmentLocation::Remote => Ok(None),
    }
}

fn projected_agent_config(invocation: &ToolCall) -> Result<Config, FunctionCallError> {
    invocation
        .agent_configuration::<Config>()
        .cloned()
        .ok_or_else(|| {
            workflow_diagnostic(
                "Workflow cannot establish an authoritative execution context because the owning sampling step did not expose its effective agent configuration",
            )
        })
}

fn workflow_diagnostic(message: impl std::fmt::Display) -> FunctionCallError {
    model_bounded_error(message)
}

fn preflight_model_launch_response(
    thread_id: ThreadId,
    config: &Config,
    resolved: &crate::discovery::ResolvedWorkflow,
) -> Result<(), FunctionCallError> {
    let run_id = resolved
        .resume_from_run_id
        .as_deref()
        .unwrap_or(GENERATED_WORKFLOW_RUN_ID);
    let workflow_name = resolved.script.meta.name.clone();
    let session_dir = workflow_session_dir(&config.codex_home, thread_id);
    let transcript_dir = session_dir
        .join("subagents/workflows")
        .join(run_id)
        .display()
        .to_string();
    let slug = workflow_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('-');
    let script_path = session_dir
        .join("workflows/scripts")
        .join(format!("{slug}-{run_id}.js"))
        .display()
        .to_string();
    let preview = WorkflowLaunch {
        status: "async_launched".to_string(),
        task_id: GENERATED_WORKFLOW_TASK_ID.to_string(),
        task_type: "local_workflow".to_string(),
        workflow_name: workflow_name.clone(),
        run_id: run_id.to_string(),
        summary: format!("Running workflow {workflow_name}"),
        transcript_dir,
        script_path,
    };
    model_bounded_json_value(WORKFLOW_TOOL_NAME, &model_launch_response(&preview)).map(|_| ())
}

fn model_launch_response(launch: &WorkflowLaunch) -> serde_json::Value {
    serde_json::json!({
        "status": launch.status,
        "taskId": launch.task_id,
        "taskType": launch.task_type,
        "workflowName": launch.workflow_name,
        "runId": launch.run_id,
        "summary": launch.summary,
        "transcriptDir": launch.transcript_dir,
        "scriptPath": launch.script_path,
    })
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push_str("...");
    }
    bounded
}

fn bounded_approval_failure(prefix: &str, detail: &str) -> String {
    let detail = detail.trim();
    let message = if detail.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {detail}")
    };
    truncate_model_text(&message, MODEL_ERROR_MAX_BYTES)
}

fn canonical_json_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json_value).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json_value(value)))
                    .collect(),
            )
        }
        other => other,
    }
}

fn approval_arguments_preview(arguments: &str) -> String {
    let full_preview_bytes = APPROVAL_PREVIEW_SEGMENT_BYTES.saturating_mul(2);
    if arguments.len() <= full_preview_bytes {
        return format!("Arguments (canonical, complete):\n{arguments}");
    }

    let head_end = floor_char_boundary(arguments, APPROVAL_PREVIEW_SEGMENT_BYTES);
    let tail_start = ceil_char_boundary(
        arguments,
        arguments
            .len()
            .saturating_sub(APPROVAL_PREVIEW_SEGMENT_BYTES),
    );
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "Arguments preview (canonical, INCOMPLETE: {omitted} bytes omitted; verify the arguments SHA-256 before approving):\n--- first {head_end} bytes ---\n{}\n--- omitted {omitted} bytes ---\n--- last {} bytes ---\n{}",
        &arguments[..head_end],
        arguments.len().saturating_sub(tail_start),
        &arguments[tail_start..],
    )
}

fn approval_script_preview(source: &str) -> String {
    let full_preview_bytes = APPROVAL_PREVIEW_SEGMENT_BYTES.saturating_mul(2);
    if source.len() <= full_preview_bytes {
        return format!("Script (complete):\n{source}");
    }

    let head_end = floor_char_boundary(source, APPROVAL_PREVIEW_SEGMENT_BYTES);
    let tail_start = ceil_char_boundary(
        source,
        source.len().saturating_sub(APPROVAL_PREVIEW_SEGMENT_BYTES),
    );
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "Script preview (INCOMPLETE: {omitted} bytes omitted; verify the SHA-256 before approving):\n--- first {head_end} bytes ---\n{}\n--- omitted {omitted} bytes ---\n--- last {} bytes ---\n{}",
        &source[..head_end],
        source.len().saturating_sub(tail_start),
        &source[tail_start..],
    )
}

fn approval_children_summary(composition: &FrozenWorkflowComposition) -> String {
    if composition.child_count() == 0 {
        return "Frozen child workflows: none".to_string();
    }

    let mut summary = format!(
        "Frozen child workflows ({}; complete bindings and sources are hash-bound in the approval action):",
        composition.child_count()
    );
    for child in composition.children() {
        let binding = serde_json::to_string(&child.reference).unwrap_or_default();
        summary.push_str(&format!(
            "\n\nBinding: {binding}\nSource: {}\nScript size: {} bytes\nScript SHA-256: {}\n{}",
            child.origin.approval_label(),
            child.script.source.len(),
            child.script_sha256,
            approval_script_preview(&child.script.source),
        ));
    }
    if summary.len() <= APPROVAL_CHILD_SUMMARY_BYTES {
        return summary;
    }
    let end = floor_char_boundary(&summary, APPROVAL_CHILD_SUMMARY_BYTES);
    format!(
        "{}\n\nChild preview truncated; review the complete hash-bound child sources in the approval action.",
        &summary[..end]
    )
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
