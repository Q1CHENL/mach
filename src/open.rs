//! Open URLs in the system browser (or the platform handler).

use std::process::{Command, Stdio};

/// Launch `url` with the OS default handler (`open` / `xdg-open` / `start`).
pub fn open_url(raw: &str) -> Result<(), String> {
    let url = normalize_url(raw).ok_or_else(|| "empty link".to_string())?;
    let mut cmd = platform_command(&url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open {url}: {e}"))
}

/// Add a scheme when the user typed a bare host (`example.com`).
pub fn normalize_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("file:")
        || lower.contains("://")
    {
        return Some(url.to_string());
    }
    Some(format!("https://{url}"))
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
}
