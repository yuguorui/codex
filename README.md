<p align="center"><strong>Codex++</strong> is a fork of <a href="https://github.com/openai/codex">openai/codex</a> that adds third-party model provider support (Chat Completions, Anthropic, custom providers) alongside the original OpenAI Responses API.</p>

<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>

<p align="center">Codex++ runs locally on your computer as the <code>codex++</code> command. It builds as a standalone executable with zero dependencies.</p>

---

## What is Codex++?

Codex++ extends the official [Codex CLI](https://github.com/openai/codex) with:

- **Chat Completions API** (`wire_api = "chat"`) — connect to any OpenAI-compatible provider: Ollama, vLLM, LiteLLM, DeepSeek, Mistral, Groq, Together, DashScope, and more.
- **Anthropic Messages API** (`wire_api = "anthropic"`) — use Claude models directly through Anthropic's native API.
- **Auth header scheme control** (`env_key_auth`) — choose between `Bearer` and `x-api-key` auth per provider.
- **Provider-specific request fields** (`extra_body`) — merge arbitrary JSON into request bodies for provider-specific features like `enable_thinking` or `thinking_budget`.
- **429 rate-limit retry** — automatic exponential backoff for rate-limited responses.

For a detailed technical breakdown, see [`codex-rs/README.md`](./codex-rs/README.md).

## Quickstart

### One-command install (recommended)

```shell
curl -fsSL https://github.com/yuguorui/codex/releases/latest/download/install-fork.sh | sh
```

The install script resolves the latest release, verifies SHA-256 checksums, stores the standalone package under `~/.codex/packages/standalone`, and installs the `codex++` command into `~/.local/bin`.

Environment variables:
- `CODEX_INSTALL_DIR` — change the install directory
- `CODEX_BIN_NAME` — override the command name
- `CODEX_RELEASE_REPOSITORY` — override the release repository

### Build from source

```shell
cd codex-rs
cargo build --release --bin codex
# Binary appears at target/release/codex
# Copy or symlink it as codex++ to match the fork release command name
```

The standalone installers download from `https://releases.openai.com/codex` by default and fall back to GitHub Releases if a metadata or asset download is unavailable. To force GitHub Releases, set `CODEX_INSTALLER_USE_RELEASES_OPENAI_COM` to `false` (`0` and `no` are also accepted):

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_INSTALLER_USE_RELEASES_OPENAI_COM=false sh
```

```powershell
$env:CODEX_INSTALLER_USE_RELEASES_OPENAI_COM='false'; irm https://chatgpt.com/codex/install.ps1 | iex
```

Codex CLI can also be installed via the following package managers:

### Quick usage

```shell
# Use with a Chat Completions provider
codex++ -m ollama/qwen3
codex++ -m deepseek/deepseek-chat
codex++ -m dashscope/qwen-plus

# Use with Anthropic
codex++ -m anthropic/claude-sonnet-4-20250514

# Use with reasoning effort control
codex++ -m deepseek/deepseek-chat --reasoning-effort low
codex++ -m anthropic/claude-sonnet-4-20250514 --reasoning-effort high
```

## Configuration

Codex++ uses `config.toml` (same location as upstream: `~/.codex/config.toml`). Add provider blocks to configure third-party models:

```toml
# Chat Completions provider (Ollama, vLLM, DeepSeek, etc.)
[model_provider.ollama]
name = "Ollama"
base_url = "http://localhost:11434/v1"
env_key = "OLLAMA_API_KEY"
wire_api = "chat"

# Anthropic (Claude)
[model_provider.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com"
env_key = "ANTHROPIC_API_KEY"
env_key_auth = "x-api-key"
wire_api = "anthropic"

# DashScope (Qwen) with thinking enabled
[model_provider.dashscope]
name = "DashScope"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
env_key = "DASHSCOPE_API_KEY"
wire_api = "chat"
extra_body = { "enable_thinking" = true }
```

See [`codex-rs/README.md`](./codex-rs/README.md) for full configuration reference including `extra_body`, `extra_headers`, `env_key_auth`, `request_max_retry_delay_ms`, and reasoning effort mapping.

## Upstream Features

All features from [openai/codex](https://github.com/openai/codex) are inherited unchanged. See the [upstream documentation](https://developers.openai.com/codex) for details on config, MCP, notifications, sandbox, exec, and more.

## Docs

- [**Codex++ Technical Reference**](./codex-rs/README.md) — detailed wire API docs, configuration, and code organization
- [**Upstream Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

## License

This repository is licensed under the [Apache-2.0 License](LICENSE).
