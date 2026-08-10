use codex_tools::ToolEnvironment;
use codex_tools::ToolExecutionEnvironment;

use crate::environment_selection::opaque_executor_id;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::apply_granted_turn_permissions;

pub(super) async fn project_execution_environments(
    invocation: &ToolInvocation,
) -> (Vec<ToolEnvironment<'_>>, Vec<ToolExecutionEnvironment>) {
    let mut environments = Vec::new();
    let mut execution_environments = Vec::new();
    for environment in invocation.step_context.environments.turn_environments() {
        let cwd = environment.cwd().clone();
        let native_cwd = cwd.to_abs_path().ok();
        let additional_permissions = if let Some(native_cwd) = native_cwd.as_ref() {
            apply_granted_turn_permissions(
                invocation.session.as_ref(),
                &environment.selection.environment_id,
                native_cwd.as_path(),
                SandboxPermissions::UseDefault,
                /*additional_permissions*/ None,
            )
            .await
            .additional_permissions
        } else {
            // Granted turn permissions are host-native paths and cannot be projected onto a
            // foreign executor. The executor's captured sandbox context remains authoritative.
            None
        };
        let file_system_sandbox_context = environment.sandbox_context(additional_permissions);
        let environment_id = environment.selection.environment_id.clone();
        let file_system = environment.environment.get_filesystem();
        let executor_id = opaque_executor_id(&environment.environment);
        execution_environments.push(ToolExecutionEnvironment::new(
            environment_id.clone(),
            cwd,
            Some(environment.selection()),
            environment.environment.is_remote(),
            executor_id.clone(),
            file_system.clone(),
            file_system_sandbox_context.clone(),
            environment.environment.clone(),
        ));
        if let Some(native_cwd) = native_cwd {
            environments.push(ToolEnvironment::new(
                environment_id,
                native_cwd,
                file_system,
                file_system_sandbox_context,
                executor_id,
                environment.environment.clone(),
            ));
        }
    }
    (environments, execution_environments)
}
