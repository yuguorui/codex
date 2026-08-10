use crate::FunctionCallError;
use crate::ToolName;
use crate::ToolPayload;
use codex_extension_items::ExtensionItem;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemSandboxContext;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;
use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

/// Raw response history snapshot available when an extension tool is invoked.
#[derive(Clone, Debug, Default)]
pub struct ConversationHistory {
    items: Arc<[ResponseItem]>,
}

impl ConversationHistory {
    pub fn new(items: Vec<ResponseItem>) -> Self {
        Self {
            items: items.into(),
        }
    }

    pub fn items(&self) -> &[ResponseItem] {
        &self.items
    }
}

/// Future returned when an extension tool emits a visible turn-item lifecycle event.
pub type TurnItemEmissionFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub type ToolApprovalFuture<'a> = Pin<Box<dyn Future<Output = ToolApprovalDecision> + Send + 'a>>;
pub type ToolApprovalOutcomeFuture<'a> =
    Pin<Box<dyn Future<Output = ToolApprovalOutcome> + Send + 'a>>;
pub type TurnActivityFuture<'a> = Pin<Box<dyn Future<Output = Option<TurnActivity>> + Send + 'a>>;

/// How the host will review a structured extension approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolApprovalReviewMode {
    /// A user reviews the request.
    User,
    /// An automatic reviewer handles the request.
    Automatic,
    /// Automatic review is mandatory, including when the ordinary approval policy is `never`.
    StrictAutomatic,
}

/// Host activity that may interrupt a blocking extension tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnActivity {
    /// New input was steered into the turn that owns the tool call.
    UserInput,
}

/// Read-only subscription to activity on the turn that owns an extension tool call.
///
/// Implementations must not consume the underlying turn input. Once activity is
/// observed, both [`Self::observed`] and subsequent [`Self::wait`] calls should
/// continue to report it so extension tools can safely check more than once.
pub trait TurnActivitySubscription: Send + Sync {
    /// Returns activity already observed by this subscription, if any.
    fn observed(&self) -> Option<TurnActivity>;

    /// Waits until activity is observed or the host can no longer observe the turn.
    fn wait<'a>(&'a self) -> TurnActivityFuture<'a>;
}

/// Read-only view of the token target shared by a turn and its background work.
pub trait ToolTokenBudget: Send + Sync {
    fn total(&self) -> u64;

    fn spent(&self) -> u64;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalRequest {
    pub call_id: String,
    pub id: String,
    pub header: String,
    pub question: String,
    pub approve_label: String,
    pub deny_label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalReviewRequest {
    pub prompt: ToolApprovalRequest,
    pub action: serde_json::Value,
    pub artifact: Option<ToolApprovalArtifact>,
}

/// Immutable, content-addressed data made available to an automatic approval reviewer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalArtifact {
    sha256: String,
    contents: Arc<str>,
}

impl ToolApprovalArtifact {
    /// Creates a content-addressed artifact from its exact UTF-8 contents.
    pub fn from_contents(contents: String) -> Self {
        let sha256 = sha256_hex(&contents);
        Self::new(sha256, contents)
    }

    /// Creates an artifact with an externally computed content hash.
    pub fn new(sha256: String, contents: String) -> Self {
        Self {
            sha256,
            contents: contents.into(),
        }
    }

    /// Returns the expected SHA-256 digest.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Returns the exact artifact contents.
    pub fn contents(&self) -> &str {
        &self.contents
    }

    /// Checks that the digest binds the exact stored contents.
    pub fn has_valid_sha256(&self) -> bool {
        sha256_hex(&self.contents) == self.sha256
    }
}

/// Returns the lowercase SHA-256 digest of UTF-8 text.
pub fn sha256_hex(contents: &str) -> String {
    format!("{:x}", Sha256::digest(contents.as_bytes()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolApprovalDecision {
    Approved,
    Denied,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolApprovalDenialSource {
    User,
    AutomaticReviewer,
    Configuration,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolApprovalOutcome {
    Approved,
    Denied {
        rejection: String,
        source: ToolApprovalDenialSource,
    },
    TimedOut {
        rejection: String,
    },
    Cancelled {
        reason: String,
    },
    Unavailable,
}

/// Visible turn items that an extension may publish into the host lifecycle.
#[derive(Clone, Debug)]
pub struct ExtensionTurnItem {
    /// Canonical extension item plus compatibility events derived by its owner.
    ///
    /// Core intentionally does not inspect extension-owned payloads, so it
    /// cannot derive their legacy fanout. It emits the canonical lifecycle
    /// event first, then these extension-provided events. Core also skips
    /// global turn-item contributors here so extensions cannot mutate items
    /// owned by other extensions.
    pub item: ExtensionItem,
    pub legacy_events: Vec<EventMsg>,
}

impl ExtensionTurnItem {
    pub fn workflow_input_analysis(id: String) -> Self {
        Self {
            item: ExtensionItem::WorkflowInputAnalysis(
                codex_extension_items::workflow::WorkflowInputAnalysisItem { id },
            ),
            legacy_events: Vec::new(),
        }
    }

    pub fn workflow_result_read(id: String, run_id: Option<String>) -> Self {
        Self {
            item: ExtensionItem::WorkflowResultRead(
                codex_extension_items::workflow::WorkflowResultReadItem {
                    id,
                    run_id,
                    status: codex_extension_items::workflow::WorkflowResultReadStatus::InProgress,
                },
            ),
            legacy_events: Vec::new(),
        }
    }
}

/// Host-provided capability for extension tools to emit visible turn items.
///
/// Implementations route lifecycle events through the host's normal item event
/// pipeline and client delivery.
pub trait TurnItemEmitter: Send + Sync {
    /// Emits the beginning of one visible turn item.
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a>;

    /// Emits one completed visible turn item.
    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a>;

    /// Requests a host-rendered confirmation before an extension performs a sensitive action.
    fn request_approval<'a>(&'a self, _request: ToolApprovalRequest) -> ToolApprovalFuture<'a> {
        Box::pin(std::future::ready(ToolApprovalDecision::Unavailable))
    }

    /// Requests approval with a structured action and a reason-preserving outcome.
    ///
    /// The default delegates to the legacy approval API so existing host emitters
    /// remain compatible. Hosts that support automatic review should override this
    /// method and review the complete structured action.
    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        Box::pin(async move {
            match self.request_approval(request.prompt).await {
                ToolApprovalDecision::Approved => ToolApprovalOutcome::Approved,
                ToolApprovalDecision::Denied => ToolApprovalOutcome::Denied {
                    rejection: "approval was denied".to_string(),
                    source: ToolApprovalDenialSource::Unknown,
                },
                ToolApprovalDecision::Unavailable => ToolApprovalOutcome::Unavailable,
            }
        })
    }

    /// Requests a reason-preserving decision directly from the user.
    ///
    /// This deliberately skips automatic review and permission hooks. It is
    /// intended for fallback only after the host has already evaluated policy.
    fn request_user_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        Box::pin(async move {
            match self.request_approval(request).await {
                ToolApprovalDecision::Approved => ToolApprovalOutcome::Approved,
                ToolApprovalDecision::Denied => ToolApprovalOutcome::Denied {
                    rejection: "approval was denied".to_string(),
                    source: ToolApprovalDenialSource::User,
                },
                ToolApprovalDecision::Unavailable => ToolApprovalOutcome::Unavailable,
            }
        })
    }

    /// Returns the host-selected review mode for extension approvals in this tool call.
    fn approval_review_mode(&self) -> ToolApprovalReviewMode {
        ToolApprovalReviewMode::User
    }

    /// Returns the host's live shared token budget when one is configured.
    fn token_budget(&self) -> Option<Arc<dyn ToolTokenBudget>> {
        None
    }

    /// Returns a read-only subscription to activity on the owning turn when supported.
    fn turn_activity(&self) -> Option<Arc<dyn TurnActivitySubscription>> {
        None
    }

    /// Returns URI-aware execution environments captured by the host for this tool call.
    fn execution_environments(&self) -> Vec<ToolExecutionEnvironment> {
        Vec::new()
    }
}

/// Host-owned turn environment summary visible to extension tools.
#[derive(Clone)]
pub struct ToolEnvironment<'call> {
    /// Stable host environment id used to route executor-scoped capabilities.
    pub environment_id: String,
    /// Effective working directory for this turn in the environment.
    pub cwd: AbsolutePathBuf,
    /// Filesystem implementation for this environment.
    pub file_system: Arc<dyn ExecutorFileSystem>,
    /// Sandbox context to use for filesystem operations.
    pub file_system_sandbox_context: FileSystemSandboxContext,
    // TODO(anp): Replace the marker with callback-scoped environment access.
    pub _lifetime: PhantomData<&'call ()>,
    executor_id: String,
    executor: ToolExecutorIdentity,
}

/// URI-aware execution environment context for extensions that support remote filesystems.
#[derive(Clone)]
pub struct ToolExecutionEnvironment {
    /// Stable host environment id used to route executor-scoped capabilities.
    pub environment_id: String,
    /// Effective working directory in the executor's path convention.
    pub cwd: PathUri,
    /// Exact turn selection captured with this tool call, including roots and configuration.
    pub selection: Option<TurnEnvironmentSelection>,
    /// Whether this filesystem is hosted by a remote executor.
    pub is_remote: bool,
    /// Opaque process-scoped identity of the concrete executor selected for this call.
    pub executor_id: String,
    /// Filesystem implementation for this environment.
    pub file_system: Arc<dyn ExecutorFileSystem>,
    /// Sandbox context to use for filesystem operations.
    pub file_system_sandbox_context: FileSystemSandboxContext,
    executor: ToolExecutorIdentity,
}

#[derive(Clone)]
struct ToolExecutorIdentity {
    instance: Arc<dyn Any + Send + Sync>,
}

impl ToolExecutorIdentity {
    fn new<T>(instance: Arc<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        Self { instance }
    }

    fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.instance, &other.instance)
    }
}

impl<'call> ToolEnvironment<'call> {
    /// Creates a local-path projection bound to a concrete host executor.
    pub fn new<T>(
        environment_id: String,
        cwd: AbsolutePathBuf,
        file_system: Arc<dyn ExecutorFileSystem>,
        file_system_sandbox_context: FileSystemSandboxContext,
        executor_id: String,
        executor: Arc<T>,
    ) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            environment_id,
            cwd,
            file_system,
            file_system_sandbox_context,
            _lifetime: PhantomData,
            executor_id,
            executor: ToolExecutorIdentity::new(executor),
        }
    }
}

impl ToolExecutionEnvironment {
    /// Creates a URI-aware projection bound to a concrete host executor.
    #[allow(clippy::too_many_arguments)]
    pub fn new<T>(
        environment_id: String,
        cwd: PathUri,
        selection: Option<TurnEnvironmentSelection>,
        is_remote: bool,
        executor_id: String,
        file_system: Arc<dyn ExecutorFileSystem>,
        file_system_sandbox_context: FileSystemSandboxContext,
        executor: Arc<T>,
    ) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            environment_id,
            cwd,
            selection,
            is_remote,
            executor_id,
            file_system,
            file_system_sandbox_context,
            executor: ToolExecutorIdentity::new(executor),
        }
    }

    pub fn has_same_executor(&self, other: &Self) -> bool {
        self.executor.ptr_eq(&other.executor)
    }

    /// Verifies the concrete executor instance without exposing its address.
    pub fn has_executor<T>(&self, executor: &Arc<T>) -> bool
    where
        T: Any + Send + Sync,
    {
        self.executor
            .ptr_eq(&ToolExecutorIdentity::new(Arc::clone(executor)))
    }
}

/// Turn-item emitter used when a caller does not expose visible item emission.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTurnItemEmitter;

impl TurnItemEmitter for NoopTurnItemEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }
}

/// Host-visible source for a model tool call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCallSource {
    /// The model invoked the tool directly.
    Direct,
    /// Code mode invoked the tool while executing a runtime cell.
    CodeMode {
        /// Runtime cell that issued the nested tool request.
        cell_id: String,
        /// Code-mode's per-cell tool invocation id.
        runtime_tool_call_id: String,
    },
}

/// Opaque host-projected configuration for launching an extension-owned agent.
///
/// The host stores its concrete configuration type here so extensions can inherit
/// the effective owning step without making `codex-tools` depend on that type.
#[derive(Clone)]
pub struct ToolAgentConfiguration {
    value: Arc<dyn Any + Send + Sync>,
}

impl ToolAgentConfiguration {
    pub fn new<T>(value: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            value: Arc::new(value),
        }
    }

    pub fn get<T>(&self) -> Option<&T>
    where
        T: Any + Send + Sync,
    {
        self.value.downcast_ref()
    }
}

impl std::fmt::Debug for ToolAgentConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolAgentConfiguration")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ToolCall<'call> {
    pub turn_id: String,
    pub call_id: String,
    pub tool_name: ToolName,
    pub model: String,
    pub codex_turn_metadata: Option<String>,
    pub truncation_policy: TruncationPolicy,
    pub source: ToolCallSource,
    pub conversation_history: ConversationHistory,
    pub turn_item_emitter: Arc<dyn TurnItemEmitter>,
    pub environments: Vec<ToolEnvironment<'call>>,
    pub agent_configuration: Option<ToolAgentConfiguration>,
    pub payload: ToolPayload,
}

impl std::fmt::Debug for ToolCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall")
            .field("turn_id", &self.turn_id)
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .field("model", &self.model)
            .field(
                "has_codex_turn_metadata",
                &self.codex_turn_metadata.is_some(),
            )
            .field("truncation_policy", &self.truncation_policy)
            .field("source", &self.source)
            .field("conversation_history", &self.conversation_history)
            .field("turn_item_emitter", &"<host turn item emitter>")
            .field("environment_count", &self.environments.len())
            .field(
                "has_agent_configuration",
                &self.agent_configuration.is_some(),
            )
            .field("payload", &self.payload)
            .finish()
    }
}

impl ToolCall<'_> {
    /// Returns the response-content budget, bounded by the tool's own size limit.
    ///
    /// Direct calls use the host's effective text-output allowance. Code Mode receives
    /// typed results without that truncation, so only the tool's limit applies.
    /// Callers must include serialization overhead when fitting a response to this budget.
    pub fn response_byte_budget(&self, max_response_bytes: usize) -> usize {
        match &self.source {
            ToolCallSource::Direct => {
                max_response_bytes.min((self.truncation_policy * 1.2).byte_budget())
            }
            ToolCallSource::CodeMode {
                cell_id: _,
                runtime_tool_call_id: _,
            } => max_response_bytes,
        }
    }

    /// Returns the host-projected effective agent configuration when its type matches `T`.
    pub fn agent_configuration<T>(&self) -> Option<&T>
    where
        T: Any + Send + Sync,
    {
        self.agent_configuration.as_ref()?.get()
    }

    /// Returns a subscription that can interrupt blocking work when the owning turn changes.
    pub fn turn_activity(&self) -> Option<Arc<dyn TurnActivitySubscription>> {
        self.turn_item_emitter.turn_activity()
    }

    /// Returns URI-aware host environments, falling back to legacy local environments.
    pub fn execution_environments(&self) -> Vec<ToolExecutionEnvironment> {
        let environments = self.turn_item_emitter.execution_environments();
        if !environments.is_empty() {
            return environments;
        }
        self.environments
            .iter()
            .map(|environment| ToolExecutionEnvironment {
                environment_id: environment.environment_id.clone(),
                cwd: PathUri::from_abs_path(&environment.cwd),
                selection: None,
                is_remote: false,
                executor_id: environment.executor_id.clone(),
                file_system: environment.file_system.clone(),
                file_system_sandbox_context: environment.file_system_sandbox_context.clone(),
                executor: environment.executor.clone(),
            })
            .collect()
    }

    pub fn function_arguments(&self) -> Result<&str, FunctionCallError> {
        match &self.payload {
            ToolPayload::Function { arguments } => Ok(arguments),
            _ => Err(FunctionCallError::Fatal(format!(
                "tool {} invoked with incompatible payload",
                self.tool_name
            ))),
        }
    }
}
