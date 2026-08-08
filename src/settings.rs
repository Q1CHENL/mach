//! User settings persisted by [`crate::store::Store`].

use serde::{Deserialize, Serialize};

pub const THEMES: [&str; 7] = ["purple", "cyan", "blue", "red", "yellow", "green", "white"];
pub const DATE_FORMATS: [&str; 3] = ["Y-M-D", "D-M-Y", "M-D-Y"];
/// Where the task preview / docked editor sits relative to the list.
pub const PREVIEW_POSITIONS: [&str; 2] = ["bottom", "right"];

/// How tasks are ordered **inside** each category. All Tasks always stacks
/// categories in sidebar order; this only rearranges rows within a group.
pub const SORTS: [&str; 4] = ["manual", "important", "done", "due"];

/// What the settings panel calls each sort.
pub fn sort_label(sort: &str) -> &'static str {
    match sort {
        "important" => "Most important first",
        "done" => "Done last",
        "due" => "By due date",
        // "manual", legacy "category", and anything unknown: persisted order.
        _ => "As added",
    }
}

/// Display name for a theme id (`"purple"` → `"Purple"`).
pub fn theme_label(color: &str) -> String {
    let mut chars = color.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Settings label for preview placement.
pub fn preview_position_label(pos: &str) -> &'static str {
    match pos {
        "right" => "Right (bottom if narrow)",
        _ => "Bottom (hidden if narrow)",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_date_format")]
    pub date_format: String,
    #[serde(default = "default_color")]
    pub selected_color: String,
    #[serde(default = "default_sort")]
    pub sort: String,
    /// `"bottom"` under the task list, or `"right"` beside it (wide terminals).
    #[serde(default = "default_preview_position")]
    pub preview_position: String,
    /// When true, completed tasks stay on disk but leave the list until
    /// `/done` shows them again.
    #[serde(default)]
    pub hide_done: bool,
    #[serde(default)]
    pub last_run_version: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            date_format: default_date_format(),
            selected_color: default_color(),
            sort: default_sort(),
            preview_position: default_preview_position(),
            hide_done: false,
            last_run_version: None,
        }
    }
}

impl Settings {
    /// Normalize values imported from the legacy JSON settings file.
    ///
    /// SQLite-backed settings are validated on every write and therefore do
    /// not need this compatibility path.
    pub fn normalized(mut self) -> Self {
        if !THEMES.contains(&self.selected_color.as_str()) {
            self.selected_color = default_color();
        }
        if !DATE_FORMATS.contains(&self.date_format.as_str()) {
            self.date_format = default_date_format();
        }
        // Legacy "category" meant All-Tasks grouping; that is now always on,
        // so map it to as-added within each group.
        if self.sort == "category" || !SORTS.contains(&self.sort.as_str()) {
            self.sort = default_sort();
        }
        if !PREVIEW_POSITIONS.contains(&self.preview_position.as_str()) {
            self.preview_position = default_preview_position();
        }
        self
    }

    /// Record `version` in memory and report whether this was the first run.
    /// The caller persists the changed settings through [`crate::store::Store`].
    pub fn take_first_run(&mut self, version: &str) -> bool {
        let first = self.last_run_version.is_none();
        if self.last_run_version.as_deref() != Some(version) {
            self.last_run_version = Some(version.to_string());
        }
        first
    }
}

fn default_date_format() -> String {
    "Y-M-D".to_string()
}

fn default_color() -> String {
    "white".to_string()
}

fn default_sort() -> String {
    "manual".to_string()
}

fn default_preview_position() -> String {
    "bottom".to_string()
}

/// Step a string setting by `delta` (±1) in a list, wrapping around.
pub fn cycle_by(values: &[&str], current: &str, delta: isize) -> String {
    if values.is_empty() {
        return current.to_string();
    }
    let idx = values.iter().position(|v| *v == current).unwrap_or(0) as isize;
    let n = values.len() as isize;
    let next = (idx + delta).rem_euclid(n) as usize;
    values[next].to_string()
}
