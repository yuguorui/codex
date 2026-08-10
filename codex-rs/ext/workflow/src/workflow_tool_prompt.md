Launch a deterministic JavaScript workflow that coordinates Codex subagents. Use Workflow only
after explicit user or system opt-in to multi-agent orchestration. Before authoring or modifying an
inline script, load the `$workflow` skill when available for the complete script format and runtime
API.

Invoke a saved workflow by `name`, a new workflow by `script`, or an existing script by
`scriptPath`. The call returns immediately with identifiers and paths for the background run.
Lifecycle events arrive automatically; do not sleep or poll transcript files.

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
    agent(`Inspect ${area}`, { label: `inspect:${index}`, phase: "Inspect" })
  )
);
return reports.filter(Boolean);
```

The body runs in an async context and must return a JSON-compatible value. The runtime globals are:

- `args`: the JSON value passed to this tool.
- `agent(prompt, options?)`: runs one subagent; `prompt` must be one non-empty string. Options include
  `label`, `phase`, `schema`, `model`, `effort`, `isolation`, `agentType`, and `stallMs`.
- `pipeline(items, ...stages)`: advances each item independently through every stage.
- `parallel(thunks)`: runs functions concurrently and returns `null` for failed calls.
- `phase(title)` and `log(message)`: report progress.
- `budget`: exposes `total`, `spent()`, and `remaining()`.
- `workflow(nameOrRef, childArgs?)`: runs a saved child workflow.

Scripts have no filesystem, process, network, Node.js module, dynamic import, or string-code access.
Ambient clocks and randomness are unavailable; pass nondeterministic inputs through `args`.
