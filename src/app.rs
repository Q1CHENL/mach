//! Application state and every operation the UI can trigger.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::widgets::{ListState, TableState};
use unicode_segmentation::UnicodeSegmentation;

use crate::due;
use crate::form::{CategoryForm, TaskDraft, TaskForm};
use crate::image::ImageStore;
use crate::model::{
    ALL_CATEGORY, Category, MAX_CATEGORY_COUNT, MAX_CATEGORY_NAME_LEN, MAX_TASK_COUNT,
    MAX_TITLE_LEN, Task, caseless_key,
};
use crate::settings::Settings;
use crate::store::{
    Attachment, CategoryPatch, RelativePosition, Store, StoreData, StoreError, TaskPatch,
};
use crate::text_input::TextInput;
use crate::theme::Theme;

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
    Help,
    Settings,
    Welcome,
}

impl Mode {
    /// Anything drawn on top of the two panels.
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            Mode::Help | Mode::Settings | Mode::Welcome | Mode::TaskForm | Mode::CategoryForm
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
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
    /// Ctrl+C again leaves mach.
    Quit,
}

/// How long a double-press confirm stays armed.
const CONFIRM_WINDOW: Duration = Duration::from_millis(2000);

/// Idle gap after which type-to-jump starts a new query.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(800);

/// Rects from the last frame, used to hit-test mouse events.
#[derive(Debug, Default, Clone, Copy)]
pub struct Areas {
    pub sidebar: Rect,
    pub tasks: Rect,
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
}

pub const SETTINGS_ITEMS: [&str; 4] = ["Sort", "Theme", "Date format", "Preview"];

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
    store_revision: u64,
    pub tasks: Vec<Task>,
    pub categories: Vec<Category>,
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
    pub settings_index: usize,
    /// First help content row currently visible.
    pub help_scroll: usize,
    pub message: Option<Message>,
    /// Pending entity-bound destructive action and its deadline.
    pub pending: Option<(Confirm, Instant)>,
    /// Last click `(time, panel, row)` for double-click detection.
    pub last_click: Option<(Instant, Focus, usize)>,
    pub should_quit: bool,
    pub areas: Areas,
    /// Body/preview image store.
    pub images: ImageStore,
    pub(crate) attachments: Vec<Attachment>,
    /// Type-to-jump buffer (Tasks/Sidebar focus); cleared on timeout.
    typeahead: String,
    typeahead_at: Option<Instant>,
    /// Needs a redraw.
    pub dirty: bool,
    /// Incremented on task mutation (invalidates preview cache).
    pub data_gen: u64,
    /// Per-category `(done, total)`, parallel to `categories`.
    cat_progress: Vec<(usize, usize)>,
    /// Cached body editor for the read-only preview pane.
    pub preview_form: Option<TaskForm>,
    preview_task_id: Option<String>,
    preview_gen: u64,
    /// Entity snapshots captured when an edit dialog opens. Save compares only
    /// editable fields so unrelated changes (for example, another agent
    /// toggling `done`) are preserved instead of becoming false conflicts.
    task_edit_base: Option<Task>,
    category_edit_base: Option<Category>,
    /// In-flight `/update` check.
    update_rx: Option<Receiver<Result<crate::update::CheckResult, String>>>,
    /// Whether persistence polling has failed since its last successful pass.
    /// Repeated failures are quiet so they cannot continuously replace messages
    /// or disarm destructive confirmations; success rearms reporting.
    external_poll_failed: bool,
}

impl App {
    pub fn new(version: &str) -> Result<Self, StoreError> {
        Self::with_store(version, Store::open_default(None)?)
    }

    pub fn with_store(version: &str, mut store: Store) -> Result<Self, StoreError> {
        // Only ever shown once, not again on every upgrade. The transaction
        // reads fresh state, so two concurrently-starting processes cannot
        // both treat an existing profile as new.
        let initial = store.snapshot()?;
        let (first_run, snapshot) = if initial.settings.last_run_version.as_deref() == Some(version)
        {
            (false, initial)
        } else {
            store.update_with_snapshot(|data| Ok(data.settings.take_first_run(version)))?
        };
        let StoreData {
            revision,
            categories: real_cats,
            tasks,
            settings,
            attachments,
        } = snapshot;
        // "All Tasks" is a view only — prepended in memory, never saved.
        let mut categories = vec![Category::all_tasks()];
        categories.extend(real_cats);
        let mut images = ImageStore::with_root(store.images_dir().to_path_buf());
        images.set_attachments(&attachments);

        let mut app = Self {
            store,
            store_revision: revision,
            tasks,
            categories,
            settings,
            focus: Focus::Tasks,
            mode: if first_run {
                Mode::Welcome
            } else {
                Mode::Normal
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
            update_rx: None,
            external_poll_failed: false,
        };
        app.rebuild_view();
        Ok(app)
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
        if revision == self.store_revision || self.form.is_some() || self.category_form.is_some() {
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
            tasks,
            settings,
            attachments,
        } = snapshot;
        self.store_revision = revision;
        self.tasks = tasks;
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

    /// Start a non-blocking GitHub release check (for `/update`).
    pub fn start_update_check(&mut self) {
        if self.update_rx.is_some() {
            self.info("Already checking for updates…");
            return;
        }
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("mach-update-check".into())
            .spawn(move || {
                let _ = tx.send(crate::update::check());
            }) {
            Ok(_) => {
                self.update_rx = Some(rx);
                self.info("Checking for updates…");
            }
            Err(error) => self.error(format!("Could not start update check: {error}")),
        }
    }

    /// Apply a finished update check, if any. Returns true when UI should redraw.
    pub fn poll_update_check(&mut self) -> bool {
        let Some(rx) = &self.update_rx else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(info)) => {
                self.update_rx = None;
                if info.newer {
                    self.info(format!(
                        "v{} → v{} available · run: mach update --install",
                        info.current, info.latest
                    ));
                } else {
                    self.info(info.summary());
                }
                true
            }
            Ok(Err(err)) => {
                self.update_rx = None;
                self.error(err);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.update_rx = None;
                self.error("Update check failed");
                true
            }
        }
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
        let Some((id, generation)) = self.selected_task().map(|t| (t.id.clone(), self.data_gen))
        else {
            self.invalidate_preview();
            return;
        };
        if self.preview_task_id.as_deref() == Some(id.as_str())
            && self.preview_gen == generation
            && self.preview_form.is_some()
        {
            return;
        }
        let Some(task) = self.selected_task().cloned() else {
            self.invalidate_preview();
            return;
        };
        let mut form = TaskForm::edit(&task);
        form.set_categories(&self.categories, task.category_id.as_deref());
        form.set_image_root(self.images.root().to_path_buf());
        form.set_attachments(&self.attachments);
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
                        && (contains_ignore_case(&t.title, &q) || body_contains(t, &q))
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
        let mut rows = Vec::with_capacity(self.view.len() + self.categories.len());
        let mut prev: Option<Option<&str>> = None;
        for (vi, &ti) in self.view.iter().enumerate() {
            let key = self.tasks[ti].category_id.as_deref();
            if prev != Some(key) {
                let title = match key {
                    Some(id) => self.category_name(id).unwrap_or("Unknown").to_string(),
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
        use std::collections::HashMap;
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
            "due" => view.sort_by_cached_key(|i| {
                let due = &self.tasks[*i].due;
                (due.is_empty(), due::sort_key(due))
            }),
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
        let now = Instant::now();
        if self
            .typeahead_at
            .is_none_or(|t| now.duration_since(t) > TYPEAHEAD_TIMEOUT)
        {
            self.typeahead.clear();
        }
        let limit = match self.focus {
            Focus::Tasks => MAX_TITLE_LEN,
            Focus::Sidebar => MAX_CATEGORY_NAME_LEN,
        };
        if self.typeahead.graphemes(true).count() < limit {
            self.typeahead.push(c);
        }
        self.typeahead_at = Some(now);

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

    fn clear_typeahead(&mut self) {
        self.typeahead.clear();
        self.typeahead_at = None;
    }

    // ------------------------------------------------------------ mutation

    fn recompute_cat_progress(&mut self) {
        let mut all_done = 0usize;
        let mut all_total = 0usize;
        let mut per: Vec<(usize, usize)> = self.categories.iter().map(|_| (0, 0)).collect();
        for t in &self.tasks {
            all_total += 1;
            if t.done {
                all_done += 1;
            }
            if let Some(cid) = t.category_id.as_deref()
                && let Some(idx) = self.categories.iter().position(|c| c.id == cid)
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
                let importance =
                    (data.task(&id)?.importance + 1) % (crate::model::MAX_IMPORTANCE + 1);
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
        let mut form = TaskForm::new();
        let category = (!self.is_all_view()).then(|| self.current_category_id());
        form.set_categories(&self.categories, category);
        form.set_image_root(self.images.root().to_path_buf());
        form.set_attachments(&self.attachments);
        self.task_edit_base = None;
        self.form = Some(form);
        self.mode = Mode::TaskForm;
    }

    /// Opens the dialog on the selected task.
    pub fn open_edit_task(&mut self) {
        if let Some(task) = self.selected_task().cloned() {
            let mut form = TaskForm::edit(&task);
            form.set_categories(&self.categories, task.category_id.as_deref());
            form.set_image_root(self.images.root().to_path_buf());
            form.set_attachments(&self.attachments);
            // Decode body pictures off the UI thread so the dialog opens
            // immediately; they fill in on the next frames.
            self.images.prefetch(form.body.images());
            self.task_edit_base = Some(task);
            self.form = Some(form);
            self.mode = Mode::TaskForm;
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
        let (inline_due, title) = due::parse(draft.title.trim());
        if title.is_empty() || self.tasks.len() >= MAX_TASK_COUNT {
            return None;
        }
        let due = if draft.due.is_empty() {
            &inline_due
        } else {
            &draft.due
        };
        let body = draft.body.clone();
        let category_id = draft.category_id.clone();
        let importance = draft.importance;
        let task = match self.update_store(|data| {
            data.create_task(title, body, due.to_string(), importance, category_id)
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
        let (inline_due, title) = due::parse(draft.title.trim());
        if title.is_empty() {
            return false;
        }
        let due = if draft.due.is_empty() {
            &inline_due
        } else {
            &draft.due
        };
        let expected = self.task_edit_base.clone();
        let id = id.to_string();
        let due = due.to_string();
        let patch = match expected.as_ref() {
            Some(base) => TaskPatch {
                title: (title != base.title).then_some(title),
                body: (draft.body != base.body).then(|| draft.body.clone()),
                due: (due != base.due).then_some(due),
                importance: (draft.importance != base.importance).then_some(draft.importance),
                category_id: (draft.category_id != base.category_id)
                    .then(|| draft.category_id.clone()),
                ..TaskPatch::default()
            },
            None => TaskPatch {
                title: Some(title),
                body: Some(draft.body.clone()),
                due: Some(due),
                importance: Some(draft.importance),
                category_id: Some(draft.category_id.clone()),
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
                    && caseless_key(existing_name.trim()) == caseless_key(name.trim())
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

    // -------------------------------------------------------------- slash / search

    /// Open the `/` command palette.
    pub fn open_slash(&mut self) {
        if self.searching {
            self.end_search();
        }
        self.mode = Mode::Slash;
        self.input = TextInput::new("", 128);
        self.slash_index = 0;
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
        self.set_message(text.into(), MessageKind::Info, 2000);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.set_message(text.into(), MessageKind::Error, 2500);
    }

    fn set_message(&mut self, text: String, kind: MessageKind, millis: u64) {
        self.set_message_until(text, kind, Instant::now() + Duration::from_millis(millis));
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

    /// Arm a destructive key on its second press, and say so.
    pub fn ask_confirm(&mut self, confirm: Confirm, prompt: impl Into<String>) {
        let until = Instant::now() + CONFIRM_WINDOW;
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

/// Unicode-caseless contains. `folded_needle` must already be normalized.
/// ASCII path avoids allocating.
fn contains_ignore_case(haystack: &str, folded_needle: &str) -> bool {
    if folded_needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && folded_needle.is_ascii() {
        return haystack
            .as_bytes()
            .windows(folded_needle.len())
            .any(|w| w.eq_ignore_ascii_case(folded_needle.as_bytes()));
    }
    caseless_key(haystack).contains(folded_needle)
}

/// Whether any prose or to-do in the body mentions `query`.
fn body_contains(task: &Task, query: &str) -> bool {
    task.body.iter().any(|block| match block {
        crate::model::Block::Text { text }
        | crate::model::Block::Todo { text, .. }
        | crate::model::Block::Bullet { text }
        | crate::model::Block::Number { text }
        | crate::model::Block::Link { url: text } => contains_ignore_case(text, query),
        crate::model::Block::Image { .. } => false,
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
}
