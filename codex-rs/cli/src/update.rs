use std::fs::File;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::RouteAwareClientPool;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_install_context::StandalonePlatform;
use codex_tui::UpdateAction;
use http::header::USER_AGENT;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use tempfile::TempDir;

mod install;

const RELEASE_REPOSITORY: &str = "yuguorui/codex";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/yuguorui/codex/releases/latest";
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_CHECKSUM_MANIFEST_BYTES: u64 = 64 * 1024;
const PROGRESS_WIDTH: usize = 32;

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

#[derive(Debug)]
struct InstallLayout {
    standalone_root: PathBuf,
    releases_dir: PathBuf,
    bin_dir: PathBuf,
    bin_name: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum UpdateOutcome {
    AlreadyUpToDate,
    Installed,
}

pub(crate) async fn run(action: UpdateAction) -> Result<UpdateOutcome> {
    let expected_platform = if cfg!(windows) {
        StandalonePlatform::Windows
    } else {
        StandalonePlatform::Unix
    };
    let InstallMethod::Standalone { platform, .. } = &InstallContext::current().method else {
        bail!("Codex++ standalone installation could not be detected");
    };
    if *platform != expected_platform {
        bail!("Codex++ update platform does not match the current installation");
    }
    match (action, cfg!(windows)) {
        (UpdateAction::StandaloneWindows, true) | (UpdateAction::StandaloneUnix, false) => {}
        _ => bail!("Codex++ update action does not match the current platform"),
    }

    let Some(target) = standalone_target() else {
        bail!("this Codex++ build target does not have a standalone release");
    };
    let layout = install_layout()?;
    let http = RouteAwareClientPool::new_without_request_logging(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Other,
    );
    let version = latest_release_version(&http).await?;
    let release_dir = layout.releases_dir.join(format!("{version}-{target}"));
    let asset = format!("codex-package-{target}.tar.gz");

    if codex_cli::CODEX_CLI_DISPLAY_VERSION == version
        && install::release_is_complete(&release_dir, target)?
    {
        install::activate_release(&layout, &release_dir)?;
        println!("Codex++ {version} is already up to date.");
        return Ok(UpdateOutcome::AlreadyUpToDate);
    }

    println!("Downloading Codex++ {version} for {target}...");
    let checksums_url = format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/rust-v{version}/codex-package_SHA256SUMS"
    );
    let archive_url = format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/rust-v{version}/{asset}"
    );
    let checksum_manifest = fetch_checksum_manifest(&http, &checksums_url).await?;
    let expected_checksum = checksum_for_asset(&checksum_manifest, &asset)?;

    let download_dir = TempDir::new_in(&layout.releases_dir).with_context(|| {
        format!(
            "failed to create download directory under {}",
            layout.releases_dir.display()
        )
    })?;
    let archive_path = download_dir.path().join(&asset);
    let actual_checksum = download_archive(&http, &archive_url, &archive_path).await?;
    if !checksums_equal(&actual_checksum, &expected_checksum) {
        bail!("checksum mismatch for {asset}: expected {expected_checksum}, got {actual_checksum}");
    }

    let staging_dir = layout
        .releases_dir
        .join(format!(".staging.{version}-{target}.{}", process::id()));
    if staging_dir.exists() {
        std::fs::remove_dir_all(&staging_dir).with_context(|| {
            format!(
                "failed to remove stale staging directory {}",
                staging_dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&staging_dir).with_context(|| {
        format!(
            "failed to create staging directory {}",
            staging_dir.display()
        )
    })?;
    install::extract_archive(&archive_path, &staging_dir)?;
    install::validate_release(&staging_dir, target)?;

    install::replace_release_directory(&staging_dir, &release_dir)?;
    install::activate_release(&layout, &release_dir)?;
    println!(
        "Installed Codex++ {version} for {target} at {}",
        release_dir.display()
    );
    println!("Ensure {} is on your PATH.", layout.bin_dir.display());
    println!("Restart any running Codex++ processes to use this version.");
    Ok(UpdateOutcome::Installed)
}

fn standalone_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

fn install_layout() -> Result<InstallLayout> {
    let InstallMethod::Standalone { release_dir, .. } = &InstallContext::current().method else {
        bail!("Codex++ standalone release directory could not be detected");
    };
    let Some(releases_dir) = release_dir.parent() else {
        bail!("Codex++ standalone release directory has no parent");
    };
    let Some(standalone_root) = releases_dir.parent() else {
        bail!("Codex++ standalone releases directory has no parent");
    };
    let bin_dir = std::env::var_os("CODEX_INSTALL_DIR").map_or_else(home_local_bin, PathBuf::from);
    let bin_name = std::env::var("CODEX_BIN_NAME").unwrap_or_else(|_| "codex++".to_string());
    Ok(InstallLayout {
        standalone_root: standalone_root.to_path_buf(),
        releases_dir: releases_dir.to_path_buf(),
        bin_dir,
        bin_name,
    })
}

fn home_local_bin() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".local/bin"),
        |home| PathBuf::from(home).join(".local/bin"),
    )
}

async fn latest_release_version(http: &RouteAwareClientPool) -> Result<String> {
    let response = http
        .get(LATEST_RELEASE_URL)
        .header(
            USER_AGENT,
            format!("codex-cli/{}", codex_cli::CODEX_CLI_DISPLAY_VERSION),
        )
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .context("failed to request the latest Codex++ release")?
        .error_for_status()
        .context("failed to resolve the latest Codex++ release")?;
    let release: GitHubRelease = response
        .json()
        .await
        .context("failed to decode the latest Codex++ release")?;
    let version = release
        .tag_name
        .strip_prefix("rust-v")
        .context("latest Codex++ release tag is not a fork release tag")?;
    Ok(version.to_string())
}

async fn fetch_checksum_manifest(http: &RouteAwareClientPool, url: &str) -> Result<String> {
    let mut response = http
        .get(url)
        .header(
            USER_AGENT,
            format!("codex-cli/{}", codex_cli::CODEX_CLI_DISPLAY_VERSION),
        )
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to request {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download {url}"))?;
    if let Some(length) = response.content_length()
        && length > MAX_CHECKSUM_MANIFEST_BYTES
    {
        bail!("checksum manifest from {url} is unexpectedly large");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read {url}"))?
    {
        if body.len() as u64 + chunk.len() as u64 > MAX_CHECKSUM_MANIFEST_BYTES {
            bail!("checksum manifest from {url} exceeds {MAX_CHECKSUM_MANIFEST_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).with_context(|| format!("checksum manifest from {url} is not UTF-8"))
}

fn checksum_for_asset(manifest: &str, asset: &str) -> Result<String> {
    let expected = manifest.lines().find_map(|line| {
        let (checksum, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start_matches('*').trim();
        (name == asset).then_some(checksum.to_string())
    });
    let expected = expected.with_context(|| format!("checksum for {asset} not found"))?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("checksum for {asset} is not a valid SHA-256 digest");
    }
    Ok(expected)
}

async fn download_archive(http: &RouteAwareClientPool, url: &str, output: &Path) -> Result<String> {
    let mut response = http
        .get(url)
        .header(
            USER_AGENT,
            format!("codex-cli/{}", codex_cli::CODEX_CLI_DISPLAY_VERSION),
        )
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to request {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download {url}"))?;
    let total = response.content_length();
    let mut output_file =
        File::create(output).with_context(|| format!("failed to create {}", output.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0_u64;
    let mut progress = DownloadProgress::new(std::io::stdout().is_terminal());

    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read {url}"))?
    {
        output_file
            .write_all(&chunk)
            .with_context(|| format!("failed to write {}", output.display()))?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        progress.update(downloaded, total)?;
    }
    progress.finish()?;
    if let Some(total) = total
        && downloaded != total
    {
        bail!("download from {url} ended early: received {downloaded} of {total} bytes");
    }
    Ok(hex_digest(hasher))
}

struct DownloadProgress {
    enabled: bool,
    last_percent: Option<u8>,
}

impl DownloadProgress {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_percent: None,
        }
    }

    fn update(&mut self, downloaded: u64, total: Option<u64>) -> Result<()> {
        let Some(total) = total else {
            return Ok(());
        };
        let percent = u8::try_from(downloaded.saturating_mul(100) / total.max(1)).unwrap_or(100);
        if self.last_percent == Some(percent) {
            return Ok(());
        }
        self.last_percent = Some(percent);
        if self.enabled {
            print_progress(percent, downloaded, total)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.enabled {
            println!();
        }
        Ok(())
    }
}

fn print_progress(percent: u8, downloaded: u64, total: u64) -> Result<()> {
    let filled = usize::from(percent) * PROGRESS_WIDTH / 100;
    print!(
        "\rDownloading [{}{}] {:>3}% {}/{} MiB",
        "#".repeat(filled),
        " ".repeat(PROGRESS_WIDTH - filled),
        percent,
        downloaded / 1024 / 1024,
        total / 1024 / 1024
    );
    std::io::stdout().flush()?;
    Ok(())
}

fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn checksums_equal(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
