/// The current Codex CLI version as embedded at compile time.
///
/// Mirrors `codex_cli::CODEX_CLI_DISPLAY_VERSION`: when the
/// `CODEX_FORK_RELEASE_VERSION` environment variable is set at build time
/// (for example by the fork-release workflow), that value is used; otherwise
/// it falls back to the crate's `CARGO_PKG_VERSION`.
pub const CODEX_CLI_VERSION: &str = env!("CODEX_CLI_DISPLAY_VERSION");
