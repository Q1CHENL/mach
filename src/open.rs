//! Open URLs in the system browser (or the platform handler).

use std::process::{Command, Stdio};

/// Launch `url` with the OS default handler (`open` / `xdg-open` / `start`).
pub fn open_url(raw: &str) -> Result<(), String> {
    let url = normalize_url(raw).ok_or_else(|| "empty or unsupported link".to_string())?;
    let mut cmd = platform_command(&url);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open {url}: {e}"))?;

    // Dropping a running `Child` does not reap it on Unix. Keep the UI
    // non-blocking while ensuring the short-lived launcher cannot become a
    // zombie process.
    std::thread::Builder::new()
        .name("mach-open-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map(|_| ())
        .map_err(|e| format!("could not monitor link opener: {e}"))
}

/// Add a scheme when the user typed a bare host (`example.com`).
pub fn normalize_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() {
        return None;
    }
    if url.chars().any(char::is_control) {
        return None;
    }
    if has_supported_scheme(url) {
        return Some(url.to_string());
    }
    if has_explicit_scheme(url) {
        return None;
    }
    Some(format!("https://{url}"))
}

pub(crate) fn has_supported_scheme(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("file:")
}

fn has_explicit_scheme(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return false;
    };
    let scheme = &url[..colon];
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }

    // A bare hostname may carry a numeric port. It is not a URI scheme even
    // though both syntaxes use a colon (`localhost:3000/tasks`).
    let port = url[colon + 1..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    port.is_empty() || !port.chars().all(|c| c.is_ascii_digit())
}

fn platform_command(url: &str) -> Command {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        // `--` so a URL that happens to start with `-` is not read as a flag.
        c.args(["--", url]);
        c
    } else if cfg!(target_os = "windows") {
        // rundll32 takes the URL as one argument, unlike `cmd /C start`,
        // where `&` in a URL would end the command and run what follows.
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        // Linux and other Unix: prefer xdg-open.
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_an_existing_scheme() {
        assert_eq!(
            normalize_url("https://x.ai/foo"),
            Some("https://x.ai/foo".into())
        );
        assert_eq!(normalize_url("mailto:a@b.c"), Some("mailto:a@b.c".into()));
        assert_eq!(
            normalize_url("file:///tmp/task.txt"),
            Some("file:///tmp/task.txt".into())
        );
    }

    #[test]
    fn adds_https_to_a_bare_host() {
        assert_eq!(
            normalize_url("example.com/path"),
            Some("https://example.com/path".into())
        );
    }

    #[test]
    fn rejects_blank() {
        assert_eq!(normalize_url("  "), None);
    }

    #[test]
    fn rejects_terminal_controls_and_unapproved_handlers() {
        assert_eq!(normalize_url("https://example.com/\u{1b}[31m"), None);
        assert_eq!(normalize_url("javascript://alert"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("data:text/html,hello"), None);
        assert_eq!(normalize_url("ssh://example.com"), None);
    }

    #[test]
    fn treats_a_bare_host_with_a_port_as_https() {
        assert_eq!(
            normalize_url("localhost:3000/tasks"),
            Some("https://localhost:3000/tasks".into())
        );
    }
}
