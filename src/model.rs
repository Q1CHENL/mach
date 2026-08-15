//! Task and category types for the local todo store.
//!
//! SQLite is the current on-disk format. [`SCHEMA_VERSION`] identifies only
//! the legacy JSON envelopes accepted by the one-time importer. "All Tasks"
//! is never stored — it is a UI view over every task.

use caseless::Caseless;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use unicode_normalization::UnicodeNormalization;

/// Legacy `tasks.json` / `categories.json` envelope schema accepted on import.
pub const SCHEMA_VERSION: u32 = 3;

pub const MAX_TASK_COUNT: usize = 1024;
pub const MAX_IMPORTANCE: u8 = 3;
pub const MAX_TITLE_LEN: usize = 256;
pub const MAX_NOTES_LINE_LEN: usize = 512;
pub const MAX_DESCRIPTION_LINES: usize = 256;
pub const MAX_CATEGORY_NAME_LEN: usize = 64;
pub const MAX_CATEGORY_DESC_LINE_LEN: usize = 256;
pub const MAX_CATEGORY_DESC_LINES: usize = 64;
pub const MAX_CATEGORY_COUNT: usize = 128;
pub const MAX_LABEL_COUNT: usize = 128;
pub const MAX_LABELS_PER_TASK: usize = 32;
pub const MAX_LABEL_NAME_LEN: usize = 64;
pub const MAX_LABEL_COLOR_LEN: usize = 6;

/// Byte budget paired with a user-visible grapheme limit. This keeps a single
/// grapheme with pathological combining sequences from bypassing every text
/// length boundary while leaving generous room for emoji ZWJ sequences.
pub const fn text_byte_limit(max_graphemes: usize) -> usize {
    let scaled = max_graphemes.saturating_mul(16);
    if scaled < 256 { 256 } else { scaled }
}

/// Stable Unicode compatibility-normalized, default-case-folded text used by
/// every caseless identity and search path.
pub fn caseless_key(value: &str) -> String {
    value.nfkc().default_case_fold().nfkc().collect()
}

/// Whether `haystack` contains an already-normalized caseless search key.
pub fn caseless_contains(haystack: &str, folded_needle: &str) -> bool {
    if folded_needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && folded_needle.is_ascii() {
        return haystack
            .as_bytes()
            .windows(folded_needle.len())
            .any(|window| window.eq_ignore_ascii_case(folded_needle.as_bytes()));
    }
    caseless_key(haystack).contains(folded_needle)
}

/// Sentinel category id for the "All Tasks" view. Not written to disk.
pub const ALL_CATEGORY: &str = "";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable identity.
    pub id: String,
    pub title: String,
    #[serde(default, alias = "body")]
    pub description: Vec<Block>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub due: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub importance: u8,
    /// Real category uuid, or `None` if uncategorized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// Stable label identities in the store's canonical label order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label_ids: Vec<String>,
}

impl Task {
    pub fn new(title: &str, importance: u8, category_id: Option<String>, due: &str) -> Self {
        Self {
            id: new_uuid(),
            title: title.to_string(),
            description: Vec::new(),
            due: due.to_string(),
            created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            done: false,
            importance: importance.min(MAX_IMPORTANCE),
            category_id,
            label_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    Todo {
        #[serde(default)]
        text: String,
        #[serde(default)]
        done: bool,
    },
    Bullet {
        #[serde(default)]
        text: String,
    },
    Number {
        #[serde(default)]
        text: String,
    },
    /// A URL (or any link target) the user can open elsewhere.
    Link {
        #[serde(default)]
        url: String,
    },
    Image {
        /// Immutable content-addressed attachment ID. During form editing this
        /// field may temporarily hold a source path; the store imports and
        /// rewrites it before persistence.
        #[serde(alias = "path")]
        attachment_id: String,
    },
}

impl Block {
    pub fn text(text: &str) -> Self {
        Self::Text {
            text: text.to_string(),
        }
    }

    pub fn todo(text: &str, done: bool) -> Self {
        Self::Todo {
            text: text.to_string(),
            done,
        }
    }

    pub fn bullet(text: &str) -> Self {
        Self::Bullet {
            text: text.to_string(),
        }
    }

    pub fn number(text: &str) -> Self {
        Self::Number {
            text: text.to_string(),
        }
    }

    pub fn link(url: &str) -> Self {
        Self::Link {
            url: url.to_string(),
        }
    }

    pub fn image(reference: &str) -> Self {
        Self::Image {
            attachment_id: reference.to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text { text }
            | Self::Todo { text, .. }
            | Self::Bullet { text }
            | Self::Number { text }
            | Self::Link { url: text } => text.trim().is_empty(),
            Self::Image { .. } => false,
        }
    }
}

/// Whether a task title or textual description block contains a caseless key.
/// Image attachment identities are storage metadata, not searchable task text.
pub fn task_text_contains(task: &Task, folded_query: &str) -> bool {
    caseless_contains(&task.title, folded_query)
        || task.description.iter().any(|block| match block {
            Block::Text { text }
            | Block::Todo { text, .. }
            | Block::Bullet { text }
            | Block::Number { text }
            | Block::Link { url: text } => caseless_contains(text, folded_query),
            Block::Image { .. } => false,
        })
}

/// Whether a task's searchable text or assigned label names contain a caseless key.
pub(crate) fn task_matches_query(task: &Task, labels: &[Label], folded_query: &str) -> bool {
    task_text_contains(task, folded_query)
        || labels_for_task(task, labels).any(|label| caseless_contains(&label.name, folded_query))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl Category {
    pub fn new(name: &str) -> Self {
        Self {
            id: new_uuid(),
            name: name.to_string(),
            description: String::new(),
        }
    }

    /// In-memory only: the sidebar's "All Tasks" row.
    pub fn all_tasks() -> Self {
        Self {
            id: ALL_CATEGORY.to_string(),
            name: "All tasks".to_string(),
            description: String::new(),
        }
    }

    pub fn is_all(&self) -> bool {
        self.id == ALL_CATEGORY
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelColor {
    #[default]
    Red,
    Orange,
    Yellow,
    Lime,
    Green,
    Teal,
    Cyan,
    Blue,
    Indigo,
    Purple,
    Pink,
    Brown,
}

impl LabelColor {
    /// Colors persisted by current databases and archives.
    pub const ALL: [Self; 12] = [
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Lime,
        Self::Green,
        Self::Teal,
        Self::Cyan,
        Self::Blue,
        Self::Indigo,
        Self::Purple,
        Self::Pink,
        Self::Brown,
    ];

    /// Colors offered for new labels and explicit color selection.
    pub const SWATCHES: [Self; 12] = Self::ALL;

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Lime => "lime",
            Self::Green => "green",
            Self::Teal => "teal",
            Self::Cyan => "cyan",
            Self::Blue => "blue",
            Self::Indigo => "indigo",
            Self::Purple => "purple",
            Self::Pink => "pink",
            Self::Brown => "brown",
        }
    }

    pub fn automatic(position: usize) -> Self {
        Self::SWATCHES[position % Self::SWATCHES.len()]
    }

    pub fn least_used(labels: &[Label]) -> Self {
        let counts =
            Self::SWATCHES.map(|color| labels.iter().filter(|label| label.color == color).count());
        Self::SWATCHES
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|(position, _)| (counts[*position], *position))
            .map(|(_, color)| color)
            .unwrap_or_default()
    }
}

impl fmt::Display for LabelColor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LabelColor {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|color| color.as_str() == value)
            .ok_or_else(|| format!("unknown label color {value:?}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: LabelColor,
}

impl Label {
    pub fn new(name: &str, color: LabelColor) -> Self {
        Self {
            id: new_uuid(),
            name: name.to_string(),
            color,
        }
    }
}

/// Labels assigned to a task, in the store's canonical label order.
pub fn labels_for_task<'a>(task: &Task, labels: &'a [Label]) -> impl Iterator<Item = &'a Label> {
    labels
        .iter()
        .filter(|label| task.label_ids.contains(&label.id))
}

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn importance_marks(importance: u8) -> String {
    "⚑".repeat(importance.min(MAX_IMPORTANCE) as usize)
}

pub const fn next_importance(importance: u8) -> u8 {
    (importance + 1) % (MAX_IMPORTANCE + 1)
}

/// Unicode-normalized, case-folded identity for category names.
pub fn category_name_key(value: &str) -> String {
    caseless_key(value.trim())
}

/// Unicode-normalized, case-folded identity for label names.
pub fn label_name_key(value: &str) -> String {
    caseless_key(value.trim())
}

pub fn todo_progress(task: &Task) -> Option<(usize, usize)> {
    let mut done = 0usize;
    let mut total = 0usize;
    for b in &task.description {
        if let Block::Todo { done: d, .. } = b {
            total += 1;
            if *d {
                done += 1;
            }
        }
    }
    (total > 0).then_some((done, total))
}

pub fn has_prose_or_image(task: &Task) -> bool {
    task.description
        .iter()
        .any(|b| !matches!(b, Block::Todo { .. }) && !b.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{LabelColor, MAX_LABEL_COLOR_LEN};

    #[test]
    fn label_colors_have_stable_storage_names_and_order() {
        let names = LabelColor::ALL
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "red", "orange", "yellow", "lime", "green", "teal", "cyan", "blue", "indigo",
                "purple", "pink", "brown",
            ]
        );
        assert_eq!(
            names.iter().map(|name| name.len()).max(),
            Some(MAX_LABEL_COLOR_LEN)
        );
        for color in LabelColor::ALL {
            assert_eq!(color.to_string().parse::<LabelColor>().unwrap(), color);
        }
        assert!("violet".parse::<LabelColor>().is_err());
        assert!("gray".parse::<LabelColor>().is_err());
        assert_eq!(
            serde_json::to_string(&LabelColor::Brown).unwrap(),
            r#""brown""#
        );
        assert_eq!(
            LabelColor::SWATCHES,
            [
                LabelColor::Red,
                LabelColor::Orange,
                LabelColor::Yellow,
                LabelColor::Lime,
                LabelColor::Green,
                LabelColor::Teal,
                LabelColor::Cyan,
                LabelColor::Blue,
                LabelColor::Indigo,
                LabelColor::Purple,
                LabelColor::Pink,
                LabelColor::Brown,
            ]
        );
        assert_eq!(
            LabelColor::automatic(LabelColor::SWATCHES.len()),
            LabelColor::Red
        );
    }
}
