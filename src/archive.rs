//! Portable, versioned task archives shared by the CLI and TUI.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::model::{Block, Category, Task, caseless_key};
use crate::settings::Settings;
use crate::store::{Attachment, Store, StoreData, StoreError};

const ARCHIVE_FORMAT: &str = "mach-archive";
const ARCHIVE_SCHEMA: u32 = 1;
const MANIFEST_PATH: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum ArchiveError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    Store(StoreError),
    Invalid(String),
    Conflict(String),
}

impl ArchiveError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Conflict(_) | Self::Store(StoreError::Conflict { .. }) => "conflict",
            Self::Io { .. } | Self::Store(StoreError::Io { .. }) => "io",
            Self::Store(StoreError::Database(_)) => "database",
            Self::Store(StoreError::UnsupportedLegacySchema { .. })
            | Self::Store(StoreError::UnsupportedDatabaseSchema { .. }) => "schema",
            Self::Store(StoreError::StaleEntity { .. }) => "conflict",
            Self::Store(StoreError::NotFound { .. }) => "not_found",
            Self::Store(StoreError::Ambiguous { .. }) => "ambiguous",
            Self::Store(StoreError::Validation(_)) => "validation",
            Self::Store(StoreError::Corrupt(_)) => "corrupt",
            Self::Store(StoreError::Json { .. }) => "legacy_json",
            Self::Zip(_) | Self::Json(_) | Self::Invalid(_) => "archive",
        }
    }
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} {}: {source}", path.display()),
            Self::Zip(error) => write!(f, "invalid archive: {error}"),
            Self::Json(error) => write!(f, "invalid archive manifest: {error}"),
            Self::Store(error) => error.fmt(f),
            Self::Invalid(message) | Self::Conflict(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Zip(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Invalid(_) | Self::Conflict(_) => None,
        }
    }
}

impl From<zip::result::ZipError> for ArchiveError {
    fn from(value: zip::result::ZipError) -> Self {
        Self::Zip(value)
    }
}

impl From<serde_json::Error> for ArchiveError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<StoreError> for ArchiveError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExportSummary {
    pub path: PathBuf,
    pub tasks: usize,
    pub categories: usize,
    pub images: usize,
}

impl ExportSummary {
    pub(crate) fn short_path(&self) -> String {
        if let Ok(directory) = std::env::current_dir()
            && let Ok(relative) = self.path.strip_prefix(directory)
            && !relative.as_os_str().is_empty()
        {
            return format!("./{}", relative.display());
        }
        if let Some(home) = dirs::home_dir()
            && let Ok(relative) = self.path.strip_prefix(home)
        {
            return format!("~/{}", relative.display());
        }
        self.path.display().to_string()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ImportSummary {
    pub path: PathBuf,
    pub tasks_added: usize,
    pub tasks_unchanged: usize,
    pub categories_added: usize,
    pub categories_unchanged: usize,
    pub images_added: usize,
    pub images_unchanged: usize,
}

impl ImportSummary {
    fn changed(&self) -> bool {
        self.tasks_added > 0 || self.categories_added > 0 || self.images_added > 0
    }
}

pub(crate) fn content_count_text(tasks: usize, categories: usize, images: usize) -> String {
    format!(
        "{} {}, {} {}, and {} {}",
        tasks,
        plural(tasks, "task", "tasks"),
        categories,
        plural(categories, "category", "categories"),
        images,
        plural(images, "image", "images")
    )
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

#[derive(Debug)]
pub(crate) struct ImportOutcome {
    pub summary: ImportSummary,
    pub snapshot: StoreData,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    schema: u32,
    categories: Vec<ArchiveCategory>,
    tasks: Vec<ArchiveTask>,
    attachments: Vec<ArchiveAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveCategory {
    id: String,
    name: String,
    description: String,
}

impl From<&Category> for ArchiveCategory {
    fn from(category: &Category) -> Self {
        Self {
            id: category.id.clone(),
            name: category.name.clone(),
            description: category.description.clone(),
        }
    }
}

impl From<ArchiveCategory> for Category {
    fn from(category: ArchiveCategory) -> Self {
        Self {
            id: category.id,
            name: category.name,
            description: category.description,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveTask {
    id: String,
    title: String,
    body: Vec<ArchiveBlock>,
    due: String,
    created: String,
    done: bool,
    importance: u8,
    category_id: Option<String>,
}

impl From<&Task> for ArchiveTask {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            title: task.title.clone(),
            body: task.body.iter().map(ArchiveBlock::from).collect(),
            due: task.due.clone(),
            created: task.created.clone(),
            done: task.done,
            importance: task.importance,
            category_id: task.category_id.clone(),
        }
    }
}

impl From<ArchiveTask> for Task {
    fn from(task: ArchiveTask) -> Self {
        Self {
            id: task.id,
            title: task.title,
            body: task.body.into_iter().map(Block::from).collect(),
            due: task.due,
            created: task.created,
            done: task.done,
            importance: task.importance,
            category_id: task.category_id,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ArchiveBlock {
    Text { text: String },
    Todo { text: String, done: bool },
    Bullet { text: String },
    Number { text: String },
    Link { url: String },
    Image { attachment_id: String },
}

impl From<&Block> for ArchiveBlock {
    fn from(block: &Block) -> Self {
        match block {
            Block::Text { text } => Self::Text { text: text.clone() },
            Block::Todo { text, done } => Self::Todo {
                text: text.clone(),
                done: *done,
            },
            Block::Bullet { text } => Self::Bullet { text: text.clone() },
            Block::Number { text } => Self::Number { text: text.clone() },
            Block::Link { url } => Self::Link { url: url.clone() },
            Block::Image { attachment_id } => Self::Image {
                attachment_id: attachment_id.clone(),
            },
        }
    }
}

impl From<ArchiveBlock> for Block {
    fn from(block: ArchiveBlock) -> Self {
        match block {
            ArchiveBlock::Text { text } => Self::Text { text },
            ArchiveBlock::Todo { text, done } => Self::Todo { text, done },
            ArchiveBlock::Bullet { text } => Self::Bullet { text },
            ArchiveBlock::Number { text } => Self::Number { text },
            ArchiveBlock::Link { url } => Self::Link { url },
            ArchiveBlock::Image { attachment_id } => Self::Image { attachment_id },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveAttachment {
    id: String,
    sha256: String,
    media_type: String,
    byte_len: u64,
    file: String,
}

impl ArchiveAttachment {
    fn from_store(attachment: &Attachment) -> Self {
        Self {
            id: attachment.id.clone(),
            sha256: attachment.sha256.clone(),
            media_type: attachment.media_type.clone(),
            byte_len: attachment.byte_len,
            file: format!("images/{}", attachment.storage_name),
        }
    }

    fn into_imported(self) -> Result<ImportedAttachment, ArchiveError> {
        let storage_name = self
            .file
            .strip_prefix("images/")
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| {
                ArchiveError::Invalid(format!(
                    "archive attachment {:?} has invalid file {:?}",
                    self.id, self.file
                ))
            })?
            .to_string();
        Ok(ImportedAttachment {
            metadata: Attachment {
                id: self.id,
                sha256: self.sha256,
                media_type: self.media_type,
                byte_len: self.byte_len,
                storage_name,
            },
            file: self.file,
        })
    }
}

#[derive(Debug, Clone)]
struct ImportedAttachment {
    metadata: Attachment,
    file: String,
}

struct ImportedArchive {
    categories: Vec<Category>,
    tasks: Vec<Task>,
    attachments: Vec<ImportedAttachment>,
}

struct MergePlan {
    categories: Vec<Category>,
    tasks: Vec<Task>,
    attachments: Vec<Attachment>,
    summary: ImportSummary,
}

pub(crate) fn export(
    store: &Store,
    requested_path: Option<&Path>,
) -> Result<ExportSummary, ArchiveError> {
    let path = match requested_path {
        Some(path) => absolute_user_path(path)?,
        None => default_export_path()?,
    };
    if path.exists() {
        return Err(ArchiveError::Invalid(format!(
            "archive already exists: {}",
            path.display()
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        ArchiveError::Invalid(format!("archive path has no parent: {}", path.display()))
    })?;
    if !parent.is_dir() {
        return Err(ArchiveError::Invalid(format!(
            "archive directory does not exist: {}",
            parent.display()
        )));
    }

    let snapshot = store.snapshot()?;
    let (manifest, attachments) = manifest_from_snapshot(&snapshot)?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ArchiveError::Invalid(format!(
            "archive manifest exceeds {} MiB",
            MAX_MANIFEST_BYTES / 1024 / 1024
        )));
    }

    let temp_path = parent.join(format!(".mach-export-{}.tmp", uuid::Uuid::new_v4()));
    let (mut temp_guard, file) = TempFile::create(&temp_path)?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o600);
    writer.start_file(MANIFEST_PATH, options)?;
    writer
        .write_all(&manifest_bytes)
        .map_err(|source| ArchiveError::io("write archive manifest to", &temp_path, source))?;
    writer
        .write_all(b"\n")
        .map_err(|source| ArchiveError::io("write archive manifest to", &temp_path, source))?;

    for attachment in &attachments {
        let source_path = store.images_dir().join(&attachment.metadata.storage_name);
        writer.start_file(&attachment.file, options)?;
        write_verified_attachment(&source_path, &attachment.metadata, &mut writer)?;
    }
    let file = writer.finish()?;
    file.sync_all()
        .map_err(|source| ArchiveError::io("sync archive", &temp_path, source))?;
    drop(file);

    fs::hard_link(&temp_path, &path).map_err(|source| {
        if source.kind() == io::ErrorKind::AlreadyExists {
            ArchiveError::Invalid(format!("archive already exists: {}", path.display()))
        } else {
            ArchiveError::io("install archive at", &path, source)
        }
    })?;
    fs::remove_file(&temp_path)
        .map_err(|source| ArchiveError::io("remove archive temporary file", &temp_path, source))?;
    temp_guard.disarm();
    sync_directory(parent)?;

    Ok(ExportSummary {
        path,
        tasks: snapshot.tasks.len(),
        categories: snapshot.categories.len(),
        images: attachments.len(),
    })
}

pub(crate) fn import(
    store: &mut Store,
    requested_path: &Path,
) -> Result<ImportOutcome, ArchiveError> {
    let path = absolute_user_path(requested_path)?;
    let file =
        File::open(&path).map_err(|source| ArchiveError::io("open archive", &path, source))?;
    if !file
        .metadata()
        .map_err(|source| ArchiveError::io("inspect archive", &path, source))?
        .is_file()
    {
        return Err(ArchiveError::Invalid(format!(
            "archive is not a regular file: {}",
            path.display()
        )));
    }
    let mut zip = ZipArchive::new(file)?;
    let names = inspect_entries(&mut zip)?;
    let imported = read_manifest(&mut zip, &names)?;
    let current = store.snapshot()?;
    let mut plan = plan_merge(&current, &imported, &path)?;

    let stage = StageDirectory::create(store.data_dir())?;
    let staged = stage_attachments(&mut zip, &imported.attachments, &stage.path)?;
    let mut installed = Vec::with_capacity(staged.len());
    for (expected, staged_path) in staged {
        let actual = store.import_attachment_from(&staged_path)?;
        if actual != expected {
            return Err(ArchiveError::Invalid(format!(
                "archive image {} does not match its manifest metadata",
                expected.id
            )));
        }
        installed.push(actual);
    }
    installed.sort_by(|left, right| left.id.cmp(&right.id));
    let installed_by_id: HashMap<_, _> = installed
        .into_iter()
        .map(|attachment| (attachment.id.clone(), attachment))
        .collect();
    for attachment in &mut plan.attachments {
        *attachment = installed_by_id
            .get(&attachment.id)
            .cloned()
            .ok_or_else(|| {
                ArchiveError::Invalid(format!("archive image {} was not installed", attachment.id))
            })?;
    }

    if !plan.summary.changed() {
        return Ok(ImportOutcome {
            summary: plan.summary,
            snapshot: current,
        });
    }

    let expected_revision = current.revision;
    let summary = plan.summary.clone();
    let (summary, snapshot) =
        store.update_if_revision_with_snapshot(expected_revision, |data| {
            data.categories.append(&mut plan.categories);
            data.tasks.append(&mut plan.tasks);
            data.attachments.append(&mut plan.attachments);
            data.attachments
                .sort_by(|left, right| left.id.cmp(&right.id));
            Ok(summary)
        })?;
    Ok(ImportOutcome { summary, snapshot })
}

fn manifest_from_snapshot(
    snapshot: &StoreData,
) -> Result<(Manifest, Vec<ImportedAttachment>), ArchiveError> {
    let referenced: BTreeSet<_> = snapshot
        .tasks
        .iter()
        .flat_map(|task| task.body.iter())
        .filter_map(|block| match block {
            Block::Image { attachment_id } => Some(attachment_id.as_str()),
            _ => None,
        })
        .collect();
    let by_id: HashMap<_, _> = snapshot
        .attachments()
        .iter()
        .map(|attachment| (attachment.id.as_str(), attachment))
        .collect();
    let attachments: Vec<_> = referenced
        .into_iter()
        .map(|id| {
            let attachment = by_id.get(id).copied().ok_or_else(|| {
                ArchiveError::Invalid(format!("task refers to unknown image attachment {id}"))
            })?;
            Ok(ImportedAttachment {
                metadata: attachment.clone(),
                file: format!("images/{}", attachment.storage_name),
            })
        })
        .collect::<Result<_, ArchiveError>>()?;
    let manifest = Manifest {
        format: ARCHIVE_FORMAT.into(),
        schema: ARCHIVE_SCHEMA,
        categories: snapshot
            .categories
            .iter()
            .map(ArchiveCategory::from)
            .collect(),
        tasks: snapshot.tasks.iter().map(ArchiveTask::from).collect(),
        attachments: attachments
            .iter()
            .map(|attachment| ArchiveAttachment::from_store(&attachment.metadata))
            .collect(),
    };
    Ok((manifest, attachments))
}

fn inspect_entries(zip: &mut ZipArchive<File>) -> Result<HashSet<String>, ArchiveError> {
    let mut names = HashSet::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        if !entry.is_file() || entry.encrypted() {
            return Err(ArchiveError::Invalid(format!(
                "archive entry {:?} must be an unencrypted regular file",
                entry.name()
            )));
        }
        if entry.compression() != CompressionMethod::Stored {
            return Err(ArchiveError::Invalid(format!(
                "archive entry {:?} uses unsupported compression",
                entry.name()
            )));
        }
        if !names.insert(entry.name().to_string()) {
            return Err(ArchiveError::Invalid(format!(
                "archive contains duplicate entry {:?}",
                entry.name()
            )));
        }
    }
    Ok(names)
}

fn read_manifest(
    zip: &mut ZipArchive<File>,
    names: &HashSet<String>,
) -> Result<ImportedArchive, ArchiveError> {
    if !names.contains(MANIFEST_PATH) {
        return Err(ArchiveError::Invalid(
            "archive does not contain manifest.json".into(),
        ));
    }
    let manifest = {
        let mut entry = zip.by_name(MANIFEST_PATH)?;
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err(ArchiveError::Invalid(format!(
                "archive manifest exceeds {} MiB",
                MAX_MANIFEST_BYTES / 1024 / 1024
            )));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .by_ref()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| {
                ArchiveError::Invalid(format!("could not read archive manifest: {source}"))
            })?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ArchiveError::Invalid(format!(
                "archive manifest exceeds {} MiB",
                MAX_MANIFEST_BYTES / 1024 / 1024
            )));
        }
        serde_json::from_slice::<Manifest>(&bytes)?
    };
    if manifest.format != ARCHIVE_FORMAT {
        return Err(ArchiveError::Invalid(format!(
            "unsupported archive format {:?}",
            manifest.format
        )));
    }
    if manifest.schema != ARCHIVE_SCHEMA {
        return Err(ArchiveError::Invalid(format!(
            "unsupported archive schema {}; expected {}",
            manifest.schema, ARCHIVE_SCHEMA
        )));
    }

    let attachments: Vec<_> = manifest
        .attachments
        .into_iter()
        .map(ArchiveAttachment::into_imported)
        .collect::<Result<_, _>>()?;
    let expected_names: HashSet<_> = std::iter::once(MANIFEST_PATH.to_string())
        .chain(attachments.iter().map(|attachment| attachment.file.clone()))
        .collect();
    if &expected_names != names {
        let mut unexpected: Vec<_> = names.difference(&expected_names).cloned().collect();
        let mut missing: Vec<_> = expected_names.difference(names).cloned().collect();
        unexpected.sort();
        missing.sort();
        return Err(ArchiveError::Invalid(format!(
            "archive entries do not match the manifest (unexpected: {unexpected:?}, missing: {missing:?})"
        )));
    }

    let categories: Vec<Category> = manifest
        .categories
        .into_iter()
        .map(Category::from)
        .collect();
    let tasks: Vec<Task> = manifest.tasks.into_iter().map(Task::from).collect();
    validate_imported_data(&categories, &tasks, &attachments)?;
    Ok(ImportedArchive {
        categories,
        tasks,
        attachments,
    })
}

fn validate_imported_data(
    categories: &[Category],
    tasks: &[Task],
    attachments: &[ImportedAttachment],
) -> Result<(), ArchiveError> {
    let referenced: HashSet<_> = tasks
        .iter()
        .flat_map(|task| task.body.iter())
        .filter_map(|block| match block {
            Block::Image { attachment_id } => Some(attachment_id.as_str()),
            _ => None,
        })
        .collect();
    let declared: HashSet<_> = attachments
        .iter()
        .map(|attachment| attachment.metadata.id.as_str())
        .collect();
    if referenced != declared {
        return Err(ArchiveError::Invalid(
            "archive image entries must exactly match task image references".into(),
        ));
    }
    let mut data = StoreData {
        revision: 0,
        categories: categories.to_vec(),
        tasks: tasks.to_vec(),
        settings: Settings::default(),
        attachments: attachments
            .iter()
            .map(|attachment| attachment.metadata.clone())
            .collect(),
    };
    data.validate_as_stored()
        .map_err(|error| ArchiveError::Invalid(format!("invalid archive data: {error}")))
}

fn plan_merge(
    current: &StoreData,
    imported: &ImportedArchive,
    path: &Path,
) -> Result<MergePlan, ArchiveError> {
    let current_categories: HashMap<_, _> = current
        .categories
        .iter()
        .map(|category| (category.id.as_str(), category))
        .collect();
    let current_category_names: HashMap<_, _> = current
        .categories
        .iter()
        .map(|category| (caseless_key(&category.name), category))
        .collect();
    let current_tasks: HashMap<_, _> = current
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect();
    let current_attachments: HashMap<_, _> = current
        .attachments()
        .iter()
        .map(|attachment| (attachment.id.as_str(), attachment))
        .collect();

    let mut merged = current.clone();
    let mut categories = Vec::new();
    let mut categories_unchanged = 0usize;
    for category in &imported.categories {
        if let Some(existing) = current_categories.get(category.id.as_str()).copied() {
            if existing == category {
                categories_unchanged += 1;
                continue;
            }
            return Err(ArchiveError::Conflict(format!(
                "category id {} conflicts with existing category {:?}",
                category.id, existing.name
            )));
        }
        if let Some(existing) = current_category_names
            .get(&caseless_key(&category.name))
            .copied()
        {
            return Err(ArchiveError::Conflict(format!(
                "category {:?} conflicts with existing category id {}",
                category.name, existing.id
            )));
        }
        categories.push(category.clone());
    }
    merged.categories.extend(categories.iter().cloned());

    let mut tasks = Vec::new();
    let mut tasks_unchanged = 0usize;
    for task in &imported.tasks {
        if let Some(existing) = current_tasks.get(task.id.as_str()).copied() {
            if existing == task {
                tasks_unchanged += 1;
                continue;
            }
            return Err(ArchiveError::Conflict(format!(
                "task id {} conflicts with existing task {:?}",
                task.id, existing.title
            )));
        }
        tasks.push(task.clone());
    }
    merged.tasks.extend(tasks.iter().cloned());

    let mut attachments = Vec::new();
    let mut images_unchanged = 0usize;
    for imported_attachment in &imported.attachments {
        let attachment = &imported_attachment.metadata;
        if let Some(existing) = current_attachments.get(attachment.id.as_str()).copied() {
            if existing == attachment {
                images_unchanged += 1;
                continue;
            }
            return Err(ArchiveError::Conflict(format!(
                "image attachment {} conflicts with existing metadata",
                attachment.id
            )));
        }
        attachments.push(attachment.clone());
    }
    merged.attachments.extend(attachments.iter().cloned());
    merged
        .attachments
        .sort_by(|left, right| left.id.cmp(&right.id));
    merged.validate_as_stored().map_err(|error| {
        ArchiveError::Conflict(format!("archive cannot be merged into this store: {error}"))
    })?;

    Ok(MergePlan {
        summary: ImportSummary {
            path: path.to_path_buf(),
            tasks_added: tasks.len(),
            tasks_unchanged,
            categories_added: categories.len(),
            categories_unchanged,
            images_added: attachments.len(),
            images_unchanged,
        },
        categories,
        tasks,
        attachments,
    })
}

fn stage_attachments(
    zip: &mut ZipArchive<File>,
    attachments: &[ImportedAttachment],
    directory: &Path,
) -> Result<Vec<(Attachment, PathBuf)>, ArchiveError> {
    let mut staged = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let path = directory.join(&attachment.metadata.storage_name);
        let mut output = create_private_file(&path)?;
        let mut entry = zip.by_name(&attachment.file)?;
        if entry.size() != attachment.metadata.byte_len {
            return Err(ArchiveError::Invalid(format!(
                "archive image {} has length {}, expected {}",
                attachment.metadata.id,
                entry.size(),
                attachment.metadata.byte_len
            )));
        }
        let mut hasher = Sha256::new();
        let mut byte_len = 0u64;
        let mut prefix = [0u8; 32];
        let mut prefix_len = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer).map_err(|source| {
                ArchiveError::Invalid(format!(
                    "could not read archive image {}: {source}",
                    attachment.metadata.id
                ))
            })?;
            if read == 0 {
                break;
            }
            byte_len = byte_len
                .checked_add(read as u64)
                .ok_or_else(|| ArchiveError::Invalid("archive image is too large".into()))?;
            if byte_len > attachment.metadata.byte_len {
                return Err(ArchiveError::Invalid(format!(
                    "archive image {} exceeds its declared length",
                    attachment.metadata.id
                )));
            }
            if prefix_len < prefix.len() {
                let count = (prefix.len() - prefix_len).min(read);
                prefix[prefix_len..prefix_len + count].copy_from_slice(&buffer[..count]);
                prefix_len += count;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|source| ArchiveError::io("stage archive image at", &path, source))?;
        }
        output
            .sync_all()
            .map_err(|source| ArchiveError::io("sync staged archive image", &path, source))?;
        if byte_len != attachment.metadata.byte_len {
            return Err(ArchiveError::Invalid(format!(
                "archive image {} has length {byte_len}, expected {}",
                attachment.metadata.id, attachment.metadata.byte_len
            )));
        }
        let hash = format!("{:x}", hasher.finalize());
        if hash != attachment.metadata.sha256 || hash != attachment.metadata.id {
            return Err(ArchiveError::Invalid(format!(
                "archive image {} failed SHA-256 verification",
                attachment.metadata.id
            )));
        }
        let media_type = media_type_for_image(&prefix[..prefix_len]).ok_or_else(|| {
            ArchiveError::Invalid(format!(
                "archive image {} is not PNG, JPEG, GIF, or WebP",
                attachment.metadata.id
            ))
        })?;
        if media_type != attachment.metadata.media_type {
            return Err(ArchiveError::Invalid(format!(
                "archive image {} has media type {media_type}, expected {}",
                attachment.metadata.id, attachment.metadata.media_type
            )));
        }
        crate::image::load_dynamic(&path).map_err(|error| {
            ArchiveError::Invalid(format!(
                "archive image {} could not be decoded: {error}",
                attachment.metadata.id
            ))
        })?;
        staged.push((attachment.metadata.clone(), path));
    }
    Ok(staged)
}

fn media_type_for_image(prefix: &[u8]) -> Option<&'static str> {
    match image::guess_format(prefix).ok()? {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

fn write_verified_attachment(
    source_path: &Path,
    attachment: &Attachment,
    writer: &mut ZipWriter<File>,
) -> Result<(), ArchiveError> {
    let mut source = File::open(source_path)
        .map_err(|error| ArchiveError::io("open managed image", source_path, error))?;
    let metadata = source
        .metadata()
        .map_err(|error| ArchiveError::io("inspect managed image", source_path, error))?;
    if !metadata.is_file() || metadata.len() != attachment.byte_len {
        return Err(ArchiveError::Invalid(format!(
            "managed image {} does not match its stored length",
            source_path.display()
        )));
    }
    let mut hasher = Sha256::new();
    let mut byte_len = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| ArchiveError::io("read managed image", source_path, error))?;
        if read == 0 {
            break;
        }
        byte_len += read as u64;
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read]).map_err(|error| {
            ArchiveError::io("write managed image to archive", source_path, error)
        })?;
    }
    let hash = format!("{:x}", hasher.finalize());
    if byte_len != attachment.byte_len || hash != attachment.sha256 || hash != attachment.id {
        return Err(ArchiveError::Invalid(format!(
            "managed image {} failed SHA-256 verification",
            source_path.display()
        )));
    }
    Ok(())
}

fn default_export_path() -> Result<PathBuf, ArchiveError> {
    let directory = std::env::current_dir().map_err(|source| {
        ArchiveError::io("read current directory for archive", Path::new("."), source)
    })?;
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let base = format!("mach-export-{timestamp}");
    for suffix in 0..10_000usize {
        let name = if suffix == 0 {
            format!("{base}.mach")
        } else {
            format!("{base}-{}.mach", suffix + 1)
        };
        let path = directory.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(ArchiveError::Invalid(
        "could not choose an unused archive filename".into(),
    ))
}

fn absolute_user_path(path: &Path) -> Result<PathBuf, ArchiveError> {
    let expanded = if path == Path::new("~") {
        dirs::home_dir()
            .ok_or_else(|| ArchiveError::Invalid("home directory is unavailable".into()))?
    } else if let Ok(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| ArchiveError::Invalid("home directory is unavailable".into()))?
            .join(rest)
    } else {
        path.to_path_buf()
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(expanded))
            .map_err(|source| ArchiveError::io("read current directory", Path::new("."), source))
    }
}

fn create_private_file(path: &Path) -> Result<File, ArchiveError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|source| ArchiveError::io("create private file", path, source))
}

fn sync_directory(path: &Path) -> Result<(), ArchiveError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ArchiveError::io("sync directory", path, source))?;
    }
    Ok(())
}

struct TempFile {
    path: Option<PathBuf>,
}

impl TempFile {
    fn create(path: &Path) -> Result<(Self, File), ArchiveError> {
        let file = create_private_file(path)?;
        Ok((
            Self {
                path: Some(path.to_path_buf()),
            },
            file,
        ))
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

struct StageDirectory {
    path: PathBuf,
}

impl StageDirectory {
    fn create(data_dir: &Path) -> Result<Self, ArchiveError> {
        let path = data_dir.join(format!(".mach-import-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)
            .map_err(|source| ArchiveError::io("create import staging directory", &path, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|source| {
                ArchiveError::io("set import staging permissions on", &path, source)
            })?;
        }
        Ok(Self { path })
    }
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mach-archive-{label}-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store_with_image(directory: &Path) -> Store {
        let source = directory.join("source.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([20, 40, 60, 255]))
            .save(&source)
            .unwrap();
        let mut store = Store::open(directory.join("store")).unwrap();
        store
            .update(|data| {
                let mut task = Task::new("with image", 0, None, "");
                task.body = vec![Block::image(source.to_str().unwrap())];
                data.insert_task(task)?;
                Ok(())
            })
            .unwrap();
        store
    }

    #[test]
    fn tampered_image_is_rejected_before_the_destination_changes() {
        let source_directory = TestDirectory::new("tampered-source");
        let destination_directory = TestDirectory::new("tampered-destination");
        let output_directory = TestDirectory::new("tampered-output");
        let source = store_with_image(&source_directory.0);
        let attachment = source.snapshot().unwrap().attachments()[0].clone();
        let archive_path = output_directory.0.join("tasks.mach");
        export(&source, Some(&archive_path)).unwrap();

        let offset = {
            let file = File::open(&archive_path).unwrap();
            let mut archive = ZipArchive::new(file).unwrap();
            archive
                .by_name(&format!("images/{}", attachment.storage_name))
                .unwrap()
                .data_start()
                .unwrap()
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&archive_path)
            .unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xff;
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_all().unwrap();

        let mut destination = Store::open(&destination_directory.0).unwrap();
        let before = destination.snapshot().unwrap();
        let error = import(&mut destination, &archive_path).unwrap_err();
        assert_eq!(error.kind(), "archive", "{error}");
        let after = destination.snapshot().unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.tasks, before.tasks);
        assert!(after.attachments().is_empty());
        assert!(fs::read_dir(destination.data_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".mach-import-")
        }));
    }

    #[test]
    fn caseless_category_name_conflict_aborts_the_merge() {
        let source_directory = TestDirectory::new("category-source");
        let destination_directory = TestDirectory::new("category-destination");
        let output_directory = TestDirectory::new("category-output");
        let mut source = Store::open(&source_directory.0).unwrap();
        source
            .update(|data| {
                data.create_category("Work", "from archive")?;
                Ok(())
            })
            .unwrap();
        let archive_path = output_directory.0.join("tasks.mach");
        export(&source, Some(&archive_path)).unwrap();

        let mut destination = Store::open(&destination_directory.0).unwrap();
        destination
            .update(|data| {
                data.create_category("work", "already here")?;
                Ok(())
            })
            .unwrap();
        let before = destination.snapshot().unwrap();
        let error = import(&mut destination, &archive_path).unwrap_err();
        assert_eq!(error.kind(), "conflict");
        let after = destination.snapshot().unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.categories, before.categories);
    }
}
