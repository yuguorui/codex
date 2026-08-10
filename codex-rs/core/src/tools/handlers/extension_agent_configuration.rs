use codex_config::Constrained;
use codex_tools::ToolAgentConfiguration;

use crate::tools::context::ToolInvocation;

pub(super) async fn project_agent_configuration(
    invocation: &ToolInvocation,
) -> ToolAgentConfiguration {
    let step = invocation.step_context.as_ref();
    let mut config = (*step.turn.config).clone();
    let base_instructions = invocation.session.get_base_instructions().await;

    config.model = Some(step.model_info.slug.clone());
    config.model_reasoning_effort = step.reasoning_effort.clone();
    config.model_reasoning_summary = Some(step.reasoning_summary);
    config.service_tier.clone_from(&step.service_tier);
    config.permissions.approval_policy = Constrained::allow_only(step.approval_policy);
    config.approvals_reviewer = step.approvals_reviewer;
    config.base_instructions = Some(base_instructions.text);
    config.base_instructions_provenance = base_instructions.provenance;
    config
        .developer_instructions
        .clone_from(&step.turn.developer_instructions);
    config.personality = step.turn.personality;
    config.model_provider = step.turn.provider.info().clone();

    ToolAgentConfiguration::new(config)
}
