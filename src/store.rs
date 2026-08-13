//! SQLite-backed persistence for tasks, categories, and settings.
//!
//! A [`Store`] is an explicit instance: callers can open independent data
//! directories in one process. Writes use `BEGIN IMMEDIATE`, validate a fresh
//! snapshot, and commit tasks/categories/settings/attachment metadata plus a
//! monotonic revision in one transaction. Image bytes are immutable,
//! content-addressed files beside the database. Legacy JSON is imported once
//! and left untouched.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::due;
use crate::model::{
    Block, Category, Label, LabelColor, MAX_CATEGORY_COUNT, MAX_CATEGORY_DESC_LINE_LEN,
    MAX_CATEGORY_DESC_LINES, MAX_CATEGORY_NAME_LEN, MAX_DESCRIPTION_LINES, MAX_IMPORTANCE,
    MAX_LABEL_COUNT, MAX_LABEL_NAME_LEN, MAX_LABELS_PER_TASK, MAX_NOTES_LINE_LEN, MAX_TASK_COUNT,
    MAX_TITLE_LEN, SCHEMA_VERSION, Task, category_name_key, label_name_key, text_byte_limit,
};
use crate::settings::{DATE_FORMATS, PREVIEW_POSITIONS, SORTS, Settings, THEMES};

const DATABASE_FILE: &str = "mach.db";
const DATABASE_SCHEMA_VERSION: i64 = 3;
const LEGACY_MIGRATION_KEY: &str = "legacy_json_migrated";
pub(crate) const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const ID_MAX_BYTES: usize = 128;
pub(crate) const DUE_MAX_BYTES: usize = 128;
pub(crate) const CREATED_MAX_BYTES: usize = 64;
const SETTINGS_VALUE_MAX_BYTES: usize = 128;
const MAX_LEGACY_JSON_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SQLITE_VALUE_BYTES: i32 = 8 * 1024 * 1024;
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const ATTACHMENT_ID_LEN: usize = 64;

#[derive(Debug)]
pub enum StoreError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    Database(rusqlite::Error),
    UnsupportedLegacySchema {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    UnsupportedDatabaseSchema {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    Conflict {
        expected: u64,
        actual: u64,
    },
    MetadataConflict {
        entity: &'static str,
        name: String,
        field: &'static str,
    },
    StaleEntity {
        entity: &'static str,
        id: String,
    },
    NotFound {
        entity: &'static str,
        query: String,
    },
    Ambiguous {
        entity: &'static str,
        query: String,
        matches: Vec<String>,
    },
    Validation(String),
    Corrupt(String),
}

impl StoreError {
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    pub(crate) fn io(operation: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} {}: {source}", path.display()),
            Self::Json { path, source } => {
                write!(f, "could not parse {}: {source}", path.display())
            }
            Self::Database(source) => write!(f, "database error: {source}"),
            Self::UnsupportedLegacySchema {
                path,
                found,
                expected,
            } => write!(
                f,
                "{} uses unsupported schema {found} (expected {expected})",
                path.display()
            ),
            Self::UnsupportedDatabaseSchema {
                path,
                found,
                expected,
            } => write!(
                f,
                "{} uses unsupported database schema {found} (expected {expected})",
                path.display()
            ),
            Self::Conflict { expected, actual } => write!(
                f,
                "store changed since it was loaded (expected revision {expected}, found {actual})"
            ),
            Self::MetadataConflict {
                entity,
                name,
                field,
            } => write!(
                f,
                "{entity} {name:?} already exists with a different {field}"
            ),
            Self::StaleEntity { entity, id } => {
                write!(f, "{entity} {id:?} changed since it was loaded")
            }
            Self::NotFound { entity, query } => {
                write!(f, "no {entity} matching {query:?}")
            }
            Self::Ambiguous {
                entity,
                query,
                matches,
            } => write!(
                f,
                "ambiguous {entity} {query:?}; matches: {}",
                matches.join(", ")
            ),
            Self::Validation(message) => write!(f, "invalid data: {message}"),
            Self::Corrupt(message) => write!(f, "corrupt database: {message}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::Database(source) => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StoreData {
    pub revision: u64,
    pub categories: Vec<Category>,
    pub labels: Vec<Label>,
    pub tasks: Vec<Task>,
    pub settings: Settings,
    pub(crate) attachments: Vec<Attachment>,
}

/// Immutable metadata for one content-addressed image owned by this store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub sha256: String,
    pub media_type: String,
    pub byte_len: u64,
    pub storage_name: String,
}

/// Verified attachment bytes staged by a caller for installation inside the
/// same write transaction as their task references.
#[derive(Debug, Clone)]
pub(crate) struct StagedAttachment {
    pub metadata: Attachment,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<Vec<Block>>,
    pub due: Option<String>,
    pub done: Option<bool>,
    pub importance: Option<u8>,
    /// `None` leaves the category unchanged; `Some(None)` clears it.
    pub category_id: Option<Option<String>>,
    /// `None` leaves labels unchanged; values are normalized into store order.
    pub label_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct CategoryPatch {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LabelPatch {
    pub name: Option<String>,
    pub color: Option<LabelColor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeScope {
    All,
    Category(String),
    Uncategorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativePosition {
    Before,
    After,
}

impl StoreData {
    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    /// Resolve a task by full id or unique id prefix.
    pub fn resolve_task_id(&self, query: &str) -> Result<String, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(StoreError::validation("task id cannot be empty"));
        }
        validate_byte_limit(query, ID_MAX_BYTES, "task id query")?;
        if let Some(task) = self.tasks.iter().find(|task| task.id == query) {
            return Ok(task.id.clone());
        }
        let matches: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.id.starts_with(query))
            .map(|task| task.id.clone())
            .collect();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(StoreError::NotFound {
                entity: "task",
                query: query.to_string(),
            }),
            _ => Err(StoreError::Ambiguous {
                entity: "task id",
                query: query.to_string(),
                matches,
            }),
        }
    }

    /// Resolve a category by id, Unicode-caseless name, or unique name prefix.
    pub fn resolve_category_id(&self, query: &str) -> Result<String, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(StoreError::validation("category name cannot be empty"));
        }
        if let Some(category) = self.categories.iter().find(|category| category.id == query) {
            return Ok(category.id.clone());
        }
        validate_byte_limit(
            query,
            text_byte_limit(MAX_CATEGORY_NAME_LEN),
            "category query",
        )?;
        let folded = category_name_key(query);
        if let Some(category) = self
            .categories
            .iter()
            .find(|category| category_name_key(&category.name) == folded)
        {
            return Ok(category.id.clone());
        }
        let matches: Vec<_> = self
            .categories
            .iter()
            .filter(|category| category_name_has_prefix(&category.name, &folded))
            .collect();
        match matches.as_slice() {
            [category] => Ok(category.id.clone()),
            [] => Err(StoreError::NotFound {
                entity: "category",
                query: query.to_string(),
            }),
            _ => Err(StoreError::Ambiguous {
                entity: "category",
                query: query.to_string(),
                matches: matches
                    .into_iter()
                    .map(|category| category.name.clone())
                    .collect(),
            }),
        }
    }

    /// Resolve a label by id, Unicode-caseless name, or unique name prefix.
    pub fn resolve_label_id(&self, query: &str) -> Result<String, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(StoreError::validation("label name cannot be empty"));
        }
        if let Some(label) = self.labels.iter().find(|label| label.id == query) {
            return Ok(label.id.clone());
        }
        validate_byte_limit(query, text_byte_limit(MAX_LABEL_NAME_LEN), "label query")?;
        let folded = label_name_key(query);
        if let Some(label) = self
            .labels
            .iter()
            .find(|label| label_name_key(&label.name) == folded)
        {
            return Ok(label.id.clone());
        }
        let matches: Vec<_> = self
            .labels
            .iter()
            .filter(|label| label_name_has_prefix(&label.name, &folded))
            .collect();
        match matches.as_slice() {
            [label] => Ok(label.id.clone()),
            [] => Err(StoreError::NotFound {
                entity: "label",
                query: query.to_string(),
            }),
            _ => Err(StoreError::Ambiguous {
                entity: "label",
                query: query.to_string(),
                matches: matches
                    .into_iter()
                    .map(|label| label.name.clone())
                    .collect(),
            }),
        }
    }

    pub fn task(&self, id: &str) -> Result<&Task, StoreError> {
        self.task_index(id).map(|index| &self.tasks[index])
    }

    fn task_index(&self, id: &str) -> Result<usize, StoreError> {
        self.tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| StoreError::NotFound {
                entity: "task",
                query: id.to_string(),
            })
    }

    pub fn category(&self, id: &str) -> Result<&Category, StoreError> {
        self.category_index(id).map(|index| &self.categories[index])
    }

    pub fn label(&self, id: &str) -> Result<&Label, StoreError> {
        self.label_index(id).map(|index| &self.labels[index])
    }

    fn label_index(&self, id: &str) -> Result<usize, StoreError> {
        self.labels
            .iter()
            .position(|label| label.id == id)
            .ok_or_else(|| StoreError::NotFound {
                entity: "label",
                query: id.to_string(),
            })
    }

    fn category_index(&self, id: &str) -> Result<usize, StoreError> {
        self.categories
            .iter()
            .position(|category| category.id == id)
            .ok_or_else(|| StoreError::NotFound {
                entity: "category",
                query: id.to_string(),
            })
    }

    pub fn create_task(
        &mut self,
        title: impl Into<String>,
        description: Vec<Block>,
        due: impl Into<String>,
        importance: u8,
        category_id: Option<String>,
    ) -> Result<Task, StoreError> {
        if importance > MAX_IMPORTANCE {
            return Err(StoreError::validation(format!(
                "importance must be 0-{MAX_IMPORTANCE}"
            )));
        }
        let title = title.into();
        let due = due.into();
        let mut task = Task::new(&title, importance, category_id, &due);
        task.description = description;
        self.insert_task(task)
    }

    pub fn insert_task(&mut self, task: Task) -> Result<Task, StoreError> {
        let index = self.tasks.len();
        self.tasks.push(task);
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.tasks.truncate(index);
            return Err(error);
        }
        Ok(self.tasks[index].clone())
    }

    pub fn edit_task(&mut self, id: &str, patch: TaskPatch) -> Result<Task, StoreError> {
        let index = self.task_index(id)?;
        let before = self.tasks[index].clone();
        {
            let task = &mut self.tasks[index];
            if let Some(title) = patch.title {
                task.title = title;
            }
            if let Some(description) = patch.description {
                task.description = description;
            }
            if let Some(due) = patch.due {
                task.due = due;
            }
            if let Some(done) = patch.done {
                task.done = done;
            }
            if let Some(importance) = patch.importance {
                task.importance = importance;
            }
            if let Some(category_id) = patch.category_id {
                task.category_id = category_id;
            }
            if let Some(label_ids) = patch.label_ids {
                task.label_ids = label_ids;
            }
        }
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.tasks[index] = before;
            return Err(error);
        }
        Ok(self.tasks[index].clone())
    }

    /// Apply only the fields represented by `patch`, but fail if one of those
    /// fields changed since `expected` was loaded. Unrelated concurrent edits
    /// (for example toggling `done` while a title form is open) are preserved.
    pub fn edit_task_if_unchanged(
        &mut self,
        expected: &Task,
        patch: TaskPatch,
    ) -> Result<Task, StoreError> {
        let current = self
            .tasks
            .iter()
            .find(|task| task.id == expected.id)
            .ok_or_else(|| StoreError::StaleEntity {
                entity: "task",
                id: expected.id.clone(),
            })?;
        let stale = field_conflicts(patch.title.as_ref(), &current.title, &expected.title)
            || field_conflicts(
                patch.description.as_ref(),
                &current.description,
                &expected.description,
            )
            || field_conflicts(patch.due.as_ref(), &current.due, &expected.due)
            || field_conflicts(patch.done.as_ref(), &current.done, &expected.done)
            || field_conflicts(
                patch.importance.as_ref(),
                &current.importance,
                &expected.importance,
            )
            || field_conflicts(
                patch.category_id.as_ref(),
                &current.category_id,
                &expected.category_id,
            )
            || field_conflicts(
                patch.label_ids.as_ref(),
                &current.label_ids,
                &expected.label_ids,
            );
        if stale {
            return Err(StoreError::StaleEntity {
                entity: "task",
                id: expected.id.clone(),
            });
        }
        self.edit_task(&expected.id, patch)
    }

    pub fn delete_task(&mut self, id: &str) -> Result<Task, StoreError> {
        let index = self.task_index(id)?;
        Ok(self.tasks.remove(index))
    }

    pub fn set_task_done(&mut self, id: &str, done: bool) -> Result<Task, StoreError> {
        self.edit_task(
            id,
            TaskPatch {
                done: Some(done),
                ..TaskPatch::default()
            },
        )
    }

    pub fn toggle_task_done(&mut self, id: &str) -> Result<Task, StoreError> {
        let done = !self.task(id)?.done;
        self.set_task_done(id, done)
    }

    pub fn set_task_importance(&mut self, id: &str, importance: u8) -> Result<Task, StoreError> {
        self.edit_task(
            id,
            TaskPatch {
                importance: Some(importance),
                ..TaskPatch::default()
            },
        )
    }

    pub fn set_task_category(
        &mut self,
        id: &str,
        category_id: Option<String>,
    ) -> Result<Task, StoreError> {
        self.edit_task(
            id,
            TaskPatch {
                category_id: Some(category_id),
                ..TaskPatch::default()
            },
        )
    }

    pub fn set_task_labels(
        &mut self,
        id: &str,
        label_ids: Vec<String>,
    ) -> Result<Task, StoreError> {
        self.edit_task(
            id,
            TaskPatch {
                label_ids: Some(label_ids),
                ..TaskPatch::default()
            },
        )
    }

    pub fn move_task(&mut self, id: &str, target: usize) -> Result<(), StoreError> {
        let index = self.task_index(id)?;
        if target >= self.tasks.len() {
            return Err(StoreError::validation(format!(
                "task target index {target} is out of range"
            )));
        }
        if self.tasks[index].category_id != self.tasks[target].category_id {
            return Err(StoreError::validation(
                "tasks can only be reordered within the same category",
            ));
        }
        let task = self.tasks.remove(index);
        self.tasks.insert(target, task);
        Ok(())
    }

    pub fn move_task_relative(
        &mut self,
        id: &str,
        target_id: &str,
        position: RelativePosition,
    ) -> Result<Task, StoreError> {
        if id == target_id {
            return Err(StoreError::validation(
                "cannot move a task relative to itself",
            ));
        }
        let index = self.task_index(id)?;
        let target_index = self.task_index(target_id)?;
        if self.tasks[index].category_id != self.tasks[target_index].category_id {
            return Err(StoreError::validation(
                "tasks can only be reordered within the same category",
            ));
        }
        let task = self.tasks.remove(index);
        let target_after_removal = target_index - usize::from(index < target_index);
        let insertion = match position {
            RelativePosition::Before => target_after_removal,
            RelativePosition::After => target_after_removal + 1,
        };
        self.tasks.insert(insertion, task.clone());
        Ok(task)
    }

    pub fn purge_completed(&mut self, scope: &PurgeScope) -> Result<Vec<Task>, StoreError> {
        if let PurgeScope::Category(id) = scope {
            self.category(id)?;
        }
        Ok(self.remove_tasks(|task| {
            let in_scope = match scope {
                PurgeScope::All => true,
                PurgeScope::Category(id) => task.category_id.as_deref() == Some(id),
                PurgeScope::Uncategorized => task.category_id.is_none(),
            };
            task.done && in_scope
        }))
    }

    /// Purge only the completed tasks captured by a confirmation prompt.
    pub fn purge_completed_ids(&mut self, ids: &[String]) -> Result<Vec<Task>, StoreError> {
        let ids: HashSet<_> = ids.iter().map(String::as_str).collect();
        Ok(self.remove_tasks(|task| task.done && ids.contains(task.id.as_str())))
    }

    fn remove_tasks(&mut self, mut should_remove: impl FnMut(&Task) -> bool) -> Vec<Task> {
        let mut removed = Vec::new();
        self.tasks.retain(|task| {
            let remove = should_remove(task);
            if remove {
                removed.push(task.clone());
            }
            !remove
        });
        removed
    }

    pub fn create_category(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Category, StoreError> {
        let name = name.into();
        let mut category = Category::new(&name);
        category.description = description.into();
        self.insert_category(category)
    }

    /// Return the category with this exact name identity, or create it.
    /// Supplied metadata is an assertion and never edits an existing category.
    pub fn ensure_category(
        &mut self,
        name: impl Into<String>,
        description: Option<String>,
    ) -> Result<(Category, bool), StoreError> {
        let name = name.into();
        let name_key = category_name_key(&name);
        if let Some(category) = self
            .categories
            .iter()
            .find(|category| category_name_key(&category.name) == name_key)
        {
            if description
                .as_ref()
                .is_some_and(|description| description != &category.description)
            {
                return Err(StoreError::MetadataConflict {
                    entity: "category",
                    name: category.name.clone(),
                    field: "description",
                });
            }
            return Ok((category.clone(), false));
        }

        let category = self.create_category(name.trim(), description.unwrap_or_default())?;
        Ok((category, true))
    }

    pub fn insert_category(&mut self, category: Category) -> Result<Category, StoreError> {
        let index = self.categories.len();
        self.categories.push(category);
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.categories.truncate(index);
            return Err(error);
        }
        Ok(self.categories[index].clone())
    }

    pub fn edit_category(
        &mut self,
        id: &str,
        patch: CategoryPatch,
    ) -> Result<Category, StoreError> {
        let index = self.category_index(id)?;
        let before = self.categories[index].clone();
        {
            let category = &mut self.categories[index];
            if let Some(name) = patch.name {
                category.name = name;
            }
            if let Some(description) = patch.description {
                category.description = description;
            }
        }
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.categories[index] = before;
            return Err(error);
        }
        Ok(self.categories[index].clone())
    }

    pub fn edit_category_if_unchanged(
        &mut self,
        expected: &Category,
        patch: CategoryPatch,
    ) -> Result<Category, StoreError> {
        let current = self
            .categories
            .iter()
            .find(|category| category.id == expected.id)
            .ok_or_else(|| StoreError::StaleEntity {
                entity: "category",
                id: expected.id.clone(),
            })?;
        let stale = field_conflicts(patch.name.as_ref(), &current.name, &expected.name)
            || field_conflicts(
                patch.description.as_ref(),
                &current.description,
                &expected.description,
            );
        if stale {
            return Err(StoreError::StaleEntity {
                entity: "category",
                id: expected.id.clone(),
            });
        }
        self.edit_category(&expected.id, patch)
    }

    /// Delete a category while preserving its tasks as uncategorized.
    pub fn delete_category(&mut self, id: &str) -> Result<Category, StoreError> {
        let index = self.category_index(id)?;
        let category = self.categories.remove(index);
        for task in &mut self.tasks {
            if task.category_id.as_deref() == Some(id) {
                task.category_id = None;
            }
        }
        Ok(category)
    }

    pub fn create_label(&mut self, name: impl Into<String>) -> Result<Label, StoreError> {
        let color = LabelColor::least_used(&self.labels);
        self.create_label_with_color(name, color)
    }

    /// Return the label with this exact name identity, or create it.
    /// A supplied color is an assertion and never recolors an existing label.
    pub fn ensure_label(
        &mut self,
        name: impl Into<String>,
        color: Option<LabelColor>,
    ) -> Result<(Label, bool), StoreError> {
        let name = name.into();
        let name_key = label_name_key(&name);
        if let Some(label) = self
            .labels
            .iter()
            .find(|label| label_name_key(&label.name) == name_key)
        {
            if color.is_some_and(|color| color != label.color) {
                return Err(StoreError::MetadataConflict {
                    entity: "label",
                    name: label.name.clone(),
                    field: "color",
                });
            }
            return Ok((label.clone(), false));
        }

        let label = match color {
            Some(color) => self.create_label_with_color(name, color)?,
            None => self.create_label(name)?,
        };
        Ok((label, true))
    }

    pub fn create_label_with_color(
        &mut self,
        name: impl Into<String>,
        color: LabelColor,
    ) -> Result<Label, StoreError> {
        let name = name.into();
        self.insert_label(Label::new(&name, color))
    }

    pub fn insert_label(&mut self, label: Label) -> Result<Label, StoreError> {
        let index = self.labels.len();
        self.labels.push(label);
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.labels.truncate(index);
            return Err(error);
        }
        Ok(self.labels[index].clone())
    }

    pub fn edit_label(&mut self, id: &str, patch: LabelPatch) -> Result<Label, StoreError> {
        let index = self.label_index(id)?;
        let before = self.labels[index].clone();
        if let Some(name) = patch.name {
            self.labels[index].name = name;
        }
        if let Some(color) = patch.color {
            self.labels[index].color = color;
        }
        if let Err(error) = self.normalize_and_validate_new_write() {
            self.labels[index] = before;
            return Err(error);
        }
        Ok(self.labels[index].clone())
    }

    /// Delete a global label while preserving every task that used it.
    pub fn delete_label(&mut self, id: &str) -> Result<Label, StoreError> {
        let index = self.label_index(id)?;
        let label = self.labels.remove(index);
        for task in &mut self.tasks {
            task.label_ids.retain(|label_id| label_id != id);
        }
        Ok(label)
    }

    pub fn move_category(&mut self, id: &str, target: usize) -> Result<(), StoreError> {
        let index = self.category_index(id)?;
        if target >= self.categories.len() {
            return Err(StoreError::validation(format!(
                "category target index {target} is out of range"
            )));
        }
        let category = self.categories.remove(index);
        self.categories.insert(target, category);
        Ok(())
    }

    pub fn move_category_relative(
        &mut self,
        id: &str,
        target_id: &str,
        position: RelativePosition,
    ) -> Result<Category, StoreError> {
        if id == target_id {
            return Err(StoreError::validation(
                "cannot move a category relative to itself",
            ));
        }
        let index = self.category_index(id)?;
        let target_index = self.category_index(target_id)?;
        let category = self.categories.remove(index);
        let target_after_removal = target_index - usize::from(index < target_index);
        let insertion = match position {
            RelativePosition::Before => target_after_removal,
            RelativePosition::After => target_after_removal + 1,
        };
        self.categories.insert(insertion, category.clone());
        Ok(category)
    }

    pub fn replace_settings(&mut self, settings: Settings) -> Result<Settings, StoreError> {
        self.update_settings(move |current| *current = settings)
    }

    pub fn update_settings(
        &mut self,
        operation: impl FnOnce(&mut Settings),
    ) -> Result<Settings, StoreError> {
        let before = self.settings.clone();
        operation(&mut self.settings);
        if let Err(error) = validate_settings(&self.settings) {
            self.settings = before;
            return Err(error);
        }
        Ok(self.settings.clone())
    }

    pub(crate) fn validate_as_stored(&mut self) -> Result<(), StoreError> {
        normalize_and_validate(
            self,
            Local::now().naive_local(),
            DueMode::Stored,
            AttachmentMode::Persisted,
        )
    }

    fn normalize_and_validate_new_write(&mut self) -> Result<(), StoreError> {
        normalize_and_validate(
            self,
            Local::now().naive_local(),
            DueMode::NewWrite,
            AttachmentMode::Draft,
        )
    }
}

/// A three-way field merge conflicts only when the remote and desired values
/// both diverged from the captured base in different directions.
fn field_conflicts<T: PartialEq>(desired: Option<&T>, current: &T, expected: &T) -> bool {
    desired.is_some_and(|desired| current != expected && current != desired)
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub database: PathBuf,
    pub tasks: PathBuf,
    pub categories: PathBuf,
    pub settings: PathBuf,
    pub images: PathBuf,
}

impl Paths {
    fn new(dir: PathBuf) -> Self {
        Self {
            database: dir.join(DATABASE_FILE),
            tasks: dir.join("tasks.json"),
            categories: dir.join("categories.json"),
            settings: dir.join("settings.json"),
            images: dir.join("images"),
            dir,
        }
    }
}

pub struct Store {
    connection: Connection,
    paths: Paths,
    persistent_attachments: bool,
}

enum TransactionOutcome<R> {
    Changed(R),
    Unchanged(R),
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let paths = Paths::new(expand_user(dir.as_ref().to_path_buf())?);
        ensure_private_directory(&paths.dir)?;
        prepare_private_database_file(&paths.database)?;
        let mut connection = Connection::open(&paths.database)?;
        set_private_file(&paths.database)?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        configure_resource_limits(&connection)?;
        initialize_schema(&mut connection, &paths.database)?;
        configure_connection(&connection)?;
        sqlite_quick_check(&connection, "")?;
        let mut store = Self {
            connection,
            paths,
            persistent_attachments: true,
        };
        store.migrate_legacy_json()?;
        store.reconcile_attachment_storage()?;
        Ok(store)
    }

    /// Open an ephemeral store with no filesystem persistence.
    ///
    /// `data_dir` is only the logical base for relative image references. The
    /// directory is not created or modified.
    pub fn open_in_memory_with_paths(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let paths = Paths::new(expand_user(data_dir.as_ref().to_path_buf())?);
        let mut connection = Connection::open_in_memory()?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        configure_resource_limits(&connection)?;
        initialize_schema(&mut connection, Path::new(":memory:"))?;
        configure_in_memory_connection(&connection)?;
        sqlite_quick_check(&connection, "")?;
        connection.execute(
            "INSERT INTO metadata(key, value) VALUES (?1, '1')",
            [LEGACY_MIGRATION_KEY],
        )?;
        Ok(Self {
            connection,
            paths,
            persistent_attachments: false,
        })
    }

    pub fn open_default(explicit: Option<PathBuf>) -> Result<Self, StoreError> {
        Self::open(resolve_data_dir(explicit)?)
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn data_dir(&self) -> &Path {
        &self.paths.dir
    }

    pub fn images_dir(&self) -> &Path {
        &self.paths.images
    }

    pub fn database_path(&self) -> &Path {
        &self.paths.database
    }

    /// Cheap external-change probe for a long-running TUI.
    pub fn revision(&self) -> Result<u64, StoreError> {
        read_revision(&self.connection)
    }

    pub fn snapshot(&self) -> Result<StoreData, StoreError> {
        let tx = self.connection.unchecked_transaction()?;
        let data = load_snapshot(&tx)?;
        tx.commit()?;
        Ok(data)
    }

    pub fn load_settings(&self) -> Result<Settings, StoreError> {
        Ok(self.snapshot()?.settings)
    }

    /// Atomically return an exact-name category or create it. Finding an
    /// existing match does not write the store or advance its revision.
    pub fn ensure_category(
        &mut self,
        name: impl Into<String>,
        description: Option<String>,
    ) -> Result<(Category, bool), StoreError> {
        let name = name.into();
        self.update_inner_outcome(None, &[], |data| {
            let result = data.ensure_category(name, description)?;
            Ok(if result.1 {
                TransactionOutcome::Changed(result)
            } else {
                TransactionOutcome::Unchanged(result)
            })
        })
        .map(|(result, _)| result)
    }

    /// Atomically return an exact-name label or create it. Finding an existing
    /// match does not write the store or advance its revision.
    pub fn ensure_label(
        &mut self,
        name: impl Into<String>,
        color: Option<LabelColor>,
    ) -> Result<(Label, bool), StoreError> {
        let name = name.into();
        self.update_inner_outcome(None, &[], |data| {
            let result = data.ensure_label(name, color)?;
            Ok(if result.1 {
                TransactionOutcome::Changed(result)
            } else {
                TransactionOutcome::Unchanged(result)
            })
        })
        .map(|(result, _)| result)
    }

    pub fn save_settings(&mut self, settings: &Settings) -> Result<(), StoreError> {
        self.update(|data| {
            data.replace_settings(settings.clone())?;
            Ok(())
        })
    }

    /// Run a read-modify-write against a fresh snapshot under
    /// `BEGIN IMMEDIATE`. Every successful call increments `revision` once.
    pub fn update<R>(
        &mut self,
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.update_with_snapshot(operation)
            .map(|(result, _)| result)
    }

    /// Commit a mutation and return the exact normalized snapshot that was
    /// persisted, including its new revision.
    pub fn update_with_snapshot<R>(
        &mut self,
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<(R, StoreData), StoreError> {
        self.update_inner(None, &[], operation)
    }

    /// Apply a mutation only if the caller's snapshot is still current.
    pub fn update_if_revision<R>(
        &mut self,
        expected_revision: u64,
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.update_if_revision_with_snapshot(expected_revision, operation)
            .map(|(result, _)| result)
    }

    pub fn update_if_revision_with_snapshot<R>(
        &mut self,
        expected_revision: u64,
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<(R, StoreData), StoreError> {
        self.update_inner(Some(expected_revision), &[], operation)
    }

    pub(crate) fn update_if_revision_with_staged_attachments<R>(
        &mut self,
        expected_revision: u64,
        staged_attachments: &[StagedAttachment],
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<(R, StoreData), StoreError> {
        self.update_inner(Some(expected_revision), staged_attachments, operation)
    }

    fn update_inner<R>(
        &mut self,
        expected_revision: Option<u64>,
        staged_attachments: &[StagedAttachment],
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<(R, StoreData), StoreError> {
        self.update_inner_outcome(expected_revision, staged_attachments, |data| {
            operation(data).map(TransactionOutcome::Changed)
        })
    }

    fn update_inner_outcome<R>(
        &mut self,
        expected_revision: Option<u64>,
        staged_attachments: &[StagedAttachment],
        operation: impl FnOnce(&mut StoreData) -> Result<TransactionOutcome<R>, StoreError>,
    ) -> Result<(R, StoreData), StoreError> {
        let images_root = self
            .persistent_attachments
            .then(|| self.paths.images.clone());
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut installed_attachments = Vec::new();
        let prepared = (|| {
            let before = load_snapshot(&tx)?;
            let base_revision = before.revision;
            if let Some(expected) = expected_revision
                && expected != base_revision
            {
                return Err(StoreError::Conflict {
                    expected,
                    actual: base_revision,
                });
            }
            let mut data = before.clone();
            match operation(&mut data)? {
                TransactionOutcome::Unchanged(result) => Ok((result, before)),
                TransactionOutcome::Changed(result) => {
                    import_task_description_attachments(
                        &mut data,
                        images_root.as_deref(),
                        staged_attachments,
                        &mut installed_attachments,
                    )?;
                    prune_unreferenced_attachments(&mut data);
                    normalize_and_validate(
                        &mut data,
                        Local::now().naive_local(),
                        DueMode::NewWrite,
                        AttachmentMode::Persisted,
                    )?;
                    let next_revision = base_revision
                        .checked_add(1)
                        .ok_or_else(|| StoreError::Corrupt("revision overflow".into()))?;
                    data.revision = next_revision;
                    persist_diff(&tx, &before, &data)?;
                    Ok((result, data))
                }
            }
        })();

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = remove_installed_attachment_files(&installed_attachments);
                let rollback = tx.rollback();
                cleanup?;
                rollback?;
                return Err(error);
            }
        };

        if let Err(error) = tx.commit() {
            if let Some(images_root) = images_root.as_deref() {
                cleanup_attachments_after_failed_commit(
                    &mut self.connection,
                    images_root,
                    &installed_attachments,
                )?;
            }
            return Err(error.into());
        }
        let _ = self.cleanup_pending_attachments();
        Ok(prepared)
    }

    fn migrate_legacy_json(&mut self) -> Result<(), StoreError> {
        let images_root = self.paths.images.clone();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if migration_complete(&tx)? {
            tx.commit()?;
            return Ok(());
        }
        let mut installed_attachments = Vec::new();
        let prepared = (|| {
            let existing = load_snapshot(&tx)?;
            if !existing.categories.is_empty()
                || !existing.tasks.is_empty()
                || !existing.attachments.is_empty()
                || existing.revision != 0
            {
                return Err(StoreError::Corrupt(
                    "database contains data but has no completed legacy migration marker".into(),
                ));
            }

            let categories_file = read_optional_json::<CategoriesFile>(&self.paths.categories)?;
            let tasks_file = read_optional_json::<TasksFile>(&self.paths.tasks)?;
            let settings = read_optional_json::<Settings>(&self.paths.settings)?;
            validate_legacy_schema(
                &self.paths.categories,
                categories_file.as_ref().map(|file| file.schema),
            )?;
            validate_legacy_schema(
                &self.paths.tasks,
                tasks_file.as_ref().map(|file| file.schema),
            )?;

            let has_legacy =
                categories_file.is_some() || tasks_file.is_some() || settings.is_some();
            if has_legacy {
                let mut data = StoreData {
                    revision: 1,
                    categories: categories_file
                        .map(|file| {
                            file.categories
                                .into_iter()
                                .filter(|category| !category.is_all())
                                .collect()
                        })
                        .unwrap_or_default(),
                    labels: Vec::new(),
                    tasks: tasks_file.map(|file| file.tasks).unwrap_or_default(),
                    settings: settings.unwrap_or_default().normalized(),
                    attachments: Vec::new(),
                };
                import_task_description_attachments(
                    &mut data,
                    Some(&images_root),
                    &[],
                    &mut installed_attachments,
                )?;
                prune_unreferenced_attachments(&mut data);
                // Legacy relative values used the reader's current date/year. Freeze
                // that interpretation now so it cannot drift after migration.
                normalize_and_validate(
                    &mut data,
                    Local::now().naive_local(),
                    DueMode::LegacyMigration,
                    AttachmentMode::Persisted,
                )?;
                persist_diff(&tx, &existing, &data)?;
            }
            tx.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, '1')",
                [LEGACY_MIGRATION_KEY],
            )?;
            Ok(())
        })();

        if let Err(error) = prepared {
            let cleanup = remove_installed_attachment_files(&installed_attachments);
            let rollback = tx.rollback();
            cleanup?;
            rollback?;
            return Err(error);
        }
        if let Err(error) = tx.commit() {
            cleanup_attachments_after_failed_commit(
                &mut self.connection,
                &images_root,
                &installed_attachments,
            )?;
            return Err(error.into());
        }
        Ok(())
    }

    fn cleanup_pending_attachments(&mut self) -> Result<(), StoreError> {
        if self.persistent_attachments {
            cleanup_pending_attachment_files(&mut self.connection, &self.paths.images)?;
        }
        Ok(())
    }

    fn reconcile_attachment_storage(&mut self) -> Result<(), StoreError> {
        if self.persistent_attachments {
            reconcile_attachment_files(&mut self.connection, &self.paths.images)?;
        }
        Ok(())
    }
}

pub fn resolve_data_dir(explicit: Option<PathBuf>) -> Result<PathBuf, StoreError> {
    resolve_data_dir_from(
        explicit,
        std::env::var_os("MACH_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn resolve_data_dir_from(
    explicit: Option<PathBuf>,
    configured: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, StoreError> {
    if let Some(dir) = explicit.or(configured) {
        return expand_user_with_home(dir, home.as_deref());
    }
    home.map(|home| home.join(".mach")).ok_or_else(|| {
        StoreError::validation("could not determine the home directory; use --dir or set MACH_DIR")
    })
}

fn expand_user(path: PathBuf) -> Result<PathBuf, StoreError> {
    let home = dirs::home_dir();
    expand_user_with_home(path, home.as_deref())
}

fn expand_user_with_home(path: PathBuf, home: Option<&Path>) -> Result<PathBuf, StoreError> {
    if path.as_os_str().is_empty() {
        return Err(StoreError::validation("data directory cannot be empty"));
    }
    let text = path.to_string_lossy();
    if text == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| StoreError::validation("cannot expand ~ without a home directory"));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home
            .map(|home| home.join(rest))
            .ok_or_else(|| StoreError::validation("cannot expand ~ without a home directory"));
    }
    Ok(path)
}

pub(crate) fn configure_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Corrupt(format!(
            "SQLite refused WAL mode (using {mode})"
        )));
    }
    Ok(())
}

pub(crate) fn configure_resource_limits(connection: &Connection) -> Result<(), StoreError> {
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
    Ok(())
}

fn configure_in_memory_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "MEMORY")?;
    Ok(())
}

fn initialize_schema(connection: &mut Connection, path: &Path) -> Result<(), StoreError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if !matches!(version, 0 | 1 | 2 | DATABASE_SCHEMA_VERSION) {
        return Err(StoreError::UnsupportedDatabaseSchema {
            path: path.to_path_buf(),
            found: version,
            expected: DATABASE_SCHEMA_VERSION,
        });
    }

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if version == 1 {
        tx.execute_batch(
            "ALTER TABLE tasks RENAME COLUMN body_json TO description_json;
             DROP INDEX IF EXISTS task_attachments_by_attachment;
             ALTER TABLE task_attachments RENAME TO task_description_attachments;
             CREATE INDEX task_description_attachments_by_attachment
                 ON task_description_attachments(attachment_id);",
        )?;
    }
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS app_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            revision INTEGER NOT NULL CHECK (revision >= 0),
            settings_json TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS categories (
            id TEXT PRIMARY KEY,
            position INTEGER NOT NULL UNIQUE CHECK (position >= 0),
            name TEXT NOT NULL CHECK (length(trim(name)) > 0),
            name_key TEXT NOT NULL UNIQUE CHECK (length(name_key) > 0),
            description TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            position INTEGER NOT NULL UNIQUE CHECK (position >= 0),
            title TEXT NOT NULL CHECK (length(trim(title)) > 0),
            description_json TEXT NOT NULL,
            due TEXT NOT NULL,
            created TEXT NOT NULL,
            done INTEGER NOT NULL CHECK (done IN (0, 1)),
            importance INTEGER NOT NULL CHECK (importance BETWEEN 0 AND 3),
            category_id TEXT REFERENCES categories(id) ON DELETE SET NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS labels (
            id TEXT PRIMARY KEY,
            position INTEGER NOT NULL UNIQUE CHECK (position >= 0),
            name TEXT NOT NULL CHECK (length(trim(name)) > 0),
            name_key TEXT NOT NULL UNIQUE CHECK (length(name_key) > 0),
            color TEXT NOT NULL DEFAULT 'red'
                CHECK (color IN ('red', 'orange', 'yellow', 'lime', 'green', 'teal', 'cyan', 'blue', 'indigo', 'purple', 'pink', 'brown'))
        ) STRICT;
        CREATE TABLE IF NOT EXISTS task_labels (
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
            position INTEGER NOT NULL CHECK (position >= 0),
            PRIMARY KEY (task_id, label_id),
            UNIQUE (task_id, position)
        ) STRICT;
        CREATE TABLE IF NOT EXISTS attachments (
            id TEXT PRIMARY KEY,
            sha256 TEXT NOT NULL UNIQUE CHECK (sha256 = id),
            media_type TEXT NOT NULL,
            byte_len INTEGER NOT NULL CHECK (byte_len > 0),
            storage_name TEXT NOT NULL UNIQUE
        ) STRICT;
        CREATE TABLE IF NOT EXISTS task_description_attachments (
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            block_index INTEGER NOT NULL CHECK (block_index >= 0),
            attachment_id TEXT NOT NULL REFERENCES attachments(id) ON DELETE RESTRICT,
            PRIMARY KEY (task_id, block_index)
        ) STRICT;
        CREATE TABLE IF NOT EXISTS attachment_gc (
            storage_name TEXT PRIMARY KEY
        ) STRICT;
        CREATE INDEX IF NOT EXISTS task_description_attachments_by_attachment
            ON task_description_attachments(attachment_id);
        CREATE INDEX IF NOT EXISTS task_labels_by_label ON task_labels(label_id);
        ",
    )?;
    let settings = serde_json::to_string(&Settings::default()).map_err(|source| {
        StoreError::Corrupt(format!("could not encode default settings: {source}"))
    })?;
    tx.execute(
        "INSERT OR IGNORE INTO app_state(id, revision, settings_json) VALUES (1, 0, ?1)",
        [settings],
    )?;
    if version != DATABASE_SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn sqlite_quick_check(
    connection: &Connection,
    error_prefix: &str,
) -> Result<(), StoreError> {
    let result: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(StoreError::Corrupt(format!(
            "{error_prefix}SQLite quick check failed: {result}"
        )));
    }
    Ok(())
}

fn migration_complete(connection: &Connection) -> Result<bool, StoreError> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            [LEGACY_MIGRATION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref() == Some("1"))
}

fn read_revision(connection: &Connection) -> Result<u64, StoreError> {
    let value: i64 =
        connection.query_row("SELECT revision FROM app_state WHERE id = 1", [], |row| {
            row.get(0)
        })?;
    u64::try_from(value).map_err(|_| StoreError::Corrupt(format!("negative revision {value}")))
}

fn load_snapshot(connection: &Connection) -> Result<StoreData, StoreError> {
    let (revision, settings_json): (i64, String) = connection.query_row(
        "SELECT revision, settings_json FROM app_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Corrupt(format!("negative revision {revision}")))?;
    let settings: Settings = serde_json::from_str(&settings_json)
        .map_err(|error| StoreError::Corrupt(format!("invalid settings JSON: {error}")))?;

    let mut attachment_statement = connection.prepare(
        "SELECT id, sha256, media_type, byte_len, storage_name FROM attachments ORDER BY id",
    )?;
    let attachment_rows = attachment_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut attachments = Vec::new();
    for row in attachment_rows {
        let (id, sha256, media_type, byte_len, storage_name) = row?;
        attachments.push(Attachment {
            id,
            sha256,
            media_type,
            byte_len: u64::try_from(byte_len).map_err(|_| {
                StoreError::Corrupt(format!("attachment has invalid byte length {byte_len}"))
            })?,
            storage_name,
        });
    }

    let mut categories_statement = connection.prepare(
        "SELECT position, id, name, name_key, description FROM categories ORDER BY position",
    )?;
    let category_rows = categories_statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Category {
                id: row.get(1)?,
                name: row.get(2)?,
                description: row.get(4)?,
            },
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut categories = Vec::new();
    for (expected_position, row) in category_rows.enumerate() {
        if expected_position >= MAX_CATEGORY_COUNT {
            return Err(StoreError::Corrupt(format!(
                "category count exceeds {MAX_CATEGORY_COUNT}"
            )));
        }
        let (stored_position, category, stored_name_key) = row?;
        validate_stored_position(stored_position, expected_position, "category")?;
        let expected_name_key = category_name_key(&category.name);
        if stored_name_key != expected_name_key {
            return Err(StoreError::Corrupt(format!(
                "category {:?} has identity key {stored_name_key:?}, expected {expected_name_key:?}",
                category.id
            )));
        }
        categories.push(category);
    }

    let mut labels_statement = connection
        .prepare("SELECT position, id, name, name_key, color FROM labels ORDER BY position")?;
    let label_rows = labels_statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut labels = Vec::new();
    for (expected_position, row) in label_rows.enumerate() {
        if expected_position >= MAX_LABEL_COUNT {
            return Err(StoreError::Corrupt(format!(
                "label count exceeds {MAX_LABEL_COUNT}"
            )));
        }
        let (stored_position, id, name, stored_name_key, stored_color) = row?;
        validate_stored_position(stored_position, expected_position, "label")?;
        let color = stored_color.parse::<LabelColor>().map_err(|_| {
            StoreError::Corrupt(format!("label {id:?} has unknown color {stored_color:?}"))
        })?;
        let label = Label { id, name, color };
        let expected_name_key = label_name_key(&label.name);
        if stored_name_key != expected_name_key {
            return Err(StoreError::Corrupt(format!(
                "label {:?} has identity key {stored_name_key:?}, expected {expected_name_key:?}",
                label.id
            )));
        }
        labels.push(label);
    }

    let mut tasks_statement = connection.prepare(
        "SELECT position, id, title, description_json, due, created, done, importance, category_id
         FROM tasks ORDER BY position",
    )?;
    let rows = tasks_statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut tasks = Vec::new();
    for (expected_position, row) in rows.enumerate() {
        if expected_position >= MAX_TASK_COUNT {
            return Err(StoreError::Corrupt(format!(
                "task count exceeds {MAX_TASK_COUNT}"
            )));
        }
        let (
            stored_position,
            id,
            title,
            description_json,
            due,
            created,
            done,
            importance,
            category_id,
        ) = row?;
        validate_stored_position(stored_position, expected_position, "task")?;
        let description =
            serde_json::from_str::<Vec<Block>>(&description_json).map_err(|error| {
                StoreError::Corrupt(format!("task {id:?} has invalid description JSON: {error}"))
            })?;
        let importance = u8::try_from(importance).map_err(|_| {
            StoreError::Corrupt(format!("task {id:?} has invalid importance {importance}"))
        })?;
        tasks.push(Task {
            id,
            title,
            description,
            due,
            created,
            done: done != 0,
            importance,
            category_id,
            label_ids: Vec::new(),
        });
    }
    let task_indices: HashMap<_, _> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.clone(), index))
        .collect();
    let mut expected_label_positions: HashMap<String, usize> = HashMap::new();
    let mut task_labels_statement = connection.prepare(
        "SELECT task_id, position, label_id FROM task_labels ORDER BY task_id, position",
    )?;
    let task_label_rows = task_labels_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in task_label_rows {
        let (task_id, stored_position, label_id) = row?;
        let task_index = task_indices.get(&task_id).copied().ok_or_else(|| {
            StoreError::Corrupt(format!(
                "task label assignment refers to unknown task {task_id:?}"
            ))
        })?;
        let expected_position = expected_label_positions.entry(task_id.clone()).or_default();
        validate_stored_position(stored_position, *expected_position, "task label")?;
        *expected_position += 1;
        tasks[task_index].label_ids.push(label_id);
    }
    validate_task_attachment_rows(connection, &tasks)?;
    let mut data = StoreData {
        revision,
        categories,
        labels,
        tasks,
        settings,
        attachments,
    };
    data.validate_as_stored().map_err(|error| match error {
        StoreError::Validation(message) => StoreError::Corrupt(message),
        other => other,
    })?;
    Ok(data)
}

fn validate_task_attachment_rows(
    connection: &Connection,
    tasks: &[Task],
) -> Result<(), StoreError> {
    let expected: HashSet<(String, usize, String)> = tasks
        .iter()
        .flat_map(|task| {
            task.description
                .iter()
                .enumerate()
                .filter_map(|(block_index, block)| match block {
                    Block::Image { attachment_id } => {
                        Some((task.id.clone(), block_index, attachment_id.clone()))
                    }
                    _ => None,
                })
        })
        .collect();
    let mut statement = connection.prepare(
        "SELECT task_id, block_index, attachment_id
         FROM task_description_attachments ORDER BY task_id, block_index",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut stored = HashSet::new();
    for row in rows {
        let (task_id, block_index, attachment_id) = row?;
        let block_index = usize::try_from(block_index).map_err(|_| {
            StoreError::Corrupt(format!(
                "task {task_id:?} has invalid attachment reference index {block_index}"
            ))
        })?;
        stored.insert((task_id, block_index, attachment_id));
    }
    if stored != expected {
        return Err(StoreError::Corrupt(
            "task attachment reference rows do not match task description JSON".into(),
        ));
    }
    Ok(())
}

fn validate_stored_position(stored: i64, expected: usize, entity: &str) -> Result<(), StoreError> {
    let expected = i64::try_from(expected)
        .map_err(|_| StoreError::Corrupt(format!("{entity} position exceeds integer range")))?;
    if stored != expected {
        return Err(StoreError::Corrupt(format!(
            "{entity} position {stored} is not contiguous (expected {expected})"
        )));
    }
    Ok(())
}

/// Persist only rows whose identity, content, or position changed.
///
/// Positions and category names have UNIQUE constraints. Rows that move or
/// change names are first assigned transaction-private values outside the
/// validated application domain, which makes swaps and insertions safe without
/// deleting and recreating unrelated rows.
fn persist_diff(
    tx: &Transaction<'_>,
    before: &StoreData,
    after: &StoreData,
) -> Result<(), StoreError> {
    let before_categories: HashMap<&str, (usize, &Category)> = before
        .categories
        .iter()
        .enumerate()
        .map(|(position, category)| (category.id.as_str(), (position, category)))
        .collect();
    let after_categories: HashMap<&str, (usize, &Category)> = after
        .categories
        .iter()
        .enumerate()
        .map(|(position, category)| (category.id.as_str(), (position, category)))
        .collect();
    let before_labels: HashMap<&str, (usize, &Label)> = before
        .labels
        .iter()
        .enumerate()
        .map(|(position, label)| (label.id.as_str(), (position, label)))
        .collect();
    let after_labels: HashMap<&str, (usize, &Label)> = after
        .labels
        .iter()
        .enumerate()
        .map(|(position, label)| (label.id.as_str(), (position, label)))
        .collect();
    let before_tasks: HashMap<&str, (usize, &Task)> = before
        .tasks
        .iter()
        .enumerate()
        .map(|(position, task)| (task.id.as_str(), (position, task)))
        .collect();
    let after_tasks: HashMap<&str, (usize, &Task)> = after
        .tasks
        .iter()
        .enumerate()
        .map(|(position, task)| (task.id.as_str(), (position, task)))
        .collect();
    let before_attachments: HashMap<&str, &Attachment> = before
        .attachments
        .iter()
        .map(|attachment| (attachment.id.as_str(), attachment))
        .collect();
    let after_attachments: HashMap<&str, &Attachment> = after
        .attachments
        .iter()
        .map(|attachment| (attachment.id.as_str(), attachment))
        .collect();

    for attachment in &before.attachments {
        if after_attachments
            .get(attachment.id.as_str())
            .is_some_and(|current| *current != attachment)
        {
            return Err(StoreError::Validation(format!(
                "attachment {:?} metadata is immutable",
                attachment.id
            )));
        }
    }
    for attachment in &after.attachments {
        if !before_attachments.contains_key(attachment.id.as_str()) {
            tx.execute(
                "INSERT INTO attachments(id, sha256, media_type, byte_len, storage_name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    attachment.id,
                    attachment.sha256,
                    attachment.media_type,
                    sqlite_attachment_size(attachment.byte_len)?,
                    attachment.storage_name,
                ],
            )?;
            tx.execute(
                "DELETE FROM attachment_gc WHERE storage_name = ?1",
                [&attachment.storage_name],
            )?;
        }
    }

    // Remove tasks first so deleting a task and its category does not produce
    // an unnecessary ON DELETE SET NULL update.
    for task in &before.tasks {
        if !after_tasks.contains_key(task.id.as_str()) {
            execute_one(
                tx,
                "DELETE FROM tasks WHERE id = ?1",
                [task.id.as_str()],
                "task",
                &task.id,
            )?;
        }
    }

    // Free every old identity key that may be replaced. Control characters are
    // rejected by validation, so these temporary values cannot collide with
    // application data and are never visible outside this transaction.
    let mut temporary_name_index = 0usize;
    for category in &before.categories {
        let name_changed_or_removed = after_categories
            .get(category.id.as_str())
            .is_none_or(|(_, current)| current.name != category.name);
        if name_changed_or_removed {
            let temporary_name = format!("\u{1f}mach-category-{temporary_name_index}");
            temporary_name_index += 1;
            execute_one(
                tx,
                "UPDATE categories SET name = ?1, name_key = ?1 WHERE id = ?2",
                params![temporary_name, category.id],
                "category",
                &category.id,
            )?;
        }
    }
    let mut temporary_label_name_index = 0usize;
    for label in &before.labels {
        let name_changed_or_removed = after_labels
            .get(label.id.as_str())
            .is_none_or(|(_, current)| current.name != label.name);
        if name_changed_or_removed {
            let temporary_name = format!("\u{1f}mach-label-{temporary_label_name_index}");
            temporary_label_name_index += 1;
            execute_one(
                tx,
                "UPDATE labels SET name = ?1, name_key = ?1 WHERE id = ?2",
                params![temporary_name, label.id],
                "label",
                &label.id,
            )?;
        }
    }

    let category_position_base = before.categories.len().max(after.categories.len());
    let mut category_position_offset = 0usize;
    for (old_position, category) in before.categories.iter().enumerate() {
        if let Some((new_position, _)) = after_categories.get(category.id.as_str()).copied()
            && new_position != old_position
        {
            let temporary = temporary_position(
                category_position_base,
                category_position_offset,
                "categories",
            )?;
            category_position_offset += 1;
            execute_one(
                tx,
                "UPDATE categories SET position = ?1 WHERE id = ?2",
                params![temporary, category.id],
                "category",
                &category.id,
            )?;
        }
    }
    for category in &after.categories {
        if !before_categories.contains_key(category.id.as_str()) {
            let temporary = temporary_position(
                category_position_base,
                category_position_offset,
                "categories",
            )?;
            category_position_offset += 1;
            tx.execute(
                "INSERT INTO categories(id, position, name, name_key, description)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    category.id,
                    temporary,
                    category.name,
                    category_name_key(&category.name),
                    category.description
                ],
            )?;
        }
    }

    let label_position_base = before.labels.len().max(after.labels.len());
    let mut label_position_offset = 0usize;
    for (old_position, label) in before.labels.iter().enumerate() {
        if let Some((new_position, _)) = after_labels.get(label.id.as_str()).copied()
            && new_position != old_position
        {
            let temporary =
                temporary_position(label_position_base, label_position_offset, "labels")?;
            label_position_offset += 1;
            execute_one(
                tx,
                "UPDATE labels SET position = ?1 WHERE id = ?2",
                params![temporary, label.id],
                "label",
                &label.id,
            )?;
        }
    }
    for label in &after.labels {
        if !before_labels.contains_key(label.id.as_str()) {
            let temporary =
                temporary_position(label_position_base, label_position_offset, "labels")?;
            label_position_offset += 1;
            tx.execute(
                "INSERT INTO labels(id, position, name, name_key, color)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    label.id,
                    temporary,
                    label.name,
                    label_name_key(&label.name),
                    label.color.as_str(),
                ],
            )?;
        }
    }

    let task_position_base = before.tasks.len().max(after.tasks.len());
    let mut task_position_offset = 0usize;
    for (old_position, task) in before.tasks.iter().enumerate() {
        if let Some((new_position, _)) = after_tasks.get(task.id.as_str()).copied()
            && new_position != old_position
        {
            let temporary = temporary_position(task_position_base, task_position_offset, "tasks")?;
            task_position_offset += 1;
            execute_one(
                tx,
                "UPDATE tasks SET position = ?1 WHERE id = ?2",
                params![temporary, task.id],
                "task",
                &task.id,
            )?;
        }
    }

    // New categories now exist, so task foreign keys can safely move to them
    // before obsolete categories are deleted.
    for task in &after.tasks {
        if let Some((_, previous)) = before_tasks.get(task.id.as_str()).copied()
            && !task_row_equal(previous, task)
        {
            if previous.description == task.description {
                execute_one(
                    tx,
                    "UPDATE tasks SET
                        title = ?1, due = ?2, created = ?3, done = ?4,
                        importance = ?5, category_id = ?6
                     WHERE id = ?7",
                    params![
                        task.title,
                        task.due,
                        task.created,
                        i64::from(task.done),
                        i64::from(task.importance),
                        task.category_id,
                        task.id,
                    ],
                    "task",
                    &task.id,
                )?;
            } else {
                let description_json = encode_task_description(task)?;
                execute_one(
                    tx,
                    "UPDATE tasks SET
                        title = ?1, description_json = ?2, due = ?3, created = ?4,
                        done = ?5, importance = ?6, category_id = ?7
                     WHERE id = ?8",
                    params![
                        task.title,
                        description_json,
                        task.due,
                        task.created,
                        i64::from(task.done),
                        i64::from(task.importance),
                        task.category_id,
                        task.id,
                    ],
                    "task",
                    &task.id,
                )?;
            }
        }
    }
    for task in &after.tasks {
        if !before_tasks.contains_key(task.id.as_str()) {
            let temporary = temporary_position(task_position_base, task_position_offset, "tasks")?;
            task_position_offset += 1;
            let description_json = encode_task_description(task)?;
            tx.execute(
                "INSERT INTO tasks(
                    id, position, title, description_json, due, created, done, importance, category_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    task.id,
                    temporary,
                    task.title,
                    description_json,
                    task.due,
                    task.created,
                    i64::from(task.done),
                    i64::from(task.importance),
                    task.category_id,
                ],
            )?;
        }
    }

    for task in &after.tasks {
        let description_changed_or_new = before_tasks
            .get(task.id.as_str())
            .is_none_or(|(_, previous)| previous.description != task.description);
        if description_changed_or_new {
            tx.execute(
                "DELETE FROM task_description_attachments WHERE task_id = ?1",
                [&task.id],
            )?;
            insert_task_attachment_rows(tx, task)?;
        }
    }
    for task in &after.tasks {
        let labels_changed_or_new = before_tasks
            .get(task.id.as_str())
            .is_none_or(|(_, previous)| previous.label_ids != task.label_ids);
        if labels_changed_or_new {
            tx.execute("DELETE FROM task_labels WHERE task_id = ?1", [&task.id])?;
            insert_task_label_rows(tx, task)?;
        }
    }

    for attachment in &before.attachments {
        if !after_attachments.contains_key(attachment.id.as_str()) {
            tx.execute(
                "INSERT OR IGNORE INTO attachment_gc(storage_name) VALUES (?1)",
                [&attachment.storage_name],
            )?;
            execute_one(
                tx,
                "DELETE FROM attachments WHERE id = ?1",
                [attachment.id.as_str()],
                "attachment",
                &attachment.id,
            )?;
        }
    }

    for category in &before.categories {
        if !after_categories.contains_key(category.id.as_str()) {
            execute_one(
                tx,
                "DELETE FROM categories WHERE id = ?1",
                [category.id.as_str()],
                "category",
                &category.id,
            )?;
        }
    }

    for label in &before.labels {
        if !after_labels.contains_key(label.id.as_str()) {
            execute_one(
                tx,
                "DELETE FROM labels WHERE id = ?1",
                [label.id.as_str()],
                "label",
                &label.id,
            )?;
        }
    }

    for category in &after.categories {
        if let Some((_, previous)) = before_categories.get(category.id.as_str()).copied()
            && previous != category
        {
            execute_one(
                tx,
                "UPDATE categories
                 SET name = ?1, name_key = ?2, description = ?3
                 WHERE id = ?4",
                params![
                    category.name,
                    category_name_key(&category.name),
                    category.description,
                    category.id
                ],
                "category",
                &category.id,
            )?;
        }
    }
    for label in &after.labels {
        if let Some((_, previous)) = before_labels.get(label.id.as_str()).copied()
            && previous != label
        {
            execute_one(
                tx,
                "UPDATE labels SET name = ?1, name_key = ?2, color = ?3 WHERE id = ?4",
                params![
                    label.name,
                    label_name_key(&label.name),
                    label.color.as_str(),
                    label.id,
                ],
                "label",
                &label.id,
            )?;
        }
    }

    // All rows whose final slots changed are currently at unique temporary
    // positions. Rows omitted here kept the same slot, so final assignment
    // cannot collide with them.
    for (position, category) in after.categories.iter().enumerate() {
        let moved_or_new = before_categories
            .get(category.id.as_str())
            .is_none_or(|(old_position, _)| *old_position != position);
        if moved_or_new {
            let position = sqlite_position(position, "categories")?;
            execute_one(
                tx,
                "UPDATE categories SET position = ?1 WHERE id = ?2",
                params![position, category.id],
                "category",
                &category.id,
            )?;
        }
    }
    for (position, label) in after.labels.iter().enumerate() {
        let moved_or_new = before_labels
            .get(label.id.as_str())
            .is_none_or(|(old_position, _)| *old_position != position);
        if moved_or_new {
            let position = sqlite_position(position, "labels")?;
            execute_one(
                tx,
                "UPDATE labels SET position = ?1 WHERE id = ?2",
                params![position, label.id],
                "label",
                &label.id,
            )?;
        }
    }
    for (position, task) in after.tasks.iter().enumerate() {
        let moved_or_new = before_tasks
            .get(task.id.as_str())
            .is_none_or(|(old_position, _)| *old_position != position);
        if moved_or_new {
            let position = sqlite_position(position, "tasks")?;
            execute_one(
                tx,
                "UPDATE tasks SET position = ?1 WHERE id = ?2",
                params![position, task.id],
                "task",
                &task.id,
            )?;
        }
    }

    let settings = (before.settings != after.settings).then_some(&after.settings);
    persist_app_state(tx, after.revision, settings)?;
    Ok(())
}

fn task_row_equal(left: &Task, right: &Task) -> bool {
    left.id == right.id
        && left.title == right.title
        && left.description == right.description
        && left.due == right.due
        && left.created == right.created
        && left.done == right.done
        && left.importance == right.importance
        && left.category_id == right.category_id
}

fn execute_one<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    sql: &str,
    params: P,
    entity: &str,
    id: &str,
) -> Result<(), StoreError> {
    let changed = tx.execute(sql, params)?;
    if changed != 1 {
        return Err(StoreError::Corrupt(format!(
            "expected to change one {entity} {id:?}, changed {changed}"
        )));
    }
    Ok(())
}

fn temporary_position(base: usize, offset: usize, entity: &str) -> Result<i64, StoreError> {
    let position = base
        .checked_add(offset)
        .ok_or_else(|| StoreError::Validation(format!("too many {entity}")))?;
    sqlite_position(position, entity)
}

fn sqlite_position(position: usize, entity: &str) -> Result<i64, StoreError> {
    i64::try_from(position).map_err(|_| StoreError::Validation(format!("too many {entity}")))
}

fn sqlite_attachment_size(byte_len: u64) -> Result<i64, StoreError> {
    i64::try_from(byte_len)
        .map_err(|_| StoreError::Validation("attachment byte length exceeds integer range".into()))
}

fn insert_task_attachment_rows(tx: &Transaction<'_>, task: &Task) -> Result<(), StoreError> {
    let mut statement = tx.prepare(
        "INSERT INTO task_description_attachments(task_id, block_index, attachment_id)
         VALUES (?1, ?2, ?3)",
    )?;
    for (block_index, block) in task.description.iter().enumerate() {
        let Block::Image { attachment_id } = block else {
            continue;
        };
        statement.execute(params![
            task.id,
            sqlite_position(block_index, "task attachment blocks")?,
            attachment_id,
        ])?;
    }
    Ok(())
}

fn insert_task_label_rows(tx: &Transaction<'_>, task: &Task) -> Result<(), StoreError> {
    let mut statement =
        tx.prepare("INSERT INTO task_labels(task_id, label_id, position) VALUES (?1, ?2, ?3)")?;
    for (position, label_id) in task.label_ids.iter().enumerate() {
        statement.execute(params![
            task.id,
            label_id,
            sqlite_position(position, "task labels")?,
        ])?;
    }
    Ok(())
}

fn encode_task_description(task: &Task) -> Result<String, StoreError> {
    serde_json::to_string(&task.description).map_err(|error| {
        StoreError::Corrupt(format!("could not encode task {:?}: {error}", task.id))
    })
}

fn persist_app_state(
    tx: &Transaction<'_>,
    revision: u64,
    settings: Option<&Settings>,
) -> Result<(), StoreError> {
    let revision = i64::try_from(revision)
        .map_err(|_| StoreError::Corrupt("revision exceeds SQLite integer range".into()))?;
    let changed = if let Some(settings) = settings {
        let settings_json = serde_json::to_string(settings)
            .map_err(|error| StoreError::Corrupt(format!("could not encode settings: {error}")))?;
        tx.execute(
            "UPDATE app_state SET revision = ?1, settings_json = ?2 WHERE id = 1",
            params![revision, settings_json],
        )?
    } else {
        tx.execute(
            "UPDATE app_state SET revision = ?1 WHERE id = 1",
            [revision],
        )?
    };
    if changed != 1 {
        return Err(StoreError::Corrupt(format!(
            "expected to update app state, changed {changed} rows"
        )));
    }
    Ok(())
}

fn import_task_description_attachments(
    data: &mut StoreData,
    images_root: Option<&Path>,
    staged_attachments: &[StagedAttachment],
    installed_attachments: &mut Vec<InstalledAttachmentFile>,
) -> Result<(), StoreError> {
    let mut known: HashMap<String, usize> = data
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| (attachment.id.clone(), index))
        .collect();
    let mut staged_by_id = HashMap::with_capacity(staged_attachments.len());
    for staged in staged_attachments {
        if staged_by_id
            .insert(staged.metadata.id.as_str(), staged)
            .is_some()
        {
            return Err(StoreError::Validation(format!(
                "staged attachment {:?} is duplicated",
                staged.metadata.id
            )));
        }
    }

    for task in &mut data.tasks {
        let mut owner = None;
        for block in &mut task.description {
            let Block::Image { attachment_id } = block else {
                continue;
            };
            if known.contains_key(attachment_id) {
                continue;
            }
            let owner = owner.get_or_insert_with(|| format!("task {:?}", task.id));
            *attachment_id = import_attachment_reference(
                attachment_id,
                owner,
                images_root,
                &staged_by_id,
                &mut known,
                &mut data.attachments,
                installed_attachments,
            )?;
        }
    }
    data.attachments
        .sort_by(|left, right| left.id.cmp(&right.id));
    Ok(())
}

fn import_attachment_reference(
    reference: &str,
    owner: &str,
    images_root: Option<&Path>,
    staged_by_id: &HashMap<&str, &StagedAttachment>,
    known: &mut HashMap<String, usize>,
    attachments: &mut Vec<Attachment>,
    installed_attachments: &mut Vec<InstalledAttachmentFile>,
) -> Result<String, StoreError> {
    let Some(images_root) = images_root else {
        return Err(StoreError::Validation(
            "image attachments require a persistent store".into(),
        ));
    };
    let (source_path, expected) = if is_attachment_id(reference) {
        let staged = staged_by_id.get(reference).ok_or_else(|| {
            StoreError::Validation(format!(
                "{owner} refers to unknown attachment {reference:?}"
            ))
        })?;
        (staged.path.clone(), Some(&staged.metadata))
    } else {
        (crate::image::expand_in(reference, images_root), None)
    };
    let ImportedAttachmentFile {
        metadata,
        installed,
    } = import_attachment_from_path(&source_path, images_root)?;
    if let Some(installed) = installed {
        installed_attachments.push(installed);
    }
    if expected.is_some_and(|expected| expected != &metadata) {
        return Err(StoreError::Validation(format!(
            "staged attachment {reference:?} does not match its verified metadata"
        )));
    }
    if let Some(&index) = known.get(&metadata.id) {
        if attachments[index] != metadata {
            return Err(StoreError::Corrupt(format!(
                "attachment {:?} metadata does not match imported content",
                metadata.id
            )));
        }
        return Ok(metadata.id);
    }
    let id = metadata.id.clone();
    known.insert(id.clone(), attachments.len());
    attachments.push(metadata);
    Ok(id)
}

#[derive(Debug, Clone)]
struct InstalledAttachmentFile {
    id: String,
    path: PathBuf,
}

struct ImportedAttachmentFile {
    metadata: Attachment,
    installed: Option<InstalledAttachmentFile>,
}

fn import_attachment_from_path(
    source_path: &Path,
    images_root: &Path,
) -> Result<ImportedAttachmentFile, StoreError> {
    let mut source = fs::File::open(source_path)
        .map_err(|error| StoreError::io("open image attachment", source_path, error))?;
    let metadata = source
        .metadata()
        .map_err(|error| StoreError::io("inspect image attachment", source_path, error))?;
    if !metadata.is_file() {
        return Err(StoreError::Validation(format!(
            "image attachment {} is not a regular file",
            source_path.display()
        )));
    }

    ensure_private_directory(images_root)?;
    let temp_path = images_root.join(format!(".mach-attachment-{}.tmp", uuid::Uuid::new_v4()));
    let mut temp = open_private_attachment_temp(&temp_path)?;
    let mut installed_path = None;
    let result = (|| {
        let mut hasher = Sha256::new();
        let mut byte_len = 0_u64;
        let mut prefix = [0_u8; 32];
        let mut prefix_len = 0usize;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|error| StoreError::io("read image attachment", source_path, error))?;
            if read == 0 {
                break;
            }
            byte_len = byte_len
                .checked_add(read as u64)
                .ok_or_else(|| StoreError::Validation("image attachment is too large".into()))?;
            if byte_len > MAX_ATTACHMENT_BYTES {
                return Err(StoreError::Validation(format!(
                    "image attachment {} exceeds the {} MiB safety limit",
                    source_path.display(),
                    MAX_ATTACHMENT_BYTES / 1024 / 1024
                )));
            }
            if prefix_len < prefix.len() {
                let copy = (prefix.len() - prefix_len).min(read);
                prefix[prefix_len..prefix_len + copy].copy_from_slice(&buffer[..copy]);
                prefix_len += copy;
            }
            hasher.update(&buffer[..read]);
            temp.write_all(&buffer[..read]).map_err(|error| {
                StoreError::io("write managed image attachment", &temp_path, error)
            })?;
        }
        if byte_len == 0 {
            return Err(StoreError::Validation(format!(
                "image attachment {} is empty",
                source_path.display()
            )));
        }
        let format = image::guess_format(&prefix[..prefix_len]).map_err(|_| {
            StoreError::Validation(format!(
                "image attachment {} is not a supported PNG, JPEG, GIF, or WebP image",
                source_path.display()
            ))
        })?;
        let format = crate::image::managed_attachment_format(format).ok_or_else(|| {
            StoreError::Validation(format!(
                "image attachment {} is not a supported PNG, JPEG, GIF, or WebP image",
                source_path.display()
            ))
        })?;
        temp.sync_all()
            .map_err(|error| StoreError::io("sync managed image attachment", &temp_path, error))?;
        drop(temp);
        crate::image::load_dynamic(&temp_path).map_err(StoreError::Validation)?;

        let id = format!("{:x}", hasher.finalize());
        let storage_name = format!("{id}.{}", format.extension);
        let destination = images_root.join(&storage_name);
        if destination.exists() {
            let (stored_hash, stored_len) = hash_attachment_file(&destination)?;
            if stored_hash != id || stored_len != byte_len {
                return Err(StoreError::Corrupt(format!(
                    "managed attachment {} does not match its content address",
                    destination.display()
                )));
            }
            fs::remove_file(&temp_path).map_err(|error| {
                StoreError::io("remove duplicate image attachment", &temp_path, error)
            })?;
        } else {
            fs::rename(&temp_path, &destination).map_err(|error| {
                StoreError::io("install managed image attachment", &destination, error)
            })?;
            installed_path = Some(destination.clone());
            set_private_file(&destination)?;
            fs::File::open(images_root)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| StoreError::io("sync image directory", images_root, error))?;
        }
        Ok(Attachment {
            id: id.clone(),
            sha256: id,
            media_type: format.media_type.into(),
            byte_len,
            storage_name,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
        if let Some(path) = installed_path.as_deref() {
            let _ = fs::remove_file(path);
        }
    }
    let metadata = result?;
    let installed = installed_path.map(|path| InstalledAttachmentFile {
        id: metadata.id.clone(),
        path,
    });
    Ok(ImportedAttachmentFile {
        metadata,
        installed,
    })
}

fn open_private_attachment_temp(path: &Path) -> Result<fs::File, StoreError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| StoreError::io("create managed image attachment", path, error))
}

fn hash_attachment_file(path: &Path) -> Result<(String, u64), StoreError> {
    let mut file = fs::File::open(path)
        .map_err(|error| StoreError::io("open managed image attachment", path, error))?;
    let mut hasher = Sha256::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| StoreError::io("read managed image attachment", path, error))?;
        if read == 0 {
            break;
        }
        byte_len = byte_len
            .checked_add(read as u64)
            .ok_or_else(|| StoreError::Corrupt("managed attachment is too large".into()))?;
        if byte_len > MAX_ATTACHMENT_BYTES {
            return Err(StoreError::Corrupt(format!(
                "managed attachment {} exceeds the safety limit",
                path.display()
            )));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), byte_len))
}

pub(crate) fn is_attachment_id(value: &str) -> bool {
    value.len() == ATTACHMENT_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn is_managed_attachment_name(value: &str) -> bool {
    let Some((id, extension)) = value.rsplit_once('.') else {
        return false;
    };
    is_attachment_id(id) && crate::image::is_managed_attachment_extension(extension)
}

fn is_managed_attachment_temp_name(value: &str) -> bool {
    value
        .strip_prefix(".mach-attachment-")
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

fn prune_unreferenced_attachments(data: &mut StoreData) {
    let referenced: HashSet<_> = data
        .tasks
        .iter()
        .flat_map(|task| {
            task.description.iter().filter_map(|block| match block {
                Block::Image { attachment_id } => Some(attachment_id.as_str()),
                _ => None,
            })
        })
        .collect();
    data.attachments
        .retain(|attachment| referenced.contains(attachment.id.as_str()));
}

fn remove_managed_attachment_file(path: &Path) -> Result<bool, StoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StoreError::io(
            "remove managed image attachment",
            path,
            error,
        )),
    }
}

fn sync_images_directory(images_root: &Path) -> Result<(), StoreError> {
    if !images_root.exists() {
        return Ok(());
    }
    fs::File::open(images_root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StoreError::io("sync image directory", images_root, error))
}

fn remove_installed_attachment_files(
    installed_attachments: &[InstalledAttachmentFile],
) -> Result<(), StoreError> {
    let mut directory = None;
    let mut removed = false;
    for attachment in installed_attachments {
        removed |= remove_managed_attachment_file(&attachment.path)?;
        directory = attachment.path.parent();
    }
    if removed && let Some(directory) = directory {
        sync_images_directory(directory)?;
    }
    Ok(())
}

fn cleanup_attachments_after_failed_commit(
    connection: &mut Connection,
    images_root: &Path,
    installed_attachments: &[InstalledAttachmentFile],
) -> Result<(), StoreError> {
    if installed_attachments.is_empty() {
        return Ok(());
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut unowned = Vec::new();
    for attachment in installed_attachments {
        let owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE id = ?1)",
            [&attachment.id],
            |row| row.get(0),
        )?;
        if !owned {
            unowned.push(attachment.clone());
        }
    }
    remove_installed_attachment_files(&unowned)?;
    tx.commit()?;
    sync_images_directory(images_root)?;
    Ok(())
}

fn cleanup_pending_attachment_files(
    connection: &mut Connection,
    images_root: &Path,
) -> Result<(), StoreError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending = {
        let mut statement =
            tx.prepare("SELECT storage_name FROM attachment_gc ORDER BY storage_name")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut removed = false;
    for storage_name in pending {
        if !is_managed_attachment_name(&storage_name) {
            return Err(StoreError::Corrupt(format!(
                "attachment cleanup entry has invalid storage name {storage_name:?}"
            )));
        }
        let owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE storage_name = ?1)",
            [&storage_name],
            |row| row.get(0),
        )?;
        if !owned {
            removed |= remove_managed_attachment_file(&images_root.join(&storage_name))?;
        }
        tx.execute(
            "DELETE FROM attachment_gc WHERE storage_name = ?1",
            [&storage_name],
        )?;
    }
    if removed {
        sync_images_directory(images_root)?;
    }
    tx.commit()?;
    Ok(())
}

fn reconcile_attachment_files(
    connection: &mut Connection,
    images_root: &Path,
) -> Result<(), StoreError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let unreferenced = {
        let mut statement = tx.prepare(
            "SELECT attachments.id, attachments.storage_name
             FROM attachments
             WHERE NOT EXISTS (
                 SELECT 1 FROM task_description_attachments
                 WHERE task_description_attachments.attachment_id = attachments.id
             )
             ORDER BY attachments.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (id, storage_name) in unreferenced {
        if !is_managed_attachment_name(&storage_name)
            || !storage_name.starts_with(&format!("{id}."))
        {
            return Err(StoreError::Corrupt(format!(
                "attachment {id:?} has invalid storage name {storage_name:?}"
            )));
        }
        tx.execute(
            "INSERT OR IGNORE INTO attachment_gc(storage_name) VALUES (?1)",
            [&storage_name],
        )?;
        tx.execute("DELETE FROM attachments WHERE id = ?1", [&id])?;
    }

    let owned = {
        let mut statement = tx.prepare("SELECT storage_name FROM attachments")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<HashSet<_>, _>>()?
    };
    if let Some(invalid) = owned
        .iter()
        .find(|storage_name| !is_managed_attachment_name(storage_name))
    {
        return Err(StoreError::Corrupt(format!(
            "attachment has invalid storage name {invalid:?}"
        )));
    }
    let pending = {
        let mut statement = tx.prepare("SELECT storage_name FROM attachment_gc")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if let Some(invalid) = pending
        .iter()
        .find(|storage_name| !is_managed_attachment_name(storage_name))
    {
        return Err(StoreError::Corrupt(format!(
            "attachment cleanup entry has invalid storage name {invalid:?}"
        )));
    }

    let mut removed = false;
    if images_root.exists() {
        let entries = fs::read_dir(images_root)
            .map_err(|error| StoreError::io("read image directory", images_root, error))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| StoreError::io("read image directory", images_root, error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stale_temp = is_managed_attachment_temp_name(name);
            let orphaned_managed = is_managed_attachment_name(name) && !owned.contains(name);
            if !stale_temp && !orphaned_managed {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                StoreError::io("inspect image directory entry", &entry.path(), error)
            })?;
            if file_type.is_file() || file_type.is_symlink() {
                removed |= remove_managed_attachment_file(&entry.path())?;
            }
        }
    }
    if removed {
        sync_images_directory(images_root)?;
    }
    tx.execute("DELETE FROM attachment_gc", [])?;
    tx.commit()?;
    Ok(())
}

#[derive(Clone, Copy)]
enum DueMode {
    NewWrite,
    LegacyMigration,
    Stored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttachmentMode {
    Draft,
    Persisted,
}

fn normalize_and_validate(
    data: &mut StoreData,
    now: NaiveDateTime,
    due_mode: DueMode,
    attachment_mode: AttachmentMode,
) -> Result<(), StoreError> {
    // This compatibility-only field moved to the application-level updater
    // store. Never let it re-enter persisted task settings.
    data.settings.last_update_check_at = None;
    if data.categories.len() > MAX_CATEGORY_COUNT {
        return Err(StoreError::Validation(format!(
            "category limit is {MAX_CATEGORY_COUNT}"
        )));
    }
    if data.tasks.len() > MAX_TASK_COUNT {
        return Err(StoreError::Validation(format!(
            "task limit is {MAX_TASK_COUNT}"
        )));
    }
    if data.labels.len() > MAX_LABEL_COUNT {
        return Err(StoreError::Validation(format!(
            "label limit is {MAX_LABEL_COUNT}"
        )));
    }

    let attachment_ids = validate_attachments(&data.attachments)?;

    let mut category_ids = HashSet::new();
    let mut category_names = HashSet::new();
    for category in &data.categories {
        validate_single_line(&category.id, "category id")?;
        validate_byte_limit(&category.id, ID_MAX_BYTES, "category id")?;
        if category.is_all() {
            return Err(StoreError::Validation(
                "real category id cannot be empty".into(),
            ));
        }
        if !category_ids.insert(category.id.as_str()) {
            return Err(StoreError::Validation(format!(
                "category id {:?} must be unique",
                category.id
            )));
        }
        validate_single_line(&category.name, "category name")?;
        validate_byte_limit(
            &category.name,
            text_byte_limit(MAX_CATEGORY_NAME_LEN),
            "category name",
        )?;
        let name = category.name.trim();
        if name.is_empty() {
            return Err(StoreError::Validation(
                "category name cannot be empty".into(),
            ));
        }
        if name.graphemes(true).count() > MAX_CATEGORY_NAME_LEN {
            return Err(StoreError::Validation(format!(
                "category name {:?} exceeds {MAX_CATEGORY_NAME_LEN} characters",
                category.name
            )));
        }
        if !category_names.insert(category_name_key(name)) {
            return Err(StoreError::Validation(format!(
                "category names must be unique (duplicate {:?})",
                category.name
            )));
        }
        validate_multiline(
            &category.description,
            MAX_CATEGORY_DESC_LINES,
            MAX_CATEGORY_DESC_LINE_LEN,
            "category description",
        )?;
    }

    let mut label_ids = HashSet::new();
    let mut label_names = HashSet::new();
    let mut label_positions = HashMap::new();
    for (position, label) in data.labels.iter_mut().enumerate() {
        validate_single_line(&label.id, "label id")?;
        validate_byte_limit(&label.id, ID_MAX_BYTES, "label id")?;
        if label.id.is_empty() || !label_ids.insert(label.id.clone()) {
            return Err(StoreError::Validation(format!(
                "label id {:?} must be nonempty and unique",
                label.id
            )));
        }
        validate_single_line(&label.name, "label name")?;
        validate_byte_limit(
            &label.name,
            text_byte_limit(MAX_LABEL_NAME_LEN),
            "label name",
        )?;
        let trimmed = label.name.trim();
        if trimmed.is_empty() {
            return Err(StoreError::Validation("label name cannot be empty".into()));
        }
        if trimmed.starts_with('#') {
            return Err(StoreError::Validation(
                "label name must not start with '#'".into(),
            ));
        }
        if trimmed.graphemes(true).count() > MAX_LABEL_NAME_LEN {
            return Err(StoreError::Validation(format!(
                "label name {:?} exceeds {MAX_LABEL_NAME_LEN} characters",
                label.name
            )));
        }
        if matches!(due_mode, DueMode::Stored) && trimmed != label.name {
            return Err(StoreError::Validation(format!(
                "label {:?} has noncanonical surrounding whitespace",
                label.id
            )));
        }
        label.name = trimmed.to_string();
        if !label_names.insert(label_name_key(&label.name)) {
            return Err(StoreError::Validation("label names must be unique".into()));
        }
        label_positions.insert(label.id.clone(), position);
    }

    let mut task_ids = HashSet::new();
    for task in &mut data.tasks {
        validate_single_line(&task.id, "task id")?;
        validate_byte_limit(&task.id, ID_MAX_BYTES, "task id")?;
        if task.id.is_empty() || !task_ids.insert(task.id.as_str()) {
            return Err(StoreError::Validation(format!(
                "task id {:?} must be nonempty and unique",
                task.id
            )));
        }
        validate_single_line(&task.title, "task title")?;
        validate_byte_limit(&task.title, text_byte_limit(MAX_TITLE_LEN), "task title")?;
        if task.title.trim().is_empty() {
            return Err(StoreError::Validation(format!(
                "task {:?} title cannot be empty",
                task.id
            )));
        }
        if task.title.graphemes(true).count() > MAX_TITLE_LEN {
            return Err(StoreError::Validation(format!(
                "task {:?} title exceeds {MAX_TITLE_LEN} characters",
                task.id
            )));
        }
        if task.importance > MAX_IMPORTANCE {
            return Err(StoreError::Validation(format!(
                "task {:?} importance must be 0-{MAX_IMPORTANCE}",
                task.id
            )));
        }
        if task.description.len() > MAX_DESCRIPTION_LINES {
            return Err(StoreError::Validation(format!(
                "task {:?} description exceeds {MAX_DESCRIPTION_LINES} blocks",
                task.id
            )));
        }
        for block in &task.description {
            validate_block(block, &task.id)?;
            if let Block::Image { attachment_id } = block {
                let known = attachment_ids.contains(attachment_id.as_str());
                if attachment_mode == AttachmentMode::Persisted && !known {
                    return Err(StoreError::Validation(format!(
                        "task {:?} refers to unknown attachment {attachment_id:?}",
                        task.id
                    )));
                }
                if attachment_mode == AttachmentMode::Draft
                    && is_attachment_id(attachment_id)
                    && !known
                {
                    return Err(StoreError::Validation(format!(
                        "task {:?} refers to unknown attachment {attachment_id:?}",
                        task.id
                    )));
                }
            }
        }
        if let Some(category_id) = task.category_id.as_deref() {
            validate_single_line(category_id, "task category id")?;
            validate_byte_limit(category_id, ID_MAX_BYTES, "task category id")?;
            if !category_ids.contains(category_id) {
                return Err(StoreError::Validation(format!(
                    "task {:?} refers to unknown category {category_id:?}",
                    task.id
                )));
            }
        }
        if task.label_ids.len() > MAX_LABELS_PER_TASK {
            return Err(StoreError::Validation(format!(
                "task {:?} label limit is {MAX_LABELS_PER_TASK}",
                task.id
            )));
        }
        let mut assigned = HashSet::new();
        for label_id in &task.label_ids {
            validate_single_line(label_id, "task label id")?;
            validate_byte_limit(label_id, ID_MAX_BYTES, "task label id")?;
            if !assigned.insert(label_id.clone()) {
                return Err(StoreError::Validation(format!(
                    "task {:?} assigns label {label_id:?} more than once",
                    task.id
                )));
            }
            if !label_positions.contains_key(label_id) {
                return Err(StoreError::Validation(format!(
                    "task {:?} refers to unknown label {label_id:?}",
                    task.id
                )));
            }
        }
        let mut canonical_label_ids = task.label_ids.clone();
        canonical_label_ids.sort_by_key(|label_id| label_positions[label_id]);
        if matches!(due_mode, DueMode::Stored) && canonical_label_ids != task.label_ids {
            return Err(StoreError::Validation(format!(
                "task {:?} labels are not in canonical store order",
                task.id
            )));
        }
        task.label_ids = canonical_label_ids;
        validate_single_line(&task.due, "task due")?;
        validate_byte_limit(&task.due, DUE_MAX_BYTES, "task due")?;
        let normalized_due = match due_mode {
            DueMode::NewWrite | DueMode::Stored => due::normalize_for_write_at(&task.due, now),
            DueMode::LegacyMigration => due::normalize_legacy_at(&task.due, now),
        }
        .map_err(|error| StoreError::Validation(format!("task {:?} has {error}", task.id)))?;
        if matches!(due_mode, DueMode::Stored) && normalized_due != task.due {
            return Err(StoreError::Validation(format!(
                "task {:?} has noncanonical due value {:?}",
                task.id, task.due
            )));
        }
        task.due = normalized_due;
        validate_single_line(&task.created, "task creation timestamp")?;
        validate_byte_limit(&task.created, CREATED_MAX_BYTES, "task creation timestamp")?;
        NaiveDateTime::parse_from_str(&task.created, "%Y-%m-%d %H:%M:%S").map_err(|_| {
            StoreError::Validation(format!(
                "task {:?} has invalid creation timestamp {:?}",
                task.id, task.created
            ))
        })?;
    }
    validate_settings(&data.settings)
}

fn validate_block(block: &Block, task_id: &str) -> Result<(), StoreError> {
    let (kind, value) = match block {
        Block::Text { text } => ("text", text),
        Block::Todo { text, .. } => ("subtask", text),
        Block::Bullet { text } => ("bullet", text),
        Block::Number { text } => ("number", text),
        Block::Link { url } => ("link", url),
        Block::Image { attachment_id } => ("image attachment", attachment_id),
    };
    validate_single_line(value, kind)?;
    validate_byte_limit(value, text_byte_limit(MAX_NOTES_LINE_LEN), kind)?;
    if value.graphemes(true).count() > MAX_NOTES_LINE_LEN {
        return Err(StoreError::Validation(format!(
            "task {task_id:?} {kind} exceeds {MAX_NOTES_LINE_LEN} characters"
        )));
    }
    Ok(())
}

fn validate_attachments(attachments: &[Attachment]) -> Result<HashSet<&str>, StoreError> {
    let mut ids = HashSet::new();
    let mut storage_names = HashSet::new();
    for attachment in attachments {
        if !is_attachment_id(&attachment.id) || attachment.sha256 != attachment.id {
            return Err(StoreError::Validation(format!(
                "attachment {:?} has an invalid content address",
                attachment.id
            )));
        }
        if !ids.insert(attachment.id.as_str()) {
            return Err(StoreError::Validation(format!(
                "attachment id {:?} must be unique",
                attachment.id
            )));
        }
        if attachment.byte_len == 0 || attachment.byte_len > MAX_ATTACHMENT_BYTES {
            return Err(StoreError::Validation(format!(
                "attachment {:?} has invalid byte length {}",
                attachment.id, attachment.byte_len
            )));
        }
        let format = crate::image::managed_attachment_format_for_media_type(&attachment.media_type)
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "attachment {:?} has unsupported media type {:?}",
                    attachment.id, attachment.media_type
                ))
            })?;
        let expected_storage_name = format!("{}.{}", attachment.id, format.extension);
        if attachment.storage_name != expected_storage_name {
            return Err(StoreError::Validation(format!(
                "attachment {:?} has invalid storage name {:?}",
                attachment.id, attachment.storage_name
            )));
        }
        if !storage_names.insert(attachment.storage_name.as_str()) {
            return Err(StoreError::Validation(format!(
                "attachment storage name {:?} must be unique",
                attachment.storage_name
            )));
        }
    }
    Ok(ids)
}

fn validate_multiline(
    value: &str,
    max_lines: usize,
    max_line_len: usize,
    label: &str,
) -> Result<(), StoreError> {
    let max_line_bytes = text_byte_limit(max_line_len);
    let max_total_bytes = max_lines.saturating_mul(max_line_bytes.saturating_add(1));
    validate_byte_limit(value, max_total_bytes, label)?;
    if value
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(StoreError::Validation(format!(
            "{label} contains a control character"
        )));
    }
    for (index, line) in value.split('\n').enumerate() {
        if index >= max_lines {
            return Err(StoreError::Validation(format!(
                "{label} exceeds {max_lines} lines"
            )));
        }
        if line.len() > max_line_bytes {
            return Err(StoreError::Validation(format!(
                "{label} line exceeds {max_line_bytes} bytes"
            )));
        }
        if line.graphemes(true).count() > max_line_len {
            return Err(StoreError::Validation(format!(
                "{label} line exceeds {max_line_len} characters"
            )));
        }
    }
    Ok(())
}

fn validate_single_line(value: &str, label: &str) -> Result<(), StoreError> {
    if value.chars().any(char::is_control) {
        return Err(StoreError::Validation(format!(
            "{label} contains a control character"
        )));
    }
    Ok(())
}

fn validate_byte_limit(value: &str, max_bytes: usize, label: &str) -> Result<(), StoreError> {
    if value.len() > max_bytes {
        return Err(StoreError::Validation(format!(
            "{label} exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn category_name_has_prefix(name: &str, folded_query: &str) -> bool {
    let normalized: String = name.trim().nfkc().collect();
    normalized
        .char_indices()
        .skip(1)
        .map(|(index, _)| index)
        .chain(std::iter::once(normalized.len()))
        .any(|end| category_name_key(&normalized[..end]) == folded_query)
}

fn label_name_has_prefix(name: &str, folded_query: &str) -> bool {
    let normalized: String = name.trim().nfkc().collect();
    normalized
        .char_indices()
        .skip(1)
        .map(|(index, _)| index)
        .chain(std::iter::once(normalized.len()))
        .any(|end| label_name_key(&normalized[..end]) == folded_query)
}

fn validate_settings(settings: &Settings) -> Result<(), StoreError> {
    validate_single_line(&settings.date_format, "date format")?;
    validate_byte_limit(
        &settings.date_format,
        SETTINGS_VALUE_MAX_BYTES,
        "date format",
    )?;
    validate_single_line(&settings.selected_color, "theme")?;
    validate_byte_limit(&settings.selected_color, SETTINGS_VALUE_MAX_BYTES, "theme")?;
    validate_single_line(&settings.sort, "sort")?;
    validate_byte_limit(&settings.sort, SETTINGS_VALUE_MAX_BYTES, "sort")?;
    validate_single_line(&settings.preview_position, "preview position")?;
    validate_byte_limit(
        &settings.preview_position,
        SETTINGS_VALUE_MAX_BYTES,
        "preview position",
    )?;
    if let Some(version) = settings.last_run_version.as_deref() {
        validate_single_line(version, "last-run version")?;
        validate_byte_limit(version, SETTINGS_VALUE_MAX_BYTES, "last-run version")?;
    }
    if !DATE_FORMATS.contains(&settings.date_format.as_str()) {
        return Err(StoreError::Validation(format!(
            "unknown date format {:?}",
            settings.date_format
        )));
    }
    if !THEMES.contains(&settings.selected_color.as_str()) {
        return Err(StoreError::Validation(format!(
            "unknown theme {:?}",
            settings.selected_color
        )));
    }
    if !SORTS.contains(&settings.sort.as_str()) {
        return Err(StoreError::Validation(format!(
            "unknown sort {:?}",
            settings.sort
        )));
    }
    if !PREVIEW_POSITIONS.contains(&settings.preview_position.as_str()) {
        return Err(StoreError::Validation(format!(
            "unknown preview position {:?}",
            settings.preview_position
        )));
    }
    Ok(())
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::io("read", path, error)),
    };
    let size = file
        .metadata()
        .map_err(|error| StoreError::io("inspect", path, error))?
        .len();
    if size > MAX_LEGACY_JSON_BYTES {
        return Err(StoreError::Validation(format!(
            "legacy file {} is larger than the {} MiB safety limit",
            path.display(),
            MAX_LEGACY_JSON_BYTES / 1024 / 1024
        )));
    }
    serde_json::from_reader(std::io::BufReader::new(file))
        .map(Some)
        .map_err(|source| StoreError::Json {
            path: path.to_path_buf(),
            source,
        })
}

fn validate_legacy_schema(path: &Path, schema: Option<u32>) -> Result<(), StoreError> {
    if let Some(found) = schema
        && found != SCHEMA_VERSION
    {
        return Err(StoreError::UnsupportedLegacySchema {
            path: path.to_path_buf(),
            found,
            expected: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TasksFile {
    schema: u32,
    tasks: Vec<Task>,
}

#[derive(Debug, Deserialize)]
struct CategoriesFile {
    schema: u32,
    categories: Vec<Category>,
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), StoreError> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|parent_error| {
                    StoreError::io("create parent directory", parent, parent_error)
                })?;
            }
            match fs::create_dir(path) {
                Ok(()) => true,
                Err(retry)
                    if retry.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() =>
                {
                    false
                }
                Err(retry) => {
                    return Err(StoreError::io("create directory", path, retry));
                }
            }
        }
        Err(error) => return Err(StoreError::io("create directory", path, error)),
    };
    if created {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| StoreError::io("set permissions on", path, error))?;
        }
    }
    Ok(())
}

pub(crate) fn prepare_private_database_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(StoreError::io("create database", path, error)),
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn set_private_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| StoreError::io("set permissions on", path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_directory_requires_a_home_when_no_path_is_configured() {
        let error = resolve_data_dir_from(None, None, None)
            .expect_err("missing home must not silently select the working directory");
        assert!(matches!(error, StoreError::Validation(_)));

        assert_eq!(
            resolve_data_dir_from(Some(PathBuf::from("/tmp/mach")), None, None).unwrap(),
            PathBuf::from("/tmp/mach")
        );
        assert_eq!(
            resolve_data_dir_from(None, Some(PathBuf::from("/tmp/configured")), None).unwrap(),
            PathBuf::from("/tmp/configured")
        );
        assert!(resolve_data_dir_from(Some(PathBuf::from("~/.mach")), None, None).is_err());
    }
}
