//! Check for and install newer builds.
//!
//! Source of truth: GitHub Releases on `Q1CHENL/mach`. Preferred install is
//! the release binary via `install.sh` into `~/.local/bin`. Manual only —
//! nothing runs unless the user asks (`/update` or `mach update`).

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

/// Repo used for release checks and install.
pub const REPO: &str = "Q1CHENL/mach";
pub const GIT_URL: &str = "https://github.com/Q1CHENL/mach";
const RELEASES_URL: &str = "https://api.github.com/repos/Q1CHENL/mach/releases?per_page=30";
const INSTALL_SH: &str = "https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh";
const USER_AGENT: &str = concat!("mach/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    pub newer: bool,
    pub prerelease: bool,
    pub release_url: String,
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
        format!(
            "curl -fsSL {INSTALL_SH} | sh\n\
             # or from source: cargo install --git {GIT_URL} --force"
        )
    }
}

/// Built-in version of this binary.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Ask GitHub what the latest release is and compare to this binary.
///
/// Picks the highest non-draft semver that shares this binary's major line
/// when possible (so a `0.x` Rust build is not told to "upgrade" to an old
/// `1.x` Python tag still marked latest on GitHub).
pub fn check() -> Result<CheckResult, String> {
    let current = current_version().to_string();
    let body = http_get(RELEASES_URL)?;
    let releases: Vec<GhRelease> = serde_json::from_str(&body)
        .map_err(|e| format!("could not parse GitHub release JSON: {e}"))?;

    let candidates: Vec<_> = releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter(|r| !normalize_tag(&r.tag_name).is_empty())
        .collect();

    if candidates.is_empty() {
        return Err("no GitHub releases yet — publish one, or install from git".into());
    }

    let current_major = parse_semver(&current).map(|(m, _, _)| m);
    // Same major only — a 0.x Rust binary must not treat leftover 1.x
    // Python tags as upgrades. No matching release → already "latest".
    if let Some(release) = pick_release_for_major(&candidates, current_major) {
        let latest = normalize_tag(&release.tag_name);
        let newer = is_newer(&latest, &current).unwrap_or(false);
        return Ok(CheckResult {
            current,
            latest,
            newer,
            prerelease: release.prerelease,
            release_url: if release.html_url.is_empty() {
                format!("{GIT_URL}/releases")
            } else {
                release.html_url.clone()
            },
        });
    }

    Ok(CheckResult {
        current: current.clone(),
        latest: current,
        newer: false,
        prerelease: false,
        release_url: format!("{GIT_URL}/releases"),
    })
}

/// Highest non-draft release whose major matches `current_major`.
fn pick_release_for_major(
    releases: &[GhRelease],
    current_major: Option<u64>,
) -> Option<&GhRelease> {
    let major = current_major?;
    let mut same: Vec<&GhRelease> = releases
        .iter()
        .filter(|r| parse_semver(&r.tag_name).is_some_and(|(m, _, _)| m == major))
        .collect();
    same.sort_by_key(|r| std::cmp::Reverse(parse_semver(&r.tag_name)));
    same.into_iter().next()
}

/// Re-run `install.sh` (release binary → `~/.local/bin`).
pub fn install() -> Result<(), String> {
    let status = Command::new("sh")
        .args(["-c", &format!("curl -fsSL {INSTALL_SH} | sh")])
        .status()
        .map_err(|e| format!("could not run install.sh: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("install.sh failed (exit {status})"))
    }
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
}

fn http_get(url: &str) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(map_ureq_err)?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read response: {e}"))
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

/// Strip a leading `v` and whitespace.
pub fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches('v').trim().to_string()
}

/// True when `latest` is a higher semver than `current` (major.minor.patch).
pub fn is_newer(latest: &str, current: &str) -> Option<bool> {
    let a = parse_semver(latest)?;
    let b = parse_semver(current)?;
    Some(a > b)
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = normalize_tag(s);
    // Drop pre-release / build metadata: 1.2.3-rc.1+meta → 1.2.3
    let core = s.split(['-', '+']).next().unwrap_or(&s);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn install_hint_mentions_curl_and_cargo() {
        let r = CheckResult {
            current: "0.1.0".into(),
            latest: "0.1.0".into(),
            newer: false,
            prerelease: false,
            release_url: String::new(),
        };
        let h = r.install_hint();
        assert!(h.contains("install.sh"));
        assert!(h.contains("curl -fsSL"));
        assert!(h.contains("cargo install --git"));
    }
}
