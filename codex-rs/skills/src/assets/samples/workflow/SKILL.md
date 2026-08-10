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
or `.join(...)`, filter nullable agent results before consuming them, and bound aggregate material
before synthesis. Return the final JSON-compatible value from the script.

For a new script, pass it inline to Workflow. For an iteration, edit the persisted `scriptPath` and
resume the stopped or failed run with `resumeFromRunId` when replay is useful.
