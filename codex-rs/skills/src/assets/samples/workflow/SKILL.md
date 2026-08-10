---
name: workflow
description: Author, revise, or diagnose deterministic JavaScript scripts for Workflow multi-agent orchestration. Use before composing an inline Workflow script or editing or resuming an existing script.
---

# Workflow Authoring

Read [references/api.md](references/api.md) before writing or modifying a workflow script.

Start by identifying a bounded work list in the owning agent. Use `pipeline()` when each item can
advance independently through several stages, and `parallel()` when the next step needs all prior
results. Keep deterministic grouping, filtering, ranking, and deduplication in JavaScript.

Every `agent()` prompt is one non-empty string. Assemble prompt fragments with a template literal
for task instructions. Pass variable data through named `inputs`, especially complete
upstream agent results. Use one synthesis agent with the complete input set, and return the final
JSON-compatible value from the script.
Use `agentSettled()` when script logic needs an explicit status and failure kind for branching;
use `agent()` for its `null` and exception behavior.

Use default fail-soft `parallel()` for independent exploration where missing slots are acceptable.
Use `parallel(thunks, { requireAll: true })` for critical synthesis fan-in, or explicitly verify the
result count before continuing, so final synthesis receives every required upstream result.

For a new script, pass it inline to Workflow. For an iteration, edit the persisted `scriptPath` and
resume the stopped or failed run with `resumeFromRunId` when replay is useful. Provide exactly one
of `script`, `name`, or `scriptPath` in each invocation; remote calls use inline `script`.
When the owning turn needs the final result before it can continue, call `WaitWorkflow` with the
launch result's `runId`. Use `WaitWorkflows` for bounded `any` or `all` waits across several runs.
Both waits are repeatable and preserve steered input when they return `interruptedByUserInput: true`.
Use `ListWorkflowAgents` to find a completed Workflow agent's `agentId`, then call `wait_agent`
when its intermediate detail is useful. Use `ReadWorkflowResult` for the terminal Workflow result;
choose `maxBytes` when useful and continue from `nextOffset` only while the result is incomplete.

Use `agent(prompt, { inputs: { reports } })` for synthesis. The downstream agent receives
`AnalyzeWorkflowInputs`, which runs synchronous pure JavaScript against deep-frozen
`globalThis.inputs` in a fresh V8 isolate. Tell the agent to inspect every required input and use
multiple calls when needed, returning focused views for reasoning while the complete inputs remain
available.
