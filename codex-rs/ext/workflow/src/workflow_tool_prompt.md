Launch a deterministic JavaScript workflow that coordinates Codex subagents after explicit user or
system opt-in to multi-agent orchestration. Before authoring or modifying an
inline script, load the `$workflow` skill when available for the complete script format and runtime
API.

Invoke a saved workflow by `name`, a new workflow by `script`, or an existing script by
`scriptPath`; select one source field. Foreign execution-environment filesystems use an inline
`script`. The call returns immediately with identifiers and paths for the background run. When
the next critical-path step depends on one result, call `WaitWorkflow`; for several runs, call
`WaitWorkflows` with `mode: "any"` or `mode: "all"`. Repeated waits are safe. If a wait returns
`interruptedByUserInput: true`, handle the preserved user input before deciding whether to wait
again. Lifecycle events arrive automatically; use those events and the wait tools for status.

Use `ListWorkflows` for focused status discovery in the current owning thread and
`ListWorkflowAgents` to page agent states by stable index. When intermediate detail is useful,
inspect a completed entry's `agentId` and call `wait_agent` with that id; repeated waits remain
readable. Terminal Workflow results are returned inline when small; otherwise use
`ReadWorkflowResult`, optionally choosing `maxBytes`, and continue from `nextOffset` only while
`complete` is false. Use `StopWorkflow` to stop an active run and `RetryWorkflowAgent` or
`SkipWorkflowAgent` for an agent awaiting a decision. The owning model retains these orchestration
tools; workflow agents return their result to the script.

An inline script must start with a pure literal metadata declaration:

```javascript
export const meta = {
  name: "inspect",
  description: "Inspect several areas",
  phases: [{ title: "Inspect" }],
};

phase("Inspect");
const reports = await parallel(
  args.areas.map((area, index) => () =>
    agent("Inspect the area supplied in inputs and return a detailed report.", {
      label: `inspect:${index}`,
      phase: "Inspect",
      inputs: { area },
    })
  ),
  { requireAll: true },
);
return agent(
  "Synthesize every report into one result. Use AnalyzeWorkflowInputs to inspect and aggregate the complete reports before writing the answer.",
  { label: "synthesize", phase: "Synthesize", inputs: { reports } },
);
```

The body runs in an async context and must return a JSON-compatible value. The runtime globals are:

- `args`: the JSON value passed to this tool.
- `agent(prompt, options?)`: runs one subagent; `prompt` must be one non-empty string. Options include
  `label`, `phase`, `schema`, `model`, `effort`, `isolation`, `agentType`, `stallMs`, and `inputs`.
  Named `inputs` stay structured outside the prompt. An agent with inputs
  receives the host-side `AnalyzeWorkflowInputs` tool for synchronous JavaScript
  filtering and aggregation over a deep-frozen `globalThis.inputs` object.
- `pipeline(items, ...stages)`: advances each item independently through every stage.
- `parallel(thunks, options?)`: runs functions concurrently. The default exploration-oriented mode
  returns `null` for failed calls. Use `{ requireAll: true }` for critical fan-in so any failed
  thunk completes the barrier with a focused failure summary before synthesis.
- `phase(title)` and `log(message)`: report progress.
- `workflow(nameOrRef, childArgs?)`: runs an approved local child Workflow. Use a literal `"name"`,
  `{ name: "name" }`, or `{ scriptPath: "path.js" }` as the first argument; child sources are
  resolved and frozen into the top-level approval. A composition can reference multiple static
  child Workflows and invoke each approved child wherever its result is needed.

Scripts run in a deterministic, self-contained JavaScript environment with the Workflow globals.
Pass time, randomness, and other external values through `args`.
Keep prompts to task instructions. Pass all runtime data, especially complete upstream agent
results, through named `inputs`. The downstream agent can call `AnalyzeWorkflowInputs` repeatedly
to inspect, filter, rank, and aggregate the complete deep-frozen inputs inside fresh V8 isolates.
Use one synthesis agent over the complete input set. Runtime validation checks prompts before model
submission. Automatic review handles complete actions directly and routes larger actions according
to the configured review policy.
