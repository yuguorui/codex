use std::env;

pub(super) const DEFAULT_MAX_READ_OUTPUT_TOKENS: usize = 25_000;
const FILE_READ_MAX_OUTPUT_TOKENS_ENV: &str = "CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS";

pub(super) fn file_read_output_token_limit() -> usize {
    env::var(FILE_READ_MAX_OUTPUT_TOKENS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_READ_OUTPUT_TOKENS)
}
