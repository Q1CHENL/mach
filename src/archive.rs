//! Portable, versioned task archives shared by the CLI and TUI.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use chrono::Local;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::model::{
    Block, Category, MAX_BODY_LINES, MAX_CATEGORY_COUNT, MAX_CATEGORY_DESC_LINE_LEN,
    MAX_CATEGORY_DESC_LINES, MAX_CATEGORY_NAME_LEN, MAX_NOTES_LINE_LEN, MAX_TASK_COUNT,
    MAX_TITLE_LEN, Task, caseless_key, text_byte_limit,
};
use crate::settings::Settings;
use crate::store::{
    ATTACHMENT_ID_LEN, Attachment, CREATED_MAX_BYTES, DUE_MAX_BYTES, ID_MAX_BYTES,
    StagedAttachment, Store, StoreData, StoreError,
};

const ARCHIVE_FORMAT: &str = "mach-archive";
const ARCHIVE_SCHEMA: u32 = 1;
const MANIFEST_PATH: &str = "manifest.json";
const JSON_ESCAPE_EXPANSION: u64 = 6;
const MAX_ARCHIVE_BLOCK_COUNT: usize = MAX_TASK_COUNT * MAX_BODY_LINES;
const MAX_ARCHIVE_ATTACHMENT_COUNT: usize = MAX_ARCHIVE_BLOCK_COUNT;
const MAX_ARCHIVE_FORMAT_BYTES: usize = 64;
const MAX_ARCHIVE_MEDIA_TYPE_BYTES: usize = 32;
const MAX_ARCHIVE_FILE_BYTES: usize = "images/".len() + ATTACHMENT_ID_LEN + ".webp".len();
const MAX_CATEGORY_DESCRIPTION_BYTES: u64 =
    MAX_CATEGORY_DESC_LINES as u64 * (text_byte_limit(MAX_CATEGORY_DESC_LINE_LEN) as u64 + 1);
const MAX_JSON_STRING_BYTES: u64 = MAX_CATEGORY_DESCRIPTION_BYTES * JSON_ESCAPE_EXPANSION;
const MAX_MANIFEST_STRING_BYTES: u64 = ARCHIVE_FORMAT.len() as u64
    + MAX_CATEGORY_COUNT as u64
        * (ID_MAX_BYTES as u64
            + text_byte_limit(MAX_CATEGORY_NAME_LEN) as u64
            + MAX_CATEGORY_DESCRIPTION_BYTES)
    + MAX_TASK_COUNT as u64
        * (ID_MAX_BYTES as u64
            + text_byte_limit(MAX_TITLE_LEN) as u64
            + DUE_MAX_BYTES as u64
            + CREATED_MAX_BYTES as u64
            + ID_MAX_BYTES as u64)
    + MAX_ARCHIVE_BLOCK_COUNT as u64 * text_byte_limit(MAX_NOTES_LINE_LEN) as u64
    + MAX_ARCHIVE_ATTACHMENT_COUNT as u64
        * ((ATTACHMENT_ID_LEN * 3 + "images/".len() + ".webp".len()) as u64
            + "image/jpeg".len() as u64);
// Pretty JSON punctuation, field names, booleans, numbers, indentation, and
// newlines stay well below 1 KiB per logical record. Keeping that allowance
// separate makes the text escaping bound auditable against Store limits.
const MAX_MANIFEST_STRUCTURE_BYTES: u64 = (1
    + MAX_CATEGORY_COUNT
    + MAX_TASK_COUNT
    + MAX_ARCHIVE_BLOCK_COUNT
    + MAX_ARCHIVE_ATTACHMENT_COUNT) as u64
    * 1024;
const MAX_MANIFEST_BYTES: u64 =
    MAX_MANIFEST_STRING_BYTES * JSON_ESCAPE_EXPANSION + MAX_MANIFEST_STRUCTURE_BYTES + 1;

#[derive(Debug)]
pub(crate) enum ArchiveError {
    Cancelled,
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
            Self::Cancelled => "cancelled",
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
            Self::Cancelled => f.write_str("archive operation cancelled"),
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
            Self::Cancelled => None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveProgress {
    Preparing,
    Attachments { completed: usize, total: usize },
    Finalizing,
}

const ARCHIVE_ACTIVE: u8 = 0;
const ARCHIVE_CANCELLED: u8 = 1;
const ARCHIVE_FINALIZING: u8 = 2;

#[derive(Debug)]
pub(crate) struct ArchiveControl {
    state: AtomicU8,
}

impl ArchiveControl {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(ARCHIVE_ACTIVE),
        }
    }

    /// Returns false once the worker has atomically crossed the point where
    /// cancellation could race an archive installation or database commit.
    pub(crate) fn request_cancel(&self) -> bool {
        match self.state.compare_exchange(
            ARCHIVE_ACTIVE,
            ARCHIVE_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(ARCHIVE_CANCELLED) => true,
            Err(ARCHIVE_FINALIZING) => false,
            Err(other) => unreachable!("invalid archive control state {other}"),
        }
    }

    fn check_cancelled(&self) -> Result<(), ArchiveError> {
        if self.state.load(Ordering::Acquire) == ARCHIVE_CANCELLED {
            Err(ArchiveError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn begin_finalizing(&self) -> Result<(), ArchiveError> {
        match self.state.compare_exchange(
            ARCHIVE_ACTIVE,
            ARCHIVE_FINALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(ARCHIVE_FINALIZING) => Ok(()),
            Err(ARCHIVE_CANCELLED) => Err(ArchiveError::Cancelled),
            Err(other) => unreachable!("invalid archive control state {other}"),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARCHIVE_CANCELLED
    }
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

struct ByteLimitWriter<W> {
    inner: W,
    limit: u64,
    written: u64,
}

impl<W> ByteLimitWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            limit,
            written: 0,
        }
    }
}

impl<W: Write> Write for ByteLimitWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let requested = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("archive manifest write length overflow"))?;
        if self.written.saturating_add(requested) > self.limit {
            return Err(manifest_limit_error());
        }
        let written = self.inner.write(buffer)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct ByteLimitReader<R> {
    inner: R,
    limit: u64,
    read: u64,
}

struct CancellationReader<'a, R> {
    inner: R,
    control: &'a ArchiveControl,
}

impl<'a, R> CancellationReader<'a, R> {
    fn new(inner: R, control: &'a ArchiveControl) -> Self {
        Self { inner, control }
    }
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.control.is_cancelled() {
            return Err(cancelled_io_error());
        }
        self.inner.read(buffer)
    }
}

struct CancellationWriter<'a, W> {
    inner: W,
    control: &'a ArchiveControl,
}

impl<'a, W> CancellationWriter<'a, W> {
    fn new(inner: W, control: &'a ArchiveControl) -> Self {
        Self { inner, control }
    }
}

impl<W: Write> Write for CancellationWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.control.is_cancelled() {
            return Err(cancelled_io_error());
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.control.is_cancelled() {
            return Err(cancelled_io_error());
        }
        self.inner.flush()
    }
}

struct JsonStringLimitReader<R> {
    inner: R,
    limit: u64,
    in_string: bool,
    escaped: bool,
    string_bytes: u64,
}

impl<R> JsonStringLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            in_string: false,
            escaped: false,
            string_bytes: 0,
        }
    }
}

impl<R: Read> Read for JsonStringLimitReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        for byte in &buffer[..read] {
            if !self.in_string {
                if *byte == b'"' {
                    self.in_string = true;
                    self.escaped = false;
                    self.string_bytes = 0;
                }
                continue;
            }
            if !self.escaped && *byte == b'"' {
                self.in_string = false;
                continue;
            }
            self.string_bytes = self.string_bytes.saturating_add(1);
            if self.string_bytes > self.limit {
                return Err(io::Error::other(format!(
                    "archive manifest contains a JSON string exceeding {} bytes",
                    self.limit
                )));
            }
            self.escaped = !self.escaped && *byte == b'\\';
        }
        Ok(read)
    }
}

impl<R> ByteLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            read: 0,
        }
    }
}

impl<R: Read> Read for ByteLimitReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.read < self.limit {
            let remaining = self.limit - self.read;
            let allowed = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("read allowance cannot exceed the caller's buffer");
            let read = self.inner.read(&mut buffer[..allowed])?;
            self.read += read as u64;
            return Ok(read);
        }

        let mut probe = [0u8; 1];
        match self.inner.read(&mut probe)? {
            0 => Ok(0),
            _ => Err(manifest_limit_error()),
        }
    }
}

fn manifest_limit_error() -> io::Error {
    io::Error::other(format!(
        "archive manifest exceeds {} MiB",
        MAX_MANIFEST_BYTES / 1024 / 1024
    ))
}

fn cancelled_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "archive operation cancelled")
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(deserialize_with = "deserialize_archive_format")]
    format: String,
    schema: u32,
    #[serde(deserialize_with = "deserialize_categories")]
    categories: Vec<ArchiveCategory>,
    #[serde(deserialize_with = "deserialize_tasks")]
    tasks: Vec<ArchiveTask>,
    #[serde(deserialize_with = "deserialize_attachments")]
    attachments: Vec<ArchiveAttachment>,
}

#[derive(Serialize)]
struct ExportManifest<'a> {
    format: &'static str,
    schema: u32,
    categories: Vec<ExportCategory<'a>>,
    tasks: Vec<ExportTask<'a>>,
    attachments: Vec<ExportAttachment<'a>>,
}

#[derive(Serialize)]
struct ExportCategory<'a> {
    id: &'a str,
    name: &'a str,
    description: &'a str,
}

impl<'a> From<&'a Category> for ExportCategory<'a> {
    fn from(category: &'a Category) -> Self {
        Self {
            id: &category.id,
            name: &category.name,
            description: &category.description,
        }
    }
}

#[derive(Serialize)]
struct ExportTask<'a> {
    id: &'a str,
    title: &'a str,
    body: &'a [Block],
    due: &'a str,
    created: &'a str,
    done: bool,
    importance: u8,
    category_id: Option<&'a str>,
}

impl<'a> From<&'a Task> for ExportTask<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            id: &task.id,
            title: &task.title,
            body: &task.body,
            due: &task.due,
            created: &task.created,
            done: task.done,
            importance: task.importance,
            category_id: task.category_id.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct ExportAttachment<'a> {
    #[serde(skip)]
    metadata: &'a Attachment,
    id: &'a str,
    sha256: &'a str,
    media_type: &'a str,
    byte_len: u64,
    file: String,
}

impl<'a> ExportAttachment<'a> {
    fn from_store(attachment: &'a Attachment) -> Self {
        Self {
            metadata: attachment,
            id: &attachment.id,
            sha256: &attachment.sha256,
            media_type: &attachment.media_type,
            byte_len: attachment.byte_len,
            file: format!("images/{}", attachment.storage_name),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveCategory {
    #[serde(deserialize_with = "deserialize_id")]
    id: String,
    #[serde(deserialize_with = "deserialize_category_name")]
    name: String,
    #[serde(deserialize_with = "deserialize_category_description")]
    description: String,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveTask {
    #[serde(deserialize_with = "deserialize_id")]
    id: String,
    #[serde(deserialize_with = "deserialize_task_title")]
    title: String,
    #[serde(deserialize_with = "deserialize_blocks")]
    body: Vec<ArchiveBlock>,
    #[serde(deserialize_with = "deserialize_due")]
    due: String,
    #[serde(deserialize_with = "deserialize_created")]
    created: String,
    done: bool,
    importance: u8,
    #[serde(deserialize_with = "deserialize_optional_id")]
    category_id: Option<String>,
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ArchiveBlock {
    Text {
        #[serde(deserialize_with = "deserialize_block_value")]
        text: String,
    },
    Todo {
        #[serde(deserialize_with = "deserialize_block_value")]
        text: String,
        done: bool,
    },
    Bullet {
        #[serde(deserialize_with = "deserialize_block_value")]
        text: String,
    },
    Number {
        #[serde(deserialize_with = "deserialize_block_value")]
        text: String,
    },
    Link {
        #[serde(deserialize_with = "deserialize_block_value")]
        url: String,
    },
    Image {
        #[serde(deserialize_with = "deserialize_block_value")]
        attachment_id: String,
    },
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveAttachment {
    #[serde(deserialize_with = "deserialize_attachment_id")]
    id: String,
    #[serde(deserialize_with = "deserialize_attachment_id")]
    sha256: String,
    #[serde(deserialize_with = "deserialize_media_type")]
    media_type: String,
    byte_len: u64,
    #[serde(deserialize_with = "deserialize_archive_file")]
    file: String,
}

impl ArchiveAttachment {
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

fn deserialize_archive_format<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, MAX_ARCHIVE_FORMAT_BYTES, "archive format")
}

fn deserialize_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, ID_MAX_BYTES, "record id")
}

fn deserialize_optional_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if value
        .as_ref()
        .is_some_and(|value| value.len() > ID_MAX_BYTES)
    {
        return Err(de::Error::custom(format!(
            "optional record id exceeds {ID_MAX_BYTES} bytes"
        )));
    }
    Ok(value)
}

fn deserialize_category_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        text_byte_limit(MAX_CATEGORY_NAME_LEN),
        "category name",
    )
}

fn deserialize_category_description<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        MAX_CATEGORY_DESCRIPTION_BYTES as usize,
        "category description",
    )
}

fn deserialize_task_title<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, text_byte_limit(MAX_TITLE_LEN), "task title")
}

fn deserialize_due<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, DUE_MAX_BYTES, "task due value")
}

fn deserialize_created<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, CREATED_MAX_BYTES, "task creation timestamp")
}

fn deserialize_block_value<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        text_byte_limit(MAX_NOTES_LINE_LEN),
        "task body value",
    )
}

fn deserialize_attachment_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        ATTACHMENT_ID_LEN,
        "attachment content address",
    )
}

fn deserialize_media_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        MAX_ARCHIVE_MEDIA_TYPE_BYTES,
        "attachment media type",
    )
}

fn deserialize_archive_file<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(
        deserializer,
        MAX_ARCHIVE_FILE_BYTES,
        "attachment archive path",
    )
}

fn deserialize_bounded_string<'de, D>(
    deserializer: D,
    limit: usize,
    label: &'static str,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedStringVisitor {
        limit: usize,
        label: &'static str,
    }

    impl<'de> Visitor<'de> for BoundedStringVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "{} no longer than {} bytes",
                self.label, self.limit
            )
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value.len() > self.limit {
                return Err(E::custom(format!(
                    "{} exceeds {} bytes",
                    self.label, self.limit
                )));
            }
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value.len() > self.limit {
                return Err(E::custom(format!(
                    "{} exceeds {} bytes",
                    self.label, self.limit
                )));
            }
            Ok(value)
        }
    }

    deserializer.deserialize_string(BoundedStringVisitor { limit, label })
}

fn deserialize_categories<'de, D>(deserializer: D) -> Result<Vec<ArchiveCategory>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_CATEGORY_COUNT, "archive category")
}

fn deserialize_tasks<'de, D>(deserializer: D) -> Result<Vec<ArchiveTask>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_TASK_COUNT, "archive task")
}

fn deserialize_blocks<'de, D>(deserializer: D) -> Result<Vec<ArchiveBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_BODY_LINES, "task body block")
}

fn deserialize_attachments<'de, D>(deserializer: D) -> Result<Vec<ArchiveAttachment>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_ARCHIVE_ATTACHMENT_COUNT,
        "archive attachment",
    )
}

fn deserialize_bounded_vec<'de, D, T>(
    deserializer: D,
    limit: usize,
    label: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVecVisitor<T> {
        limit: usize,
        label: &'static str,
        marker: PhantomData<T>,
    }

    impl<'de, T> Visitor<'de> for BoundedVecVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {} {} records", self.limit, self.label)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let capacity = sequence.size_hint().unwrap_or(0).min(self.limit);
            let mut values = Vec::with_capacity(capacity);
            while let Some(value) = sequence.next_element()? {
                if values.len() == self.limit {
                    return Err(de::Error::custom(format!(
                        "{} count exceeds {}",
                        self.label, self.limit
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor {
        limit,
        label,
        marker: PhantomData,
    })
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
    summary: ImportSummary,
}

pub(crate) fn export(
    store: &Store,
    requested_path: Option<&Path>,
) -> Result<ExportSummary, ArchiveError> {
    let control = ArchiveControl::new();
    export_with_progress(store, requested_path, &control, |_| {})
}

pub(crate) fn export_with_progress(
    store: &Store,
    requested_path: Option<&Path>,
    control: &ArchiveControl,
    mut progress: impl FnMut(ArchiveProgress),
) -> Result<ExportSummary, ArchiveError> {
    progress(ArchiveProgress::Preparing);
    control.check_cancelled()?;
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
    let manifest = manifest_from_snapshot(&snapshot)?;
    control.check_cancelled()?;

    let temp_path = parent.join(format!(".mach-export-{}.tmp", uuid::Uuid::new_v4()));
    let (mut temp_guard, file) = TempFile::create(&temp_path)?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o600);
    writer.start_file(MANIFEST_PATH, options.large_file(true))?;
    {
        let cancellation_writer = CancellationWriter::new(&mut writer, control);
        let mut manifest_writer = ByteLimitWriter::new(cancellation_writer, MAX_MANIFEST_BYTES);
        if let Err(error) = serde_json::to_writer_pretty(&mut manifest_writer, &manifest) {
            control.check_cancelled()?;
            return Err(error.into());
        }
        if let Err(source) = manifest_writer.write_all(b"\n") {
            control.check_cancelled()?;
            return Err(ArchiveError::io(
                "write archive manifest to",
                &temp_path,
                source,
            ));
        }
    }

    let attachment_total = manifest.attachments.len();
    progress(ArchiveProgress::Attachments {
        completed: 0,
        total: attachment_total,
    });
    for (index, attachment) in manifest.attachments.iter().enumerate() {
        control.check_cancelled()?;
        let source_path = store.images_dir().join(&attachment.metadata.storage_name);
        writer.start_file(&attachment.file, options)?;
        write_verified_attachment(&source_path, attachment.metadata, &mut writer, control)?;
        progress(ArchiveProgress::Attachments {
            completed: index + 1,
            total: attachment_total,
        });
    }
    control.begin_finalizing()?;
    progress(ArchiveProgress::Finalizing);
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
        images: manifest.attachments.len(),
    })
}

pub(crate) fn import(
    store: &mut Store,
    requested_path: &Path,
) -> Result<ImportOutcome, ArchiveError> {
    let control = ArchiveControl::new();
    import_with_progress(store, requested_path, &control, |_| {})
}

pub(crate) fn import_with_progress(
    store: &mut Store,
    requested_path: &Path,
    control: &ArchiveControl,
    mut progress: impl FnMut(ArchiveProgress),
) -> Result<ImportOutcome, ArchiveError> {
    progress(ArchiveProgress::Preparing);
    control.check_cancelled()?;
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
    let names = inspect_entries(&mut zip, control)?;
    let imported = read_manifest(&mut zip, &names, control)?;
    control.check_cancelled()?;
    let current = store.snapshot()?;
    let mut plan = plan_merge(&current, &imported, &path)?;

    let stage = StageDirectory::create(store.data_dir())?;
    let staged = stage_attachments(
        &mut zip,
        &imported.attachments,
        &stage.path,
        control,
        &mut progress,
    )?;

    control.begin_finalizing()?;
    progress(ArchiveProgress::Finalizing);
    if !plan.summary.changed() {
        return Ok(ImportOutcome {
            summary: plan.summary,
        });
    }

    let expected_revision = current.revision;
    let summary = plan.summary.clone();
    let (summary, _) =
        store.update_if_revision_with_staged_attachments(expected_revision, &staged, |data| {
            data.categories.append(&mut plan.categories);
            data.tasks.append(&mut plan.tasks);
            Ok(summary)
        })?;
    Ok(ImportOutcome { summary })
}

fn manifest_from_snapshot(snapshot: &StoreData) -> Result<ExportManifest<'_>, ArchiveError> {
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
    let attachments = referenced
        .into_iter()
        .map(|id| {
            let attachment = by_id.get(id).copied().ok_or_else(|| {
                ArchiveError::Invalid(format!("task refers to unknown image attachment {id}"))
            })?;
            Ok(ExportAttachment::from_store(attachment))
        })
        .collect::<Result<_, ArchiveError>>()?;
    Ok(ExportManifest {
        format: ARCHIVE_FORMAT,
        schema: ARCHIVE_SCHEMA,
        categories: snapshot
            .categories
            .iter()
            .map(ExportCategory::from)
            .collect(),
        tasks: snapshot.tasks.iter().map(ExportTask::from).collect(),
        attachments,
    })
}

fn inspect_entries(
    zip: &mut ZipArchive<File>,
    control: &ArchiveControl,
) -> Result<HashSet<String>, ArchiveError> {
    if zip.len() > MAX_ARCHIVE_ATTACHMENT_COUNT + 1 {
        return Err(ArchiveError::Invalid(format!(
            "archive contains more than {} entries",
            MAX_ARCHIVE_ATTACHMENT_COUNT + 1
        )));
    }
    let mut names = HashSet::with_capacity(zip.len());
    for index in 0..zip.len() {
        control.check_cancelled()?;
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
    control: &ArchiveControl,
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
        let cancellation_reader = CancellationReader::new(&mut entry, control);
        let byte_reader = ByteLimitReader::new(cancellation_reader, MAX_MANIFEST_BYTES);
        let mut reader = JsonStringLimitReader::new(byte_reader, MAX_JSON_STRING_BYTES);
        match serde_json::from_reader::<_, Manifest>(&mut reader) {
            Ok(manifest) => manifest,
            Err(error) => {
                control.check_cancelled()?;
                return Err(error.into());
            }
        }
    };
    control.check_cancelled()?;
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
    })
}

fn stage_attachments(
    zip: &mut ZipArchive<File>,
    attachments: &[ImportedAttachment],
    directory: &Path,
    control: &ArchiveControl,
    progress: &mut impl FnMut(ArchiveProgress),
) -> Result<Vec<StagedAttachment>, ArchiveError> {
    let mut staged = Vec::with_capacity(attachments.len());
    progress(ArchiveProgress::Attachments {
        completed: 0,
        total: attachments.len(),
    });
    for (index, attachment) in attachments.iter().enumerate() {
        control.check_cancelled()?;
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
            control.check_cancelled()?;
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
        control.check_cancelled()?;
        staged.push(StagedAttachment {
            metadata: attachment.metadata.clone(),
            path,
        });
        progress(ArchiveProgress::Attachments {
            completed: index + 1,
            total: attachments.len(),
        });
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
    control: &ArchiveControl,
) -> Result<(), ArchiveError> {
    control.check_cancelled()?;
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
        control.check_cancelled()?;
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
    fn finalization_and_cancellation_have_one_atomic_winner() {
        let cancelled = ArchiveControl::new();
        assert!(cancelled.request_cancel());
        assert!(matches!(
            cancelled.begin_finalizing(),
            Err(ArchiveError::Cancelled)
        ));

        let finalizing = ArchiveControl::new();
        finalizing.begin_finalizing().unwrap();
        assert!(!finalizing.request_cancel());
        assert!(finalizing.check_cancelled().is_ok());
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

    #[test]
    fn manifest_limit_covers_every_store_valid_task_body() {
        let maximum_raw_body_bytes = crate::model::MAX_TASK_COUNT as u64
            * crate::model::MAX_BODY_LINES as u64
            * crate::model::text_byte_limit(crate::model::MAX_NOTES_LINE_LEN) as u64;

        let escaped_body_bytes = maximum_raw_body_bytes * 6;

        assert!(
            MAX_MANIFEST_BYTES > escaped_body_bytes,
            "the archive ceiling must cover worst-case JSON escaping plus metadata"
        );
    }

    #[test]
    fn manifest_stream_writer_enforces_its_byte_ceiling() {
        let mut output = Vec::new();
        let mut writer = ByteLimitWriter::new(&mut output, 5);

        writer.write_all(b"12345").unwrap();
        let error = writer
            .write_all(b"6")
            .expect_err("the writer must reject bytes beyond its ceiling");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(output, b"12345");
    }

    #[test]
    fn manifest_stream_reader_rejects_input_beyond_its_byte_ceiling() {
        let mut reader = ByteLimitReader::new(&b"123456"[..], 5);
        let mut output = Vec::new();

        let error = reader
            .read_to_end(&mut output)
            .expect_err("the reader must probe for and reject excess bytes");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(output, b"12345");
    }

    #[test]
    fn manifest_stream_reader_bounds_each_json_string_before_deserialization() {
        let input = br#"{"field":"123456"}"#;
        let mut reader = JsonStringLimitReader::new(&input[..], 5);

        let error = serde_json::from_reader::<_, serde_json::Value>(&mut reader)
            .expect_err("an oversized JSON string must be rejected while reading");

        assert!(error.is_io());
        assert!(error.to_string().contains("JSON string exceeding 5 bytes"));
    }

    #[test]
    fn manifest_deserialization_bounds_task_count_during_sequence_growth() {
        let tasks: Vec<_> = (0..=MAX_TASK_COUNT)
            .map(|index| {
                serde_json::json!({
                    "id": format!("task-{index}"),
                    "title": "task",
                    "body": [],
                    "due": "",
                    "created": "2026-08-10 00:00:00",
                    "done": false,
                    "importance": 0,
                    "category_id": null
                })
            })
            .collect();
        let manifest = serde_json::json!({
            "format": ARCHIVE_FORMAT,
            "schema": ARCHIVE_SCHEMA,
            "categories": [],
            "tasks": tasks,
            "attachments": []
        });

        let error = serde_json::from_value::<Manifest>(manifest)
            .expect_err("the deserializer must stop beyond the Store task count");

        assert!(error.to_string().contains("archive task count exceeds"));
    }

    #[test]
    fn manifest_deserialization_rejects_oversized_fields_before_accumulating_them() {
        let manifest = serde_json::json!({
            "format": ARCHIVE_FORMAT,
            "schema": ARCHIVE_SCHEMA,
            "categories": [],
            "tasks": [{
                "id": "task-1",
                "title": "task",
                "body": [{"type": "text", "text": "x".repeat(text_byte_limit(MAX_NOTES_LINE_LEN) + 1)}],
                "due": "",
                "created": "2026-08-10 00:00:00",
                "done": false,
                "importance": 0,
                "category_id": null
            }],
            "attachments": []
        });

        let error = serde_json::from_value::<Manifest>(manifest)
            .expect_err("invalid field bytes must be rejected during deserialization");

        assert!(error.to_string().contains("task body value exceeds"));
    }
}
