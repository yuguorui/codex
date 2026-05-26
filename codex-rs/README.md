# Codex++ (Rust Implementation)

**Codex++** is a fork of [openai/codex](https://github.com/openai/codex) that adds support for third-party model providers (Chat Completions, Anthropic, custom providers) alongside the original OpenAI Responses API. It builds as a standalone executable with a zero-dependency install and is installed as the `codex++` command.

## Installing

Codex++ is distributed independently from the upstream Codex CLI. It cannot be installed via `npm` (`@openai/codex`) or Homebrew (`brew install --cask codex`) - those install the official upstream version without Chat/Anthropic wire support. Use one of the methods below instead.

### One-command install (recommended)

Codex++ publishes its own pre-built binaries via a dedicated GitHub Actions release workflow. To install:

```shell
curl -fsSL https://github.com/yuguorui/codex/releases/latest/download/install-fork.sh | sh
```

The install script resolves the latest release, verifies SHA-256 checksums, stores the standalone package under `~/.codex/packages/standalone`, and installs the `codex++` command into `~/.local/bin`. Set `CODEX_INSTALL_DIR` to change the install directory, `CODEX_BIN_NAME` to override the command name, and `CODEX_RELEASE_REPOSITORY` to override the release repository.

You can also build from source:

```shell
cd codex-rs
cargo build --release --bin codex
# The binary appears at target/release/codex.
# Copy or symlink it as codex++ if you want to match the fork release command name.
```

## What's New in Codex++

### Chat Completions API (`wire_api = "chat"`)

Adds a full Chat Completions API wire protocol (`/v1/chat/completions` with SSE streaming) as an alternative to the Responses API. This enables Codex++ to work with any OpenAI-compatible provider that exposes the Chat Completions endpoint, including local inference servers like Ollama, vLLM, and LiteLLM, as well as cloud providers such as DeepSeek, Mistral, Groq, and Together.

**Configuration example (Ollama):**

```toml
[model_provider.ollama]
name = "Ollama"
base_url = "http://localhost:11434/v1"
env_key = "OLLAMA_API_KEY"
wire_api = "chat"
```

Key implementation details:

- Converts Codex's internal Responses-format prompts into Chat Completions format (messages, tool_calls, tool responses, reasoning).
- Coalesces consecutive function/tool calls into a single assistant message (required by the Chat API spec).
- Namespace tools (`mcp__calendar__lookup_order`) are encoded with reversible compound names so they remain callable and traceable.
- Custom tools (e.g. `apply_patch`) are wrapped as function tools with a `{ "input": "..." }` parameter schema.
- Supports `stream_options.include_usage` for token-usage reporting.
- Merges `extra_body` fields into the request body for provider-specific options (e.g. `enable_thinking`, `thinking_budget`).

### Anthropic Messages API (`wire_api = "anthropic"`)

Adds a full Anthropic Messages API wire protocol (`/v1/messages` with SSE streaming). This enables Codex++ to work directly with Claude models through Anthropic's native API.

**Configuration example (Anthropic direct):**

```toml
[model_provider.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com"
env_key = "ANTHROPIC_API_KEY"
env_key_auth = "x-api-key"
wire_api = "anthropic"
```

Key implementation details:

- Sends prompts to `/v1/messages` with `anthropic-version: 2023-06-01` header and SSE streaming.
- Maps Codex reasoning effort to Anthropic thinking modes:
  - No effort specified: `Adaptive` (broadest model compatibility, supported by Claude 4+).
  - `none`: No thinking block.
  - `low`/`minimal`: `Enabled` with `budget_tokens = 1024`.
  - `medium`: `Enabled` with `budget_tokens = 2048`.
  - `high`/`xhigh`: `Enabled` with `budget_tokens = 3072`.
- Handles `RedactedThinking` blocks for context replay with encrypted thinking content.
- Converts system/developer messages to Anthropic's `system` field.
- Supports both `x-api-key` and `Bearer` auth schemes via `env_key_auth`.

### `env_key_auth` - Auth Header Scheme

New config field that controls how the API key from `env_key` is sent:

- `"bearer"` - sends `Authorization: Bearer <key>` (default for most providers).
- `"x-api-key"` - sends `x-api-key: <key>` (default for Anthropic `wire_api` if `env_key_auth` is unset).

This is especially useful for Anthropic, which requires the `x-api-key` header rather than `Authorization: Bearer`.

### `extra_body` - Provider-Specific Request Body Fields

New config field that lets you merge arbitrary JSON fields into the request body sent to the provider. This is useful for toggling provider-specific features that Codex doesn't expose directly.

Examples:

```toml
# DashScope / Qwen: enable thinking with a budget
[model_provider.dashscope]
extra_body = { "enable_thinking" = true, "thinking_budget" = 1024 }

# Anthropic: override thinking mode to adaptive
[model_provider.anthropic]
extra_body = { "thinking" = { "type" = "adaptive" } }
```

`extra_body` is deep-merged into the request body, so nested objects merge recursively. Provider-level `extra_body` is applied first, then per-request `extra_body` (from model info) is merged on top.

### `extra_headers` / `env_extra_headers` - Alias Header Fields

New aliases `extra_headers` and `env_extra_headers` are accepted alongside the existing `http_headers` and `env_http_headers`. These aliases match the naming convention used by other OpenAI-compatible tools, making config more portable.

### 429 Rate-Limit Retry

Custom model providers now automatically retry on HTTP 429 (rate-limit) responses, with exponential backoff capped by `request_max_retry_delay_ms` (default 10 s, max 300 s). Previously, only 5xx and transport errors were retried.

New config field:

```toml
[model_provider.my_provider]
request_max_retry_delay_ms = 15_000   # cap backoff at 15 seconds
```

### Fork Release Workflow

A dedicated GitHub Actions workflow (`fork-release.yml`) builds and publishes pre-built binaries for macOS (Apple Silicon) and Linux (x86_64/musl) on every manual trigger. Releases are tagged `rust-v<yyyymmddhhmm>` and include SHA-256 checksums.

## Connecting to Chat and Anthropic Providers

### Chat Completions providers (Ollama, vLLM, LiteLLM, DeepSeek, etc.)

Any server that implements the OpenAI Chat Completions API (`/v1/chat/completions`) can be used with Codex++ by setting `wire_api = "chat"`:

```toml
# Example: local Ollama
[model_provider.ollama]
name = "Ollama"
base_url = "http://localhost:11434/v1"
wire_api = "chat"

# Example: DeepSeek API
[model_provider.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
env_key = "DEEPSEEK_API_KEY"
wire_api = "chat"

# Example: DashScope (Qwen) with thinking enabled
[model_provider.dashscope]
name = "DashScope"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
env_key = "DASHSCOPE_API_KEY"
wire_api = "chat"
extra_body = { "enable_thinking" = true }
```

Then run:

```shell
codex++ -m ollama/qwen3
codex++ -m deepseek/deepseek-chat
codex++ -m dashscope/qwen-plus
```

### Anthropic (Claude) providers

Use `wire_api = "anthropic"` with `env_key_auth = "x-api-key"` (or leave it unset; Anthropic wire_api defaults to `x-api-key`):

```toml
# Anthropic direct
[model_provider.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com"
env_key = "ANTHROPIC_API_KEY"
env_key_auth = "x-api-key"
wire_api = "anthropic"

# Anthropic via a proxy/gateway that expects Bearer auth
[model_provider.anthropic_proxy]
name = "Anthropic Proxy"
base_url = "https://my-gateway.example.com"
env_key = "PROXY_API_KEY"
env_key_auth = "bearer"
wire_api = "anthropic"

# Override thinking mode via extra_body
[model_provider.anthropic]
extra_body = { "thinking" = { "type" = "enabled", "budget_tokens" = 16000 } }
```

Then run:

```shell
codex++ -m anthropic/claude-sonnet-4-20250514
```

### Using reasoning / thinking effort

Both Chat and Anthropic wire APIs respect the `--reasoning-effort` flag:

```shell
# Chat provider with low reasoning
codex++ -m deepseek/deepseek-chat --reasoning-effort low

# Anthropic with high reasoning (budget_tokens = 3072)
codex++ -m anthropic/claude-sonnet-4-20250514 --reasoning-effort high

# Anthropic with no reasoning (no thinking block sent)
codex++ -m anthropic/claude-sonnet-4-20250514 --reasoning-effort none
```

For Anthropic, if no reasoning effort is specified, `Adaptive` thinking mode is used by default (the model decides when to think). You can override this with `extra_body` as shown above.

## Upstream Features

The following features are inherited from the upstream openai/codex repository and remain unchanged:

### Config

Codex supports a rich set of configuration options. Note that the Rust CLI uses `config.toml` instead of `config.json`. See [`docs/config.md`](../docs/config.md) for details.

### Model Context Protocol Support

- **MCP client** - Codex++ connects to MCP servers on startup. See [`docs/config.md`](../docs/config.md#connecting-to-mcp-servers).
- **MCP server (experimental)** - Run `codex++ mcp-server` to let other MCP clients use Codex++ as a tool.

### Notifications

You can enable notifications by configuring a script that is run whenever the agent finishes a turn. See [`docs/config.md`](../docs/config.md#notify).

### `codex++ exec` to run programmatically/non-interactively

To run Codex++ non-interactively, run `codex++ exec PROMPT` (you can also pass the prompt via `stdin`). Use `codex++ exec --ephemeral ...` to run without persisting session rollout files to disk.

### Codex Sandbox

Use `codex++ sandbox [COMMAND]...` to test commands under the sandbox. The `--sandbox` flag (`-s`) lets you pick the sandbox policy:

```shell
codex++ --sandbox read-only        # default
codex++ --sandbox workspace-write  # allow writes in workspace
codex++ --sandbox danger-full-access  # no sandbox
```

## Code Organization

This folder is the root of a Cargo workspace. The key crates are:

- [`core/`](./core) - business logic for Codex sessions, prompt building, and client orchestration.
- [`exec/`](./exec) - headless CLI for automation (`codex++ exec`).
- [`tui/`](./tui) - fullscreen TUI built with [Ratatui](https://ratatui.rs/).
- [`cli/`](./cli) - CLI multitool that provides the aforementioned CLIs via subcommands.
- [`codex-api/`](./codex-api) - API client with endpoint implementations for Responses, Chat Completions, and Anthropic Messages.
- [`model-provider-info/`](./model-provider-info) - provider configuration types (`WireApi`, `EnvKeyAuthScheme`, `extra_body`, etc.).
- [`model-provider/`](./model-provider) - provider resolution, auth dispatch, and API retry configuration.

If you want to contribute or inspect behavior in detail, start by reading the module-level `README.md` files under each crate and run the project workspace from the top-level `codex-rs` directory so shared config, features, and build scripts stay aligned.
