//! Central approval policy-stage execution and reviewer routing.

use crate::command_canonicalization::canonicalize_command_for_approval;
use crate::guardian::GuardianNetworkAccessTrigger;
use crate::guardian::GuardianReviewContext;
use crate::guardian::GuardianReviewOptions;
use crate::guardian::guardian_timeout_message;
use crate::guardian::new_guardian_review_id;
use crate::guardian::review_approval_request;
use crate::guardian::review_approval_request_with_cancel;
use crate::guardian::routes_approval_policy_to_guardian;
use crate::guardian::spawn_approval_request_review;
use crate::hook_runtime::run_permission_request_hooks;
use crate::mcp_tool_call::request_mcp_tool_user_approval;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::events::truncate_rejection_message;
use crate::tools::hook_names::HookToolName;
use crate::tools::runtimes::apply_patch::ApplyPatchApprovalKey;
use crate::tools::runtimes::unified_exec::UnifiedExecApprovalKey;
use crate::tools::sandboxing::ApprovalRequestReasons;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::with_cached_approval;
use codex_analytics::GuardianApprovalRequestSource;
use codex_config::types::AppToolApproval;
use codex_hooks::PermissionRequestDecision;
use codex_otel::ToolDecisionSource;
use codex_protocol::approvals::ExecPolicyAmendment;
#[cfg(unix)]
use codex_protocol::approvals::GuardianCommandSource;
use codex_protocol::approvals::NetworkApprovalContext;
use codex_protocol::approvals::NetworkApprovalProtocol;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::error::CodexErr;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::NetworkPolicyRuleAction;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_tools::ToolApprovalDenialSource;
use codex_tools::ToolApprovalOutcome;
use codex_tools::ToolApprovalRequest;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_string::truncate_middle_with_token_budget;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::warn;

const EXTENSION_REJECTION_MAX_TOKENS: usize = 800;

#[derive(Clone)]
pub(crate) struct ApprovalContext {
    pub(crate) review_context: GuardianReviewContext,
    pub(crate) cancellation_token: Option<CancellationToken>,
    pub(crate) call_id: String,
    pub(crate) tool_name: ToolName,
    pub(crate) strict_auto_review: bool,
    pub(crate) approval_reason: Option<String>,
    pub(crate) retry_reason: Option<String>,
    pub(crate) network_approval_context: Option<NetworkApprovalContext>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ApprovalAction {
    ExecCommand {
        id: String,
        environment_id: String,
        command: Vec<String>,
        hook_command: String,
        cwd: PathUri,
        sandbox_permissions: SandboxPermissions,
        additional_permissions: Option<AdditionalPermissionProfile>,
        justification: Option<String>,
        tty: bool,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    },
    #[cfg(unix)]
    Execve {
        id: String,
        approval_id: String,
        environment_id: String,
        source: GuardianCommandSource,
        program: AbsolutePathBuf,
        argv: Vec<String>,
        command: Vec<String>,
        cwd: AbsolutePathBuf,
        additional_permissions: Option<AdditionalPermissionProfile>,
    },
    ApplyPatch {
        id: String,
        environment_id: String,
        cwd: PathUri,
        files: Vec<PathUri>,
        patch: String,
        changes: Arc<HashMap<PathBuf, FileChange>>,
        permissions_preapproved: bool,
    },
    McpToolCall {
        id: String,
        server: String,
        tool_name: String,
        arguments: Option<serde_json::Value>,
        connector_id: Option<String>,
        connector_name: Option<String>,
        connector_description: Option<String>,
        connected_account_email: Option<String>,
        tool_title: Option<String>,
        tool_description: Option<String>,
        annotations: Option<crate::guardian::GuardianMcpAnnotations>,
        hook_tool_name: HookToolName,
        approval_policy: AskForApproval,
        reviewer: ApprovalsReviewer,
        approval_mode: AppToolApproval,
        allow_session_remember: bool,
        allow_persistent_approval: bool,
    },
    ExtensionTool {
        id: String,
        tool_name: String,
        hook_tool_name: HookToolName,
        prompt: ToolApprovalRequest,
        action: serde_json::Value,
        artifact: Option<codex_tools::ToolApprovalArtifact>,
    },
    NetworkAccess {
        id: String,
        turn_id: String,
        environment_id: String,
        target: String,
        host: String,
        protocol: NetworkApprovalProtocol,
        port: u16,
        trigger: Option<GuardianNetworkAccessTrigger>,
        hook_command: String,
        hook_run_id: String,
        command: Vec<String>,
        cwd: AbsolutePathBuf,
    },
    RequestPermissions {
        id: String,
        turn_id: String,
        reason: Option<String>,
        permissions: RequestPermissionProfile,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum ApprovalCacheKey {
    ExecCommand(UnifiedExecApprovalKey),
    ApplyPatch(ApplyPatchApprovalKey),
}

impl ApprovalAction {
    pub(crate) fn permission_request_payload(&self) -> PermissionRequestPayload {
        match self {
            Self::ExecCommand {
                hook_command,
                justification,
                ..
            } => PermissionRequestPayload::bash(hook_command.clone(), justification.clone()),
            #[cfg(unix)]
            Self::Execve { command, .. } => PermissionRequestPayload::bash(
                codex_shell_command::parse_command::shlex_join(command),
                /*description*/ None,
            ),
            Self::ApplyPatch { patch, .. } => PermissionRequestPayload {
                tool_name: HookToolName::apply_patch(),
                tool_input: serde_json::json!({ "command": patch }),
            },
            Self::McpToolCall {
                hook_tool_name,
                arguments,
                ..
            } => PermissionRequestPayload {
                tool_name: hook_tool_name.clone(),
                tool_input: arguments
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            },
            Self::ExtensionTool {
                hook_tool_name,
                action,
                ..
            } => PermissionRequestPayload {
                tool_name: hook_tool_name.clone(),
                tool_input: action.clone(),
            },
            Self::NetworkAccess {
                hook_command,
                target,
                ..
            } => PermissionRequestPayload::bash(
                hook_command.clone(),
                Some(format!("network-access {target}")),
            ),
            Self::RequestPermissions {
                reason,
                permissions,
                ..
            } => PermissionRequestPayload {
                tool_name: HookToolName::new("request_permissions"),
                tool_input: serde_json::json!({
                    "reason": reason,
                    "permissions": permissions,
                }),
            },
        }
    }

    pub(crate) fn cache_keys(&self) -> Vec<ApprovalCacheKey> {
        match self {
            Self::ExecCommand {
                environment_id,
                command,
                cwd,
                tty,
                sandbox_permissions,
                additional_permissions,
                ..
            } => vec![ApprovalCacheKey::ExecCommand(UnifiedExecApprovalKey {
                environment_id: environment_id.clone(),
                executable: command.first().cloned(),
                command: canonicalize_command_for_approval(command),
                cwd: cwd.clone(),
                tty: *tty,
                sandbox_permissions: *sandbox_permissions,
                additional_permissions: additional_permissions.clone(),
            })],
            #[cfg(unix)]
            Self::Execve { .. } => Vec::new(),
            Self::McpToolCall { .. }
            | Self::ExtensionTool { .. }
            | Self::NetworkAccess { .. }
            | Self::RequestPermissions { .. } => Vec::new(),
            Self::ApplyPatch {
                environment_id,
                files,
                ..
            } => files
                .iter()
                .cloned()
                .map(|path| {
                    ApprovalCacheKey::ApplyPatch(ApplyPatchApprovalKey {
                        environment_id: environment_id.clone(),
                        path,
                    })
                })
                .collect(),
        }
    }

    pub(super) fn into_guardian_request(
        self,
    ) -> std::io::Result<crate::guardian::GuardianApprovalRequest> {
        Ok(match self {
            Self::ExecCommand {
                id,
                environment_id,
                command,
                cwd,
                sandbox_permissions,
                additional_permissions,
                justification,
                tty,
                ..
            } => crate::guardian::GuardianApprovalRequest::ExecCommand {
                id,
                command,
                cwd: guardian_cwd(&environment_id, cwd)?,
                sandbox_permissions,
                additional_permissions,
                justification,
                tty,
            },
            #[cfg(unix)]
            Self::Execve {
                id,
                source,
                program,
                argv,
                cwd,
                additional_permissions,
                ..
            } => crate::guardian::GuardianApprovalRequest::Execve {
                id,
                source,
                program: program.to_string_lossy().into_owned(),
                argv,
                cwd,
                additional_permissions,
            },
            Self::ApplyPatch {
                id,
                environment_id,
                cwd,
                files,
                patch,
                ..
            } => crate::guardian::GuardianApprovalRequest::ApplyPatch {
                id,
                cwd: guardian_cwd(&environment_id, cwd)?,
                files: files
                    .into_iter()
                    .map(|path| path.to_abs_path())
                    .collect::<std::io::Result<Vec<_>>>()?,
                patch,
            },
            Self::McpToolCall {
                id,
                server,
                tool_name,
                arguments,
                connector_id,
                connector_name,
                connector_description,
                connected_account_email,
                tool_title,
                tool_description,
                annotations,
                ..
            } => crate::guardian::GuardianApprovalRequest::McpToolCall {
                id,
                server,
                tool_name,
                arguments,
                connector_id,
                connector_name,
                connector_description,
                connected_account_email,
                tool_title,
                tool_description,
                annotations,
            },
            Self::ExtensionTool {
                id,
                tool_name,
                action,
                artifact,
                ..
            } => {
                let Some(artifact) = artifact else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "extension automatic review requires a content-addressed artifact",
                    ));
                };
                if !artifact.has_valid_sha256() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "extension approval artifact failed SHA-256 verification",
                    ));
                }
                let artifact_action: serde_json::Value =
                    serde_json::from_str(artifact.contents()).map_err(std::io::Error::other)?;
                if artifact_action != action {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "extension approval artifact does not match the requested action",
                    ));
                }
                crate::guardian::GuardianApprovalRequest::ExtensionTool {
                    id,
                    tool_name,
                    artifact: crate::guardian::GuardianApprovalArtifact::new(artifact),
                }
            }
            Self::NetworkAccess {
                id,
                turn_id,
                target,
                host,
                protocol,
                port,
                trigger,
                ..
            } => crate::guardian::GuardianApprovalRequest::NetworkAccess {
                id,
                turn_id,
                target,
                host,
                protocol,
                port,
                trigger,
            },
            Self::RequestPermissions {
                id,
                turn_id,
                reason,
                permissions,
            } => crate::guardian::GuardianApprovalRequest::RequestPermissions {
                id,
                turn_id,
                reason,
                permissions,
            },
        })
    }
}

fn guardian_cwd(environment_id: &str, cwd: PathUri) -> std::io::Result<AbsolutePathBuf> {
    match cwd.to_abs_path() {
        Ok(cwd) => Ok(cwd),
        Err(err) if environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID => Err(err),
        Err(_) => {
            let cwd_display = cwd.to_string();
            let path = cwd.to_url().to_file_path().map_err(|()| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("local cwd URI `{cwd_display}` is not a host-native path"),
                )
            })?;
            AbsolutePathBuf::from_absolute_path_checked(path).map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("local cwd URI `{cwd_display}` is not absolute: {err}"),
                )
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalReviewer {
    Guardian,
    User,
}

impl ApprovalReviewer {
    fn for_policy(approval_policy: AskForApproval, reviewer: ApprovalsReviewer) -> Self {
        if routes_approval_policy_to_guardian(approval_policy, reviewer) {
            Self::Guardian
        } else {
            Self::User
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalResolutionSource {
    Hook,
    Guardian,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApprovalResolution {
    decision: ReviewDecision,
    source: ApprovalResolutionSource,
}

impl ApprovalResolution {
    fn into_tool_result(self, model_info: &ModelInfo) -> Result<ReviewDecision, ToolError> {
        let source = self.source;
        match self.decision {
            ReviewDecision::ApprovedMcpPolicyAmendment => {
                error!("Tool approval received ApprovedMcpPolicyAmendment");
                Err(ToolError::Rejected(
                    "Error while requesting approval".to_string(),
                ))
            }
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } if network_policy_amendment.action == NetworkPolicyRuleAction::Deny => {
                let rejection = match source {
                    ApprovalResolutionSource::Hook => "rejected by configuration",
                    ApprovalResolutionSource::Guardian => {
                        "automatic approval review denied the action"
                    }
                    ApprovalResolutionSource::User => "rejected by user",
                };
                Err(ToolError::Rejected(rejection.to_string()))
            }
            ReviewDecision::Denied { rejection } => Err(ToolError::Rejected(rejection)),
            ReviewDecision::TimedOut => {
                Err(ToolError::Rejected(guardian_timeout_message(model_info)))
            }
            ReviewDecision::Abort => Err(ToolError::Codex(CodexErr::TurnAborted)),
            decision => Ok(decision),
        }
    }

    fn into_extension_outcome(self, model_info: &ModelInfo) -> ToolApprovalOutcome {
        let denial_source = match self.source {
            ApprovalResolutionSource::Hook => ToolApprovalDenialSource::Configuration,
            ApprovalResolutionSource::Guardian => ToolApprovalDenialSource::AutomaticReviewer,
            ApprovalResolutionSource::User => ToolApprovalDenialSource::User,
        };
        match self.decision {
            ReviewDecision::Approved
            | ReviewDecision::ApprovedExecpolicyAmendment { .. }
            | ReviewDecision::ApprovedForSession
            | ReviewDecision::ApprovedMcpPolicyAmendment => ToolApprovalOutcome::Approved,
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => match network_policy_amendment.action {
                NetworkPolicyRuleAction::Allow => ToolApprovalOutcome::Approved,
                NetworkPolicyRuleAction::Deny => ToolApprovalOutcome::Denied {
                    rejection: "approval denied by network policy".to_string(),
                    source: denial_source,
                },
            },
            ReviewDecision::Denied { rejection } => ToolApprovalOutcome::Denied {
                rejection: truncate_extension_rejection(&rejection),
                source: denial_source,
            },
            ReviewDecision::TimedOut => ToolApprovalOutcome::TimedOut {
                rejection: truncate_extension_rejection(&guardian_timeout_message(model_info)),
            },
            ReviewDecision::Abort => {
                let reason = match self.source {
                    ApprovalResolutionSource::Hook => "approval was cancelled by configuration",
                    ApprovalResolutionSource::Guardian => "automatic approval review was cancelled",
                    ApprovalResolutionSource::User => "approval was cancelled by the user",
                };
                ToolApprovalOutcome::Cancelled {
                    reason: reason.to_string(),
                }
            }
        }
    }
}

fn truncate_extension_rejection(rejection: &str) -> String {
    truncate_middle_with_token_budget(rejection, EXTENSION_REJECTION_MAX_TOKENS).0
}

impl Session {
    pub(crate) async fn request_approval(
        self: &Arc<Self>,
        action: ApprovalAction,
        ctx: ApprovalContext,
    ) -> Result<ReviewDecision, ToolError> {
        let is_mcp_tool_call = matches!(&action, ApprovalAction::McpToolCall { .. });
        let is_network_approval = matches!(&action, ApprovalAction::NetworkAccess { .. });
        let resolution = self.resolve_approval(action, &ctx).await;

        if is_mcp_tool_call && resolution.decision == ReviewDecision::ApprovedMcpPolicyAmendment {
            return Ok(resolution.decision);
        }
        if is_network_approval {
            match (&resolution.decision, resolution.source) {
                (
                    ReviewDecision::NetworkPolicyAmendment {
                        network_policy_amendment,
                    },
                    _,
                ) if network_policy_amendment.action == NetworkPolicyRuleAction::Deny => {
                    return Ok(resolution.decision);
                }
                (ReviewDecision::Abort, ApprovalResolutionSource::Guardian) => {
                    return Err(ToolError::Rejected(
                        "automatic approval review was cancelled".to_string(),
                    ));
                }
                _ => {}
            }
        }
        resolution.into_tool_result(ctx.review_context.turn().model_info())
    }

    pub(crate) async fn request_extension_tool_approval(
        self: &Arc<Self>,
        action: ApprovalAction,
        ctx: ApprovalContext,
    ) -> ToolApprovalOutcome {
        debug_assert!(matches!(&action, ApprovalAction::ExtensionTool { .. }));
        self.resolve_approval(action, &ctx)
            .await
            .into_extension_outcome(ctx.review_context.turn().model_info())
    }

    pub(crate) async fn request_extension_tool_user_approval(
        self: &Arc<Self>,
        request: ToolApprovalRequest,
        ctx: ApprovalContext,
    ) -> ToolApprovalOutcome {
        let resolution = ApprovalResolution {
            decision: request_extension_tool_user_decision(
                self,
                ctx.review_context.turn(),
                &ctx.call_id,
                &request,
            )
            .await,
            source: ApprovalResolutionSource::User,
        };
        record_resolution(&ctx, &resolution);
        resolution.into_extension_outcome(ctx.review_context.turn().model_info())
    }

    async fn resolve_approval(
        self: &Arc<Self>,
        action: ApprovalAction,
        ctx: &ApprovalContext,
    ) -> ApprovalResolution {
        let is_network_approval = matches!(&action, ApprovalAction::NetworkAccess { .. });
        let permission_request_run_id = match &action {
            #[cfg(unix)]
            ApprovalAction::Execve { approval_id, .. } => approval_id.clone(),
            ApprovalAction::NetworkAccess { hook_run_id, .. } => hook_run_id.clone(),
            _ if ctx.retry_reason.is_some() => format!("{}:retry", ctx.call_id),
            _ => ctx.call_id.clone(),
        };

        // Approval precedence is:
        // 1. Hooks
        // 2. If StrictAutoReview || Guardian enabled, then Guardian. Else, user.
        let resolution = match run_permission_request_hooks(
            self,
            ctx.review_context.turn(),
            &permission_request_run_id,
            action.permission_request_payload(),
        )
        .await
        {
            Some(PermissionRequestDecision::Allow) => ApprovalResolution {
                decision: ReviewDecision::Approved,
                source: ApprovalResolutionSource::Hook,
            },
            Some(PermissionRequestDecision::Deny { message }) => ApprovalResolution {
                decision: ReviewDecision::denied(message),
                source: ApprovalResolutionSource::Hook,
            },
            None => self.request_reviewer_approval(action, ctx).await,
        };
        // Network approvals record their final telemetry after validation and persistence.
        if !is_network_approval {
            record_resolution(ctx, &resolution);
        }
        resolution
    }

    async fn request_reviewer_approval(
        self: &Arc<Self>,
        action: ApprovalAction,
        ctx: &ApprovalContext,
    ) -> ApprovalResolution {
        let reviewer = if ctx.strict_auto_review {
            ApprovalReviewer::Guardian
        } else if let ApprovalAction::McpToolCall {
            approval_policy,
            reviewer,
            ..
        } = &action
        {
            ApprovalReviewer::for_policy(*approval_policy, *reviewer)
        } else {
            ApprovalReviewer::for_policy(
                ctx.review_context.approval_policy,
                ctx.review_context.approvals_reviewer,
            )
        };

        let decision = match reviewer {
            ApprovalReviewer::Guardian => {
                let review: std::pin::Pin<
                    Box<dyn std::future::Future<Output = ReviewDecision> + Send + '_>,
                > = Box::pin(self.request_guardian_approval(action, ctx));
                review.await
            }
            ApprovalReviewer::User => self.request_user_approval(&action, ctx).await,
        };
        let source = match reviewer {
            ApprovalReviewer::Guardian => ApprovalResolutionSource::Guardian,
            ApprovalReviewer::User => ApprovalResolutionSource::User,
        };
        ApprovalResolution { decision, source }
    }

    pub(crate) async fn request_guardian_approval(
        self: &Arc<Self>,
        action: ApprovalAction,
        ctx: &ApprovalContext,
    ) -> ReviewDecision {
        let is_network_approval = matches!(&action, ApprovalAction::NetworkAccess { .. });
        let review_id = new_guardian_review_id();
        let action = match action.into_guardian_request() {
            Ok(action) => action,
            Err(err) => {
                tracing::error!(%err, "failed to build automatic approval action");
                return ReviewDecision::denied(format!(
                    "automatic approval review could not prepare the action: {err}"
                ));
            }
        };

        if let Some(cancellation_token) = &ctx.cancellation_token {
            let review = spawn_approval_request_review(
                Arc::clone(self),
                ctx.review_context.clone(),
                review_id,
                action,
                ApprovalRequestReasons {
                    approval: ctx.approval_reason.clone(),
                    retry: ctx.retry_reason.clone(),
                },
                GuardianReviewOptions {
                    plugin_attribution_override: None,
                    approval_request_source: GuardianApprovalRequestSource::MainTurn,
                    external_cancel: Some(cancellation_token.clone()),
                },
            );
            review.await.unwrap_or_else(|_| {
                ReviewDecision::denied("automatic approval review could not complete")
            })
        } else if is_network_approval {
            let review_cancel = CancellationToken::new();
            let review_cancel_guard = review_cancel.clone().drop_guard();
            let review_session = Arc::clone(self);
            let review_context = ctx.review_context.clone();
            let retry_reason = ctx.retry_reason.clone();
            let review = tokio::spawn(async move {
                review_approval_request_with_cancel(
                    &review_session,
                    review_context,
                    review_id,
                    action,
                    retry_reason,
                    GuardianReviewOptions {
                        plugin_attribution_override: None,
                        approval_request_source: GuardianApprovalRequestSource::MainTurn,
                        external_cancel: Some(review_cancel),
                    },
                )
                .await
            });
            let decision = review.await.unwrap_or_else(|err| {
                warn!("network Guardian review task failed: {err}");
                ReviewDecision::denied("automatic approval review could not complete")
            });
            drop(review_cancel_guard.disarm());
            decision
        } else {
            review_approval_request(
                self,
                ctx.review_context.clone(),
                review_id,
                action,
                ApprovalRequestReasons {
                    approval: ctx.approval_reason.clone(),
                    retry: ctx.retry_reason.clone(),
                },
            )
            .await
        }
    }

    async fn request_user_approval(
        &self,
        action: &ApprovalAction,
        ctx: &ApprovalContext,
    ) -> ReviewDecision {
        match action {
            ApprovalAction::ExecCommand {
                environment_id,
                command,
                cwd,
                additional_permissions,
                justification,
                proposed_execpolicy_amendment,
                ..
            } => {
                let cwd = match guardian_cwd(environment_id, cwd.clone()) {
                    Ok(cwd) => cwd,
                    Err(err) => {
                        tracing::error!(%err, "failed to resolve approval command cwd");
                        return ReviewDecision::denied(format!(
                            "failed to resolve approval command cwd: {err}"
                        ));
                    }
                };
                let tool_name = "unified_exec";
                let reason = ctx
                    .retry_reason
                    .clone()
                    .or_else(|| ctx.approval_reason.clone())
                    .or_else(|| justification.clone());
                let policy_fingerprint = ctx
                    .review_context
                    .environments()
                    .turn_environments()
                    .find(|environment| environment.selection.environment_id == *environment_id)
                    .and_then(|environment| environment.config().exec_policy.as_ref())
                    .map(codex_execpolicy::RequirementsExecPolicy::fingerprint);
                let cache_keys = action
                    .cache_keys()
                    .into_iter()
                    .map(|key| (key, &policy_fingerprint))
                    .collect();
                with_cached_approval(&self.services, tool_name, cache_keys, || async {
                    self.request_command_approval(
                        ctx.review_context.turn(),
                        ctx.call_id.clone(),
                        /*approval_id*/ None,
                        Some(environment_id.clone()),
                        command.clone(),
                        cwd,
                        reason,
                        ctx.network_approval_context.clone(),
                        proposed_execpolicy_amendment.clone(),
                        additional_permissions.clone(),
                        /*available_decisions*/ None,
                        /*plugin_attribution_override*/ None,
                    )
                    .await
                })
                .await
            }
            #[cfg(unix)]
            ApprovalAction::Execve {
                approval_id,
                environment_id,
                command,
                cwd,
                additional_permissions,
                ..
            } => {
                self.request_command_approval(
                    ctx.review_context.turn(),
                    ctx.call_id.clone(),
                    Some(approval_id.clone()),
                    Some(environment_id.clone()),
                    command.clone(),
                    cwd.clone(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    additional_permissions.clone(),
                    Some(vec![ReviewDecision::Approved, ReviewDecision::Abort]),
                    /*plugin_attribution_override*/ None,
                )
                .await
            }
            ApprovalAction::ApplyPatch {
                changes,
                permissions_preapproved,
                ..
            } => {
                let reason = ctx
                    .retry_reason
                    .clone()
                    .or_else(|| ctx.approval_reason.clone());
                if *permissions_preapproved && reason.is_none() {
                    return ReviewDecision::Approved;
                }
                if reason.is_some() {
                    return self
                        .request_patch_approval(
                            ctx.review_context.turn(),
                            ctx.call_id.clone(),
                            changes.as_ref().clone(),
                            reason,
                            /*grant_root*/ None,
                        )
                        .await;
                }
                with_cached_approval(
                    &self.services,
                    "apply_patch",
                    action.cache_keys(),
                    || async {
                        self.request_patch_approval(
                            ctx.review_context.turn(),
                            ctx.call_id.clone(),
                            changes.as_ref().clone(),
                            /*reason*/ None,
                            /*grant_root*/ None,
                        )
                        .await
                    },
                )
                .await
            }
            ApprovalAction::McpToolCall { .. } => {
                request_mcp_tool_user_approval(
                    self,
                    ctx.review_context.turn(),
                    &ctx.call_id,
                    action,
                )
                .await
            }
            ApprovalAction::ExtensionTool { prompt, .. } => {
                request_extension_tool_user_decision(
                    self,
                    ctx.review_context.turn(),
                    &ctx.call_id,
                    prompt,
                )
                .await
            }
            ApprovalAction::NetworkAccess {
                environment_id,
                command,
                cwd,
                ..
            } => {
                self.request_command_approval(
                    ctx.review_context.turn(),
                    ctx.call_id.clone(),
                    /*approval_id*/ None,
                    Some(environment_id.clone()),
                    command.clone(),
                    cwd.clone(),
                    ctx.approval_reason.clone(),
                    ctx.network_approval_context.clone(),
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
            }
            ApprovalAction::RequestPermissions { .. } => {
                unreachable!("permission requests are routed directly to Guardian")
            }
        }
    }
}

async fn request_extension_tool_user_decision(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    request: &ToolApprovalRequest,
) -> ReviewDecision {
    let response = session
        .request_user_input(
            turn,
            call_id.to_string(),
            codex_protocol::request_user_input::RequestUserInputArgs {
                questions: vec![
                    codex_protocol::request_user_input::RequestUserInputQuestion {
                        id: request.id.clone(),
                        header: request.header.clone(),
                        question: request.question.clone(),
                        is_other: true,
                        is_secret: false,
                        options: Some(vec![
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: request.approve_label.clone(),
                                description: "Approve this extension action.".to_string(),
                            },
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: request.deny_label.clone(),
                                description: "Do not perform this extension action.".to_string(),
                            },
                        ]),
                    },
                ],
                is_blocking: true,
                auto_resolution_ms: None,
            },
        )
        .await;
    let Some(answer) = response
        .as_ref()
        .and_then(|response| response.answers.get(&request.id))
    else {
        return ReviewDecision::Abort;
    };
    if answer
        .answers
        .iter()
        .any(|answer| answer == &request.approve_label)
    {
        return ReviewDecision::Approved;
    }
    let rejection = answer
        .answers
        .iter()
        .map(|answer| answer.trim())
        .filter(|answer| !answer.is_empty() && *answer != request.deny_label.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !rejection.is_empty() {
        return ReviewDecision::denied(truncate_rejection_message(&rejection));
    }
    if answer
        .answers
        .iter()
        .any(|answer| answer == &request.deny_label)
    {
        ReviewDecision::denied("rejected by user")
    } else {
        ReviewDecision::Abort
    }
}

fn record_resolution(ctx: &ApprovalContext, resolution: &ApprovalResolution) {
    let source = match resolution.source {
        ApprovalResolutionSource::Hook => ToolDecisionSource::Config,
        ApprovalResolutionSource::Guardian => ToolDecisionSource::AutomatedReviewer,
        ApprovalResolutionSource::User => ToolDecisionSource::User,
    };
    ctx.review_context.turn().session_telemetry.tool_decision(
        &ctx.tool_name,
        &ctx.call_id,
        &resolution.decision,
        Some(source),
    );
}

#[cfg(all(test, unix))]
#[path = "approvals_tests.rs"]
mod tests;
