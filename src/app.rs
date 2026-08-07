//! Application state and every operation the UI can trigger.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::widgets::{ListState, TableState};

use crate::due;
use crate::form::{CategoryForm, TaskDraft, TaskForm};
use crate::image::ImageStore;
use crate::model::{
    ALL_CATEGORY, Category, MAX_CATEGORY_COUNT, MAX_CATEGORY_NAME_LEN, MAX_TASK_COUNT,
    MAX_TITLE_LEN, Task,
};
use crate::settings::Settings;
use crate::store;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Backspace again deletes the selected task or category.
    Delete,
    /// Ctrl+C again leaves mach.
    Quit,
}

/// How long a double-press confirm stays armed.
const CONFIRM_WINDOW: Duration = Duration::from_millis(2000);

/// Idle gap after which type-to-jump starts a new query.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(800);

/// Debounce window for rapid toggle/flag task saves.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

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
    pub message: Option<Message>,
    /// Pending double-press confirm (`Delete` / `Quit`) and its deadline.
    pub pending: Option<(Confirm, Instant)>,
    /// Last click `(time, panel, row)` for double-click detection.
    pub last_click: Option<(Instant, Focus, usize)>,
    pub should_quit: bool,
    pub areas: Areas,
    /// Body/preview image store.
    pub images: ImageStore,
    /// Type-to-jump buffer (Tasks/Sidebar focus); cleared on timeout.
    typeahead: String,
    typeahead_at: Option<Instant>,
    /// Needs a redraw.
    pub dirty: bool,
    /// In-memory tasks differ from disk; write pending.
    tasks_dirty: bool,
    tasks_dirty_since: Option<Instant>,
    /// Categories differ from disk; write on next flush.
    categories_dirty: bool,
    /// Incremented on task mutation (invalidates preview cache).
    pub data_gen: u64,
    /// Per-category `(done, total)`, parallel to `categories`.
    cat_progress: Vec<(usize, usize)>,
    /// Cached body editor for the read-only preview pane.
    pub preview_form: Option<TaskForm>,
    preview_task_id: Option<String>,
    preview_gen: u64,
    /// In-flight `/update` check.
    update_rx: Option<Receiver<Result<crate::update::CheckResult, String>>>,
}

impl App {
    pub fn new(version: &str) -> Self {
        let mut settings = Settings::load();
        // Only ever shown once, not again on every upgrade.
        let first_run = settings.take_first_run(version);
        let (real_cats, tasks) = store::load_all();
        // "All Tasks" is a view only — prepended in memory, never saved.
        let mut categories = vec![Category::all_tasks()];
        categories.extend(real_cats);

        let mut app = Self {
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
            message: None,
            pending: None,
            last_click: None,
            should_quit: false,
            areas: Areas::default(),
            images: ImageStore::default(),
            typeahead: String::new(),
            typeahead_at: None,
            dirty: true,
            tasks_dirty: false,
            tasks_dirty_since: None,
            categories_dirty: false,
            data_gen: 0,
            cat_progress: Vec::new(),
            preview_form: None,
            preview_task_id: None,
            preview_gen: 0,
            update_rx: None,
        };
        app.rebuild_view();
        app
    }

    /// Start a non-blocking GitHub release check (for `/update`).
    pub fn start_update_check(&mut self) {
        if self.update_rx.is_some() {
            self.info("Already checking for updates…");
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::update::check());
        });
        self.update_rx = Some(rx);
        self.info("Checking for updates…");
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

    /// Rebuild [`preview_form`] if the selection or `data_gen` changed.
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
        let task = self.selected_task().expect("checked").clone();
        self.preview_form = Some(TaskForm::edit(&task));
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
        self.dirty = true;
        if self.cat_progress.len() != self.categories.len() {
            self.recompute_cat_progress();
        }
        let cat_id = self.current_category_id();
        let all = cat_id == ALL_CATEGORY;
        let hide_done = self.settings.hide_done;
        let candidates: Vec<usize> = if self.searching {
            let q = self.search_query.to_lowercase();
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
        if self.task_index >= self.view.len() {
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
            "due" => view.sort_by_key(|i| {
                let due = &self.tasks[*i].due;
                (due.is_empty(), due::sort_key(due))
            }),
            _ => {} // manual — keep tasks.json order
        }
    }

    pub fn task_count(&self) -> usize {
        self.view.len()
    }

    pub fn visible_task(&self, pos: usize) -> Option<&Task> {
        self.view.get(pos).map(|i| &self.tasks[*i])
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
        if next != self.task_index {
            self.task_index = next;
            self.dirty = true;
        }
    }

    pub fn select_task(&mut self, pos: usize) {
        if pos < self.view.len() && pos != self.task_index {
            self.task_index = pos;
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
        self.typeahead.push(c);
        self.typeahead_at = Some(now);

        let query = self.typeahead.clone();
        match self.focus {
            Focus::Tasks => {
                let titles: Vec<&str> = self
                    .view
                    .iter()
                    .map(|&i| self.tasks[i].title.as_str())
                    .collect();
                if let Some(pos) = crate::fuzzy::best_index(&query, titles) {
                    self.task_index = pos;
                }
            }
            Focus::Sidebar => {
                let names: Vec<&str> = self.categories.iter().map(|c| c.name.as_str()).collect();
                if let Some(pos) = crate::fuzzy::best_index(&query, names) {
                    self.select_category(pos);
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
        if next != self.cat_index {
            self.cat_index = next;
            self.on_category_changed();
        }
    }

    /// ↑/↓ stay inside the focused panel. Cross-panel moves use ←/→ or Tab.
    pub fn navigate_vertical(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.pending = None;
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
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Tasks,
            Focus::Tasks => Focus::Sidebar,
        };
        self.pending = None;
    }

    // ------------------------------------------------------------ mutation

    fn note_tasks_changed(&mut self) {
        self.data_gen = self.data_gen.wrapping_add(1);
        self.invalidate_preview();
        self.recompute_cat_progress();
        self.dirty = true;
    }

    /// Write tasks to disk immediately (create / edit / delete / purge).
    pub fn save_tasks(&mut self) {
        self.note_tasks_changed();
        self.tasks_dirty = true;
        self.tasks_dirty_since = None;
        self.flush_saves_now();
    }

    /// Coalesce rapid toggles (done / importance) into one disk write.
    fn save_tasks_debounced(&mut self) {
        self.note_tasks_changed();
        if self.tasks_dirty_since.is_none() {
            self.tasks_dirty_since = Some(Instant::now());
        }
        self.tasks_dirty = true;
    }

    fn save_categories(&mut self) {
        self.categories_dirty = true;
        self.dirty = true;
        self.flush_saves_now();
    }

    pub fn tasks_dirty_pending(&self) -> bool {
        self.tasks_dirty
    }

    /// Write dirty store files if the task-save debounce has elapsed.
    /// Returns true if a write ran.
    pub fn flush_saves(&mut self) -> bool {
        let due = self.tasks_dirty
            && self
                .tasks_dirty_since
                .is_some_and(|t| t.elapsed() >= SAVE_DEBOUNCE);
        if due || self.categories_dirty {
            self.flush_saves_now();
            return true;
        }
        false
    }

    /// Write any dirty store files immediately.
    pub fn flush_saves_now(&mut self) {
        if self.tasks_dirty {
            if let Err(err) = store::save_tasks(&self.tasks) {
                self.error(format!("Could not save tasks: {err}"));
            }
            self.tasks_dirty = false;
            self.tasks_dirty_since = None;
        }
        if self.categories_dirty {
            if let Err(err) = store::save_categories(&self.categories) {
                self.error(format!("Could not save categories: {err}"));
            }
            self.categories_dirty = false;
        }
    }

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
            self.tasks[i].done = !self.tasks[i].done;
            self.save_tasks_debounced();
            self.rebuild_view();
            self.select_task_by_id(&id);
        }
    }

    /// Steps a task's importance up, wrapping back to none after three.
    pub fn cycle_importance(&mut self, pos: usize) {
        if let Some(&i) = self.view.get(pos) {
            let id = self.tasks[i].id.clone();
            let task = &mut self.tasks[i];
            task.importance = (task.importance + 1) % (crate::model::MAX_IMPORTANCE + 1);
            self.save_tasks_debounced();
            self.rebuild_view();
            self.select_task_by_id(&id);
        }
    }

    /// Opens the dialog for a new task, unless the list is full.
    /// All Tasks is a view, not a category — pick a real one first.
    pub fn open_new_task(&mut self) {
        if self.is_all_view() {
            self.info("Pick a category");
            return;
        }
        if self.tasks.len() >= MAX_TASK_COUNT {
            self.error(format!(
                "You already have {MAX_TASK_COUNT} tasks in hand. Maybe deal with them first :)"
            ));
            return;
        }
        self.form = Some(TaskForm::new());
        self.mode = Mode::TaskForm;
    }

    /// Opens the dialog on the selected task.
    pub fn open_edit_task(&mut self) {
        if let Some(task) = self.selected_task() {
            let form = TaskForm::edit(task);
            // Decode body pictures off the UI thread so the dialog opens
            // immediately; they fill in on the next frames.
            self.images.prefetch(form.body.images());
            self.form = Some(form);
            self.mode = Mode::TaskForm;
        }
    }

    pub fn close_form(&mut self) {
        self.form = None;
        self.mode = Mode::Normal;
        self.focus = Focus::Tasks;
        // Drop placed graphics so they do not float over the list; pixels
        // stay in RAM for a fast reopen. GIF frames are dropped with the form.
        self.images.release_form_graphics();
        self.images.clear_preview();
    }

    /// Validates the open form and writes it back to the task list.
    pub fn submit_form(&mut self) {
        let Some(form) = &mut self.form else { return };
        let Some(draft) = form.submit() else { return };
        match form.editing.clone() {
            Some(uuid) => self.update_task(&uuid, &draft),
            None => {
                self.create_task(&draft);
            }
        }
        self.close_form();
    }

    /// Creates a task in the current category and selects it. A `[date]`
    /// left in the title is moved into `due` when `due` is empty.
    /// Refuses All Tasks — it is not a real category.
    pub fn create_task(&mut self, draft: &TaskDraft) -> Option<String> {
        if self.is_all_view() {
            return None;
        }
        let category_id = Some(self.current_category_id().to_string());
        let (inline_due, title) = due::parse(draft.title.trim());
        if title.is_empty() || self.tasks.len() >= MAX_TASK_COUNT {
            return None;
        }
        let due = if draft.due.is_empty() {
            &inline_due
        } else {
            &draft.due
        };
        let mut task = Task::new(&title, draft.importance, category_id, due);
        task.body = draft.body.clone();
        let id = task.id.clone();
        self.tasks.push(task);
        self.save_tasks();
        self.searching = false;
        self.search_query.clear();
        self.rebuild_view();
        self.select_task_by_id(&id);
        Some(id)
    }

    pub fn update_task(&mut self, id: &str, draft: &TaskDraft) {
        let (inline_due, title) = due::parse(draft.title.trim());
        if title.is_empty() {
            return;
        }
        let due = if draft.due.is_empty() {
            &inline_due
        } else {
            &draft.due
        };
        let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) else {
            return;
        };
        task.title = title;
        task.body = draft.body.clone();
        task.importance = draft.importance;
        task.due = due.to_string();
        self.save_tasks();
        self.rebuild_view();
        self.select_task_by_id(id);
    }

    pub fn delete_task(&mut self, pos: usize) {
        let Some(&i) = self.view.get(pos) else { return };
        self.tasks.remove(i);
        self.save_tasks();
        self.rebuild_view();
        if self.task_index >= self.view.len() {
            self.task_index = self.view.len().saturating_sub(1);
        }
        self.pending = None;
    }

    /// Permanently remove done tasks. In All Tasks → every done task; in a
    /// category → only that category's done tasks. Nothing is archived.
    pub fn purge(&mut self) -> usize {
        let everywhere = self.is_all_view();
        let cat_id = self.current_category_id().to_string();
        let before = self.tasks.len();
        self.tasks.retain(|t| {
            if !t.done {
                return true;
            }
            if everywhere {
                return false;
            }
            t.category_id.as_deref() != Some(cat_id.as_str())
        });
        let n = before - self.tasks.len();
        if n > 0 {
            self.save_tasks();
            self.rebuild_view();
            self.task_index = 0;
        }
        n
    }

    /// `/done` — show or hide completed tasks in the list (still on disk).
    pub fn toggle_hide_done(&mut self) -> bool {
        self.settings.hide_done = !self.settings.hide_done;
        self.settings.save();
        self.rebuild_view();
        self.settings.hide_done
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
        self.category_form = Some(CategoryForm::new());
        self.mode = Mode::CategoryForm;
    }

    /// Opens the dialog on the selected category. "All Tasks" is not a
    /// real category and cannot be edited.
    pub fn open_edit_category(&mut self) {
        if self.is_all_view() {
            return;
        }
        if let Some(category) = self.categories.get(self.cat_index) {
            self.category_form = Some(CategoryForm::edit(category));
            self.mode = Mode::CategoryForm;
        }
    }

    pub fn close_category_form(&mut self) {
        self.category_form = None;
        self.mode = Mode::Normal;
    }

    pub fn submit_category_form(&mut self) {
        let Some(form) = &mut self.category_form else {
            return;
        };
        let Some((name, description)) = form.submit() else {
            return;
        };
        let name = truncate_chars(&name, MAX_CATEGORY_NAME_LEN);
        match form.editing.clone() {
            Some(id) => {
                if let Some(cat) = self.categories.iter_mut().find(|c| c.id == id) {
                    cat.name = name;
                    cat.description = description;
                }
                self.save_categories();
            }
            None => {
                let mut category = Category::new(&name);
                category.description = description;
                self.categories.push(category);
                self.save_categories();
                self.cat_index = self.categories.len() - 1;
                self.on_category_changed();
            }
        }
        self.close_category_form();
    }

    /// Deletes the category together with the tasks filed under it.
    /// Category ids are stable UUIDs — no renumbering.
    pub fn delete_category(&mut self) {
        if self.is_all_view() {
            return;
        }
        let id = self.current_category_id().to_string();
        self.tasks
            .retain(|t| t.category_id.as_deref() != Some(id.as_str()));
        self.categories.retain(|c| c.id != id);
        self.save_categories();
        self.save_tasks();
        if self.cat_index >= self.categories.len() {
            self.cat_index = self.categories.len().saturating_sub(1);
        }
        self.pending = None;
        self.on_category_changed();
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
        self.message = Some(Message {
            text,
            kind,
            until: Instant::now() + Duration::from_millis(millis),
        });
        self.dirty = true;
    }

    /// Drop expired status messages. Returns true when the UI should redraw.
    pub fn expire_message(&mut self) -> bool {
        if let Some(m) = &self.message
            && Instant::now() >= m.until
        {
            self.message = None;
            self.dirty = true;
            return true;
        }
        false
    }

    /// Arm a destructive key on its second press, and say so.
    pub fn ask_confirm(&mut self, confirm: Confirm, prompt: impl Into<String>) {
        self.pending = Some((confirm, Instant::now() + CONFIRM_WINDOW));
        self.info(prompt);
    }

    /// Whether `confirm` is armed and still inside its window.
    pub fn awaiting(&self, confirm: Confirm) -> bool {
        matches!(self.pending, Some((armed, until)) if armed == confirm && Instant::now() < until)
    }

    // ----------------------------------------------------------- settings

    /// Step a settings row by `delta` (+1 forward, −1 back), wrapping.
    pub fn cycle_setting(&mut self, index: usize, delta: isize) {
        use crate::settings::{DATE_FORMATS, PREVIEW_POSITIONS, SORTS, THEMES, cycle_by};
        match index {
            0 => self.settings.sort = cycle_by(&SORTS, &self.settings.sort, delta),
            1 => {
                self.settings.selected_color =
                    cycle_by(&THEMES, &self.settings.selected_color, delta)
            }
            2 => {
                self.settings.date_format =
                    cycle_by(&DATE_FORMATS, &self.settings.date_format, delta)
            }
            3 => {
                self.settings.preview_position =
                    cycle_by(&PREVIEW_POSITIONS, &self.settings.preview_position, delta)
            }
            _ => {}
        }
        self.settings.save();
        self.rebuild_view();
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

/// Case-insensitive contains. `needle_lower` must already be lowercased.
/// ASCII path avoids allocating.
fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        return haystack
            .as_bytes()
            .windows(needle_lower.len())
            .any(|w| w.eq_ignore_ascii_case(needle_lower.as_bytes()));
    }
    haystack.to_lowercase().contains(needle_lower)
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

pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
