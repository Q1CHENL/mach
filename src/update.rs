//! Check for and install newer builds.
//!
//! Source of truth: GitHub Releases on `Q1CHENL/mach`. Fresh installs use the
//! release installer; self-updates download and verify the exact release asset
//! directly. Manual only — nothing runs unless the user asks (`/update` or
//! `mach update`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub destination: PathBuf,
    pub tag: String,
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
    env!("CARGO_PKG_VERSION")
}

/// Ask GitHub what the latest release is and compare to this binary.
///
/// Picks the highest stable semver release that ships both this platform's
/// binary and its checksum manifest. Requiring those assets excludes the
/// disconnected legacy Python releases without blocking legitimate majors.
pub fn check() -> Result<CheckResult, String> {
    let current = current_version().to_string();
    let body = http_get(RELEASES_URL)?;
    let releases: Vec<GhRelease> = serde_json::from_str(&body)
        .map_err(|e| format!("could not parse GitHub release JSON: {e}"))?;
    let asset_name = current_asset_name()?;
    let selected = select_release(&releases, &asset_name).ok_or_else(|| {
        format!("no stable GitHub release ships both {asset_name} and {CHECKSUMS_ASSET}")
    })?;
    let latest = selected.version.to_string();
    let newer = selected.version
        > Version::parse(&current)
            .map_err(|e| format!("invalid current version {current:?}: {e}"))?;

    Ok(CheckResult {
        current,
        latest,
        tag: selected.tag,
        newer,
        prerelease: false,
        release_url: selected.release_url,
        asset_name,
        asset_url: selected.asset_url,
        checksums_url: selected.checksums_url,
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
/// The binary is downloaded and verified in-process. No downloaded script is
/// executed. The replacement is written, synced, chmodded, and atomically
/// renamed within the destination directory before that directory is synced.
pub fn install(info: &CheckResult) -> Result<InstallResult, String> {
    validate_install_info(info)?;
    let destination = install_destination()?;
    let manifest = http_get_text(
        &info.checksums_url,
        DOWNLOAD_TIMEOUT,
        "application/octet-stream",
        map_download_err,
    )
    .map_err(|e| format!("could not download checksums for {}: {e}", info.tag))?;
    let expected_sha = checksum_for_asset(&manifest, &info.asset_name)?;
    download_verified_binary(&info.asset_url, &expected_sha, &destination)?;
    Ok(InstallResult {
        destination,
        tag: info.tag.clone(),
    })
}

fn validate_install_info(info: &CheckResult) -> Result<(), String> {
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
    if selected_version != latest || !latest.pre.is_empty() || info.latest != latest.to_string() {
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
    Ok(())
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
    let cargo_bin = cargo_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".cargo"))
        .join("bin");
    if current_exe.and_then(Path::parent) == Some(cargo_bin.as_path()) {
        return Err("this mach executable is managed by Cargo; update it with \
             'cargo install --locked mach-tui', or set MACH_INSTALL_DIR to install a release \
             binary elsewhere"
            .into());
    }
    Ok(home.join(".local/bin/mach"))
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
) -> Result<(), String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(map_download_err)?;
    write_verified_binary(response.body_mut().as_reader(), expected_sha, destination)
}

fn write_verified_binary<R: Read>(
    mut source: R,
    expected_sha: &str,
    destination: &Path,
) -> Result<(), String> {
    #[cfg(not(unix))]
    return Err("self-update is supported only on Unix platforms".into());

    #[cfg(unix)]
    {
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

        let write_result = (|| -> Result<(), String> {
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = source
                    .read(&mut buffer)
                    .map_err(|e| format!("could not read release binary: {e}"))?;
                if read == 0 {
                    break;
                }
                total = total
                    .checked_add(read as u64)
                    .ok_or_else(|| "release binary is too large".to_string())?;
                if total > MAX_BINARY_BYTES {
                    return Err(format!(
                        "release binary exceeds the {} MiB safety limit",
                        MAX_BINARY_BYTES / 1024 / 1024
                    ));
                }
                hasher.update(&buffer[..read]);
                temp_file
                    .write_all(&buffer[..read])
                    .map_err(|e| format!("could not write temporary binary: {e}"))?;
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
            Ok(())
        })();
        drop(temp_file);

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp_path, destination) {
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "could not replace {} atomically: {error}",
                destination.display()
            ));
        }
        parent_dir
            .sync_all()
            .map_err(|e| format!("could not sync install directory {}: {e}", parent.display()))?;
        Ok(())
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

fn http_get(url: &str) -> Result<String, String> {
    http_get_text(url, TIMEOUT, "application/vnd.github+json", map_ureq_err)
}

fn http_get_text(
    url: &str,
    timeout: Duration,
    accept: &str,
    map_error: fn(ureq::Error) -> String,
) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept)
        .call()
        .map_err(map_error)?;
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
    let version = Version::parse(normalized).ok()?;
    if !version.pre.is_empty() || !version.build.is_empty() || normalized != version.to_string() {
        return None;
    }
    Some(version)
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
    tag.trim()
        .strip_prefix('v')
        .unwrap_or_else(|| tag.trim())
        .to_string()
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
    fn cargo_managed_binary_requires_cargo_or_an_explicit_release_destination() {
        let home = Path::new("/home/alice");
        let cargo_home = home.join(".cargo");
        let current_exe = cargo_home.join("bin/mach");

        let error = resolve_install_destination(None, Some(home), Some(&current_exe), None)
            .expect_err("a Cargo-managed executable must not create a shadow release install");
        assert!(error.contains("Cargo"));
        assert!(error.contains("cargo install --locked mach-tui"));

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

        write_verified_binary(std::io::Cursor::new(binary), &digest, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), binary);
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
}
