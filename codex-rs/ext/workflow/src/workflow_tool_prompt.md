Launch a deterministic JavaScript workflow that coordinates Codex subagents after explicit user or
system opt-in to multi-agent orchestration. Load the `$workflow` skill before authoring or revising
a script; it contains the complete API and recovery guidance.

Provide exactly one source:

- `script`: a new inline workflow.
- `name`: a built-in, plugin, user, or project workflow.
- `scriptPath`: an existing local `.js` workflow, including one created with ordinary file tools.

Foreign execution-environment filesystems accept only inline `script`. The call launches in the
background and returns its `runId` and persisted `scriptPath`. Use `WaitWorkflow` when one result is
on the critical path, `WaitWorkflows` for a focused set, and `ReadWorkflowResult` only when a
terminal result is too large to return inline. Status and control remain with the owning agent.

An inline script starts with literal metadata and returns a JSON-compatible value from an async
body:

```javascript
export const meta = {
  name: "inspect",
  description: "Inspect selected areas",
  phases: [{ title: "Inspect" }],
};

return agent("Inspect the request supplied in inputs.", {
  label: "inspect",
  phase: "Inspect",
  inputs: { request: args },
});
```

Core globals:

- `args` is the invocation's JSON value. Pass variable runtime data through named
  `agent(..., { inputs })` values rather than concatenating it into prompts. Agents with structured
  inputs receive `AnalyzeWorkflowInputs` for bounded inspection.
- `agent()` runs a subagent; `agentSettled()` returns an explicit fulfilled or rejected status.
- `parallel()` and `pipeline()` coordinate bounded work. Use `parallel(..., { requireAll: true })`
  only when every result is required.
- `phase()` and `log()` report progress. `workflow()` invokes an approved static local child.
- Optional `meta.inputs` globs freeze bounded workspace UTF-8 text before launch. Use
  `await listInputs()` and `await readInput(path)` only when orchestration logic itself needs that
  text. Workflows without `meta.inputs` do not scan the workspace. Declared workspace inputs are
  distinct from structured agent inputs.

The isolate exposes no process or live filesystem globals. Commands, live reads, and writes belong
in agent tools. Pass time, randomness, and other external values through `args`. The owning agent
retains orchestration controls and user communication; Workflow subagents return results to the
script.
