use codex_agent_extension::AgentRunner;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::Weak;

use crate::discovery::active_plugin_workflow_roots;
use crate::service::WorkflowService;
use crate::tool::WorkflowToolExecutor;

#[derive(Clone)]
struct WorkflowThreadConfig {
    enabled: bool,
    allowed_source: bool,
    config: Config,
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
            let allowed_source = !matches!(input.session_source, SessionSource::SubAgent(_));
            if input.config.features.enabled(Feature::Workflows)
                && allowed_source
                && let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id())
            {
                let plugin_roots =
                    active_plugin_workflow_roots(&self.thread_manager, input.config).await;
                if let Err(error) = self
                    .service
                    .restore_thread(
                        thread_id,
                        input.config.clone(),
                        self.agent_runner.clone(),
                        plugin_roots,
                    )
                    .await
                {
                    tracing::warn!(%error, "failed to restore workflow snapshots");
                }
            }
            input.thread_store.insert(WorkflowThreadConfig {
                enabled: input.config.features.enabled(Feature::Workflows) && allowed_source,
                allowed_source,
                config: input.config.clone(),
            });
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
        thread_store.insert(WorkflowThreadConfig {
            enabled: new_config.features.enabled(Feature::Workflows) && allowed_source,
            allowed_source,
            config: new_config.clone(),
        });
    }
}

impl ToolContributor for WorkflowExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        let Some(thread_config) = thread_store.get::<WorkflowThreadConfig>() else {
            return Vec::new();
        };
        if !thread_config.enabled {
            return Vec::new();
        }
        let Ok(thread_id) = ThreadId::from_string(thread_store.level_id()) else {
            return Vec::new();
        };
        vec![
            Arc::new(WorkflowToolExecutor::new(
                thread_id,
                thread_config.config.clone(),
                self.service.clone(),
                self.agent_runner.clone(),
                self.thread_manager.clone(),
            )),
            Arc::new(WorkflowToolExecutor::compatibility_alias(
                thread_id,
                thread_config.config.clone(),
                self.service.clone(),
                self.agent_runner.clone(),
                self.thread_manager.clone(),
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
