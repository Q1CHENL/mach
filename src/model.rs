//! Task and category types for the local todo store.
//!
//! On-disk format uses a versioned envelope (`schema` = [`SCHEMA_VERSION`]).
//! "All Tasks" is never stored — it is a UI view over every task.

use chrono::Local;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 3;

pub const MAX_TASK_COUNT: usize = 1024;
pub const MAX_IMPORTANCE: u8 = 3;
pub const MAX_TITLE_LEN: usize = 256;
pub const MAX_NOTES_LINE_LEN: usize = 512;
pub const MAX_BODY_LINES: usize = 256;
pub const MAX_CATEGORY_NAME_LEN: usize = 64;
pub const MAX_CATEGORY_DESC_LINE_LEN: usize = 256;
pub const MAX_CATEGORY_DESC_LINES: usize = 64;
pub const MAX_CATEGORY_COUNT: usize = 128;

/// Sentinel category id for the "All Tasks" view. Not written to disk.
pub const ALL_CATEGORY: &str = "";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Stable identity.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: Vec<Block>,
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
}

impl Task {
    pub fn new(title: &str, importance: u8, category_id: Option<String>, due: &str) -> Self {
        Self {
            id: new_uuid(),
            title: title.to_string(),
            body: Vec::new(),
            due: due.to_string(),
            created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            done: false,
            importance: importance.min(MAX_IMPORTANCE),
            category_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        path: String,
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

    pub fn image(path: &str) -> Self {
        Self::Image {
            path: path.to_string(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        self.id.is_empty() || self.id == ALL_CATEGORY
    }
}

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn importance_marks(importance: u8) -> String {
    "⚑".repeat(importance.min(MAX_IMPORTANCE) as usize)
}

pub fn todo_progress(task: &Task) -> Option<(usize, usize)> {
    let mut done = 0usize;
    let mut total = 0usize;
    for b in &task.body {
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
    task.body
        .iter()
        .any(|b| !matches!(b, Block::Todo { .. }) && !b.is_empty())
}
