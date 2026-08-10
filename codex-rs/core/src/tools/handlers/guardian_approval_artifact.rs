use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;

pub(crate) const READ_GUARDIAN_APPROVAL_ARTIFACT_TOOL_NAME: &str =
    "read_guardian_approval_artifact";

pub(crate) struct ReadGuardianApprovalArtifactHandler {
    artifact: crate::guardian::GuardianApprovalArtifact,
}

impl ReadGuardianApprovalArtifactHandler {
    pub(crate) fn new(artifact: crate::guardian::GuardianApprovalArtifact) -> Self {
        Self { artifact }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadArtifactArgs {
    sha256: String,
    #[serde(default)]
    offset: usize,
}

impl ToolExecutor<ToolInvocation> for ReadGuardianApprovalArtifactHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_GUARDIAN_APPROVAL_ARTIFACT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: READ_GUARDIAN_APPROVAL_ARTIFACT_TOOL_NAME.to_string(),
            description: "Reads the complete extension action currently under automatic approval review. Continue from nextOffset until it is absent, and assess the content whose SHA-256 matches the approval descriptor.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "sha256".to_string(),
                        JsonSchema::string(Some(
                            "The artifact SHA-256 from the approval descriptor.".to_string(),
                        )),
                    ),
                    (
                        "offset".to_string(),
                        JsonSchema::integer(Some(
                            "The nextOffset returned by the preceding read; omit for the first page."
                                .to_string(),
                        )),
                    ),
                ]),
                Some(vec!["sha256".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "approval artifact reader received unsupported input".to_string(),
                ));
            };
            let args: ReadArtifactArgs = parse_arguments(&arguments)?;
            let page = self
                .artifact
                .read_page(&args.sha256, args.offset)
                .map_err(FunctionCallError::RespondToModel)?;
            Ok(Box::new(JsonToolOutput::new(serde_json::json!({
                "sha256": page.sha256,
                "offset": page.offset,
                "contents": page.contents,
                "nextOffset": page.next_offset,
            }))) as Box<dyn codex_tools::ToolOutput>)
        })
    }
}

impl CoreToolRuntime for ReadGuardianApprovalArtifactHandler {}
