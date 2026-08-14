//! Application state and every operation the UI can trigger.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::Utc;
use ratatui::layout::Rect;
use ratatui::widgets::{ListState, TableState};
use unicode_segmentation::UnicodeSegmentation;

use crate::due;
use crate::form::{CategoryForm, TaskDraft, TaskForm};
use crate::image::ImageStore;
use crate::model::{
    ALL_CATEGORY, Category, Label, LabelColor, MAX_CATEGORY_COUNT, MAX_CATEGORY_NAME_LEN,
    MAX_LABEL_COUNT, MAX_LABEL_NAME_LEN, MAX_TASK_COUNT, MAX_TITLE_LEN, Task, caseless_contains,
    caseless_key, category_name_key, task_text_contains,
};
use crate::settings::{LaunchState, Settings};
use crate::store::{
    Attachment, CategoryPatch, LabelPatch, RelativePosition, Store, StoreData, StoreError,
    TaskPatch,
};
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::update::{CheckFailure, CheckResponse};
use crate::update_state::{
    AutomaticClaim, FAILURE_RETRY_SECONDS, LEASE_SECONDS, UpdateLease, UpdateState,
    UpdateStateStore,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Tasks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// The `/` command palette (dropdown above the status bar).
    Slash,
    /// Live search after choosing Search from the palette.
    Search,
    /// The task dialog (new or edit).
    TaskForm,
    /// The category dialog (new or edit).
    CategoryForm,
    /// Global reusable label manager.
    Labels,
    Help,
    Settings,
    Welcome,
    WhatsNew,
}

impl Mode {
    /// The bottom command bar, rather than either list panel, owns input.
    pub fn command_bar_focused(self) -> bool {
        matches!(self, Mode::Slash | Mode::Search)
    }

    /// Anything drawn on top of the two panels.
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            Mode::Help
                | Mode::Settings
                | Mode::Welcome
                | Mode::WhatsNew
                | Mode::TaskForm
                | Mode::CategoryForm
                | Mode::Labels
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageLifetime {
    Brief,
    Standard,
    Long,
}

impl MessageLifetime {
    const fn duration(self) -> Duration {
        match self {
            Self::Brief => Duration::from_secs(2),
            Self::Standard => Duration::from_secs(4),
            Self::Long => Duration::from_secs(8),
        }
    }
}

pub struct Message {
    pub text: String,
    pub kind: MessageKind,
    pub until: Instant,
}

/// Something destructive waiting on a second press of the same key. Only
/// one can be armed at a time, so a half-typed delete cannot survive
/// behind a quit prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Backspace again deletes this exact task.
    DeleteTask(String),
    /// Backspace again deletes this exact category; its tasks become uncategorized.
    DeleteCategory(String),
    /// Enter purges this exact set of completed task ids.
    Purge(Vec<String>),
    /// Esc again discards the current task/category draft.
    DiscardTask(Option<String>),
    DiscardCategory(Option<String>),
    /// Backspace again deletes this exact label and unassigns it everywhere.
    DeleteLabel(String),
    /// Ctrl+C again leaves mach.
    Quit,
}

/// Idle gap after which type-to-jump starts a new query.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(800);
/// Bound free-form commands while leaving room for full filesystem paths.
const MAX_SLASH_INPUT_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateJobKind {
    Automatic,
    Install,
}

enum UpdateOutcome {
    Automatic(CheckResponse),
    UpToDate {
        info: crate::update::CheckResult,
        etag: Option<String>,
    },
    Installed {
        result: crate::update::InstallResult,
        info: crate::update::CheckResult,
        etag: Option<String>,
    },
    InstallFailed {
        message: String,
        info: crate::update::CheckResult,
        etag: Option<String>,
    },
}

enum UpdateEvent {
    DownloadProgress(crate::update::DownloadProgress),
    Finished(Box<Result<UpdateOutcome, CheckFailure>>),
}

struct UpdateJob {
    rx: Receiver<UpdateEvent>,
    kind: UpdateJobKind,
    lease: Option<UpdateLease>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveJobKind {
    Export,
    Import,
}

impl ArchiveJobKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Import => "import",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Export => "Export",
            Self::Import => "Import",
        }
    }
}

enum ArchiveOutcome {
    Export(crate::archive::ExportSummary),
    Import(crate::archive::ImportSummary),
}

enum ArchiveRequest {
    Export,
    Import(PathBuf),
}

impl ArchiveRequest {
    const fn kind(&self) -> ArchiveJobKind {
        match self {
            Self::Export => ArchiveJobKind::Export,
            Self::Import(_) => ArchiveJobKind::Import,
        }
    }
}

enum ArchiveEvent {
    Progress(crate::archive::ArchiveProgress),
    Finished(Result<ArchiveOutcome, crate::archive::ArchiveError>),
}

struct ArchiveJob {
    rx: Receiver<ArchiveEvent>,
    handle: std::thread::JoinHandle<()>,
    kind: ArchiveJobKind,
    control: Arc<crate::archive::ArchiveControl>,
    progress: crate::archive::ArchiveProgress,
    cancel_requested: bool,
}

struct UpdateNotice {
    text: String,
    available_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateActivity {
    Checking,
    Downloading(crate::update::DownloadProgress),
}

/// Rects from the last frame, used to hit-test mouse events.
#[derive(Debug, Default, Clone)]
pub struct Areas {
    pub sidebar: Rect,
    pub tasks: Rect,
    /// Inner row of the bottom command bar, including its clock.
    pub command_bar: Rect,
    /// Bottom-right task preview / docked editor, when the window is tall enough.
    pub preview: Rect,
    /// Screen columns of the flag and done markers, as the table laid
    /// them out. `done_x` is the left edge of the `[ ]`/`[✓]` column
    /// (see `ui::DONE_MARK_WIDTH`). The flag column is always reserved
    /// for up to three flags.
    pub flag_x: Option<u16>,
    pub done_x: Option<u16>,
    /// Open top-level command palette, including its border.
    pub slash_menu: Rect,
    /// Index of the first command drawn inside a clipped command palette.
    pub slash_menu_start: usize,
    /// Final badge rectangles in the global label manager.
    pub label_hits: Vec<(usize, Rect)>,
    /// Name field and selectable color swatches in the label editor.
    pub label_name_input: Rect,
    pub label_color_hits: Vec<(LabelColor, Rect)>,
}

#[derive(Debug, Clone)]
pub struct LabelEditor {
    pub editing_id: Option<String>,
    pub name: TextInput,
    pub color: LabelColor,
    pub color_focused: bool,
}

impl LabelEditor {
    fn new(editing_id: Option<String>, name: &str, color: LabelColor) -> Self {
        Self {
            editing_id,
            name: TextInput::new(name, MAX_LABEL_NAME_LEN),
            color,
            color_focused: false,
        }
    }

    pub(crate) fn move_color(&mut self, delta: isize) {
        let len = LabelColor::SWATCHES.len();
        let position = LabelColor::SWATCHES
            .iter()
            .position(|color| *color == self.color);
        let next = match position {
            Some(position) => (position as isize + delta).rem_euclid(len as isize) as usize,
            None if delta.is_negative() => len - 1,
            None => 0,
        };
        self.color = LabelColor::SWATCHES[next];
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickTarget {
    Sidebar,
    Tasks,
    Labels,
}

pub const SETTINGS_ITEMS: [&str; 4] = ["Sort", "Theme", "Date format", "Task preview"];

/// One row of the task table: a real task, or a category section header
/// (All Tasks / search only). Headers are not selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskListRow {
    Separator {
        title: String,
    },
    /// Index into [`App::view`].
    Task(usize),
}

pub struct App {
    store: Store,
    version: String,
    store_revision: u64,
    pub tasks: Vec<Task>,
    pub categories: Vec<Category>,
    pub labels: Vec<Label>,
    pub settings: Settings,
    pub focus: Focus,
    pub mode: Mode,
    /// Index into `categories`.
    pub cat_index: usize,
    /// Index into `view`.
    pub task_index: usize,
    /// Scroll position of the two panels. Selection is driven by the two
    /// indices above; ratatui keeps the offsets in these.
    pub cat_state: ListState,
    pub task_state: TableState,
    /// Indices into `tasks`, in display order.
    pub view: Vec<usize>,
    /// Table rows including category separators. Parallel to what is drawn;
    /// selection still uses `task_index` into `view`.
    pub list_rows: Vec<TaskListRow>,
    pub searching: bool,
    pub search_query: String,
    pub input: TextInput,
    /// Selected row in the `/` palette dropdown.
    pub slash_index: usize,
    /// The open task dialog, if any.
    pub form: Option<TaskForm>,
    /// The open category dialog, if any.
    pub category_form: Option<CategoryForm>,
    /// Selected row in the global label manager.
    pub label_index: usize,
    /// Inline create/rename editor with an explicit name/color focus.
    pub label_editor: Option<LabelEditor>,
    pub label_error: Option<String>,
    labels_return_to_form: bool,
    pub settings_index: usize,
    /// First help content row currently visible.
    pub help_scroll: usize,
    pub message: Option<Message>,
    /// Pending entity-bound destructive action and its deadline.
    pub pending: Option<(Confirm, Instant)>,
    /// Last click `(time, target, row)` for double-click detection.
    pub(crate) last_click: Option<(Instant, ClickTarget, usize)>,
    pub should_quit: bool,
    pub areas: Areas,
    /// Description/preview image store.
    pub images: ImageStore,
    pub(crate) attachments: Vec<Attachment>,
    /// Type-to-jump buffer for task, category, and label rows; cleared on timeout.
    typeahead: String,
    typeahead_at: Option<Instant>,
    /// Needs a redraw.
    pub dirty: bool,
    /// Incremented on task mutation (invalidates preview cache).
    pub data_gen: u64,
    /// Per-category `(done, total)`, parallel to `categories`.
    cat_progress: Vec<(usize, usize)>,
    /// Cached description editor for the read-only preview pane.
    pub preview_form: Option<TaskForm>,
    preview_task_id: Option<String>,
    preview_gen: u64,
    /// Entity snapshots captured when an edit dialog opens. Save compares only
    /// editable fields so unrelated changes (for example, another agent
    /// toggling `done`) are preserved instead of becoming false conflicts.
    task_edit_base: Option<Task>,
    category_edit_base: Option<Category>,
    /// One in-flight automatic check or explicit install.
    update_job: Option<UpdateJob>,
    /// One in-flight archive import or export. All archive I/O runs off the
    /// event-loop thread so large backups cannot freeze input or drawing.
    archive_job: Option<ArchiveJob>,
    /// A quit requested while archive work is active waits for safe
    /// cancellation or finalization instead of abandoning temporary files.
    quit_after_archive: bool,
    /// Application-level update scheduling state, independent of task data.
    update_state: Option<UpdateStateStore>,
    /// Wall-clock deadline for the next cheap state refresh.
    next_update_state_poll_at: i64,
    /// Update notices survive ordinary status messages and clear only when the
    /// user opens the `/` command palette.
    update_notice: Option<UpdateNotice>,
    /// An available version dismissed in this process stays dismissed until a
    /// newer release is discovered.
    dismissed_update_version: Option<String>,
    /// Visible work for an explicit `/update` request.
    update_activity: Option<UpdateActivity>,
    /// Whether persistence polling has failed since its last successful pass.
    /// Repeated failures are quiet so they cannot continuously replace messages
    /// or disarm destructive confirmations; success rearms reporting.
    external_poll_failed: bool,
}

impl App {
    pub fn new(version: &str) -> Result<Self, StoreError> {
        Self::with_store_and_update_state(
            version,
            Store::open_default(None)?,
            UpdateStateStore::open_default(),
        )
    }

    pub fn with_store(version: &str, store: Store) -> Result<Self, StoreError> {
        Self::with_store_and_update_state(version, store, UpdateStateStore::open_in_memory())
    }

    pub(crate) fn with_store_and_update_state(
        version: &str,
        store: Store,
        update_state: Result<UpdateStateStore, StoreError>,
    ) -> Result<Self, StoreError> {
        let initial = store.snapshot()?;
        // This is provisional until the terminal session is safely owned.
        // `record_launch` repeats the classification inside the fresh write
        // transaction, preserving the cross-process first-run contract.
        let launch = if initial.settings.last_run_version.as_deref() == Some(version) {
            LaunchState::Returning
        } else {
            let mut settings = initial.settings.clone();
            settings.record_launch(version)
        };
        let snapshot = initial;
        let StoreData {
            revision,
            categories: real_cats,
            labels,
            tasks,
            settings,
            attachments,
        } = snapshot;
        // "All Tasks" is a view only — prepended in memory, never saved.
        let mut categories = vec![Category::all_tasks()];
        categories.extend(real_cats);
        let mut images = ImageStore::with_root(store.images_dir().to_path_buf());
        images.set_attachments(&attachments);
        let (update_state, update_state_error) = match update_state {
            Ok(store) => (Some(store), None),
            Err(error) => (None, Some(error.to_string())),
        };

        let mut app = Self {
            store,
            version: version.to_string(),
            store_revision: revision,
            tasks,
            categories,
            labels,
            settings,
            focus: Focus::Tasks,
            mode: match launch {
                LaunchState::FirstRun => Mode::Welcome,
                LaunchState::Upgraded => Mode::WhatsNew,
                LaunchState::Returning => Mode::Normal,
            },
            cat_index: 0,
            task_index: 0,
            cat_state: ListState::default(),
            task_state: TableState::default(),
            view: Vec::new(),
            list_rows: Vec::new(),
            searching: false,
            search_query: String::new(),
            input: TextInput::default(),
            slash_index: 0,
            form: None,
            category_form: None,
            label_index: 0,
            label_editor: None,
            label_error: None,
            labels_return_to_form: false,
            settings_index: 0,
            help_scroll: 0,
            message: None,
            pending: None,
            last_click: None,
            should_quit: false,
            areas: Areas::default(),
            images,
            attachments,
            typeahead: String::new(),
            typeahead_at: None,
            dirty: true,
            data_gen: 0,
            cat_progress: Vec::new(),
            preview_form: None,
            preview_task_id: None,
            preview_gen: 0,
            task_edit_base: None,
            category_edit_base: None,
            update_job: None,
            archive_job: None,
            quit_after_archive: false,
            update_state,
            next_update_state_poll_at: 0,
            update_notice: None,
            dismissed_update_version: None,
            update_activity: None,
            external_poll_failed: false,
        };
        app.rebuild_view();
        if let Some(error) = update_state_error {
            app.error(format!("Automatic update checks unavailable: {error}"));
        } else {
            app.refresh_update_state(Utc::now().timestamp());
        }
        Ok(app)
    }

    /// Persist the launch only after terminal initialization succeeds.
    ///
    /// Until this runs, Welcome / What's New is merely provisional: a failed
    /// terminal setup must leave it available for the next successful launch.
    pub(crate) fn record_launch(&mut self) -> Result<(), StoreError> {
        let version = self.version.clone();
        let launch = if self.settings.last_run_version.as_deref() == Some(version.as_str()) {
            LaunchState::Returning
        } else {
            self.update_store(|data| Ok(data.settings.record_launch(&version)))?
        };
        self.mode = match launch {
            LaunchState::FirstRun => Mode::Welcome,
            LaunchState::Upgraded => Mode::WhatsNew,
            LaunchState::Returning => Mode::Normal,
        };
        self.dirty = true;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        self.store.data_dir()
    }

    /// Refresh after another process commits. Dialogs deliberately defer the
    /// visual refresh: their entity snapshot is checked transactionally when
    /// the user saves, so typed work is never replaced under the cursor.
    pub fn poll_external_changes(&mut self) -> bool {
        let revision = match self.store.revision() {
            Ok(revision) => revision,
            Err(error) => {
                return self.report_external_poll_error(format!(
                    "Could not check for external changes: {error}"
                ));
            }
        };
        if revision == self.store_revision
            || self.form.is_some()
            || self.category_form.is_some()
            || self.mode == Mode::Labels
        {
            self.external_poll_failed = false;
            return false;
        }
        match self.reload_store() {
            Ok(()) => {
                self.external_poll_failed = false;
                true
            }
            Err(error) => self
                .report_external_poll_error(format!("Could not reload external changes: {error}")),
        }
    }

    fn report_external_poll_error(&mut self, message: String) -> bool {
        if self.external_poll_failed {
            return false;
        }
        self.external_poll_failed = true;
        self.error(message);
        true
    }

    fn reload_store(&mut self) -> Result<(), StoreError> {
        let selected_category = self.current_category_id().to_string();
        let selected_task = self.selected_task().map(|task| task.id.clone());
        let snapshot = self.store.snapshot()?;
        self.apply_snapshot(snapshot, &selected_category, selected_task.as_deref());
        Ok(())
    }

    fn apply_snapshot(
        &mut self,
        snapshot: StoreData,
        selected_category: &str,
        selected_task: Option<&str>,
    ) {
        let StoreData {
            revision,
            categories,
            labels,
            tasks,
            settings,
            attachments,
        } = snapshot;
        self.store_revision = revision;
        self.tasks = tasks;
        self.labels = labels;
        self.settings = settings;
        self.attachments = attachments;
        self.images.set_attachments(&self.attachments);
        self.categories.clear();
        self.categories.push(Category::all_tasks());
        self.categories.extend(categories);
        self.cat_index = self
            .categories
            .iter()
            .position(|category| category.id == selected_category)
            .unwrap_or(0);
        self.cat_progress.clear();
        self.data_gen = self.data_gen.wrapping_add(1);
        self.invalidate_preview();
        self.rebuild_view();
        if let Some(id) = selected_task {
            self.select_task_by_id(id);
        }
        self.dirty = true;
    }

    pub(crate) fn start_export_archive(&mut self) {
        self.start_archive_worker(ArchiveRequest::Export);
    }

    pub(crate) fn start_import_archive(&mut self, path: PathBuf) {
        self.start_archive_worker(ArchiveRequest::Import(path));
    }

    fn start_archive_worker(&mut self, request: ArchiveRequest) {
        if let Some(active) = self.archive_job.as_ref() {
            self.info(format!("An {} is already running", active.kind.name()));
            return;
        }

        let kind = request.kind();
        let data_dir = self.store.data_dir().to_path_buf();
        let control = Arc::new(crate::archive::ArchiveControl::new());
        let worker_control = Arc::clone(&control);
        let (tx, rx) = mpsc::channel();
        let thread_name = match kind {
            ArchiveJobKind::Export => "mach-archive-export",
            ArchiveJobKind::Import => "mach-archive-import",
        };
        let spawn = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let result = (|| -> Result<ArchiveOutcome, crate::archive::ArchiveError> {
                    let mut store = Store::open(data_dir)?;
                    match request {
                        ArchiveRequest::Export => crate::archive::export_with_progress(
                            &store,
                            None,
                            &worker_control,
                            |progress| {
                                let _ = tx.send(ArchiveEvent::Progress(progress));
                            },
                        )
                        .map(ArchiveOutcome::Export),
                        ArchiveRequest::Import(path) => crate::archive::import_with_progress(
                            &mut store,
                            &path,
                            &worker_control,
                            |progress| {
                                let _ = tx.send(ArchiveEvent::Progress(progress));
                            },
                        )
                        .map(ArchiveOutcome::Import),
                    }
                })();
                let _ = tx.send(ArchiveEvent::Finished(result));
            });

        match spawn {
            Ok(handle) => {
                self.archive_job = Some(ArchiveJob {
                    rx,
                    handle,
                    kind,
                    control,
                    progress: crate::archive::ArchiveProgress::Preparing,
                    cancel_requested: false,
                });
                self.dirty = true;
            }
            Err(error) => self.error(format!("Could not start archive {}: {error}", kind.name())),
        }
    }

    /// Apply archive progress or completion without blocking the event loop.
    pub(crate) fn poll_archive(&mut self) -> bool {
        let mut changed = false;
        loop {
            let event = self.archive_job.as_ref().map(|job| job.rx.try_recv());
            match event {
                None | Some(Err(TryRecvError::Empty)) => return changed,
                Some(Ok(ArchiveEvent::Progress(progress))) => {
                    if let Some(job) = self.archive_job.as_mut()
                        && job.progress != progress
                    {
                        job.progress = progress;
                        changed = true;
                    }
                }
                Some(Ok(ArchiveEvent::Finished(result))) => {
                    let job = self
                        .archive_job
                        .take()
                        .expect("archive event requires an active job");
                    let kind = job.kind;
                    let _ = job.handle.join();
                    changed |= self.finish_archive(kind, result);
                    if self.quit_after_archive {
                        self.should_quit = true;
                    }
                    return changed;
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    let job = self
                        .archive_job
                        .take()
                        .expect("archive channel requires an active job");
                    let kind = job.kind;
                    let _ = job.handle.join();
                    self.error(format!("{} stopped unexpectedly", kind.title()));
                    if self.quit_after_archive {
                        self.should_quit = true;
                    }
                    return true;
                }
            }
        }
    }

    fn finish_archive(
        &mut self,
        kind: ArchiveJobKind,
        result: Result<ArchiveOutcome, crate::archive::ArchiveError>,
    ) -> bool {
        match result {
            Ok(ArchiveOutcome::Export(summary)) => {
                let contents = crate::archive::content_count_text(
                    summary.tasks,
                    summary.categories,
                    summary.labels,
                    summary.images,
                );
                self.archive_result(format!("Exported to {} · {contents}", summary.short_path()));
            }
            Ok(ArchiveOutcome::Import(summary)) => {
                if let Err(error) = self.reload_store() {
                    self.error(format!(
                        "Archive imported, but mach could not refresh: {error}"
                    ));
                    return true;
                }
                let added = crate::archive::content_count_text(
                    summary.tasks_added,
                    summary.categories_added,
                    summary.labels_added,
                    summary.images_added,
                );
                let unchanged = crate::archive::content_count_text(
                    summary.tasks_unchanged,
                    summary.categories_unchanged,
                    summary.labels_unchanged,
                    summary.images_unchanged,
                );
                let message = if !summary.changed() {
                    format!("Nothing imported; {unchanged} already present")
                } else {
                    format!("Imported {added}; {unchanged} already present")
                };
                self.archive_result(message);
            }
            Err(crate::archive::ArchiveError::Cancelled) => {
                self.info(format!("{} cancelled", kind.title()));
            }
            Err(error) => self.error(format!("Could not {}: {error}", kind.name())),
        }
        true
    }

    /// Request cancellation at the next safe I/O boundary. Once finalization
    /// begins, import may be inside its atomic database commit and must finish.
    pub(crate) fn cancel_archive(&mut self) -> bool {
        let Some(job) = self.archive_job.as_mut() else {
            return false;
        };
        if !job.control.request_cancel() {
            let title = job.kind.title();
            self.info(format!("{title} is finishing and cannot be cancelled"));
            return true;
        }
        if !job.cancel_requested {
            job.cancel_requested = true;
            self.dirty = true;
        }
        true
    }

    pub fn request_quit(&mut self) {
        let Some(job) = self.archive_job.as_mut() else {
            self.should_quit = true;
            return;
        };
        self.quit_after_archive = true;
        if job.control.request_cancel() {
            job.cancel_requested = true;
        }
        self.pending = None;
        self.message = None;
        self.dirty = true;
    }

    /// Join archive work before an event-loop error releases the process.
    /// Normal quits already defer until `poll_archive` observes completion.
    pub(crate) fn shutdown_archive(&mut self) {
        if let Some(job) = self.archive_job.take() {
            let _ = job.control.request_cancel();
            let _ = job.handle.join();
        }
    }

    /// Commit against the transaction's fresh snapshot and apply the exact
    /// normalized state returned after a successful commit.
    fn update_store<R>(
        &mut self,
        operation: impl FnOnce(&mut StoreData) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let selected_category = self.current_category_id().to_string();
        let selected_task = self.selected_task().map(|task| task.id.clone());
        let (result, snapshot) = self.store.update_with_snapshot(operation)?;
        self.apply_snapshot(snapshot, &selected_category, selected_task.as_deref());
        Ok(result)
    }

    fn report_store_error(&mut self, action: &str, error: StoreError) {
        self.error(format!("{action}: {error}"));
    }

    /// Refresh global state and start a due automatic check. The event loop
    /// calls this throughout the process lifetime; local polling is capped at
    /// once per minute while the SQLite deadline remains the source of truth.
    pub(crate) fn poll_automatic_update_schedule(&mut self) -> bool {
        self.poll_automatic_update_schedule_at(Utc::now().timestamp())
    }

    fn poll_automatic_update_schedule_at(&mut self, now: i64) -> bool {
        if now < self.next_update_state_poll_at {
            return false;
        }
        if self.update_job.is_some() {
            self.next_update_state_poll_at = now.saturating_add(1);
            return false;
        }
        let claim = match self
            .update_state
            .as_mut()
            .map(|store| store.try_claim_automatic(now))
        {
            Some(Ok(claim)) => claim,
            Some(Err(_)) => {
                self.next_update_state_poll_at = now.saturating_add(FAILURE_RETRY_SECONDS);
                return false;
            }
            None => {
                self.next_update_state_poll_at = i64::MAX;
                return false;
            }
        };
        match claim {
            AutomaticClaim::Claimed(lease) => {
                self.next_update_state_poll_at = now.saturating_add(LEASE_SECONDS);
                self.start_update_worker(UpdateJobKind::Automatic, Some(lease));
                false
            }
            AutomaticClaim::Waiting(state) => self.apply_update_state(state, now),
        }
    }

    fn refresh_update_state(&mut self, now: i64) -> bool {
        let state = match self.update_state.as_ref().map(UpdateStateStore::snapshot) {
            Some(Ok(state)) => state,
            Some(Err(_)) => {
                self.next_update_state_poll_at = now.saturating_add(FAILURE_RETRY_SECONDS);
                return false;
            }
            None => {
                self.next_update_state_poll_at = i64::MAX;
                return false;
            }
        };
        self.apply_update_state(state, now)
    }

    fn apply_update_state(&mut self, state: UpdateState, now: i64) -> bool {
        let changed = self.sync_available_update(state.latest_version.as_deref());
        self.schedule_update_state_poll(&state, now);
        changed
    }

    fn schedule_update_state_poll(&mut self, state: &UpdateState, now: i64) {
        const STATE_REFRESH_SECONDS: i64 = 60;
        let deadline = if state.automatic_check_due(now) {
            state
                .lease_until
                .filter(|_| state.lease_active(now))
                .unwrap_or(now)
        } else {
            state.next_check_at.unwrap_or(now)
        };
        self.next_update_state_poll_at = deadline.min(now.saturating_add(STATE_REFRESH_SECONDS));
    }

    /// Explicitly check for and install the latest checksum-verified release (`/update`).
    pub(crate) fn start_update_install(&mut self) {
        self.update_notice = None;
        if self
            .update_job
            .as_ref()
            .is_some_and(|job| job.kind == UpdateJobKind::Install)
        {
            self.info("Already updating…");
            return;
        }

        // Explicit user intent supersedes an automatic check. Its detached
        // worker may finish, but dropping the receiver prevents a stale result
        // from competing with the install result in the UI.
        self.update_job = None;
        let now = Utc::now().timestamp();
        let lease = self
            .update_state
            .as_mut()
            .and_then(|store| store.claim_manual(now).ok());
        self.start_update_worker(UpdateJobKind::Install, lease);
    }

    fn start_update_worker(&mut self, kind: UpdateJobKind, lease: Option<UpdateLease>) {
        let (tx, rx) = mpsc::channel();
        let thread_name = match kind {
            UpdateJobKind::Automatic => "mach-update-check",
            UpdateJobKind::Install => "mach-update-install",
        };
        let conditional_etag = lease.as_ref().and_then(|lease| lease.etag.clone());
        match std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let result = (|| -> Result<UpdateOutcome, CheckFailure> {
                    match kind {
                        UpdateJobKind::Automatic => {
                            crate::update::check_with_etag(conditional_etag.as_deref())
                                .map(UpdateOutcome::Automatic)
                        }
                        UpdateJobKind::Install => {
                            let CheckResponse::Modified { value: info, etag } =
                                crate::update::check_with_etag(None)?
                            else {
                                return Err(CheckFailure {
                                    message: "GitHub returned 304 without a conditional request"
                                        .into(),
                                    retry_at: None,
                                });
                            };
                            if !info.newer {
                                return Ok(UpdateOutcome::UpToDate { info, etag });
                            }
                            let install = crate::update::install_with_progress(&info, |progress| {
                                let _ = tx.send(UpdateEvent::DownloadProgress(progress));
                            });
                            Ok(match install {
                                Ok(result) => UpdateOutcome::Installed { result, info, etag },
                                Err(message) => UpdateOutcome::InstallFailed {
                                    message,
                                    info,
                                    etag,
                                },
                            })
                        }
                    }
                })();
                let _ = tx.send(UpdateEvent::Finished(Box::new(result)));
            }) {
            Ok(_) => {
                self.update_job = Some(UpdateJob { rx, kind, lease });
                if kind == UpdateJobKind::Install {
                    self.update_activity = Some(UpdateActivity::Checking);
                    self.dirty = true;
                }
            }
            Err(error) => {
                self.finish_update_state_failure(lease.as_ref(), Utc::now().timestamp(), None);
                if kind == UpdateJobKind::Install {
                    self.update_activity = None;
                    self.error(format!("Could not start update: {error}"));
                }
            }
        }
    }

    /// Apply finished update work, if any. Returns true when UI should redraw.
    pub(crate) fn poll_update(&mut self) -> bool {
        let mut changed = false;
        loop {
            let event = self
                .update_job
                .as_ref()
                .map(|job| (job.kind, job.rx.try_recv()));
            match event {
                None | Some((_, Err(TryRecvError::Empty))) => return changed,
                Some((_, Ok(UpdateEvent::DownloadProgress(progress)))) => {
                    let activity = UpdateActivity::Downloading(progress);
                    if self.update_activity != Some(activity) {
                        self.update_activity = Some(activity);
                        changed = true;
                    }
                }
                Some((kind, Ok(UpdateEvent::Finished(result)))) => {
                    let lease = self.update_job.take().and_then(|job| job.lease);
                    changed |= self.update_activity.take().is_some();
                    return self.finish_update(kind, lease.as_ref(), *result) || changed;
                }
                Some((kind, Err(TryRecvError::Disconnected))) => {
                    let lease = self.update_job.take().and_then(|job| job.lease);
                    changed |= self.update_activity.take().is_some();
                    self.finish_update_state_failure(lease.as_ref(), Utc::now().timestamp(), None);
                    return if kind == UpdateJobKind::Install {
                        self.show_update_message("Update failed".into(), MessageKind::Error);
                        true
                    } else {
                        changed
                    };
                }
            }
        }
    }

    fn finish_update(
        &mut self,
        kind: UpdateJobKind,
        lease: Option<&UpdateLease>,
        result: Result<UpdateOutcome, CheckFailure>,
    ) -> bool {
        let now = Utc::now().timestamp();
        match result {
            Ok(UpdateOutcome::Automatic(CheckResponse::Modified { value: info, etag })) => {
                let (committed, changed) =
                    self.finish_update_state_modified(lease, now, etag.as_deref(), &info.latest);
                if !committed {
                    return changed;
                }
                if info.newer {
                    self.set_available_update_notice(&info.latest) || changed
                } else {
                    self.sync_available_update(None) || changed
                }
            }
            Ok(UpdateOutcome::Automatic(CheckResponse::NotModified)) => {
                if let (Some(store), Some(lease)) = (self.update_state.as_mut(), lease) {
                    let _ = store.finish_not_modified(lease, now);
                }
                self.refresh_update_state(now)
            }
            Ok(UpdateOutcome::UpToDate { info, etag }) => {
                self.finish_update_state_modified(lease, now, etag.as_deref(), &info.latest);
                self.show_update_message(info.summary(), MessageKind::Info);
                true
            }
            Ok(UpdateOutcome::Installed { result, info, etag }) => {
                self.finish_update_state_modified(lease, now, etag.as_deref(), &info.latest);
                let action = match result.disposition {
                    crate::update::InstallDisposition::Installed => "Installed",
                    crate::update::InstallDisposition::AlreadyCurrent => "Already installed",
                };
                self.set_update_notice(UpdateNotice {
                    text: format!("{action} {} · restart mach", result.tag),
                    available_version: None,
                })
            }
            Ok(UpdateOutcome::InstallFailed {
                message,
                info,
                etag,
            }) => {
                self.finish_update_state_modified(lease, now, etag.as_deref(), &info.latest);
                self.show_update_message(message, MessageKind::Error);
                true
            }
            Err(error) if kind == UpdateJobKind::Install => {
                self.finish_update_state_failure(lease, now, error.retry_at);
                self.show_update_message(error.message, MessageKind::Error);
                true
            }
            Err(error) => {
                self.finish_update_state_failure(lease, now, error.retry_at);
                false
            }
        }
    }

    fn finish_update_state_modified(
        &mut self,
        lease: Option<&UpdateLease>,
        now: i64,
        etag: Option<&str>,
        latest_version: &str,
    ) -> (bool, bool) {
        let committed = match (self.update_state.as_mut(), lease) {
            (Some(store), Some(lease)) => store
                .finish_modified(lease, now, etag, latest_version)
                .unwrap_or(false),
            // Manual checks remain useful when application-level persistence
            // is unavailable; automatic workers always carry a lease.
            _ => true,
        };
        (committed, self.refresh_update_state(now))
    }

    fn finish_update_state_failure(
        &mut self,
        lease: Option<&UpdateLease>,
        now: i64,
        retry_at: Option<i64>,
    ) {
        if let (Some(store), Some(lease)) = (self.update_state.as_mut(), lease) {
            let _ = store.finish_failure(lease, now, retry_at);
        }
        self.refresh_update_state(now);
    }

    fn sync_available_update(&mut self, available_version: Option<&str>) -> bool {
        let available_version = available_version
            .filter(|version| crate::update::is_newer(version, &self.version) == Some(true));
        let Some(version) = available_version else {
            if self
                .update_notice
                .as_ref()
                .is_some_and(|notice| notice.available_version.is_some())
            {
                self.update_notice = None;
                self.dirty = true;
                return true;
            }
            return false;
        };
        if self.dismissed_update_version.as_deref() == Some(version)
            || self
                .update_notice
                .as_ref()
                .is_some_and(|notice| notice.available_version.is_none())
        {
            return false;
        }
        self.set_available_update_notice(version)
    }

    fn set_available_update_notice(&mut self, latest: &str) -> bool {
        if self
            .update_notice
            .as_ref()
            .and_then(|notice| notice.available_version.as_deref())
            == Some(latest)
        {
            return false;
        }
        self.set_update_notice(UpdateNotice {
            text: format!(
                "v{} → v{} available · run /update to install",
                self.version, latest
            ),
            available_version: Some(latest.to_string()),
        })
    }

    fn set_update_notice(&mut self, notice: UpdateNotice) -> bool {
        let visible = self.message.is_none();
        self.update_notice = Some(notice);
        if visible {
            self.dirty = true;
        }
        visible
    }

    fn show_update_message(&mut self, text: String, kind: MessageKind) {
        self.set_message(text, kind, MessageLifetime::Long);
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn invalidate_preview(&mut self) {
        self.preview_form = None;
        self.preview_task_id = None;
        self.preview_gen = 0;
    }

    /// Rebuild [`Self::preview_form`] if the selection or `data_gen` changed.
    pub fn ensure_preview(&mut self) {
        let Some(task) = self.selected_task() else {
            self.invalidate_preview();
            return;
        };
        let id = task.id.clone();
        let generation = self.data_gen;
        if self.preview_task_id.as_deref() == Some(id.as_str())
            && self.preview_gen == generation
            && self.preview_form.is_some()
        {
            return;
        }
        let task = task.clone();
        let mut form =
            TaskForm::edit_with_images(&task, self.images.root().to_path_buf(), &self.attachments);
        form.set_categories(&self.categories, task.category_id.as_deref());
        form.set_labels(&self.labels, &task.label_ids);
        self.preview_form = Some(form);
        self.preview_task_id = Some(id);
        self.preview_gen = generation;
    }

    pub fn theme(&self) -> Theme {
        Theme::new(&self.settings.selected_color)
    }

    // ---------------------------------------------------------------- view

    pub fn current_category_id(&self) -> &str {
        self.categories
            .get(self.cat_index)
            .map(|c| c.id.as_str())
            .unwrap_or(ALL_CATEGORY)
    }

    pub fn is_all_view(&self) -> bool {
        self.current_category_id() == ALL_CATEGORY
    }

    pub fn category_name(&self, id: &str) -> Option<&str> {
        self.categories
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.as_str())
    }

    pub fn label_name(&self, id: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|label| label.id == id)
            .map(|label| label.name.as_str())
    }

    /// Recompute which tasks are shown and in what order.
    ///
    /// Sort applies **inside** each category. All Tasks (and search) stack
    /// those already-sorted groups in sidebar order; a single category is
    /// just one group.
    pub fn rebuild_view(&mut self) {
        let selected_id = self.selected_task().map(|task| task.id.clone());
        self.dirty = true;
        if self.cat_progress.len() != self.categories.len() {
            self.recompute_cat_progress();
        }
        let cat_id = self.current_category_id();
        let all = cat_id == ALL_CATEGORY;
        let hide_done = self.settings.hide_done;
        let candidates: Vec<usize> = if self.searching {
            let q = caseless_key(&self.search_query);
            self.tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    !(hide_done && t.done)
                        && (task_text_contains(t, &q) || task_labels_contain(t, &self.labels, &q))
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            self.tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    (all || t.category_id.as_deref() == Some(cat_id)) && !(hide_done && t.done)
                })
                .map(|(i, _)| i)
                .collect()
        };

        // Multi-category views: stack each category's sorted slice.
        let multi = all || self.searching;
        self.view = if multi {
            self.stack_by_category(&candidates)
        } else {
            let mut view = candidates;
            self.sort_within(&mut view);
            view
        };
        if let Some(id) = selected_id {
            self.select_task_by_id(&id);
        } else if self.task_index >= self.view.len() {
            self.task_index = self.view.len().saturating_sub(1);
        }
        self.list_rows = self.build_list_rows(multi);
    }

    /// Table rows for the current `view`. Multi-category lists get a
    /// section header before each group; a single category is tasks only.
    fn build_list_rows(&self, multi: bool) -> Vec<TaskListRow> {
        if !multi {
            return (0..self.view.len()).map(TaskListRow::Task).collect();
        }
        let category_names: HashMap<_, _> = self
            .categories
            .iter()
            .map(|category| (category.id.as_str(), category.name.as_str()))
            .collect();
        let mut rows = Vec::with_capacity(self.view.len() + self.categories.len());
        let mut prev: Option<Option<&str>> = None;
        for (vi, &ti) in self.view.iter().enumerate() {
            let key = self.tasks[ti].category_id.as_deref();
            if prev != Some(key) {
                let title = match key {
                    Some(id) => category_names
                        .get(id)
                        .copied()
                        .unwrap_or("Unknown")
                        .to_string(),
                    None => "Uncategorized".to_string(),
                };
                rows.push(TaskListRow::Separator { title });
                prev = Some(key);
            }
            rows.push(TaskListRow::Task(vi));
        }
        rows
    }

    /// Visual table row for the selected task, if any.
    pub fn selected_visual_row(&self) -> Option<usize> {
        self.list_rows
            .iter()
            .position(|r| matches!(r, TaskListRow::Task(i) if *i == self.task_index))
    }

    /// `view` index under a visual table row, or `None` for a separator.
    pub fn task_at_visual_row(&self, row: usize) -> Option<usize> {
        match self.list_rows.get(row)? {
            TaskListRow::Task(i) => Some(*i),
            TaskListRow::Separator { .. } => None,
        }
    }

    /// Sidebar order of real categories, each group sorted; uncategorized last.
    fn stack_by_category(&self, candidates: &[usize]) -> Vec<usize> {
        let mut buckets: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for &i in candidates {
            buckets
                .entry(self.tasks[i].category_id.as_deref())
                .or_default()
                .push(i);
        }
        let mut view = Vec::with_capacity(candidates.len());
        for cat in self.categories.iter().filter(|c| !c.is_all()) {
            if let Some(mut group) = buckets.remove(&Some(cat.id.as_str())) {
                self.sort_within(&mut group);
                view.extend(group);
            }
        }
        // Anything not filed under a known category (or left uncategorized).
        let mut rest: Vec<usize> = buckets.into_values().flatten().collect();
        self.sort_within(&mut rest);
        view.extend(rest);
        view
    }

    /// Apply the settings sort to one category's rows (or a rest bucket).
    fn sort_within(&self, view: &mut [usize]) {
        match self.settings.sort.as_str() {
            "important" => view.sort_by_key(|i| std::cmp::Reverse(self.tasks[*i].importance)),
            "done" => view.sort_by_key(|i| self.tasks[*i].done),
            "due" => {
                let today = chrono::Local::now().date_naive();
                view.sort_by_cached_key(|i| {
                    let due = &self.tasks[*i].due;
                    (due.is_empty(), due::sort_key_at(due, today))
                });
            }
            _ => {} // manual — keep the store's explicit task order
        }
    }

    pub fn task_count(&self) -> usize {
        self.view.len()
    }

    pub fn visible_task(&self, pos: usize) -> Option<&Task> {
        self.view.get(pos).and_then(|index| self.tasks.get(*index))
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.visible_task(self.task_index)
    }

    pub fn done_count(&self) -> usize {
        self.view.iter().filter(|i| self.tasks[**i].done).count()
    }

    // ----------------------------------------------------------- selection

    pub fn move_task_selection(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        let next = (self.task_index as isize + delta).clamp(0, last as isize) as usize;
        self.select_task(next);
    }

    pub fn select_task(&mut self, pos: usize) {
        if pos < self.view.len() && pos != self.task_index {
            self.task_index = pos;
            self.cancel_pending();
            self.clear_typeahead();
            self.dirty = true;
        }
    }

    pub fn select_first_task(&mut self) {
        self.select_task(0);
    }

    pub fn select_last_task(&mut self) {
        self.select_task(self.view.len().saturating_sub(1));
    }

    /// Type-to-jump: append `c` and select the best fuzzy match (list unchanged).
    pub fn typeahead_jump(&mut self, c: char) {
        let label_picker_open = self.mode == Mode::TaskForm
            && self.form.as_ref().is_some_and(TaskForm::label_picker_open);
        let now = Instant::now();
        if self
            .typeahead_at
            .is_none_or(|t| now.duration_since(t) > TYPEAHEAD_TIMEOUT)
        {
            self.typeahead.clear();
        }
        let limit = match self.mode {
            Mode::Labels => MAX_LABEL_NAME_LEN,
            Mode::TaskForm if label_picker_open => MAX_LABEL_NAME_LEN,
            _ => match self.focus {
                Focus::Tasks => MAX_TITLE_LEN,
                Focus::Sidebar => MAX_CATEGORY_NAME_LEN,
            },
        };
        if self.typeahead.graphemes(true).count() < limit {
            self.typeahead.push(c);
        }
        self.typeahead_at = Some(now);

        if label_picker_open {
            let best = self.form.as_ref().and_then(|form| {
                let names = form.label_choices().map(|(_, name, _, _)| name);
                crate::fuzzy::best_index(&self.typeahead, names)
            });
            if let Some(pos) = best {
                if let Some(form) = &mut self.form {
                    form.select_label_picker(pos);
                }
                self.cancel_pending();
            }
            return;
        }

        if self.mode == Mode::Labels {
            let names = self.labels.iter().map(|label| label.name.as_str());
            if let Some(pos) = crate::fuzzy::best_index(&self.typeahead, names) {
                self.label_index = pos;
                self.cancel_pending();
            }
            return;
        }

        match self.focus {
            Focus::Tasks => {
                let titles = self.view.iter().map(|&i| self.tasks[i].title.as_str());
                if let Some(pos) = crate::fuzzy::best_index(&self.typeahead, titles) {
                    self.task_index = pos;
                    self.cancel_pending();
                }
            }
            Focus::Sidebar => {
                let names = self.categories.iter().map(|c| c.name.as_str());
                if let Some(pos) = crate::fuzzy::best_index(&self.typeahead, names)
                    && pos != self.cat_index
                {
                    self.cat_index = pos;
                    self.cancel_pending();
                    self.on_category_changed();
                }
            }
        }
    }

    pub fn move_category_selection(&mut self, delta: isize) {
        if self.categories.is_empty() {
            return;
        }
        let last = self.categories.len() - 1;
        let next = (self.cat_index as isize + delta).clamp(0, last as isize) as usize;
        self.select_category(next);
    }

    /// ↑/↓ stay inside the focused panel. Cross-panel moves use ←/→ or Tab.
    pub fn navigate_vertical(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.cancel_pending();
        match self.focus {
            Focus::Tasks => {
                if self.view.is_empty() {
                    return;
                }
                self.move_task_selection(delta);
            }
            Focus::Sidebar => {
                self.move_category_selection(delta);
            }
        }
    }

    pub fn select_category(&mut self, index: usize) {
        if index < self.categories.len() && index != self.cat_index {
            self.cat_index = index;
            self.cancel_pending();
            self.clear_typeahead();
            self.on_category_changed();
        }
    }

    pub fn select_last_category(&mut self) {
        self.select_category(self.categories.len().saturating_sub(1));
    }

    fn on_category_changed(&mut self) {
        self.searching = false;
        self.search_query.clear();
        self.task_index = 0;
        self.rebuild_view();
    }

    pub fn toggle_focus(&mut self) {
        let next = match self.focus {
            Focus::Sidebar => Focus::Tasks,
            Focus::Tasks => Focus::Sidebar,
        };
        let _ = self.set_focus(next);
    }

    /// Move keyboard focus. A locked search owns the task list until Esc.
    pub fn set_focus(&mut self, focus: Focus) -> bool {
        if self.searching && focus == Focus::Sidebar {
            return false;
        }
        if self.focus != focus {
            self.focus = focus;
            self.cancel_pending();
            self.clear_typeahead();
            self.dirty = true;
        }
        true
    }

    pub fn cancel_pending(&mut self) {
        if self.pending.take().is_some() && self.message.take().is_some() {
            self.dirty = true;
        }
    }

    pub(crate) fn clear_typeahead(&mut self) {
        self.typeahead.clear();
        self.typeahead_at = None;
    }

    // ------------------------------------------------------------ mutation

    fn recompute_cat_progress(&mut self) {
        let mut all_done = 0usize;
        let mut all_total = 0usize;
        let mut per: Vec<(usize, usize)> = self.categories.iter().map(|_| (0, 0)).collect();
        let category_indices: HashMap<_, _> = self
            .categories
            .iter()
            .enumerate()
            .map(|(index, category)| (category.id.as_str(), index))
            .collect();
        for t in &self.tasks {
            all_total += 1;
            if t.done {
                all_done += 1;
            }
            if let Some(cid) = t.category_id.as_deref()
                && let Some(&idx) = category_indices.get(cid)
            {
                per[idx].1 += 1;
                if t.done {
                    per[idx].0 += 1;
                }
            }
        }
        for (i, cat) in self.categories.iter().enumerate() {
            if cat.is_all() {
                per[i] = (all_done, all_total);
            }
        }
        self.cat_progress = per;
    }

    /// Keep the same task selected after the view is rebuilt and rows move.
    fn select_task_by_id(&mut self, id: &str) {
        if let Some(pos) = self.view.iter().position(|i| self.tasks[*i].id == id) {
            self.task_index = pos;
        } else if self.task_index >= self.view.len() {
            self.task_index = self.view.len().saturating_sub(1);
        }
    }

    pub fn toggle_done(&mut self, pos: usize) {
        if let Some(&i) = self.view.get(pos) {
            let id = self.tasks[i].id.clone();
            match self.update_store(|data| data.toggle_task_done(&id)) {
                Ok(_) => self.select_task_by_id(&id),
                Err(error) => self.report_store_error("Could not update task", error),
            }
        }
    }

    /// Steps a task's importance up, wrapping back to none after three.
    pub fn cycle_importance(&mut self, pos: usize) {
        if let Some(&i) = self.view.get(pos) {
            let id = self.tasks[i].id.clone();
            match self.update_store(|data| {
                let importance = crate::model::next_importance(data.task(&id)?.importance);
                data.set_task_importance(&id, importance)
            }) {
                Ok(_) => self.select_task_by_id(&id),
                Err(error) => self.report_store_error("Could not update task", error),
            }
        }
    }

    /// Reorder the selected task inside its category when using manual sort.
    /// In All Tasks, crossing a category section boundary is intentionally
    /// blocked; changing category belongs in the task form.
    pub fn move_task_order(&mut self, delta: isize) -> bool {
        if delta == 0 || self.settings.sort != "manual" || self.searching {
            return false;
        }
        let Some(current) = self.selected_task().cloned() else {
            return false;
        };
        let target_view = self.task_index as isize + delta.signum();
        if !(0..self.view.len() as isize).contains(&target_view) {
            return false;
        }
        let Some(target) = self.visible_task(target_view as usize) else {
            return false;
        };
        if target.category_id != current.category_id {
            return false;
        }
        let target_id = target.id.clone();
        let id = current.id;
        let position = if delta.is_negative() {
            RelativePosition::Before
        } else {
            RelativePosition::After
        };
        match self.update_store(|data| data.move_task_relative(&id, &target_id, position)) {
            Ok(_) => {
                self.select_task_by_id(&id);
                true
            }
            Err(error) => {
                self.report_store_error("Could not reorder task", error);
                false
            }
        }
    }

    /// Opens the dialog for a new task, unless the list is full.
    pub fn open_new_task(&mut self) {
        if self.tasks.len() >= MAX_TASK_COUNT {
            self.error(format!(
                "You already have {MAX_TASK_COUNT} tasks in hand. Maybe deal with them first :)"
            ));
            return;
        }
        let mut form =
            TaskForm::new_with_images(self.images.root().to_path_buf(), &self.attachments);
        let category = (!self.is_all_view()).then(|| self.current_category_id());
        form.set_categories(&self.categories, category);
        form.set_labels(&self.labels, &[]);
        self.task_edit_base = None;
        self.form = Some(form);
        self.mode = Mode::TaskForm;
        self.clear_typeahead();
    }

    /// Opens the dialog on the selected task.
    pub fn open_edit_task(&mut self) {
        if let Some(task) = self.selected_task().cloned() {
            let mut form = TaskForm::edit_with_images(
                &task,
                self.images.root().to_path_buf(),
                &self.attachments,
            );
            form.set_categories(&self.categories, task.category_id.as_deref());
            form.set_labels(&self.labels, &task.label_ids);
            // Decode description pictures off the UI thread so the dialog opens
            // immediately; they fill in on the next frames.
            self.images.prefetch(form.description.images());
            self.task_edit_base = Some(task);
            self.form = Some(form);
            self.mode = Mode::TaskForm;
            self.clear_typeahead();
        }
    }

    pub fn close_form(&mut self) {
        self.form = None;
        self.task_edit_base = None;
        self.mode = Mode::Normal;
        self.focus = Focus::Tasks;
        // Drop placed graphics so they do not float over the list; pixels
        // stay in RAM for a fast reopen. GIF frames are dropped with the form.
        self.images.release_form_graphics();
        self.images.clear_preview();
        self.cancel_pending();
        self.clear_typeahead();
    }

    /// Validates the open form and writes it back to the task list.
    pub fn submit_form(&mut self) {
        let Some(form) = &mut self.form else { return };
        let Some(draft) = form.submit() else { return };
        let saved = match form.editing.clone() {
            Some(uuid) => self.update_task(&uuid, &draft),
            None => self.create_task(&draft).is_some(),
        };
        if saved {
            self.close_form();
        }
    }

    /// Creates a task in the chosen category and selects it. A `[date]`
    /// left in the title is moved into `due` when `due` is empty.
    pub fn create_task(&mut self, draft: &TaskDraft) -> Option<String> {
        let (title, due) = draft.resolved_title_and_due();
        if title.is_empty() || self.tasks.len() >= MAX_TASK_COUNT {
            return None;
        }
        let description = draft.description.clone();
        let category_id = draft.category_id.clone();
        let label_ids = draft.label_ids.clone();
        let importance = draft.importance;
        let task = match self.update_store(|data| {
            let task = data.create_task(title, description, due, importance, category_id)?;
            data.set_task_labels(&task.id, label_ids)
        }) {
            Ok(task) => task,
            Err(error) => {
                let message = error.to_string();
                if let Some(form) = &mut self.form {
                    form.error = Some(message.clone());
                }
                self.report_store_error("Could not create task", error);
                return None;
            }
        };
        let id = task.id;
        self.searching = false;
        self.search_query.clear();
        self.rebuild_view();
        self.select_task_by_id(&id);
        Some(id)
    }

    pub fn update_task(&mut self, id: &str, draft: &TaskDraft) -> bool {
        let (title, due) = draft.resolved_title_and_due();
        if title.is_empty() {
            return false;
        }
        let expected = self.task_edit_base.clone();
        let id = id.to_string();
        let patch = match expected.as_ref() {
            Some(base) => TaskPatch {
                title: (title != base.title).then_some(title),
                description: (draft.description != base.description)
                    .then(|| draft.description.clone()),
                due: (due != base.due).then_some(due),
                importance: (draft.importance != base.importance).then_some(draft.importance),
                category_id: (draft.category_id != base.category_id)
                    .then(|| draft.category_id.clone()),
                label_ids: (draft.label_ids != base.label_ids).then(|| draft.label_ids.clone()),
                ..TaskPatch::default()
            },
            None => TaskPatch {
                title: Some(title),
                description: Some(draft.description.clone()),
                due: Some(due),
                importance: Some(draft.importance),
                category_id: Some(draft.category_id.clone()),
                label_ids: Some(draft.label_ids.clone()),
                ..TaskPatch::default()
            },
        };
        match self.update_store(|data| {
            if let Some(expected) = &expected {
                data.edit_task_if_unchanged(expected, patch)
            } else {
                data.edit_task(&id, patch)
            }
        }) {
            Ok(_) => {
                self.select_task_by_id(&id);
                true
            }
            Err(error) => {
                let message = edit_error_message(&error);
                if let Some(form) = &mut self.form {
                    form.error = Some(message);
                }
                self.report_store_error("Could not update task", error);
                false
            }
        }
    }

    pub fn delete_task(&mut self, pos: usize) {
        let Some(id) = self.visible_task(pos).map(|task| task.id.clone()) else {
            return;
        };
        self.delete_task_by_id(&id);
    }

    pub fn delete_task_by_id(&mut self, id: &str) -> bool {
        let id = id.to_string();
        if let Err(error) = self.update_store(|data| data.delete_task(&id)) {
            self.report_store_error("Could not delete task", error);
            return false;
        }
        self.cancel_pending();
        true
    }

    /// Permanently remove done tasks. In All Tasks → every done task; in a
    /// category → only that category's done tasks. Nothing is archived.
    pub fn purge(&mut self) -> usize {
        let ids = self.purge_candidate_ids();
        self.purge_ids(&ids)
    }

    /// Completed task ids in the current purge scope, captured for confirmation.
    pub fn purge_candidate_ids(&self) -> Vec<String> {
        let everywhere = self.is_all_view();
        let category = self.current_category_id();
        self.tasks
            .iter()
            .filter(|task| {
                task.done && (everywhere || task.category_id.as_deref() == Some(category))
            })
            .map(|task| task.id.clone())
            .collect()
    }

    /// Purge exactly the confirmed ids; newly completed tasks are never swept in.
    pub fn purge_ids(&mut self, ids: &[String]) -> usize {
        let ids = ids.to_vec();
        match self.update_store(|data| data.purge_completed_ids(&ids)) {
            Ok(removed) => {
                self.cancel_pending();
                removed.len()
            }
            Err(error) => {
                self.report_store_error("Could not purge completed tasks", error);
                0
            }
        }
    }

    /// `/done` — show or hide completed tasks in the list (still on disk).
    pub fn toggle_hide_done(&mut self) -> Option<bool> {
        match self.update_store(|data| {
            data.update_settings(|settings| settings.hide_done = !settings.hide_done)
        }) {
            Ok(settings) => Some(settings.hide_done),
            Err(error) => {
                self.report_store_error("Could not update settings", error);
                None
            }
        }
    }

    // ---------------------------------------------------------- categories

    /// Opens the dialog for a new category.
    pub fn open_new_category(&mut self) {
        // Count real categories (exclude the virtual All row).
        let real = self.categories.iter().filter(|c| !c.is_all()).count();
        if real >= MAX_CATEGORY_COUNT {
            self.error(format!("At most {MAX_CATEGORY_COUNT} categories"));
            return;
        }
        self.category_edit_base = None;
        self.category_form = Some(CategoryForm::new());
        self.mode = Mode::CategoryForm;
    }

    /// Opens the dialog on the selected category. "All Tasks" is not a
    /// real category and cannot be edited.
    pub fn open_edit_category(&mut self) {
        if self.is_all_view() {
            return;
        }
        if let Some(category) = self.categories.get(self.cat_index).cloned() {
            self.category_form = Some(CategoryForm::edit(&category));
            self.category_edit_base = Some(category);
            self.mode = Mode::CategoryForm;
        }
    }

    pub fn close_category_form(&mut self) {
        self.category_form = None;
        self.category_edit_base = None;
        self.mode = Mode::Normal;
        self.cancel_pending();
    }

    pub fn submit_category_form(&mut self) {
        let existing: Vec<(String, String)> = self
            .categories
            .iter()
            .filter(|category| !category.is_all())
            .map(|category| (category.id.clone(), category.name.clone()))
            .collect();
        let Some(form) = &mut self.category_form else {
            return;
        };
        let Some((name, description)) = form.submit_with(|name, editing| {
            let duplicate = existing.iter().any(|(id, existing_name)| {
                Some(id.as_str()) != editing
                    && category_name_key(existing_name) == category_name_key(name)
            });
            if duplicate {
                Err("A category with that name already exists".to_string())
            } else {
                Ok(())
            }
        }) else {
            return;
        };
        let name = truncate_chars(&name, MAX_CATEGORY_NAME_LEN);
        let editing = form.editing.clone();
        let expected = self.category_edit_base.clone();
        let saved = match editing {
            Some(id) => {
                let patch = match expected.as_ref() {
                    Some(base) => CategoryPatch {
                        name: (name != base.name).then_some(name),
                        description: (description != base.description).then_some(description),
                    },
                    None => CategoryPatch {
                        name: Some(name),
                        description: Some(description),
                    },
                };
                match self.update_store(|data| {
                    if let Some(expected) = &expected {
                        data.edit_category_if_unchanged(expected, patch)
                    } else {
                        data.edit_category(&id, patch)
                    }
                }) {
                    Ok(_) => true,
                    Err(error) => {
                        let message = edit_error_message(&error);
                        if let Some(form) = &mut self.category_form {
                            form.error = Some(message);
                        }
                        self.report_store_error("Could not update category", error);
                        false
                    }
                }
            }
            None => match self.update_store(|data| data.create_category(name, description)) {
                Ok(category) => {
                    self.cat_index = self
                        .categories
                        .iter()
                        .position(|item| item.id == category.id)
                        .unwrap_or(0);
                    self.on_category_changed();
                    true
                }
                Err(error) => {
                    let message = error.to_string();
                    if let Some(form) = &mut self.category_form {
                        form.error = Some(message);
                    }
                    self.report_store_error("Could not create category", error);
                    false
                }
            },
        };
        if saved {
            self.close_category_form();
        }
    }

    /// Deletes the category while preserving its tasks as Uncategorized.
    /// Category ids are stable UUIDs — no renumbering.
    pub fn delete_category(&mut self) {
        if self.is_all_view() {
            return;
        }
        let id = self.current_category_id().to_string();
        let _ = self.delete_category_by_id(&id);
    }

    pub fn delete_category_by_id(&mut self, id: &str) -> bool {
        let Some(category) = self.categories.iter().find(|category| category.id == id) else {
            return false;
        };
        if category.is_all() {
            return false;
        }
        let id = id.to_string();
        match self.update_store(|data| data.delete_category(&id)) {
            Ok(_) => {
                self.cancel_pending();
                self.cat_index = 0;
                self.on_category_changed();
                true
            }
            Err(error) => {
                self.report_store_error("Could not delete category", error);
                false
            }
        }
    }

    // --------------------------------------------------------------- labels

    pub fn open_labels(&mut self) {
        self.labels_return_to_form = false;
        self.open_labels_manager();
    }

    pub fn open_labels_from_form(&mut self) {
        if let Some(form) = &mut self.form {
            form.close_label_picker();
        }
        self.labels_return_to_form = true;
        self.open_labels_manager();
    }

    fn open_labels_manager(&mut self) {
        self.mode = Mode::Labels;
        self.label_index = self.label_index.min(self.labels.len().saturating_sub(1));
        self.label_editor = None;
        self.label_error = None;
        self.cancel_pending();
        self.clear_typeahead();
        self.dirty = true;
    }

    pub fn close_labels(&mut self) {
        if self.labels_return_to_form && self.form.is_some() {
            if let Some(form) = &mut self.form {
                form.refresh_labels(&self.labels);
            }
            self.mode = Mode::TaskForm;
        } else {
            self.mode = Mode::Normal;
        }
        self.labels_return_to_form = false;
        self.label_editor = None;
        self.label_error = None;
        self.cancel_pending();
        self.clear_typeahead();
        self.dirty = true;
    }

    pub fn move_label_selection(&mut self, delta: isize) {
        if self.labels.is_empty() {
            return;
        }
        let last = self.labels.len() - 1;
        let next = (self.label_index as isize + delta).clamp(0, last as isize) as usize;
        self.select_label(next);
    }

    pub fn select_label(&mut self, index: usize) {
        if index < self.labels.len() && index != self.label_index {
            self.label_index = index;
            self.cancel_pending();
            self.clear_typeahead();
            self.dirty = true;
        }
    }

    pub fn begin_new_label(&mut self) {
        if self.labels.len() >= MAX_LABEL_COUNT {
            self.label_error = Some(format!("At most {MAX_LABEL_COUNT} labels"));
            return;
        }
        self.label_editor = Some(LabelEditor::new(
            None,
            "",
            LabelColor::least_used(&self.labels),
        ));
        self.label_error = None;
        self.cancel_pending();
        self.clear_typeahead();
    }

    pub fn begin_rename_label(&mut self) {
        let Some(label) = self.labels.get(self.label_index) else {
            return;
        };
        self.label_editor = Some(LabelEditor::new(
            Some(label.id.clone()),
            &label.name,
            label.color,
        ));
        self.label_error = None;
        self.cancel_pending();
        self.clear_typeahead();
    }

    pub fn cancel_label_editor(&mut self) {
        self.label_editor = None;
        self.label_error = None;
        self.dirty = true;
    }

    pub fn submit_label_editor(&mut self) {
        let Some(editor) = &self.label_editor else {
            return;
        };
        let editing = editor.editing_id.clone();
        let name = editor.name.value();
        let color = editor.color;
        let result = match editing {
            Some(id) => self.update_store(|data| {
                data.edit_label(
                    &id,
                    LabelPatch {
                        name: Some(name),
                        color: Some(color),
                    },
                )
            }),
            None => self.update_store(|data| data.create_label_with_color(name, color)),
        };
        match result {
            Ok(label) => {
                self.label_index = self
                    .labels
                    .iter()
                    .position(|item| item.id == label.id)
                    .unwrap_or_default();
                self.label_editor = None;
                self.label_error = None;
            }
            Err(error) => {
                self.label_error = Some(error.to_string());
                self.dirty = true;
            }
        }
    }

    pub fn selected_label(&self) -> Option<&Label> {
        self.labels.get(self.label_index)
    }

    pub fn delete_label_by_id(&mut self, id: &str) -> bool {
        let id = id.to_string();
        match self.update_store(|data| data.delete_label(&id)) {
            Ok(_) => {
                self.label_index = self.label_index.min(self.labels.len().saturating_sub(1));
                self.cancel_pending();
                self.clear_typeahead();
                true
            }
            Err(error) => {
                self.report_store_error("Could not delete label", error);
                false
            }
        }
    }

    pub fn create_label(&mut self, name: &str) -> Result<String, String> {
        self.update_store(|data| data.create_label(name))
            .map(|label| label.id)
            .map_err(|error| error.to_string())
    }

    pub fn set_task_labels(&mut self, task_id: &str, label_ids: Vec<String>) -> Result<(), String> {
        let id = task_id.to_string();
        self.update_store(|data| data.set_task_labels(&id, label_ids))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Reorder real categories while keeping the virtual All Tasks row fixed.
    pub fn move_category_order(&mut self, delta: isize) -> bool {
        if delta == 0 || self.is_all_view() || self.searching {
            return false;
        }
        let target_display = self.cat_index as isize + delta.signum();
        if !(1..self.categories.len() as isize).contains(&target_display) {
            return false;
        }
        let id = self.current_category_id().to_string();
        let target_id = self.categories[target_display as usize].id.clone();
        let position = if delta.is_negative() {
            RelativePosition::Before
        } else {
            RelativePosition::After
        };
        match self.update_store(|data| data.move_category_relative(&id, &target_id, position)) {
            Ok(_) => {
                self.cat_index = self
                    .categories
                    .iter()
                    .position(|category| category.id == id)
                    .unwrap_or(0);
                self.on_category_changed();
                true
            }
            Err(error) => {
                self.report_store_error("Could not reorder category", error);
                false
            }
        }
    }

    /// `(done, total)` for a category. All Tasks counts every task.
    pub fn category_progress(&self, id: &str) -> (usize, usize) {
        if let Some(idx) = self.categories.iter().position(|c| c.id == id)
            && let Some(&p) = self.cat_progress.get(idx)
        {
            return p;
        }
        (0, 0)
    }

    pub(crate) fn category_progress_at(&self, index: usize) -> (usize, usize) {
        self.cat_progress.get(index).copied().unwrap_or((0, 0))
    }

    // -------------------------------------------------------------- slash / search

    /// Open the `/` command palette.
    pub fn open_slash(&mut self) {
        if self.searching {
            self.end_search();
        }
        if let Some(notice) = self.update_notice.take()
            && let Some(version) = notice.available_version
        {
            self.dismissed_update_version = Some(version);
        }
        self.mode = Mode::Slash;
        self.input = TextInput::new("", MAX_SLASH_INPUT_LEN);
        self.slash_index = 0;
        self.dirty = true;
    }

    /// Enter live search, optionally with an initial query.
    pub fn start_search(&mut self, query: &str) {
        self.mode = Mode::Search;
        self.focus = Focus::Tasks;
        self.input = TextInput::new(query, MAX_TITLE_LEN);
        self.search_query = query.to_string();
        self.searching = true;
        self.task_index = 0;
        self.rebuild_view();
    }

    pub fn update_search(&mut self) {
        self.search_query = self.input.value();
        self.searching = true;
        self.task_index = 0;
        self.rebuild_view();
    }

    /// Return keyboard input to an already locked search without rebuilding
    /// the view or moving its selected task.
    pub fn resume_search(&mut self) {
        if !self.searching {
            return;
        }
        self.mode = Mode::Search;
        self.input = TextInput::new(&self.search_query, MAX_TITLE_LEN);
        self.dirty = true;
    }

    pub fn end_search(&mut self) {
        self.searching = false;
        self.search_query.clear();
        self.task_index = 0;
        self.mode = Mode::Normal;
        self.rebuild_view();
    }

    pub fn clamp_slash_index(&mut self) {
        let n = crate::slash::matching(&self.input.value()).len();
        if n == 0 {
            self.slash_index = 0;
        } else {
            self.slash_index = self.slash_index.min(n - 1);
        }
    }

    // ------------------------------------------------------------ messages

    pub fn info(&mut self, text: impl Into<String>) {
        self.set_message(text.into(), MessageKind::Info, MessageLifetime::Brief);
    }

    pub(crate) fn archive_result(&mut self, text: impl Into<String>) {
        self.set_message(text.into(), MessageKind::Info, MessageLifetime::Long);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.set_message(text.into(), MessageKind::Error, MessageLifetime::Standard);
    }

    pub(crate) fn status_message(&self) -> Option<(&str, MessageKind)> {
        self.message
            .as_ref()
            .map(|message| (message.text.as_str(), message.kind))
            .or_else(|| {
                self.update_notice
                    .as_ref()
                    .map(|notice| (notice.text.as_str(), MessageKind::Info))
            })
    }

    pub(crate) fn update_activity(&self) -> Option<UpdateActivity> {
        self.update_activity
    }

    pub(crate) fn archive_activity_text(&self) -> Option<String> {
        let job = self.archive_job.as_ref()?;
        if self.quit_after_archive {
            let action = if job.cancel_requested {
                "Cancelling"
            } else {
                "Finishing"
            };
            return Some(format!("{action} {} before quit…", job.kind.name()));
        }
        if job.cancel_requested {
            return Some(format!("Cancelling {}…", job.kind.name()));
        }
        let text = match job.progress {
            crate::archive::ArchiveProgress::Preparing => {
                format!("Preparing {}… · Esc cancels", job.kind.name())
            }
            crate::archive::ArchiveProgress::Attachments { completed, total } if total > 0 => {
                let action = match job.kind {
                    ArchiveJobKind::Export => "Exporting",
                    ArchiveJobKind::Import => "Importing",
                };
                format!("{action} images {completed}/{total}… · Esc cancels")
            }
            crate::archive::ArchiveProgress::Attachments { .. } => {
                let action = match job.kind {
                    ArchiveJobKind::Export => "Writing export",
                    ArchiveJobKind::Import => "Reading import",
                };
                format!("{action}… · Esc cancels")
            }
            crate::archive::ArchiveProgress::Finalizing => {
                format!("Finishing {}…", job.kind.name())
            }
        };
        Some(text)
    }

    pub(crate) fn background_work_active(&self) -> bool {
        self.archive_job.is_some()
            || self
                .update_job
                .as_ref()
                .is_some_and(|job| job.kind == UpdateJobKind::Install)
    }

    fn set_message(&mut self, text: String, kind: MessageKind, lifetime: MessageLifetime) {
        self.set_message_until(text, kind, Instant::now() + lifetime.duration());
    }

    fn set_message_until(&mut self, text: String, kind: MessageKind, until: Instant) {
        // A confirmation is only safe while its matching prompt is visible.
        // Any independent status replaces that prompt and therefore disarms
        // the pending destructive action as part of the same state change.
        self.pending = None;
        self.message = Some(Message { text, kind, until });
        self.dirty = true;
    }

    /// Drop expired status messages. Returns true when the UI should redraw.
    pub fn expire_message(&mut self) -> bool {
        if let Some(m) = &self.message
            && Instant::now() >= m.until
        {
            self.pending = None;
            self.message = None;
            self.dirty = true;
            return true;
        }
        false
    }

    /// Arm a destructive action for its explicit confirmation step, and say so.
    pub fn ask_confirm(&mut self, confirm: Confirm, prompt: impl Into<String>) {
        let until = Instant::now() + MessageLifetime::Brief.duration();
        self.set_message_until(prompt.into(), MessageKind::Info, until);
        self.pending = Some((confirm, until));
    }

    /// Whether `confirm` is armed and still inside its window.
    pub fn awaiting(&self, confirm: Confirm) -> bool {
        matches!(&self.pending, Some((armed, until)) if *armed == confirm && Instant::now() < *until)
    }

    pub fn pending_confirmation(&self) -> Option<&Confirm> {
        self.pending
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(confirm, _)| confirm)
    }

    // ----------------------------------------------------------- settings

    /// Step a settings row by `delta` (+1 forward, −1 back), wrapping.
    pub fn cycle_setting(&mut self, index: usize, delta: isize) {
        use crate::settings::{DATE_FORMATS, PREVIEW_POSITIONS, SORTS, THEMES, cycle_by};
        if index >= SETTINGS_ITEMS.len() {
            return;
        }
        if let Err(error) = self.update_store(|data| {
            data.update_settings(|settings| match index {
                0 => settings.sort = cycle_by(&SORTS, &settings.sort, delta),
                1 => settings.selected_color = cycle_by(&THEMES, &settings.selected_color, delta),
                2 => settings.date_format = cycle_by(&DATE_FORMATS, &settings.date_format, delta),
                3 => {
                    settings.preview_position =
                        cycle_by(&PREVIEW_POSITIONS, &settings.preview_position, delta)
                }
                _ => {}
            })
        }) {
            self.report_store_error("Could not update settings", error);
        }
    }

    pub fn setting_value(&self, index: usize) -> String {
        match index {
            0 => crate::settings::sort_label(&self.settings.sort).to_string(),
            1 => crate::settings::theme_label(&self.settings.selected_color),
            2 => self.settings.date_format.clone(),
            3 => {
                crate::settings::preview_position_label(&self.settings.preview_position).to_string()
            }
            _ => String::new(),
        }
    }
}

fn task_labels_contain(task: &Task, labels: &[Label], query: &str) -> bool {
    task.label_ids.iter().any(|id| {
        labels
            .iter()
            .find(|label| label.id == *id)
            .is_some_and(|label| caseless_contains(&label.name, query))
    })
}

fn edit_error_message(error: &StoreError) -> String {
    match error {
        StoreError::StaleEntity { .. } => {
            format!("{error}; close and reopen the editor to load the latest values")
        }
        _ => error.to_string(),
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    s.graphemes(true).take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exported_task_archive(root: &Path, title: &str) -> PathBuf {
        let mut source = Store::open(root.join("source")).expect("open archive source");
        source
            .update(|data| {
                data.tasks.push(Task::new(title, 0, None, ""));
                Ok(())
            })
            .expect("create archived task");
        let path = root.join("tasks.mach");
        crate::archive::export(&source, Some(&path)).expect("export task archive");
        path
    }

    fn wait_for_archive(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.archive_job.is_some() && Instant::now() < deadline {
            app.poll_archive();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.archive_job.is_none(), "archive worker did not finish");
    }

    fn assert_message_lifetime(app: &App, expected: Duration) {
        let remaining = app
            .message
            .as_ref()
            .expect("temporary message")
            .until
            .saturating_duration_since(Instant::now());
        assert!(remaining <= expected, "{remaining:?} exceeds {expected:?}");
        assert!(
            remaining >= expected.saturating_sub(Duration::from_millis(100)),
            "{remaining:?} is shorter than {expected:?}"
        );
    }

    fn update_result(newer: bool) -> crate::update::CheckResult {
        crate::update::CheckResult {
            current: "0.2.0".into(),
            latest: if newer { "0.3.0" } else { "0.2.0" }.into(),
            tag: if newer { "v0.3.0" } else { "v0.2.0" }.into(),
            newer,
            prerelease: false,
            release_url: "https://example.test/release".into(),
            asset_name: "mach-aarch64-apple-darwin".into(),
            asset_url: "https://example.test/binary".into(),
            checksums_url: "https://example.test/SHA256SUMS".into(),
        }
    }

    fn automatic_outcome(newer: bool) -> UpdateOutcome {
        UpdateOutcome::Automatic(CheckResponse::Modified {
            value: update_result(newer),
            etag: None,
        })
    }

    fn update_failure(message: &str) -> CheckFailure {
        CheckFailure {
            message: message.into(),
            retry_at: None,
        }
    }

    fn finished(result: Result<UpdateOutcome, CheckFailure>) -> UpdateEvent {
        UpdateEvent::Finished(Box::new(result))
    }

    fn claim_automatic_lease(app: &mut App, now: i64) -> UpdateLease {
        let AutomaticClaim::Claimed(lease) = app
            .update_state
            .as_mut()
            .expect("test update state")
            .try_claim_automatic(now)
            .unwrap()
        else {
            panic!("automatic update should be due");
        };
        lease
    }

    #[test]
    fn typeahead_buffer_is_bounded_by_the_longest_searchable_title() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-typeahead-test")
            .expect("open in-memory store");
        let mut app = App::with_store("test", store).expect("build app");
        app.mode = Mode::Normal;

        for _ in 0..(MAX_TITLE_LEN * 2) {
            app.typeahead_jump('x');
        }

        assert!(
            app.typeahead.graphemes(true).count() <= MAX_TITLE_LEN,
            "a held key must not grow the navigation query without bound"
        );
    }

    #[test]
    fn transient_messages_use_three_shared_lifetimes() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-message-lifetime-test")
            .expect("open in-memory store");
        let mut app = App::with_store("test", store).expect("build app");

        app.info("brief info");
        assert_message_lifetime(&app, MessageLifetime::Brief.duration());

        app.ask_confirm(Confirm::Quit, "brief confirmation");
        assert_message_lifetime(&app, MessageLifetime::Brief.duration());

        app.ask_confirm(Confirm::DiscardTask(None), "brief discard confirmation");
        assert_message_lifetime(&app, MessageLifetime::Brief.duration());

        app.error("standard error");
        assert_message_lifetime(&app, MessageLifetime::Standard.duration());

        app.archive_result("long archive result");
        assert_message_lifetime(&app, MessageLifetime::Long.duration());

        app.show_update_message("long update result".into(), MessageKind::Info);
        assert_message_lifetime(&app, MessageLifetime::Long.duration());
    }

    #[test]
    fn background_import_reloads_the_completed_store() {
        let root = std::env::temp_dir().join(format!(
            "mach-background-import-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let archive = exported_task_archive(&root, "imported in the background");
        let store = Store::open(root.join("destination")).expect("open archive destination");
        let mut app = App::with_store("test", store).expect("build destination app");

        app.start_import_archive(archive);
        assert!(app.background_work_active());
        wait_for_archive(&mut app);

        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.tasks[0].title, "imported in the background");
        assert!(
            app.message
                .as_ref()
                .is_some_and(|message| message.text.contains("Imported 1 task"))
        );

        drop(app);
        std::fs::remove_dir_all(&root).expect("remove archive test directory");
    }

    #[test]
    fn cancelling_a_background_import_prevents_its_commit() {
        let root = std::env::temp_dir().join(format!(
            "mach-cancel-import-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let archive = exported_task_archive(&root, "must not be imported");
        let destination = root.join("destination");
        let store = Store::open(&destination).expect("open archive destination");
        let lock = rusqlite::Connection::open(destination.join("mach.db"))
            .expect("open destination database lock");
        lock.execute_batch("BEGIN IMMEDIATE")
            .expect("hold destination write lock");
        let mut app = App::with_store("test", store).expect("build destination app");

        app.start_import_archive(archive);
        assert!(app.cancel_archive());
        lock.execute_batch("ROLLBACK")
            .expect("release destination write lock");
        wait_for_archive(&mut app);

        assert!(app.tasks.is_empty());
        assert_eq!(
            app.message.as_ref().map(|message| message.text.as_str()),
            Some("Import cancelled")
        );

        drop(lock);
        drop(app);
        std::fs::remove_dir_all(&root).expect("remove archive test directory");
    }

    #[test]
    fn quit_waits_for_background_archive_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "mach-quit-during-import-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let archive = exported_task_archive(&root, "must not outlive mach");
        let destination = root.join("destination");
        let store = Store::open(&destination).expect("open archive destination");
        let lock = rusqlite::Connection::open(destination.join("mach.db"))
            .expect("open destination database lock");
        lock.execute_batch("BEGIN IMMEDIATE")
            .expect("hold destination write lock");
        let mut app = App::with_store("test", store).expect("build destination app");

        app.start_import_archive(archive);
        app.request_quit();
        assert!(!app.should_quit, "quit must wait for archive cleanup");
        lock.execute_batch("ROLLBACK")
            .expect("release destination write lock");
        wait_for_archive(&mut app);

        assert!(app.should_quit);
        assert!(app.tasks.is_empty());

        drop(lock);
        drop(app);
        std::fs::remove_dir_all(&root).expect("remove archive test directory");
    }

    #[test]
    fn automatic_update_claim_is_shared_across_task_stores() {
        let root = std::env::temp_dir().join(format!(
            "mach-update-claim-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let now = 1_800_000_000;
        let state_path = root.join("global").join("update.db");
        let mut first = App::with_store_and_update_state(
            "test",
            Store::open(root.join("one")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();

        assert!(matches!(
            first
                .update_state
                .as_mut()
                .unwrap()
                .try_claim_automatic(now)
                .unwrap(),
            AutomaticClaim::Claimed(_)
        ));
        drop(first);

        let mut second = App::with_store_and_update_state(
            "test",
            Store::open(root.join("two")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        assert!(matches!(
            second
                .update_state
                .as_mut()
                .unwrap()
                .try_claim_automatic(now)
                .unwrap(),
            AutomaticClaim::Waiting(_)
        ));
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_automatic_update_check_retries_before_the_daily_interval() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-update-retry-test").unwrap();
        let mut app = App::with_store("test", store).unwrap();
        let now = Utc::now().timestamp();

        let lease = claim_automatic_lease(&mut app, now);
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: Some(lease),
        });
        tx.send(finished(Err(update_failure("offline")))).unwrap();

        assert!(!app.poll_update());
        let retry_at = app
            .update_state
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .next_check_at
            .expect("failed check schedules a retry");
        assert!(retry_at < now + 24 * 60 * 60);
        assert!(matches!(
            app.update_state
                .as_mut()
                .unwrap()
                .try_claim_automatic(retry_at)
                .unwrap(),
            AutomaticClaim::Claimed(_)
        ));
    }

    #[test]
    fn available_update_notice_survives_reopening_the_app() {
        let dir = std::env::temp_dir().join(format!(
            "mach-update-notice-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let state_path = dir.join("global-update.db");
        let mut app = App::with_store_and_update_state(
            "0.2.0",
            Store::open(dir.join("tasks")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        let lease = claim_automatic_lease(&mut app, Utc::now().timestamp());
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: Some(lease),
        });
        tx.send(finished(Ok(automatic_outcome(true)))).unwrap();

        assert!(app.poll_update());
        drop(app);

        let reopened = App::with_store_and_update_state(
            "0.2.0",
            Store::open(dir.join("tasks")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        assert!(
            reopened
                .status_message()
                .is_some_and(|(text, _)| { text.contains("v0.2.0 → v0.3.0 available") })
        );
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn available_update_notice_reaches_an_already_running_instance() {
        let root = std::env::temp_dir().join(format!(
            "mach-running-update-notice-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let state_path = root.join("global-update.db");
        let mut checker = App::with_store_and_update_state(
            "0.2.0",
            Store::open(root.join("tasks-one")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        let mut observer = App::with_store_and_update_state(
            "0.2.0",
            Store::open(root.join("tasks-two")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        let now = Utc::now().timestamp();
        let lease = claim_automatic_lease(&mut checker, now);
        let (tx, rx) = mpsc::channel();
        checker.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: Some(lease),
        });
        tx.send(finished(Ok(automatic_outcome(true)))).unwrap();

        assert!(checker.poll_update());
        assert!(observer.poll_automatic_update_schedule_at(now + 1));
        assert!(
            observer
                .status_message()
                .is_some_and(|(text, _)| { text.contains("v0.2.0 → v0.3.0 available") })
        );
        drop(checker);
        drop(observer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_latest_release_is_compared_per_running_binary() {
        let root = std::env::temp_dir().join(format!(
            "mach-multiple-binaries-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let state_path = root.join("global-update.db");
        let now = 1_800_000_000;
        let mut state = UpdateStateStore::open(&state_path).unwrap();
        let AutomaticClaim::Claimed(lease) = state.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };
        state.finish_modified(&lease, now, None, "0.3.0").unwrap();
        drop(state);

        let current = App::with_store_and_update_state(
            "0.3.0",
            Store::open(root.join("current-tasks")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        assert!(current.status_message().is_none());
        drop(current);

        let older = App::with_store_and_update_state(
            "0.2.0",
            Store::open(root.join("older-tasks")).unwrap(),
            UpdateStateStore::open(&state_path),
        )
        .unwrap();
        assert!(
            older
                .status_message()
                .is_some_and(|(text, _)| { text.contains("v0.2.0 → v0.3.0 available") })
        );
        drop(older);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgraded_version_shows_whats_new_once() {
        let dir = std::env::temp_dir().join(format!(
            "mach-whats-new-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut store = Store::open(&dir).unwrap();
        store
            .update(|data| {
                data.settings.last_run_version = Some("0.1.9".into());
                Ok(())
            })
            .unwrap();

        let mut first = App::with_store("0.2.0", store).unwrap();
        assert_eq!(first.mode, Mode::WhatsNew);
        first.record_launch().unwrap();
        drop(first);

        let second = App::with_store("0.2.0", Store::open(&dir).unwrap()).unwrap();
        assert_eq!(second.mode, Mode::Normal);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recording_launch_reclassifies_a_concurrent_provisional_welcome() {
        let dir = std::env::temp_dir().join(format!(
            "mach-concurrent-launch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut first = App::with_store("0.4.0", Store::open(&dir).unwrap()).unwrap();
        let mut second = App::with_store("0.4.0", Store::open(&dir).unwrap()).unwrap();
        assert_eq!(first.mode, Mode::Welcome);
        assert_eq!(second.mode, Mode::Welcome);

        first.record_launch().unwrap();
        second.record_launch().unwrap();

        assert_eq!(first.mode, Mode::Welcome);
        assert_eq!(second.mode, Mode::Normal);
        drop(first);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tui_update_install_success_requests_restart() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-install-success-test").unwrap();
        let mut app = App::with_store("test", store).unwrap();
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Install,
            lease: None,
        });
        app.update_activity = Some(UpdateActivity::Checking);
        tx.send(finished(Ok(UpdateOutcome::Installed {
            result: crate::update::InstallResult {
                destination: "/tmp/mach-bin/mach".into(),
                tag: "v0.3.0".into(),
                disposition: crate::update::InstallDisposition::Installed,
            },
            info: update_result(true),
            etag: None,
        })))
        .unwrap();

        assert!(app.poll_update());
        assert_eq!(
            app.status_message().map(|(text, _)| text),
            Some("Installed v0.3.0 · restart mach")
        );
        assert!(app.update_activity().is_none());

        assert!(!app.expire_message());
        assert_eq!(
            app.status_message().map(|(text, _)| text),
            Some("Installed v0.3.0 · restart mach")
        );

        app.open_slash();
        assert!(app.status_message().is_none());
    }

    #[test]
    fn tui_update_reports_a_concurrently_installed_release_truthfully() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-install-race-test").unwrap();
        let mut app = App::with_store("test", store).unwrap();
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Install,
            lease: None,
        });
        app.update_activity = Some(UpdateActivity::Checking);
        tx.send(finished(Ok(UpdateOutcome::Installed {
            result: crate::update::InstallResult {
                destination: "/tmp/mach-bin/mach".into(),
                tag: "v0.3.1".into(),
                disposition: crate::update::InstallDisposition::AlreadyCurrent,
            },
            info: update_result(true),
            etag: None,
        })))
        .unwrap();

        assert!(app.poll_update());
        assert_eq!(
            app.status_message().map(|(text, _)| text),
            Some("Already installed v0.3.1 · restart mach")
        );
    }

    #[test]
    fn update_download_progress_is_applied_before_the_final_result() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-install-progress-test").unwrap();
        let mut app = App::with_store("test", store).unwrap();
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Install,
            lease: None,
        });
        app.update_activity = Some(UpdateActivity::Checking);
        tx.send(UpdateEvent::DownloadProgress(
            crate::update::DownloadProgress {
                downloaded: 512,
                total: Some(1024),
            },
        ))
        .unwrap();

        assert!(app.poll_update());
        assert_eq!(
            app.update_activity(),
            Some(UpdateActivity::Downloading(
                crate::update::DownloadProgress {
                    downloaded: 512,
                    total: Some(1024),
                }
            ))
        );
    }

    #[test]
    fn tui_update_install_error_keeps_the_recovery_command() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-install-error-test").unwrap();
        let mut app = App::with_store("test", store).unwrap();
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Install,
            lease: None,
        });
        tx.send(finished(Ok(UpdateOutcome::InstallFailed {
            message:
                "this mach executable is managed by Cargo; run cargo install --locked mach-tui"
                    .into(),
            info: update_result(true),
            etag: None,
        })))
        .unwrap();

        assert!(app.poll_update());
        let message = app.message.as_ref().expect("visible install error");
        assert_eq!(message.kind, MessageKind::Error);
        assert!(message.text.contains("cargo install --locked mach-tui"));
    }

    #[test]
    fn automatic_update_results_are_silent_unless_a_new_version_exists() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-auto-update-test").unwrap();
        let mut app = App::with_store("0.2.0", store).unwrap();
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: None,
        });
        tx.send(finished(Ok(automatic_outcome(false)))).unwrap();

        assert!(!app.poll_update());
        assert!(app.message.is_none());

        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: None,
        });
        tx.send(finished(Err(update_failure("offline")))).unwrap();

        assert!(!app.poll_update());
        assert!(app.message.is_none());
    }

    #[test]
    fn automatic_update_notice_waits_for_an_active_confirmation() {
        let store = Store::open_in_memory_with_paths("/tmp/mach-deferred-update-test").unwrap();
        let mut app = App::with_store("0.2.0", store).unwrap();
        app.ask_confirm(Confirm::Quit, "Press Ctrl+C again to quit");
        let (tx, rx) = mpsc::channel();
        app.update_job = Some(UpdateJob {
            rx,
            kind: UpdateJobKind::Automatic,
            lease: None,
        });
        tx.send(finished(Ok(automatic_outcome(true)))).unwrap();

        assert!(!app.poll_update());
        assert_eq!(app.pending_confirmation(), Some(&Confirm::Quit));
        assert_eq!(
            app.message.as_ref().map(|message| message.text.as_str()),
            Some("Press Ctrl+C again to quit")
        );

        app.cancel_pending();
        assert!(app.status_message().is_some_and(|(text, _)| {
            text.contains("v0.2.0 → v0.3.0 available · run /update to install")
        }));

        app.info("Temporary action result");
        assert_eq!(
            app.status_message().map(|(text, _)| text),
            Some("Temporary action result")
        );
        app.message.as_mut().unwrap().until = Instant::now();
        assert!(app.expire_message());
        assert!(app.status_message().is_some_and(|(text, _)| {
            text.contains("v0.2.0 → v0.3.0 available · run /update to install")
        }));

        app.open_slash();
        assert!(app.status_message().is_none());
    }
}
