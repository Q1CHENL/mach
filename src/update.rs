//! Check for and install newer builds.
//!
//! Source of truth: GitHub Releases on `Q1CHENL/mach`. Fresh installs use the
//! release installer; self-updates download and verify the exact release asset
//! directly. The TUI schedules its next background check one day after success;
//! failures retry after an hour by default and honor server backoff. Install
//! remains an explicit action through `/update` or `mach update --install`.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Repo used for release checks and install.
pub const REPO: &str = "Q1CHENL/mach";
pub const GIT_URL: &str = "https://github.com/Q1CHENL/mach";
const RELEASES_URL: &str = "https://api.github.com/repos/Q1CHENL/mach/releases?per_page=100";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/Q1CHENL/mach/releases/download";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const USER_AGENT: &str = concat!("mach/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const RELEASE_RECEIPT_DIR: &str = ".mach-release-install";
const INSTALL_LOCK_DIR: &str = ".mach-install.lock";
const INSTALL_LOCK_OWNER: &str = "owner";
const INSTALL_LOCK_WAIT: Duration = Duration::from_secs(30);
const INSTALL_LOCK_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    /// Exact Git tag selected from the GitHub release response.
    pub tag: String,
    pub newer: bool,
    pub prerelease: bool,
    pub release_url: String,
    /// Exact platform binary and URLs bound to [`tag`](Self::tag).
    pub asset_name: String,
    pub asset_url: String,
    pub checksums_url: String,
}

#[derive(Debug)]
pub(crate) enum Conditional<T> {
    Modified { value: T, etag: Option<String> },
    NotModified,
}

pub(crate) type CheckResponse = Conditional<CheckResult>;

#[derive(Debug)]
pub(crate) struct CheckFailure {
    pub(crate) message: String,
    pub(crate) retry_at: Option<i64>,
}

impl CheckFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub destination: PathBuf,
    pub tag: String,
    pub disposition: InstallDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDisposition {
    Installed,
    AlreadyCurrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadProgress {
    pub(crate) downloaded: u64,
    pub(crate) total: Option<u64>,
}

impl CheckResult {
    /// One-line status for the TUI / CLI.
    pub fn summary(&self) -> String {
        if self.newer {
            format!(
                "Update available: v{} → v{}  ({})",
                self.current, self.latest, self.release_url
            )
        } else {
            format!("Up to date (v{})", self.current)
        }
    }

    /// How to install this build.
    pub fn install_hint(&self) -> String {
        "mach update --install\n# or, for Cargo installs: cargo install --locked mach-tui".into()
    }
}

/// Built-in version of this binary.
pub fn current_version() -> &'static str {
    crate::VERSION
}

/// Ask GitHub what the latest release is and compare to this binary.
///
/// Picks the highest stable semver release that ships both this platform's
/// binary and its checksum manifest. Requiring those assets excludes the
/// disconnected legacy Python releases without blocking legitimate majors.
pub fn check() -> Result<CheckResult, String> {
    match check_with_etag(None).map_err(|error| error.message)? {
        CheckResponse::Modified { value: info, .. } => Ok(info),
        CheckResponse::NotModified => {
            Err("GitHub returned 304 without a conditional request".into())
        }
    }
}

/// Conditional release check used by the long-running TUI scheduler.
pub(crate) fn check_with_etag(etag: Option<&str>) -> Result<CheckResponse, CheckFailure> {
    let current = current_version().to_string();
    let ReleaseDocument::Modified { value: body, etag } = fetch_releases(RELEASES_URL, etag)?
    else {
        return Ok(CheckResponse::NotModified);
    };
    let releases: Vec<GhRelease> = serde_json::from_str(&body)
        .map_err(|e| CheckFailure::new(format!("could not parse GitHub release JSON: {e}")))?;
    let asset_name = current_asset_name().map_err(CheckFailure::new)?;
    let selected = select_release(&releases, &asset_name).ok_or_else(|| {
        CheckFailure::new(format!(
            "no stable GitHub release ships both {asset_name} and {CHECKSUMS_ASSET}"
        ))
    })?;
    let latest = selected.version.to_string();
    let newer = selected.version
        > Version::parse(&current)
            .map_err(|e| CheckFailure::new(format!("invalid current version {current:?}: {e}")))?;

    Ok(CheckResponse::Modified {
        value: CheckResult {
            current,
            latest,
            tag: selected.tag,
            newer,
            prerelease: false,
            release_url: selected.release_url,
            asset_name,
            asset_url: selected.asset_url,
            checksums_url: selected.checksums_url,
        },
        etag,
    })
}

#[derive(Debug)]
struct SelectedRelease {
    version: Version,
    tag: String,
    release_url: String,
    asset_url: String,
    checksums_url: String,
}

fn select_release(releases: &[GhRelease], asset_name: &str) -> Option<SelectedRelease> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let version = parse_stable_tag(&release.tag_name)?;
            let asset_url = release.asset_url(asset_name)?;
            let checksums_url = release.asset_url(CHECKSUMS_ASSET)?;
            Some(SelectedRelease {
                version,
                tag: release.tag_name.clone(),
                release_url: if release.html_url.is_empty() {
                    format!("{GIT_URL}/releases/tag/{}", release.tag_name)
                } else {
                    release.html_url.clone()
                },
                asset_url: asset_url.to_string(),
                checksums_url: checksums_url.to_string(),
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

fn current_asset_name() -> Result<String, String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("unsupported architecture {other:?}")),
    };
    let platform = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" if cfg!(target_env = "gnu") => "unknown-linux-gnu",
        "linux" => return Err("this build does not target GNU libc".into()),
        other => return Err(format!("unsupported operating system {other:?}")),
    };
    Ok(format!("mach-{arch}-{platform}"))
}

/// Install the exact release and platform asset returned by [`check`].
///
/// The binary is downloaded and checksum-verified in-process. No downloaded
/// script is executed. The replacement is written, synced, chmodded, and
/// atomically renamed within the destination directory before that directory
/// is synced.
pub fn install(info: &CheckResult) -> Result<InstallResult, String> {
    install_with_progress(info, |_| {})
}

pub(crate) fn install_with_progress(
    info: &CheckResult,
    progress: impl FnMut(DownloadProgress),
) -> Result<InstallResult, String> {
    let target_version = validate_install_info(info)?;
    let destination = install_destination()?;
    let manifest = download_checksum_manifest(&info.checksums_url)
        .map_err(|e| format!("could not download checksums for {}: {e}", info.tag))?;
    let expected_sha = checksum_for_asset(&manifest, &info.asset_name)?;
    let (installed_version, disposition) = download_verified_binary(
        &info.asset_url,
        &expected_sha,
        &destination,
        &target_version,
        progress,
    )?;
    Ok(InstallResult {
        destination,
        tag: format!("v{installed_version}"),
        disposition,
    })
}

fn validate_install_info(info: &CheckResult) -> Result<Version, String> {
    if info.current != current_version() {
        return Err(format!(
            "release check was produced for v{}, but this binary is v{}",
            info.current,
            current_version()
        ));
    }
    if !info.newer {
        return Err("refusing to install a release that is not newer than this binary".into());
    }
    let expected_asset = current_asset_name()?;
    if info.asset_name != expected_asset {
        return Err(format!(
            "refusing asset {} on this platform (expected {expected_asset})",
            info.asset_name
        ));
    }
    let selected_version = parse_stable_tag(&info.tag)
        .ok_or_else(|| format!("invalid stable release tag {:?}", info.tag))?;
    let latest = Version::parse(&info.latest)
        .map_err(|e| format!("invalid selected release version {:?}: {e}", info.latest))?;
    if selected_version != latest || !is_canonical_stable_version(&info.latest, &latest) {
        return Err("selected release tag/version is inconsistent or not stable".into());
    }
    let current = Version::parse(current_version())
        .map_err(|e| format!("invalid built-in version {:?}: {e}", current_version()))?;
    if latest <= current {
        return Err(format!(
            "refusing to install v{latest} over v{current}: updates must move forward"
        ));
    }
    let expected_asset_url = release_asset_url(&info.tag, &info.asset_name);
    if info.asset_url != expected_asset_url {
        return Err(format!(
            "selected binary URL is not bound to {} and {}",
            info.tag, info.asset_name
        ));
    }
    let expected_checksums_url = release_asset_url(&info.tag, CHECKSUMS_ASSET);
    if info.checksums_url != expected_checksums_url {
        return Err(format!(
            "selected checksum URL is not bound to {}",
            info.tag
        ));
    }
    Ok(latest)
}

fn release_asset_url(tag: &str, asset: &str) -> String {
    format!("{RELEASE_DOWNLOAD_BASE}/{tag}/{asset}")
}

fn install_destination() -> Result<PathBuf, String> {
    let explicit_install_dir = std::env::var_os("MACH_INSTALL_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = dirs::home_dir();
    let current_exe = std::env::current_exe().ok();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_install_destination(
        explicit_install_dir.as_deref(),
        home.as_deref(),
        current_exe.as_deref(),
        cargo_home.as_deref(),
    )
}

fn resolve_install_destination(
    explicit_install_dir: Option<&Path>,
    home: Option<&Path>,
    current_exe: Option<&Path>,
    cargo_home: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(install_dir) = explicit_install_dir {
        return Ok(install_dir.join("mach"));
    }

    let home = home.ok_or_else(|| "could not determine the install directory".to_string())?;
    let current_exe =
        current_exe.ok_or_else(|| "could not determine the current mach executable".to_string())?;
    let default_destination = home.join(".local/bin/mach");
    if receipted_release_version(current_exe)?.is_some() {
        return Ok(current_exe.to_path_buf());
    }

    let cargo_bin = cargo_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".cargo"))
        .join("bin");
    if is_cargo_managed(current_exe, &cargo_bin) {
        return Err("Installation managed by Cargo: \
             cargo install --locked mach-tui"
            .into());
    }
    if install_paths_match(current_exe, &default_destination) {
        return Ok(default_destination);
    }

    let current_parent = current_exe
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/path/to/mach"));
    Err(format!(
        "this mach executable at {} is managed by a package manager or another installer; update \
         it there, or set MACH_INSTALL_DIR={} to replace it with a checksum-verified release binary",
        current_exe.display(),
        current_parent.display()
    ))
}

fn install_paths_match(left: &Path, right: &Path) -> bool {
    fn normalize_parent(path: &Path) -> PathBuf {
        let Some(parent) = path.parent() else {
            return path.to_path_buf();
        };
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        path.file_name()
            .map_or(parent.clone(), |name| parent.join(name))
    }

    normalize_parent(left) == normalize_parent(right)
}

fn is_cargo_managed(current_exe: &Path, cargo_bin: &Path) -> bool {
    let Some(parent) = current_exe.parent() else {
        return false;
    };
    if install_paths_match(parent, cargo_bin) {
        return true;
    }
    if parent.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return false;
    }
    let Some(root) = parent.parent() else {
        return false;
    };
    root.join(".crates2.json").is_file() || root.join(".crates.toml").is_file()
}

fn checksum_for_asset(manifest: &str, asset_name: &str) -> Result<String, String> {
    let mut found = None;
    for line in manifest.lines() {
        let mut fields = line.split_whitespace();
        let Some(digest) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if name.trim_start_matches('*') != asset_name {
            continue;
        }
        if fields.next().is_some() {
            return Err(format!(
                "{CHECKSUMS_ASSET} contains a malformed entry for {asset_name}"
            ));
        }
        if found.is_some() {
            return Err(format!(
                "{CHECKSUMS_ASSET} contains duplicate entries for {asset_name}"
            ));
        }
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "{CHECKSUMS_ASSET} contains an invalid digest for {asset_name}"
            ));
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found.ok_or_else(|| format!("{CHECKSUMS_ASSET} has no entry for {asset_name}"))
}

fn download_verified_binary(
    url: &str,
    expected_sha: &str,
    destination: &Path,
    target_version: &Version,
    progress: impl FnMut(DownloadProgress),
) -> Result<(Version, InstallDisposition), String> {
    let mut response = download_response(url)?;
    let total = response.body().content_length();
    if total.is_some_and(|total| total > MAX_BINARY_BYTES) {
        return Err(format!(
            "release binary exceeds the {} MiB safety limit",
            MAX_BINARY_BYTES / 1024 / 1024
        ));
    }
    write_verified_binary(
        response.body_mut().as_reader(),
        expected_sha,
        destination,
        target_version,
        total,
        progress,
    )
}

fn download_response(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();
    agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(map_download_err)
}

fn write_verified_binary<R: Read>(
    mut source: R,
    expected_sha: &str,
    destination: &Path,
    target_version: &Version,
    expected_total: Option<u64>,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<(Version, InstallDisposition), String> {
    #[cfg(not(unix))]
    return Err("self-update is supported only on Unix platforms".into());

    #[cfg(unix)]
    {
        if expected_sha.len() != 64 || !expected_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("expected release digest is not a SHA-256 digest".into());
        }
        let expected_sha = expected_sha.to_ascii_lowercase();
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| "install destination has no parent directory".to_string())?;
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create install directory {}: {e}",
                parent.display()
            )
        })?;
        let parent_dir = File::open(parent)
            .map_err(|e| format!("could not open install directory {}: {e}", parent.display()))?;
        let temp_path = parent.join(format!(".mach.{}.tmp", uuid::Uuid::new_v4()));
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("could not create temporary binary: {e}"))?;

        progress(DownloadProgress {
            downloaded: 0,
            total: expected_total,
        });

        let write_result = (|| -> Result<String, String> {
            let mut hasher = Sha256::new();
            let mut downloaded = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = source
                    .read(&mut buffer)
                    .map_err(|e| format!("could not read release binary: {e}"))?;
                if read == 0 {
                    break;
                }
                downloaded = downloaded
                    .checked_add(read as u64)
                    .ok_or_else(|| "release binary is too large".to_string())?;
                if downloaded > MAX_BINARY_BYTES {
                    return Err(format!(
                        "release binary exceeds the {} MiB safety limit",
                        MAX_BINARY_BYTES / 1024 / 1024
                    ));
                }
                hasher.update(&buffer[..read]);
                temp_file
                    .write_all(&buffer[..read])
                    .map_err(|e| format!("could not write temporary binary: {e}"))?;
                progress(DownloadProgress {
                    downloaded,
                    total: expected_total,
                });
            }

            let actual_sha = format!("{:x}", hasher.finalize());
            if actual_sha != expected_sha {
                return Err(format!(
                    "SHA-256 verification failed (expected {expected_sha}, got {actual_sha})"
                ));
            }
            temp_file
                .set_permissions(fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("could not mark temporary binary executable: {e}"))?;
            temp_file
                .sync_all()
                .map_err(|e| format!("could not sync temporary binary: {e}"))?;
            Ok(actual_sha)
        })();
        drop(temp_file);

        let actual_sha = match write_result {
            Ok(actual_sha) => actual_sha,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        };

        let install_result = (|| -> Result<(Version, InstallDisposition), String> {
            let _lock = InstallLock::acquire(parent)?;
            if let Some(installed_version) =
                receipted_release_version(destination)?.filter(|version| version >= target_version)
            {
                return Ok((installed_version, InstallDisposition::AlreadyCurrent));
            }

            let (installed_version, receipt_update) =
                record_release_version(parent, &actual_sha, target_version)?;
            if let Err(error) = fs::rename(&temp_path, destination) {
                let rollback_error = receipt_update.rollback().err();
                let mut message = format!(
                    "could not replace {} atomically: {error}",
                    destination.display()
                );
                if let Some(rollback_error) = rollback_error {
                    message.push_str(&format!(
                        "; could not roll back release receipt: {rollback_error}"
                    ));
                }
                return Err(message);
            }
            parent_dir.sync_all().map_err(|e| {
                format!("could not sync install directory {}: {e}", parent.display())
            })?;
            Ok((installed_version, InstallDisposition::Installed))
        })();

        if temp_path.exists() {
            let _ = fs::remove_file(&temp_path);
        }
        install_result
    }
}

#[cfg(unix)]
fn receipted_release_version(destination: &Path) -> Result<Option<Version>, String> {
    let Some(parent) = destination.parent() else {
        return Ok(None);
    };
    let receipt_dir = parent.join(RELEASE_RECEIPT_DIR);
    if !receipt_dir.is_dir() || !destination.is_file() {
        return Ok(None);
    }
    let digest = sha256_file(destination)?;
    let receipt = receipt_dir.join(digest);
    if !receipt.is_file() {
        return Ok(None);
    }
    read_receipt_version(&receipt).map(Some)
}

#[cfg(not(unix))]
fn receipted_release_version(_destination: &Path) -> Result<Option<Version>, String> {
    Ok(None)
}

#[cfg(unix)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|e| format!("could not inspect installed binary {}: {e}", path.display()))?;
    if metadata.len() > MAX_BINARY_BYTES {
        return Err(format!(
            "installed binary {} exceeds the {} MiB safety limit",
            path.display(),
            MAX_BINARY_BYTES / 1024 / 1024
        ));
    }
    let mut file = File::open(path)
        .map_err(|e| format!("could not open installed binary {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("could not read installed binary {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn read_receipt_version(path: &Path) -> Result<Version, String> {
    let file = File::open(path)
        .map_err(|e| format!("could not open release receipt {}: {e}", path.display()))?;
    let text = read_bounded_text(file, 128)
        .map_err(|e| format!("invalid release receipt {}: {e}", path.display()))?;
    let value = text
        .strip_suffix('\n')
        .filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
        .ok_or_else(|| format!("invalid release receipt {}", path.display()))?;
    let version = Version::parse(value)
        .map_err(|e| format!("invalid release receipt {}: {e}", path.display()))?;
    if !is_canonical_stable_version(value, &version) {
        return Err(format!("invalid release receipt {}", path.display()));
    }
    Ok(version)
}

#[cfg(unix)]
fn record_release_version(
    parent: &Path,
    digest: &str,
    target_version: &Version,
) -> Result<(Version, ReceiptUpdate), String> {
    let receipt_dir = parent.join(RELEASE_RECEIPT_DIR);
    fs::create_dir_all(&receipt_dir).map_err(|e| {
        format!(
            "could not create release receipt directory {}: {e}",
            receipt_dir.display()
        )
    })?;
    let receipt = receipt_dir.join(digest);
    let previous_version = if receipt.is_file() {
        let recorded = read_receipt_version(&receipt)?;
        if recorded >= *target_version {
            return Ok((recorded, ReceiptUpdate::Unchanged));
        }
        Some(recorded)
    } else {
        None
    };

    write_release_receipt(parent, &receipt, target_version)?;
    let update = match previous_version {
        Some(previous) => ReceiptUpdate::Replaced { receipt, previous },
        None => ReceiptUpdate::Created(receipt),
    };
    Ok((target_version.clone(), update))
}

#[cfg(unix)]
fn write_release_receipt(parent: &Path, receipt: &Path, version: &Version) -> Result<(), String> {
    let receipt_dir = receipt
        .parent()
        .ok_or_else(|| "release receipt has no parent directory".to_string())?;
    let temp_path = receipt_dir.join(format!(".receipt.{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("could not create release receipt: {e}"))?;
        writeln!(file, "{version}").map_err(|e| format!("could not write release receipt: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("could not sync release receipt: {e}"))?;
        fs::rename(&temp_path, receipt)
            .map_err(|e| format!("could not publish release receipt: {e}"))?;
        File::open(receipt_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("could not sync release receipt directory: {e}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("could not sync install directory {}: {e}", parent.display()))?;
        Ok(())
    })();
    if write_result.is_err() && temp_path.exists() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(unix)]
enum ReceiptUpdate {
    Unchanged,
    Created(PathBuf),
    Replaced { receipt: PathBuf, previous: Version },
}

#[cfg(unix)]
impl ReceiptUpdate {
    fn rollback(self) -> Result<(), String> {
        let receipt = match self {
            Self::Unchanged => return Ok(()),
            Self::Replaced { receipt, previous } => {
                let parent = receipt
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| "release receipt directory has no parent".to_string())?;
                return write_release_receipt(parent, &receipt, &previous);
            }
            Self::Created(receipt) => receipt,
        };
        let receipt_dir = receipt
            .parent()
            .ok_or_else(|| "release receipt has no parent directory".to_string())?;
        let parent = receipt_dir
            .parent()
            .ok_or_else(|| "release receipt directory has no parent".to_string())?;
        fs::remove_file(&receipt)
            .map_err(|e| format!("could not remove {}: {e}", receipt.display()))?;
        File::open(receipt_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("could not sync release receipt directory: {e}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("could not sync install directory {}: {e}", parent.display()))
    }
}

#[cfg(unix)]
struct InstallLock {
    path: PathBuf,
    owner_record: String,
}

#[cfg(unix)]
impl InstallLock {
    fn acquire(parent: &Path) -> Result<Self, String> {
        let path = parent.join(INSTALL_LOCK_DIR);
        let started = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let owner_record = format!("{timestamp} {}\n", uuid::Uuid::new_v4());
                    let owner_path = path.join(INSTALL_LOCK_OWNER);
                    let initialize = (|| -> Result<(), String> {
                        let mut owner = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&owner_path)
                            .map_err(|e| format!("could not create install lock owner: {e}"))?;
                        owner
                            .write_all(owner_record.as_bytes())
                            .map_err(|e| format!("could not write install lock owner: {e}"))?;
                        owner
                            .sync_all()
                            .map_err(|e| format!("could not sync install lock owner: {e}"))?;
                        File::open(&path)
                            .and_then(|directory| directory.sync_all())
                            .map_err(|e| format!("could not sync install lock: {e}"))?;
                        Ok(())
                    })();
                    if let Err(error) = initialize {
                        let _ = fs::remove_file(owner_path);
                        let _ = fs::remove_dir(&path);
                        return Err(error);
                    }
                    return Ok(Self { path, owner_record });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "could not acquire install lock {}: {error}",
                        path.display()
                    ));
                }
            }
            if started.elapsed() >= INSTALL_LOCK_WAIT {
                return Err(format!(
                    "timed out waiting for another installer holding {}; if no installer is \
                     running, remove this stale lock directory",
                    path.display()
                ));
            }
            thread::sleep(INSTALL_LOCK_POLL);
        }
    }
}

#[cfg(unix)]
impl Drop for InstallLock {
    fn drop(&mut self) {
        let owner = self.path.join(INSTALL_LOCK_OWNER);
        if fs::read_to_string(&owner).ok().as_deref() == Some(self.owner_record.as_str()) {
            let _ = fs::remove_file(owner);
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

impl GhRelease {
    fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|asset| asset.name == name && !asset.browser_download_url.is_empty())
            .map(|asset| asset.browser_download_url.as_str())
    }
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

type ReleaseDocument = Conditional<String>;

fn fetch_releases(url: &str, etag: Option<&str>) -> Result<ReleaseDocument, CheckFailure> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let mut request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json");
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = request
        .call()
        .map_err(|error| CheckFailure::new(map_ureq_err(error)))?;
    let status = response.status().as_u16();
    if status == 304 {
        return Ok(ReleaseDocument::NotModified);
    }
    if status != 200 {
        let now = Utc::now().timestamp();
        let retry_at = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, now))
            .or_else(|| {
                let remaining = response
                    .headers()
                    .get("X-RateLimit-Remaining")
                    .and_then(|value| value.to_str().ok());
                (remaining == Some("0"))
                    .then(|| {
                        response
                            .headers()
                            .get("X-RateLimit-Reset")
                            .and_then(|value| value.to_str().ok())
                            .and_then(parse_nonnegative_decimal)
                    })
                    .flatten()
            });
        let message = if status == 404 {
            "no GitHub releases yet — publish one, or install from git".into()
        } else {
            format!("GitHub API HTTP {status}")
        };
        return Err(CheckFailure { message, retry_at });
    }
    let response_etag = response
        .headers()
        .get("ETag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = read_bounded_text(response.body_mut().as_reader(), MAX_TEXT_BYTES)
        .map_err(CheckFailure::new)?;
    Ok(ReleaseDocument::Modified {
        value: body,
        etag: response_etag,
    })
}

fn parse_retry_after(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if let Some(seconds) = parse_nonnegative_decimal(value) {
        return Some(now.saturating_add(seconds));
    }
    let timestamp = httpdate::parse_http_date(value).ok()?;
    let seconds = timestamp.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(i64::try_from(seconds).unwrap_or(i64::MAX))
}

fn parse_nonnegative_decimal(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0_i64, |number, byte| {
        number
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'))
    }))
}

fn download_checksum_manifest(url: &str) -> Result<String, String> {
    let mut response = download_response(url)?;
    read_bounded_text(response.body_mut().as_reader(), MAX_TEXT_BYTES)
}

fn read_bounded_text<R: Read>(source: R, max_bytes: u64) -> Result<String, String> {
    let mut bytes = Vec::new();
    source
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not read response: {e}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("response exceeds the {max_bytes}-byte limit"));
    }
    String::from_utf8(bytes).map_err(|e| format!("response is not valid UTF-8: {e}"))
}

fn map_download_err(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!("download HTTP {code}"),
        other => format!("download failed: {other}"),
    }
}

fn parse_stable_tag(tag: &str) -> Option<Version> {
    if tag != tag.trim() {
        return None;
    }
    let tag = tag.trim();
    let normalized = tag.strip_prefix('v').unwrap_or(tag);
    parse_stable_version(normalized)
}

pub(crate) fn parse_stable_version(value: &str) -> Option<Version> {
    let version = Version::parse(value).ok()?;
    is_canonical_stable_version(value, &version).then_some(version)
}

fn is_canonical_stable_version(value: &str, version: &Version) -> bool {
    version.pre.is_empty() && version.build.is_empty() && value == version.to_string()
}

fn map_ureq_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(404) => {
            "no GitHub releases yet — publish one, or install from git".into()
        }
        ureq::Error::StatusCode(code) => format!("GitHub API HTTP {code}"),
        other => format!("network error: {other}"),
    }
}

/// Strip one conventional leading `v` and whitespace.
pub fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    tag.strip_prefix('v').unwrap_or(tag).to_string()
}

/// True when `latest` is a higher semantic version than `current`.
pub fn is_newer(latest: &str, current: &str) -> Option<bool> {
    let a = Version::parse(&normalize_tag(latest)).ok()?;
    let b = Version::parse(&normalize_tag(current)).ok()?;
    Some(a > b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn serve_once(response: impl Into<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let response = response.into();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = request_tx.send(String::from_utf8(request).unwrap());
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/releases"), request_rx)
    }

    fn release(tag: &str, prerelease: bool, assets: &[(&str, &str)]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            html_url: format!("https://github.test/releases/tag/{tag}"),
            prerelease,
            draft: false,
            assets: assets
                .iter()
                .map(|(name, url)| GhAsset {
                    name: (*name).into(),
                    browser_download_url: (*url).into(),
                })
                .collect(),
        }
    }

    fn valid_install_result() -> CheckResult {
        let current = Version::parse(current_version()).unwrap();
        let latest = Version::new(
            current.major,
            current.minor,
            current.patch.checked_add(1).unwrap(),
        );
        let tag = format!("v{latest}");
        let asset_name = current_asset_name().unwrap();
        CheckResult {
            current: current.to_string(),
            latest: latest.to_string(),
            tag: tag.clone(),
            newer: true,
            prerelease: false,
            release_url: format!("https://github.test/releases/tag/{tag}"),
            asset_url: release_asset_url(&tag, &asset_name),
            checksums_url: release_asset_url(&tag, CHECKSUMS_ASSET),
            asset_name,
        }
    }

    #[test]
    fn normalizes_v_prefix() {
        assert_eq!(normalize_tag("v1.2.3"), "1.2.3");
        assert_eq!(normalize_tag(" 1.0.0 "), "1.0.0");
    }

    #[test]
    fn compares_semver() {
        assert_eq!(is_newer("0.2.0", "0.1.0"), Some(true));
        assert_eq!(is_newer("0.1.0", "0.1.0"), Some(false));
        assert_eq!(is_newer("0.1.0", "0.2.0"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.1.1-rc.1", "0.1.0"), Some(true));
    }

    #[test]
    fn conditional_release_request_reuses_etag_and_accepts_not_modified() {
        let (url, request) = serve_once(
            "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );

        assert!(matches!(
            fetch_releases(&url, Some("\"release-etag\"")).unwrap(),
            ReleaseDocument::NotModified
        ));
        assert!(
            request
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("if-none-match: \"release-etag\"")
        );
    }

    #[test]
    fn modified_release_response_captures_the_new_etag() {
        let (url, _) = serve_once(
            "HTTP/1.1 200 OK\r\nETag: \"next-etag\"\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
        );

        let ReleaseDocument::Modified { value: body, etag } = fetch_releases(&url, None).unwrap()
        else {
            panic!("a 200 response must carry a release document");
        };
        assert_eq!(body, "[]");
        assert_eq!(etag.as_deref(), Some("\"next-etag\""));
    }

    #[test]
    fn rate_limited_release_request_preserves_retry_after() {
        let (url, _) = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let before = Utc::now().timestamp();

        let error = fetch_releases(&url, None).expect_err("rate limit should fail the check");

        assert_eq!(error.message, "GitHub API HTTP 429");
        assert!(error.retry_at.is_some_and(|retry_at| {
            retry_at >= before + 120 && retry_at <= Utc::now().timestamp() + 120
        }));
    }

    #[test]
    fn retry_after_accepts_every_http_date_form() {
        let expected = 784_111_777;
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            let (url, _) = serve_once(format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {value}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ));

            let error = fetch_releases(&url, None).expect_err("rate limit should fail the check");

            assert_eq!(error.retry_at, Some(expected), "failed to parse {value}");
        }
    }

    #[test]
    fn rate_limit_reset_is_used_only_when_the_budget_is_exhausted() {
        let reset = Utc::now().timestamp() + 3_600;
        let (url, _) = serve_once(format!(
            "HTTP/1.1 500 Internal Server Error\r\nX-RateLimit-Remaining: 1\r\nX-RateLimit-Reset: {reset}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let error = fetch_releases(&url, None).expect_err("server error should fail the check");
        assert_eq!(error.retry_at, None);

        let (url, _) = serve_once(format!(
            "HTTP/1.1 429 Too Many Requests\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let error = fetch_releases(&url, None).expect_err("rate limit should fail the check");
        assert_eq!(error.retry_at, Some(reset));
    }

    #[test]
    fn stable_release_tags_must_be_canonical_and_not_prereleases() {
        assert_eq!(parse_stable_tag("v1.2.3").unwrap().to_string(), "1.2.3");
        assert!(parse_stable_tag("1.2.3+build.4").is_none());
        assert!(parse_stable_tag("v01.2.3").is_none());
        assert!(parse_stable_tag("v1.2.3-rc.1").is_none());
        assert!(parse_stable_tag(" v1.2.3").is_none());
    }

    #[test]
    fn install_hint_prefers_verified_self_update() {
        let r = CheckResult {
            current: "0.1.0".into(),
            latest: "0.1.0".into(),
            tag: "v0.1.0".into(),
            newer: false,
            prerelease: false,
            release_url: String::new(),
            asset_name: "mach-aarch64-apple-darwin".into(),
            asset_url: "https://example.test/mach".into(),
            checksums_url: "https://example.test/SHA256SUMS".into(),
        };
        let h = r.install_hint();
        assert!(h.contains("mach update --install"));
        assert!(h.contains("cargo install --locked mach-tui"));
        assert!(!h.contains("curl"));
    }

    #[test]
    fn cargo_managed_binary_names_its_update_command() {
        let home = Path::new("/home/alice");
        let cargo_home = home.join(".cargo");
        let current_exe = cargo_home.join("bin/mach");

        let error = resolve_install_destination(None, Some(home), Some(&current_exe), None)
            .expect_err("a Cargo-managed executable must not create a shadow release install");
        assert_eq!(
            error,
            "Installation managed by Cargo: cargo install --locked mach-tui"
        );

        assert_eq!(
            resolve_install_destination(
                Some(Path::new("/opt/mach/bin")),
                Some(home),
                Some(&current_exe),
                None,
            )
            .unwrap(),
            PathBuf::from("/opt/mach/bin/mach"),
            "an explicit destination is an intentional ownership change"
        );

        let custom_cargo_home = Path::new("/srv/cargo");
        let custom_exe = custom_cargo_home.join("bin/mach");
        assert!(
            resolve_install_destination(
                None,
                Some(home),
                Some(&custom_exe),
                Some(custom_cargo_home),
            )
            .is_err(),
            "CARGO_HOME must participate in ownership detection"
        );
    }

    #[test]
    fn externally_managed_binary_does_not_create_a_shadow_release_install() {
        let home = Path::new("/home/alice");
        let current_exe = Path::new("/opt/homebrew/bin/mach");

        let error = resolve_install_destination(None, Some(home), Some(current_exe), None)
            .expect_err("an externally managed executable must not update a shadow destination");

        assert!(error.contains("package manager"));
        assert!(error.contains("MACH_INSTALL_DIR"));
    }

    #[test]
    fn cargo_install_root_is_detected_from_its_ownership_metadata() {
        let dir = std::env::temp_dir().join(format!("mach-cargo-root-{}", uuid::Uuid::new_v4()));
        let cargo_root = dir.join("custom-cargo-root");
        let bin = cargo_root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(cargo_root.join(".crates2.json"), b"{}").unwrap();
        let current_exe = bin.join("mach");

        let error =
            resolve_install_destination(None, Some(dir.as_path()), Some(&current_exe), None)
                .expect_err(
                    "cargo install --root ownership must not create a shadow release install",
                );

        assert!(error.contains("Cargo"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_receipt_disambiguates_a_cargo_root_at_the_default_destination() {
        let dir = std::env::temp_dir().join(format!("mach-cargo-default-{}", uuid::Uuid::new_v4()));
        let home = dir.join("home");
        let cargo_root = home.join(".local");
        let bin = cargo_root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(cargo_root.join(".crates2.json"), b"{}").unwrap();
        let current_exe = bin.join("mach");
        let binary = b"ambiguous default-path binary";
        fs::write(&current_exe, binary).unwrap();

        let error = resolve_install_destination(None, Some(&home), Some(&current_exe), None)
            .expect_err("Cargo ownership must beat an unreceipted default path");
        assert!(error.contains("Cargo"));

        let receipt_dir = bin.join(RELEASE_RECEIPT_DIR);
        fs::create_dir(&receipt_dir).unwrap();
        fs::write(receipt_dir.join(sha256_hex(binary)), b"1.2.3\n").unwrap();
        assert_eq!(
            resolve_install_destination(None, Some(&home), Some(&current_exe), None).unwrap(),
            current_exe,
            "a content-bound release receipt is stronger ownership evidence"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn custom_release_destination_is_reused_only_with_a_matching_receipt() {
        let dir = std::env::temp_dir().join(format!("mach-release-root-{}", uuid::Uuid::new_v4()));
        let home = dir.join("home");
        let bin = dir.join("custom/bin");
        fs::create_dir_all(&bin).unwrap();
        let current_exe = bin.join("mach");
        let binary = b"checksum-verified release binary";
        fs::write(&current_exe, binary).unwrap();
        let receipt_dir = bin.join(RELEASE_RECEIPT_DIR);
        fs::create_dir(&receipt_dir).unwrap();
        fs::write(receipt_dir.join(sha256_hex(binary)), b"1.2.3\n").unwrap();

        assert_eq!(
            resolve_install_destination(None, Some(&home), Some(&current_exe), None).unwrap(),
            current_exe
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selector_ignores_legacy_prereleases_and_binds_required_assets() {
        let releases = vec![
            release("v1.21.9", false, &[]),
            release(
                "v2.0.0-rc.1",
                false,
                &[
                    ("mach-x86_64-unknown-linux-gnu", "https://bad/tagged-rc"),
                    (CHECKSUMS_ASSET, "https://bad/tagged-rc-sums"),
                ],
            ),
            release(
                "v0.2.0-rc.1",
                true,
                &[
                    ("mach-x86_64-unknown-linux-gnu", "https://bad/rc"),
                    (CHECKSUMS_ASSET, "https://bad/rc-sums"),
                ],
            ),
            release(
                "v0.1.2",
                false,
                &[
                    ("mach-x86_64-unknown-linux-gnu", "https://good/mach"),
                    (CHECKSUMS_ASSET, "https://good/SHA256SUMS"),
                ],
            ),
        ];

        let selected = select_release(&releases, "mach-x86_64-unknown-linux-gnu")
            .expect("stable release with both assets");

        assert_eq!(selected.version.to_string(), "0.1.2");
        assert_eq!(selected.tag, "v0.1.2");
        assert_eq!(selected.asset_url, "https://good/mach");
        assert_eq!(selected.checksums_url, "https://good/SHA256SUMS");
    }

    #[test]
    fn selector_allows_a_legitimate_major_upgrade() {
        let releases = vec![release(
            "v1.0.0",
            false,
            &[
                ("mach-aarch64-apple-darwin", "https://good/mach"),
                (CHECKSUMS_ASSET, "https://good/SHA256SUMS"),
            ],
        )];

        let selected =
            select_release(&releases, "mach-aarch64-apple-darwin").expect("major upgrade");
        assert_eq!(selected.version.to_string(), "1.0.0");
    }

    #[test]
    fn selector_still_returns_the_latest_release_when_this_build_is_ahead() {
        let releases = vec![release(
            "v0.9.0",
            false,
            &[
                ("mach-aarch64-apple-darwin", "https://good/mach"),
                (CHECKSUMS_ASSET, "https://good/SHA256SUMS"),
            ],
        )];

        let selected = select_release(&releases, "mach-aarch64-apple-darwin")
            .expect("an older eligible release is still the latest published release");
        assert_eq!(selected.version.to_string(), "0.9.0");
        assert_eq!(
            is_newer(&selected.version.to_string(), "1.0.0"),
            Some(false)
        );
    }

    #[test]
    fn selector_rejects_releases_missing_the_binary_or_checksum_manifest() {
        let releases = vec![
            release(
                "v0.3.0",
                false,
                &[(CHECKSUMS_ASSET, "https://bad/only-sums")],
            ),
            release(
                "v0.2.0",
                false,
                &[("mach-x86_64-unknown-linux-gnu", "https://bad/only-bin")],
            ),
        ];

        assert!(select_release(&releases, "mach-x86_64-unknown-linux-gnu").is_none());
    }

    #[test]
    fn checksum_parser_requires_one_exact_valid_asset_entry() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            checksum_for_asset(
                &format!("{digest}  mach-aarch64-apple-darwin\n"),
                "mach-aarch64-apple-darwin",
            )
            .unwrap(),
            digest,
        );
        assert!(checksum_for_asset(&format!("{digest}  mach-other\n"), "mach").is_err());
        assert!(checksum_for_asset(&format!("{digest}  mach\n{digest}  mach\n"), "mach",).is_err());
        assert!(checksum_for_asset(&format!("{digest}  mach extra\n"), "mach").is_err());
    }

    #[test]
    fn installer_rejects_urls_not_bound_to_the_selected_tag_and_asset() {
        let mut result = valid_install_result();
        validate_install_info(&result).unwrap();

        result.asset_url.push_str("?wrong-release");
        assert!(validate_install_info(&result).is_err());
    }

    #[test]
    fn installer_rejects_stale_or_non_update_check_results() {
        let mut stale = valid_install_result();
        stale.current = "0.0.0".into();
        assert!(
            validate_install_info(&stale)
                .unwrap_err()
                .contains("produced for")
        );

        let mut not_newer = valid_install_result();
        not_newer.newer = false;
        assert!(
            validate_install_info(&not_newer)
                .unwrap_err()
                .contains("not newer")
        );
    }

    #[test]
    fn installer_rejects_reinstalls_and_downgrades() {
        let mut reinstall = valid_install_result();
        reinstall.latest = current_version().into();
        reinstall.tag = format!("v{}", current_version());
        reinstall.asset_url = release_asset_url(&reinstall.tag, &reinstall.asset_name);
        reinstall.checksums_url = release_asset_url(&reinstall.tag, CHECKSUMS_ASSET);
        assert!(
            validate_install_info(&reinstall)
                .unwrap_err()
                .contains("must move forward")
        );

        let current = Version::parse(current_version()).unwrap();
        let lower = Version::new(0, 0, 0);
        assert!(lower < current, "test package version must be above 0.0.0");
        let mut downgrade = valid_install_result();
        downgrade.latest = lower.to_string();
        downgrade.tag = format!("v{lower}");
        downgrade.asset_url = release_asset_url(&downgrade.tag, &downgrade.asset_name);
        downgrade.checksums_url = release_asset_url(&downgrade.tag, CHECKSUMS_ASSET);
        assert!(
            validate_install_info(&downgrade)
                .unwrap_err()
                .contains("must move forward")
        );
    }

    #[test]
    fn text_responses_are_bounded() {
        assert_eq!(
            read_bounded_text(std::io::Cursor::new(b"four"), 4).unwrap(),
            "four"
        );
        assert!(
            read_bounded_text(std::io::Cursor::new(b"oversized"), 4)
                .unwrap_err()
                .contains("4-byte limit")
        );
    }

    #[test]
    fn verified_replace_preserves_the_existing_binary_on_hash_failure() {
        let dir = std::env::temp_dir().join(format!("mach-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let destination = dir.join("mach");
        fs::write(&destination, b"old binary").unwrap();

        let error = write_verified_binary(
            std::io::Cursor::new(b"corrupt download"),
            &"0".repeat(64),
            &destination,
            &Version::parse("1.0.0").unwrap(),
            None,
            |_| {},
        )
        .unwrap_err();

        assert!(error.contains("SHA-256"));
        assert_eq!(fs::read(&destination).unwrap(), b"old binary");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_replace_installs_an_executable_binary() {
        let dir = std::env::temp_dir().join(format!("mach-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let destination = dir.join("mach");
        let binary = b"verified binary";
        let digest = sha256_hex(binary);
        let version = Version::parse("1.2.3").unwrap();

        let (installed_version, disposition) = write_verified_binary(
            std::io::Cursor::new(binary),
            &digest,
            &destination,
            &version,
            None,
            |_| {},
        )
        .unwrap();

        assert_eq!(installed_version, version);
        assert_eq!(disposition, InstallDisposition::Installed);
        assert_eq!(fs::read(&destination).unwrap(), binary);
        assert_eq!(
            fs::read_to_string(dir.join(RELEASE_RECEIPT_DIR).join(&digest)).unwrap(),
            "1.2.3\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_replace_does_not_downgrade_a_newer_receipted_binary() {
        let dir = std::env::temp_dir().join(format!("mach-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let destination = dir.join("mach");
        let newer_binary = b"newer verified binary";
        fs::write(&destination, newer_binary).unwrap();
        let receipt_dir = dir.join(".mach-release-install");
        fs::create_dir(&receipt_dir).unwrap();
        fs::write(receipt_dir.join(sha256_hex(newer_binary)), b"9.9.9\n").unwrap();

        let older_binary = b"older verified binary";
        let (installed_version, disposition) = write_verified_binary(
            std::io::Cursor::new(older_binary),
            &sha256_hex(older_binary),
            &destination,
            &Version::parse("9.8.7").unwrap(),
            None,
            |_| {},
        )
        .unwrap();

        assert_eq!(installed_version, Version::parse("9.9.9").unwrap());
        assert_eq!(disposition, InstallDisposition::AlreadyCurrent);
        assert_eq!(fs::read(&destination).unwrap(), newer_binary);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_binary_rename_rolls_back_the_candidate_receipt() {
        let dir = std::env::temp_dir().join(format!("mach-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let destination = dir.join("mach");
        fs::create_dir(&destination).unwrap();
        let binary = b"checksum-verified binary";
        let digest = sha256_hex(binary);

        let error = write_verified_binary(
            std::io::Cursor::new(binary),
            &digest,
            &destination,
            &Version::parse("1.2.3").unwrap(),
            None,
            |_| {},
        )
        .unwrap_err();

        assert!(error.contains("could not replace"));
        assert!(!dir.join(RELEASE_RECEIPT_DIR).join(digest).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_install_lock_owner_cannot_remove_a_new_lock() {
        let dir = std::env::temp_dir().join(format!("mach-update-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let lock = InstallLock::acquire(&dir).unwrap();
        let lock_path = dir.join(INSTALL_LOCK_DIR);
        let replacement_record = "0 replacement-owner\n";
        fs::write(lock_path.join(INSTALL_LOCK_OWNER), replacement_record).unwrap();

        drop(lock);
        assert_eq!(
            fs::read_to_string(lock_path.join(INSTALL_LOCK_OWNER)).unwrap(),
            replacement_record
        );

        fs::remove_file(lock_path.join(INSTALL_LOCK_OWNER)).unwrap();
        fs::remove_dir(lock_path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn install_lock_serializes_destination_writers() {
        let dir = std::env::temp_dir().join(format!("mach-update-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let first = InstallLock::acquire(&dir).unwrap();
        let second_dir = dir.clone();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let second = InstallLock::acquire(&second_dir).unwrap();
            acquired_tx.send(()).unwrap();
            drop(second);
        });

        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(250))
                .is_err(),
            "a second installer must wait while the destination lock is held"
        );
        drop(first);
        acquired_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the next installer should acquire the released lock");
        waiter.join().unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn verified_replace_reports_monotonic_download_progress() {
        let dir = std::env::temp_dir().join(format!("mach-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let destination = dir.join("mach");
        let binary = vec![b'x'; 150_000];
        let digest = sha256_hex(&binary);
        let mut progress = Vec::new();

        write_verified_binary(
            std::io::Cursor::new(&binary),
            &digest,
            &destination,
            &Version::parse("1.2.3").unwrap(),
            Some(binary.len() as u64),
            |event| progress.push(event),
        )
        .unwrap();

        assert_eq!(
            progress.first(),
            Some(&DownloadProgress {
                downloaded: 0,
                total: Some(binary.len() as u64),
            })
        );
        assert_eq!(
            progress.last(),
            Some(&DownloadProgress {
                downloaded: binary.len() as u64,
                total: Some(binary.len() as u64),
            })
        );
        assert!(
            progress
                .windows(2)
                .all(|pair| pair[0].downloaded <= pair[1].downloaded)
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
