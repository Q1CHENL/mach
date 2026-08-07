//! The task dialog — title, description, due date and subtasks — used for
//! both creating and editing a task.

use std::time::Instant;

use ratatui::layout::Rect;

use crate::body::BodyEditor;
use crate::due;
use crate::duepicker::DuePicker;
use crate::image::GifPlayback;
use crate::model::{Block, MAX_TITLE_LEN, Task};
use crate::text_input::TextInput;
use crate::undo::{EditKind, History};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Due,
    Importance,
    Body,
}

impl Field {
    /// Tab order follows the layout: the two short fields, then the body.
    pub fn next(self) -> Self {
        match self {
            Self::Title => Self::Due,
            Self::Due => Self::Importance,
            Self::Importance => Self::Body,
            Self::Body => Self::Title,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Title => Self::Body,
            Self::Due => Self::Title,
            Self::Importance => Self::Due,
            Self::Body => Self::Importance,
        }
    }
}

/// Editable content of the category dialog (for undo).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CategorySnap {
    name: TextInput,
    description: BodyEditor,
    on_description: bool,
}

/// The category dialog: a name and a note about what it is for, both
/// plain text.
pub struct CategoryForm {
    pub name: TextInput,
    pub description: BodyEditor,
    pub on_description: bool,
    pub error: Option<String>,
    /// The category being edited; `None` when creating one.
    pub editing: Option<String>,
    pub name_area: Rect,
    pub description_area: Rect,
    history: History<CategorySnap>,
}

impl CategoryForm {
    pub fn new() -> Self {
        Self {
            name: TextInput::new("", crate::model::MAX_CATEGORY_NAME_LEN),
            description: BodyEditor::plain(""),
            on_description: false,
            error: None,
            editing: None,
            name_area: Rect::ZERO,
            description_area: Rect::ZERO,
            history: History::new(),
        }
    }

    pub fn edit(category: &crate::model::Category) -> Self {
        Self {
            name: TextInput::new(&category.name, crate::model::MAX_CATEGORY_NAME_LEN),
            description: BodyEditor::plain(&category.description),
            editing: Some(category.id.clone()),
            ..Self::new()
        }
    }

    pub fn title_text(&self) -> &'static str {
        if self.editing.is_some() {
            "Edit category"
        } else {
            "New category"
        }
    }

    pub fn toggle_field(&mut self) {
        self.history.break_coalesce();
        self.on_description = !self.on_description;
    }

    fn snap(&self) -> CategorySnap {
        CategorySnap {
            name: self.name.clone(),
            description: self.description.clone(),
            on_description: self.on_description,
        }
    }

    fn restore(&mut self, s: CategorySnap) {
        self.name = s.name;
        self.description = s.description;
        self.on_description = s.on_description;
        self.error = None;
    }

    /// Snapshot before a content edit.
    pub fn before_edit(&mut self, kind: EditKind) {
        if self.history.will_coalesce(kind) {
            self.history.touch_coalesce();
            return;
        }
        let snap = self.snap();
        self.history.push(snap, kind);
    }

    pub fn break_coalesce(&mut self) {
        self.history.break_coalesce();
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.history.undo(self.snap()) else {
            return false;
        };
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.history.redo(self.snap()) else {
            return false;
        };
        self.restore(next);
        true
    }

    /// `(name, description)` once there is a name to save.
    pub fn submit(&mut self) -> Option<(String, String)> {
        let name = self.name.value().trim().to_string();
        if name.is_empty() {
            self.error = Some("A name is required".to_string());
            self.on_description = false;
            return None;
        }
        self.error = None;
        Some((name, self.description.plain_value()))
    }
}

impl Default for CategoryForm {
    fn default() -> Self {
        Self::new()
    }
}

/// Where each field's box landed in the last frame, so a click can find
/// the field under the pointer.
#[derive(Debug, Default, Clone, Copy)]
pub struct FieldAreas {
    pub title: Rect,
    pub due: Rect,
    pub importance: Rect,
    pub body: Rect,
}

impl FieldAreas {
    pub fn field_at(&self, x: u16, y: u16) -> Option<Field> {
        let pos = ratatui::layout::Position { x, y };
        if self.title.contains(pos) {
            Some(Field::Title)
        } else if self.due.contains(pos) {
            Some(Field::Due)
        } else if self.importance.contains(pos) {
            Some(Field::Importance)
        } else if self.body.contains(pos) {
            Some(Field::Body)
        } else {
            None
        }
    }

    pub fn rect(&self, field: Field) -> Rect {
        match field {
            Field::Title => self.title,
            Field::Due => self.due,
            Field::Importance => self.importance,
            Field::Body => self.body,
        }
    }
}

/// The values a submitted form hands back to the app.
#[derive(Debug, Clone, Default)]
pub struct TaskDraft {
    pub title: String,
    pub due: String,
    pub importance: u8,
    pub body: Vec<Block>,
}

impl TaskDraft {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            ..Self::default()
        }
    }
}

/// Editable content of the task dialog (for undo). UI chrome is excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskSnap {
    title: TextInput,
    due: TextInput,
    importance: u8,
    body: BodyEditor,
    field: Field,
}

pub struct TaskForm {
    pub title: TextInput,
    pub due: TextInput,
    pub importance: u8,
    pub body: BodyEditor,
    pub field: Field,
    pub error: Option<String>,
    /// The task being edited; `None` when creating a new one.
    pub editing: Option<String>,
    /// Filled in while drawing; used to hit-test clicks.
    pub areas: FieldAreas,
    /// Whether the description's image is shown full size.
    pub preview: bool,
    /// Decoded GIF for preview, keyed by path (kept after close for fast reopen).
    pub gif: Option<(std::path::PathBuf, GifPlayback)>,
    /// The calendar, while a due date is being picked.
    pub picker: Option<DuePicker>,
    /// Last body click (line index), for double-click to open a picture.
    pub last_body_click: Option<(Instant, usize)>,
    /// Whether the `/` menu was open last frame — used to re-emit images
    /// after the menu is dismissed (see `ui::draw_body`).
    pub menu_was_open: bool,
    /// Body scroll after last paint — scroll changes need the same protocol
    /// drop as menu close so pictures that shrink/move do not ghost.
    pub body_scroll: u16,
    /// Screen rect of the open body `/` dropdown (for mouse hit-testing).
    pub body_menu_area: Option<Rect>,
    /// Where each body picture was drawn last frame: `(line index, screen rect)`.
    /// Clicks only select a picture when they land inside this rect — not the
    /// full-width letterbox gutter.
    pub image_hits: Vec<(usize, Rect)>,
    history: History<TaskSnap>,
}

impl TaskForm {
    pub fn new() -> Self {
        Self {
            title: TextInput::new("", MAX_TITLE_LEN),
            due: TextInput::new("", 32),
            importance: 0,
            body: BodyEditor::new(&[]),
            field: Field::Title,
            error: None,
            editing: None,
            areas: FieldAreas::default(),
            preview: false,
            gif: None,
            picker: None,
            last_body_click: None,
            menu_was_open: false,
            body_scroll: 0,
            body_menu_area: None,
            image_hits: Vec::new(),
            history: History::new(),
        }
    }

    pub fn edit(task: &Task) -> Self {
        Self {
            title: TextInput::new(&task.title, MAX_TITLE_LEN),
            due: TextInput::new(&task.due, 32),
            importance: task.importance,
            body: BodyEditor::new(&task.body),
            editing: Some(task.id.clone()),
            ..Self::new()
        }
    }

    /// Whether `(x, y)` sits on the drawn picture for body line `line`.
    pub fn image_hit_at(&self, line: usize, x: u16, y: u16) -> bool {
        use ratatui::layout::Position;
        self.image_hits
            .iter()
            .any(|(i, r)| *i == line && r.contains(Position { x, y }))
    }

    /// Open the full-size image viewer (loads GIF frames when needed).
    /// Returns an error message if a GIF was expected but could not load.
    pub fn open_image_preview(&mut self) -> Option<String> {
        // Only the image under the cursor — never fall back to "first in body".
        let Some(path) = self.body.selected_image() else {
            self.preview = false;
            return Some("No image to preview".into());
        };
        self.preview = true;
        if crate::image::is_gif(&path) {
            // Keep a decoded GIF across close/reopen while this form is open.
            if matches!(&self.gif, Some((p, _)) if p == &path) {
                return None;
            }
            match GifPlayback::load(&path) {
                Ok(gif) => {
                    self.gif = Some((path, gif));
                    None
                }
                Err(err) => {
                    self.gif = None;
                    // Still show static preview via ImageStore, but report why
                    // animation could not start.
                    Some(err)
                }
            }
        } else {
            // Different still — drop any previous GIF cache.
            if !matches!(&self.gif, Some((p, _)) if p == &path) {
                self.gif = None;
            }
            None
        }
    }

    pub fn close_image_preview(&mut self) {
        self.preview = false;
        // Keep `gif` so reopening the same animation is instant.
    }

    pub fn gif_playing(&self) -> bool {
        self.preview
            && self
                .gif
                .as_ref()
                .is_some_and(|(_, g)| g.is_animated() && !g.is_paused())
    }

    /// Advance GIF animation; returns true when the frame changed.
    pub fn tick_gif(&mut self) -> bool {
        if let Some((_, gif)) = &mut self.gif {
            return gif.tick();
        }
        false
    }

    /// Click while preview is open: pause/resume an animated GIF.
    /// Static images ignore the click (Esc still closes).
    pub fn preview_click(&mut self) {
        if let Some((_, gif)) = &mut self.gif {
            gif.toggle_pause();
        }
    }

    pub fn is_edit(&self) -> bool {
        self.editing.is_some()
    }

    pub fn title_text(&self) -> &'static str {
        if self.is_edit() {
            "Edit task"
        } else {
            "New task"
        }
    }

    fn snap(&self) -> TaskSnap {
        TaskSnap {
            title: self.title.clone(),
            due: self.due.clone(),
            importance: self.importance,
            body: self.body.clone(),
            field: self.field,
        }
    }

    fn restore(&mut self, s: TaskSnap) {
        self.title = s.title;
        self.due = s.due;
        self.importance = s.importance;
        self.body = s.body;
        self.field = s.field;
        self.error = None;
        // Overlays are not part of content history.
        self.picker = None;
        self.preview = false;
        self.body.close_menu();
    }

    /// Snapshot before a content edit (call before mutating).
    pub fn before_edit(&mut self, kind: EditKind) {
        if self.history.will_coalesce(kind) {
            self.history.touch_coalesce();
            return;
        }
        let snap = self.snap();
        self.history.push(snap, kind);
    }

    pub fn break_coalesce(&mut self) {
        self.history.break_coalesce();
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.history.undo(self.snap()) else {
            return false;
        };
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.history.redo(self.snap()) else {
            return false;
        };
        self.restore(next);
        true
    }

    /// Opens the calendar on whatever the field already reads.
    pub fn open_due_picker(&mut self) {
        self.history.break_coalesce();
        self.field = Field::Due;
        self.picker = Some(DuePicker::new(self.due.value().trim()));
    }

    /// Writes the picked day back into the field.
    pub fn take_due_picker(&mut self) {
        if self.picker.is_some() {
            self.before_edit(EditKind::Atomic);
            if let Some(picker) = self.picker.take() {
                self.due = TextInput::new(&picker.value(), 32);
            }
        }
    }

    /// Steps importance up, wrapping back to none after three.
    pub fn cycle_importance(&mut self) {
        self.before_edit(EditKind::Atomic);
        self.importance = (self.importance + 1) % (crate::model::MAX_IMPORTANCE + 1);
    }

    pub fn set_importance(&mut self, importance: u8) {
        let next = importance.min(crate::model::MAX_IMPORTANCE);
        if next == self.importance {
            return;
        }
        self.before_edit(EditKind::Atomic);
        self.importance = next;
    }

    pub fn clear_due(&mut self) {
        if self.due.is_empty() && self.picker.is_none() {
            return;
        }
        self.before_edit(EditKind::Atomic);
        self.picker = None;
        self.due = TextInput::new("", 32);
    }

    pub fn focus_next(&mut self) {
        self.history.break_coalesce();
        self.picker = None;
        self.body.close_menu();
        self.field = self.field.next();
    }

    pub fn focus_prev(&mut self) {
        self.history.break_coalesce();
        self.picker = None;
        self.body.close_menu();
        self.field = self.field.prev();
    }

    /// Move focus; dismiss the calendar and slash menu when leaving
    /// the fields that own them.
    pub fn set_field(&mut self, field: Field) {
        self.history.break_coalesce();
        if field != Field::Due {
            self.picker = None;
        }
        if field != Field::Body {
            self.body.close_menu();
        }
        self.field = field;
    }

    /// Validates the form. On success returns the draft; on failure sets
    /// `error` and focuses the offending field.
    pub fn submit(&mut self) -> Option<TaskDraft> {
        self.error = None;

        let title = self.title.value().trim().to_string();
        if title.is_empty() {
            self.error = Some("A title is required".to_string());
            self.field = Field::Title;
            return None;
        }

        let due_text = self.due.value().trim().to_string();
        if !due::is_valid(&due_text) {
            self.error = Some(format!("'{due_text}' is not a date mach understands"));
            self.field = Field::Due;
            return None;
        }

        // A `[date]` typed into the title still works, and fills the due
        // field when it was left empty. It gets the same check the Due
        // field does — otherwise it is a way around it.
        let (inline_due, title) = due::parse(&title);
        if title.is_empty() {
            self.error = Some("A title is required".to_string());
            self.field = Field::Title;
            return None;
        }
        if !due::is_valid(&inline_due) {
            self.error = Some(format!("'{inline_due}' is not a date mach understands"));
            self.field = Field::Title;
            return None;
        }

        Some(TaskDraft {
            title,
            due: if due_text.is_empty() {
                inline_due
            } else {
                due_text
            },
            importance: self.importance,
            body: self.body.value(),
        })
    }
}

impl Default for TaskForm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::EditKind;

    #[test]
    fn requires_a_title() {
        let mut form = TaskForm::new();
        assert!(form.submit().is_none());
        assert_eq!(form.field, Field::Title);
        assert!(form.error.is_some());
    }

    #[test]
    fn rejects_an_unparsable_date() {
        let mut form = TaskForm::new();
        form.title = TextInput::new("something", MAX_TITLE_LEN);
        form.due = TextInput::new("next tuesday", 32);
        assert!(form.submit().is_none());
        assert_eq!(form.field, Field::Due);
    }

    #[test]
    fn takes_a_date_typed_into_the_title() {
        let mut form = TaskForm::new();
        form.title = TextInput::new("pay rent [2030-01-02]", MAX_TITLE_LEN);
        let draft = form.submit().expect("valid");
        assert_eq!(draft.title, "pay rent");
        assert_eq!(draft.due, "2030-01-02");
    }

    #[test]
    fn an_explicit_date_wins_over_the_inline_one() {
        let mut form = TaskForm::new();
        form.title = TextInput::new("pay rent [2030-01-02]", MAX_TITLE_LEN);
        form.due = TextInput::new("09:00", 32);
        let draft = form.submit().expect("valid");
        assert_eq!(draft.due, "09:00");
    }

    #[test]
    fn undo_restores_title_and_body() {
        let mut form = TaskForm::new();
        form.before_edit(EditKind::Typing);
        form.title = TextInput::new("hello", MAX_TITLE_LEN);
        form.before_edit(EditKind::Atomic);
        form.body.insert_str("note");
        assert!(form.undo());
        assert!(form.body.is_empty() || form.body.plain_value().is_empty());
        assert_eq!(form.title.value(), "hello");
        assert!(form.undo());
        assert_eq!(form.title.value(), "");
        assert!(form.redo());
        assert_eq!(form.title.value(), "hello");
    }

    #[test]
    fn undo_restores_importance() {
        let mut form = TaskForm::new();
        form.set_importance(2);
        assert_eq!(form.importance, 2);
        assert!(form.undo());
        assert_eq!(form.importance, 0);
        assert!(form.redo());
        assert_eq!(form.importance, 2);
    }
}
