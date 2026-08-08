//! The mach wordmark and the text shown in the help overlay.

pub const BANNER: [&str; 6] = [
    r"███╗   ███╗ █████╗  ██████╗██╗  ██╗",
    r"████╗ ████║██╔══██╗██╔════╝██║  ██║",
    r"██╔████╔██║███████║██║     ███████║",
    r"██║╚██╔╝██║██╔══██║██║     ██╔══██║",
    r"██║ ╚═╝ ██║██║  ██║╚██████╗██║  ██║",
    r"╚═╝     ╚═╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝",
];

pub const BANNER_WIDTH: u16 = 35;

/// Highlights bundled into the binary for the one-time post-upgrade screen.
/// Update these alongside the package version when preparing a release.
pub(crate) const WHATS_NEW: [(&str, &str); 3] = [
    (
        "Automatic updates",
        "Daily checks; /update installs the latest release.",
    ),
    (
        "Verified downloads",
        "SHA-256 is checked before the binary is replaced.",
    ),
    (
        "Visible progress",
        "The command row becomes a full-width download bar.",
    ),
];

/// One row of the two-column key reference. Section headings are marked
/// rather than guessed from their casing — "COMMANDS (press /)" has
/// lowercase in it and would not read as a heading otherwise.
pub struct HelpRow {
    pub left: &'static str,
    pub right: &'static str,
    pub heading: bool,
}

const fn row(left: &'static str, right: &'static str) -> HelpRow {
    HelpRow {
        left,
        right,
        heading: false,
    }
}

const fn heading(left: &'static str, right: &'static str) -> HelpRow {
    HelpRow {
        left,
        right,
        heading: true,
    }
}

/// Two-column key reference. Empty strings are gaps.
pub const HELP_COLUMNS: [HelpRow; 16] = [
    heading("MOVING AROUND", "TASKS & CATEGORIES"),
    row(
        "← →          between the two panels",
        "Ctrl+A       new task / category",
    ),
    row(
        "↑ ↓          within a panel",
        "Enter        edit in the preview",
    ),
    row("⌥↑ ⌥↓       reorder in manual view", ""),
    row("PgUp PgDn    top / bottom", "Space        tick a task off"),
    row(
        "Tab          the other panel",
        "Ctrl+F       importance, 0 to 3 flags",
    ),
    row("type         jump to matching row", "Backspace ×2 delete"),
    row("Esc          back out", "Ctrl+C ×2    quit"),
    row("Mouse        click, double-click, scroll", ""),
    row("", ""),
    heading(
        "COMMANDS  (press /)",
        "PREVIEW  (Enter · when space allows)",
    ),
    row(
        "/search  /settings  /help",
        "Tab ⇧Tab     next / previous field",
    ),
    row("", "← → Space    choose category / flags"),
    row(
        "/copy  /copytitle · /done",
        "Enter        calendar · new line · open",
    ),
    row("/purge  /update  /quit", "Ctrl+Z / ⇧Z  undo · redo"),
    row("? this page", "Esc          back to the task list"),
];

pub const HELP_FOOTER: &str = "Press Esc to close · github.com/Q1CHENL/mach";

pub const EMPTY_TASKS: &str = "No active tasks here :)";
pub const NO_SEARCH_RESULTS: &str = "No tasks found :)";
