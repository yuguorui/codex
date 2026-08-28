use codex_agent_extension::AgentRunner;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_features::Feature;
use codex_protocol::ThreadId;
use std::sync::Arc;
use std::sync::Weak;

use crate::analyze_inputs::AnalyzeWorkflowInputsToolExecutor;
use crate::analyze_inputs::WorkflowInputsCapability;
use crate::control_tool::WorkflowControlToolExecutor;
use crate::service::WorkflowService;
use crate::tool::WorkflowToolExecutor;
use crate::wait_tool::WaitWorkflowToolExecutor;
use crate::workflow_result_tool::ReadWorkflowResultToolExecutor;
use crate::workflow_status_tool::ListWorkflowAgentsToolExecutor;
use crate::workflow_status_tool::ListWorkflowsToolExecutor;
use crate::workflow_status_tool::WaitWorkflowsToolExecutor;

#[derive(Clone)]
struct WorkflowThreadConfig {
    enabled: bool,
    allowed_source: bool,
    config: Config,
    analysis_inputs: Option<Arc<WorkflowInputsCapability>>,
}

struct WorkflowExtension {
    service: WorkflowService,
    agent_runner: AgentRunner,
    thread_manager: Weak<ThreadManager>,
}

impl ThreadLifecycleContributor<Config> for WorkflowExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let allowed_source = !input.session_source.is_non_root_agent();
            if input.config.features.enabled(Feature::Workflows)
                && allowed_source
                && let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id())
                && let Err(error) = self
                    .service
                    .restore_thread(thread_id, input.config.clone(), self.agent_runner.clone())
                    .await
            {
                tracing::warn!(%error, "failed to restore workflow snapshots");
            }
            input.thread_store.insert(WorkflowThreadConfig {
                enabled: input.config.features.enabled(Feature::Workflows) && allowed_source,
                allowed_source,
                config: input.config.clone(),
                analysis_inputs: input.thread_store.get::<WorkflowInputsCapability>(),
            });
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(thread_config) = input.thread_store.get::<WorkflowThreadConfig>() else {
                return;
            };
            if !thread_config.enabled {
                return;
            }
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            self.service
                .replay_pending_owning_model_completions(thread_id)
                .await;
        })
    }
}

impl ConfigContributor<Config> for WorkflowExtension {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &Config,
        new_config: &Config,
    ) {
        let allowed_source = thread_store
            .get::<WorkflowThreadConfig>()
            .is_none_or(|config| config.allowed_source);
        let analysis_inputs = thread_store
            .get::<WorkflowThreadConfig>()
            .and_then(|config| config.analysis_inputs.clone())
            .or_else(|| thread_store.get::<WorkflowInputsCapability>());
        thread_store.insert(WorkflowThreadConfig {
            enabled: new_config.features.enabled(Feature::Workflows) && allowed_source,
            allowed_source,
            config: new_config.clone(),
            analysis_inputs,
        });
    }
}

impl ToolContributor for WorkflowExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(thread_config) = thread_store.get::<WorkflowThreadConfig>() else {
            return Vec::new();
        };
        if let Some(analysis_inputs) = thread_config.analysis_inputs.clone() {
            return vec![Arc::new(AnalyzeWorkflowInputsToolExecutor::new(
                analysis_inputs,
            ))];
        }
        if !thread_config.enabled {
            return Vec::new();
        }
        let Ok(thread_id) = ThreadId::from_string(thread_store.level_id()) else {
            return Vec::new();
        };
        vec![
            Arc::new(WorkflowToolExecutor::new(
                thread_id,
                self.service.clone(),
                self.agent_runner.clone(),
                self.thread_manager.clone(),
            )),
            Arc::new(WaitWorkflowToolExecutor::new(
                thread_id,
                thread_config.config.clone(),
                self.service.clone(),
            )),
            Arc::new(WaitWorkflowsToolExecutor::new(
                thread_id,
                thread_config.config.clone(),
                self.service.clone(),
            )),
            Arc::new(ListWorkflowsToolExecutor::new(
                thread_id,
                self.service.clone(),
            )),
            Arc::new(ListWorkflowAgentsToolExecutor::new(
                thread_id,
                self.service.clone(),
            )),
            Arc::new(ReadWorkflowResultToolExecutor::new(
                thread_id,
                self.service.clone(),
            )),
            Arc::new(WorkflowControlToolExecutor::stop(
                thread_id,
                self.service.clone(),
            )),
            Arc::new(WorkflowControlToolExecutor::retry_agent(
                thread_id,
                self.service.clone(),
            )),
            Arc::new(WorkflowControlToolExecutor::skip_agent(
                thread_id,
                self.service.clone(),
            )),
        ]
    }
}

pub fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    thread_manager: Weak<ThreadManager>,
    service: WorkflowService,
) {
    let agent_runner = AgentRunner::new(thread_manager.clone());
    let extension = Arc::new(WorkflowExtension {
        service,
        agent_runner,
        thread_manager,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.tool_contributor(extension);
}

#[cfg(test)]
#[path = "extension_tests.rs"]
mod tests;
