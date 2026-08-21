use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::executable_identity_from_bytes;
use super::managed_codex_bin;
use super::parse_codex_version;

#[test]
fn managed_codex_bin_uses_the_codex_plus_package_entrypoint() {
    let codex_home = TempDir::new().expect("temp codex home");
    let package_bin = codex_home
        .path()
        .join("packages/standalone/current/bin/codex++");
    std::fs::create_dir_all(package_bin.parent().expect("package bin parent"))
        .expect("create package bin directory");
    std::fs::write(&package_bin, b"codex++").expect("write package entrypoint");

    let managed_bin = managed_codex_bin(codex_home.path());

    assert_eq!(managed_bin, package_bin);
    assert!(managed_bin.is_file());
}

#[test]
fn parses_codex_cli_version_output() {
    assert_eq!(
        parse_codex_version("codex 1.2.3\n").expect("version"),
        "1.2.3"
    );
}

#[test]
fn rejects_malformed_codex_cli_version_output() {
    assert!(parse_codex_version("codex\n").is_err());
}

#[test]
fn executable_identity_uses_binary_contents() {
    let old = executable_identity_from_bytes(b"old");
    let same = executable_identity_from_bytes(b"old");
    let new = executable_identity_from_bytes(b"new");

    assert_eq!(old, same);
    assert_ne!(old, new);
}
