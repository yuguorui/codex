use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const WORKFLOW_TOOL_NAME: &str = "Workflow";
pub const RUN_WORKFLOW_TOOL_ALIAS: &str = "RunWorkflow";

pub(crate) fn workflow_tool_spec(name: &str) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "script".to_string(),
            JsonSchema::string(Some(
                "Inline JavaScript workflow, at most 512 KiB. It must begin with `export const meta = {...}`."
                    .to_string(),
            )),
        ),
        (
            "name".to_string(),
            JsonSchema::string(Some(
                "Built-in workflow, active plugin workflow (`pluginName:workflowName`), or saved workflow discovered from user/project .codex/workflows and compatibility .claude/workflows directories."
                    .to_string(),
            )),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some(
                "Ignored compatibility field. Put description in exported meta.".to_string(),
            )),
        ),
        (
            "title".to_string(),
            JsonSchema::string(Some(
                "Ignored compatibility field. Put title in exported meta.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema {
                description: Some(
                    "JSON value exposed verbatim as the workflow global `args`; do not JSON-encode it into a string."
                        .to_string(),
                ),
                ..Default::default()
            },
        ),
        (
            "scriptPath".to_string(),
            JsonSchema::string(Some(
                "Workflow script path. Takes precedence over script and name.".to_string(),
            )),
        ),
        (
            "resumeFromRunId".to_string(),
            JsonSchema::string(Some(
                "Stopped or failed workflow run id matching ^wf_[a-z0-9-]{6,}$. The prior run must belong to this thread."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: include_str!("workflow_tool_prompt.md").to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(Vec::new()),
            Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "status": { "enum": ["async_launched", "remote_launched"] },
                "taskId": { "type": "string" },
                "taskType": { "enum": ["local_workflow", "remote_agent"] },
                "workflowName": { "type": "string" },
                "runId": { "type": "string" },
                "summary": { "type": "string" },
                "transcriptDir": { "type": "string" },
                "scriptPath": { "type": "string" },
                "sessionUrl": { "type": ["string", "null"] },
                "warning": { "type": ["string", "null"] },
                "error": { "type": ["string", "null"] }
            },
            "required": [
                "status",
                "taskId",
                "taskType",
                "workflowName",
                "runId",
                "summary",
                "transcriptDir",
                "scriptPath"
            ],
            "additionalProperties": false
        })),
    })
}
