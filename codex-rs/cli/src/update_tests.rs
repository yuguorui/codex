use pretty_assertions::assert_eq;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::path;

#[cfg(unix)]
use super::install::activate_release;
use super::install::replace_release_directory;
use super::install::validate_release;
use super::*;

#[test]
fn checksum_for_asset_finds_the_requested_digest() {
    let manifest = "\
1111111111111111111111111111111111111111111111111111111111111111  other-asset.tar.gz\n\
2222222222222222222222222222222222222222222222222222222222222222 *codex-package-test.tar.gz\n\
";
    assert_eq!(
        checksum_for_asset(manifest, "codex-package-test.tar.gz").expect("checksum should parse"),
        "2222222222222222222222222222222222222222222222222222222222222222"
    );
    assert!(checksum_for_asset(manifest, "missing.tar.gz").is_err());
}

#[test]
fn checksum_for_asset_rejects_invalid_digests() {
    assert!(
        checksum_for_asset(
            "not-a-digest codex-package-test.tar.gz",
            "codex-package-test.tar.gz"
        )
        .is_err()
    );
}

#[tokio::test]
async fn download_archive_streams_to_disk_and_hashes_the_body() {
    let server = MockServer::start().await;
    Mock::given(path("codex-package-test.tar.gz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(b"archive bytes".as_slice(), "application/octet-stream"),
        )
        .mount(&server)
        .await;
    let http = RouteAwareClientPool::new_without_request_logging(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Other,
    );
    let directory = tempdir().expect("temp directory should be created");
    let output = directory.path().join("codex-package-test.tar.gz");
    let url = format!("{}/codex-package-test.tar.gz", server.uri());
    let digest = download_archive(&http, &url, &output)
        .await
        .expect("archive should download");

    assert_eq!(digest, sha256_hex(b"archive bytes"));
    assert_eq!(
        std::fs::read(&output).expect("archive should be readable"),
        b"archive bytes"
    );
}

#[test]
fn validate_release_checks_package_metadata_and_layout() {
    let directory = tempdir().expect("temp directory should be created");
    let release_dir = directory.path();
    std::fs::write(
        release_dir.join("codex-package.json"),
        r#"{
            "version": "0.0.0",
            "target": "x86_64-unknown-linux-musl",
            "entrypoint": "bin/codex++"
        }"#,
    )
    .expect("manifest should be written");
    std::fs::create_dir(release_dir.join("bin")).expect("bin directory should be created");
    std::fs::create_dir(release_dir.join("codex-path")).expect("path directory should be created");
    std::fs::write(release_dir.join("bin/codex++"), b"entrypoint").expect("entrypoint");
    std::fs::write(release_dir.join("bin/codex-code-mode-host"), b"host").expect("host");
    std::fs::write(release_dir.join("codex-path/rg"), b"rg").expect("rg");
    if cfg!(target_os = "linux") {
        std::fs::create_dir(release_dir.join("codex-resources")).expect("resources directory");
        std::fs::write(release_dir.join("codex-resources/bwrap"), b"bwrap").expect("bwrap");
    }

    validate_release(release_dir, "x86_64-unknown-linux-musl")
        .expect("package with a placeholder manifest version should validate");
    assert!(
        validate_release(release_dir, "aarch64-apple-darwin").is_err(),
        "target mismatch should be rejected"
    );
}

#[test]
fn replace_release_directory_replaces_an_existing_release() {
    let directory = tempdir().expect("temp directory should be created");
    let release_dir = directory.path().join("202608310535-target");
    let staging_dir = directory.path().join(".staging");
    std::fs::create_dir_all(&release_dir).expect("old release directory should be created");
    std::fs::create_dir_all(&staging_dir).expect("staging directory should be created");
    std::fs::write(release_dir.join("old"), b"old").expect("old release marker");
    std::fs::write(staging_dir.join("new"), b"new").expect("new release marker");

    replace_release_directory(&staging_dir, &release_dir).expect("release should be replaced");

    assert!(!staging_dir.exists());
    assert!(!release_dir.join("old").exists());
    assert_eq!(
        std::fs::read(release_dir.join("new")).expect("new release marker should be readable"),
        b"new"
    );
}

#[cfg(unix)]
#[test]
fn activate_release_updates_current_and_bin_links() {
    let directory = tempdir().expect("temp directory should be created");
    let standalone_root = directory.path().join("standalone");
    let releases_dir = standalone_root.join("releases");
    let bin_dir = directory.path().join("bin");
    let release_dir = releases_dir.join("202608310535-target");
    std::fs::create_dir_all(release_dir.join("bin")).expect("release bin directory");
    std::fs::create_dir_all(&bin_dir).expect("bin directory");
    std::fs::write(release_dir.join("bin/codex++"), b"entrypoint").expect("entrypoint");
    std::fs::write(release_dir.join("bin/codex-code-mode-host"), b"host").expect("host");

    activate_release(
        &InstallLayout {
            standalone_root: standalone_root.clone(),
            releases_dir,
            bin_dir: bin_dir.clone(),
            bin_name: "codex++".to_string(),
        },
        &release_dir,
    )
    .expect("release should be activated");

    assert_eq!(
        std::fs::read_link(standalone_root.join("current")).expect("current link target"),
        release_dir
    );
    assert_eq!(
        std::fs::read_link(bin_dir.join("codex++")).expect("entrypoint link target"),
        release_dir.join("bin/codex++")
    );
    assert_eq!(
        std::fs::read_link(bin_dir.join("codex-code-mode-host")).expect("host link target"),
        release_dir.join("bin/codex-code-mode-host")
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher)
}
