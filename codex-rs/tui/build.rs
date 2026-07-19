fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_FORK_RELEASE_VERSION");

    let display_version = std::env::var("CODEX_FORK_RELEASE_VERSION")
        .ok()
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=CODEX_CLI_DISPLAY_VERSION={display_version}");
}
