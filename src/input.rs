//! Keyboard and mouse handling.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use std::time::{Duration, Instant};

use crate::app::{App, ClickTarget, Confirm, Focus, Mode};
use crate::form::Field;
use crate::text_input::TextInput;
use crate::undo::EditKind;

/// Two clicks on the same task within this long open it.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Handle one terminal event and report whether the screen may have changed.
pub fn handle_event(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            handle_key(app, key);
            true
        }
        Event::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
            ) =>
        {
            handle_mouse(app, m);
            true
        }
        // The terminal's own paste (Cmd+V / middle click), delivered in
        // one piece because bracketed paste is on.
        Event::Paste(text) if !text.is_empty() => {
            paste_text(app, &text);
            true
        }
        // Crossterm has already resized the terminal; the next draw picks up
        // the new dimensions without any App mutation here.
        Event::Resize(_, _) => true,
        _ => false,
    }
}

/// Puts pasted text into whatever is being typed into. Bracketed paste
/// (the terminal's own Cmd/Ctrl+V) is enough — no separate key binding.
fn paste_text(app: &mut App, text: &str) {
    if text.is_empty() {
        return;
    }
    app.cancel_pending();
    match app.mode {
        Mode::TaskForm => {
            let Some(form) = &mut app.form else { return };
            match form.field {
                Field::Title => {
                    form.before_edit(EditKind::Atomic);
                    form.title.insert_str(text);
                }
                // Selectors are changed with arrows/clicks, not pasted text.
                Field::Category | Field::Labels | Field::Due | Field::Importance => {}
                Field::Description => {
                    form.before_edit(EditKind::Atomic);
                    form.description.insert_str(text);
                }
            }
        }
        Mode::CategoryForm => {
            let Some(form) = &mut app.category_form else {
                return;
            };
            form.before_edit(EditKind::Atomic);
            if form.on_description {
                form.description.insert_str(text);
            } else {
                form.name.insert_str(text);
            }
        }
        Mode::Slash => {
            app.input.insert_str(text);
            app.slash_index = 0;
            app.clamp_slash_index();
        }
        Mode::Search => {
            app.input.insert_str(text);
            app.update_search();
        }
        Mode::Labels => {
            if let Some(editor) = &mut app.label_editor
                && !editor.color_focused
            {
                editor.name.insert_str(text);
                app.label_error = None;
                app.dirty = true;
            }
        }
        _ => {}
    }
}

/// Ctrl+Z — undo (not Ctrl+Shift+Z).
fn is_undo_chord(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Ctrl+Shift+Z or Ctrl+Y — redo.
fn is_redo_chord(key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if !ctrl || alt {
        return false;
    }
    match key.code {
        KeyCode::Char('z') | KeyCode::Char('Z') if shift => true,
        KeyCode::Char('y') | KeyCode::Char('Y') if !shift => true,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEditAction {
    SelectWord,
    SelectWordLeft,
    SelectWordRight,
    WordLeft,
    WordRight,
    SelectHome,
    SelectEnd,
    Home,
    End,
    DeleteToStart,
    DeleteToEnd,
    DeleteWordLeft,
    Insert(char),
    Backspace,
    Delete,
    SelectLeft,
    SelectRight,
    Left,
    Right,
}

impl TextEditAction {
    const fn edit_kind(self) -> Option<EditKind> {
        match self {
            Self::Insert(_) | Self::Backspace | Self::Delete => Some(EditKind::Typing),
            Self::DeleteToStart | Self::DeleteToEnd | Self::DeleteWordLeft => {
                Some(EditKind::Atomic)
            }
            _ => None,
        }
    }

    fn apply_line(self, input: &mut TextInput) {
        match self {
            Self::SelectWord => input.select_word(),
            Self::SelectWordLeft => input.select_word_left(),
            Self::SelectWordRight => input.select_word_right(),
            Self::WordLeft => input.word_left(),
            Self::WordRight => input.word_right(),
            Self::SelectHome => input.select_home(),
            Self::SelectEnd => input.select_end(),
            Self::Home => input.home(),
            Self::End => input.end(),
            Self::DeleteToStart => input.delete_to_start(),
            Self::DeleteToEnd => input.delete_to_end(),
            Self::DeleteWordLeft => input.delete_word_left(),
            Self::Insert(character) => input.insert(character),
            Self::Backspace => input.backspace(),
            Self::Delete => input.delete(),
            Self::SelectLeft => input.select_left(),
            Self::SelectRight => input.select_right(),
            Self::Left => input.left(),
            Self::Right => input.right(),
        }
    }

    fn apply_description(self, description: &mut crate::description::DescriptionEditor) {
        match self {
            Self::SelectWord => description.select_word(),
            Self::SelectWordLeft => description.select_word_left(),
            Self::SelectWordRight => description.select_word_right(),
            Self::WordLeft => description.word_left(),
            Self::WordRight => description.word_right(),
            Self::SelectHome => description.select_home(),
            Self::SelectEnd => description.select_end(),
            Self::Home => description.home(),
            Self::End => description.end(),
            Self::DeleteToStart => description.delete_to_start(),
            Self::DeleteToEnd => description.delete_to_end(),
            Self::DeleteWordLeft => description.delete_word_left(),
            Self::Insert(character) => description.insert(character),
            Self::Backspace => description.backspace(),
            Self::Delete => description.delete(),
            Self::SelectLeft => description.select_left(),
            Self::SelectRight => description.select_right(),
            Self::Left => description.left(),
            Self::Right => description.right(),
        }
    }
}

fn text_edit_action(key: KeyEvent) -> Option<TextEditAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let word = word_mod(key);
    match key.code {
        KeyCode::Char('w') | KeyCode::Char('W') if alt && shift => Some(TextEditAction::SelectWord),
        KeyCode::Char('b') | KeyCode::Char('B') if alt && shift => {
            Some(TextEditAction::SelectWordLeft)
        }
        KeyCode::Char('f') | KeyCode::Char('F') if alt && shift => {
            Some(TextEditAction::SelectWordRight)
        }
        KeyCode::Char('b') | KeyCode::Char('B') if alt => Some(TextEditAction::WordLeft),
        KeyCode::Char('f') | KeyCode::Char('F') if alt => Some(TextEditAction::WordRight),
        KeyCode::Char(c) if ctrl || alt => match c {
            'a' if shift => Some(TextEditAction::SelectHome),
            'e' if shift => Some(TextEditAction::SelectEnd),
            'a' => Some(TextEditAction::Home),
            'e' => Some(TextEditAction::End),
            'u' => Some(TextEditAction::DeleteToStart),
            'k' => Some(TextEditAction::DeleteToEnd),
            'w' | 'W' => Some(TextEditAction::DeleteWordLeft),
            _ => None,
        },
        KeyCode::Char(character) if !ctrl && !alt => Some(TextEditAction::Insert(character)),
        KeyCode::Backspace if word => Some(TextEditAction::DeleteWordLeft),
        KeyCode::Backspace => Some(TextEditAction::Backspace),
        KeyCode::Delete => Some(TextEditAction::Delete),
        KeyCode::Left if word && shift => Some(TextEditAction::SelectWordLeft),
        KeyCode::Right if word && shift => Some(TextEditAction::SelectWordRight),
        KeyCode::Left if shift => Some(TextEditAction::SelectLeft),
        KeyCode::Right if shift => Some(TextEditAction::SelectRight),
        KeyCode::Left if word => Some(TextEditAction::WordLeft),
        KeyCode::Right if word => Some(TextEditAction::WordRight),
        KeyCode::Left => Some(TextEditAction::Left),
        KeyCode::Right => Some(TextEditAction::Right),
        KeyCode::Home if shift => Some(TextEditAction::SelectHome),
        KeyCode::End if shift => Some(TextEditAction::SelectEnd),
        KeyCode::Home => Some(TextEditAction::Home),
        KeyCode::End => Some(TextEditAction::End),
        _ => None,
    }
}

/// Whether this key mutates editor content, and how to group it for undo.
fn content_edit_kind(key: KeyEvent) -> Option<EditKind> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) && ctrl && !alt && !shift {
        return Some(EditKind::Atomic);
    }
    text_edit_action(key).and_then(TextEditAction::edit_kind)
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Cmd/Ctrl+C on a selection copies it. Copying always wins over
    // quitting, so the two can share the chord.
    if is_copy_chord(key) {
        if copy_selected_description_image(app) {
            return;
        }
        // Command is Copy on macOS. When there is no selection it remains a
        // no-op instead of falling through to the editor as a literal `c`.
        if key.modifiers.contains(KeyModifiers::SUPER) {
            return;
        }
    }
    // Auto-repeat comes from one physical hold, not a second affirmative
    // action. Keep navigation/edit repeats responsive, but never let one
    // complete an armed delete, purge, discard, or quit confirmation.
    if key.kind == KeyEventKind::Repeat
        && app
            .pending_confirmation()
            .is_some_and(|confirm| confirmation_key_matches(confirm, key, app.mode))
    {
        return;
    }
    // With nothing to copy, Ctrl+C twice leaves mach — but only from
    // the two panels. Inside a dialog or the `/` line it would be far too
    // easy to throw away what was typed, and Esc already backs out there.
    if is_ctrl_c(key) && app.mode == Mode::Normal {
        if app.awaiting(Confirm::Quit) {
            app.request_quit();
        } else {
            app.ask_confirm(Confirm::Quit, "Press Ctrl+C again to quit");
        }
        return;
    }

    // Confirmations are action-specific. Any key other than that action's
    // explicit second step cancels it before normal routing continues.
    let keeps_confirmation = app
        .pending_confirmation()
        .is_none_or(|confirm| confirmation_key_matches(confirm, key, app.mode));
    if !keeps_confirmation {
        app.cancel_pending();
    }

    if key.code == KeyCode::Enter
        && app.mode == Mode::Normal
        && let Some(Confirm::Purge(ids)) = app.pending_confirmation().cloned()
    {
        let count = app.purge_ids(&ids);
        if count > 0 {
            app.info(format!("Purged {count} done task(s)"));
        }
        return;
    }

    match app.mode {
        Mode::Welcome | Mode::WhatsNew => {
            app.mode = Mode::Normal;
            if !matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                handle_key(app, key);
            }
        }
        Mode::Help => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => app.mode = Mode::Normal,
            KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
            KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
            KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
            KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
            KeyCode::Home => app.help_scroll = 0,
            KeyCode::End => app.help_scroll = usize::MAX,
            _ => {}
        },
        Mode::Settings => handle_settings_key(app, key),
        Mode::Labels => handle_labels_key(app, key),
        Mode::TaskForm => handle_form_key(app, key),
        Mode::CategoryForm => handle_category_key(app, key),
        Mode::Slash => handle_slash_key(app, key),
        Mode::Search => handle_search_key(app, key),
        _ => handle_normal_key(app, key),
    }
}

fn confirmation_key_matches(confirm: &Confirm, key: KeyEvent, mode: Mode) -> bool {
    match confirm {
        Confirm::DeleteTask(_) | Confirm::DeleteCategory(_) | Confirm::DeleteLabel(_) => {
            key.code == KeyCode::Backspace
        }
        Confirm::Purge(_) => key.code == KeyCode::Enter && mode == Mode::Normal,
        Confirm::DiscardTask(_) | Confirm::DiscardCategory(_) => key.code == KeyCode::Esc,
        Confirm::Quit => is_ctrl_c(key),
    }
}

/// ⌘C / Ctrl+C — macOS terminals often send SUPER for Command.
fn is_copy_chord(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && (key.modifiers.contains(KeyModifiers::SUPER)
            || key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Ctrl+C alone. ⌘C is Copy on macOS and must never quit, so the quit
/// chord is narrower than [`is_copy_chord`].
fn is_ctrl_c(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers == KeyModifiers::CONTROL
}

/// Copy the current selection (text, picture, or both) from a form field.
/// Returns true when the key was handled.
fn copy_selected_description_image(app: &mut App) -> bool {
    if app.mode == Mode::TaskForm
        && let Some(form) = &app.form
        && form.field == Field::Description
        && let Some(payload) = form.description.selected_payload()
    {
        finish_copy(app, payload);
        return true;
    }
    // Other fields: plain text selection only.
    if let Some(text) = selected_text_in_app(app) {
        finish_copy(app, crate::description::CopyPayload::Text(text));
        return true;
    }
    if app.mode != Mode::TaskForm {
        return false;
    }
    let Some(form) = &app.form else {
        return false;
    };
    // Full-size preview: copy that picture even with no description selection.
    if form.preview {
        let path = form
            .description
            .selected_image()
            .or_else(|| form.description.images().into_iter().next());
        if let Some(path) = path {
            finish_copy(app, crate::description::CopyPayload::Image(path));
            return true;
        }
    }
    false
}

fn selected_text_in_app(app: &App) -> Option<String> {
    match app.mode {
        Mode::TaskForm => {
            let form = app.form.as_ref()?;
            match form.field {
                Field::Title => form.title.selected_text(),
                Field::Description => form.description.selected_text(),
                Field::Category | Field::Labels | Field::Due | Field::Importance => None,
            }
        }
        Mode::CategoryForm => {
            let form = app.category_form.as_ref()?;
            if form.on_description {
                form.description.selected_text()
            } else {
                form.name.selected_text()
            }
        }
        Mode::Slash | Mode::Search => app.input.selected_text(),
        _ => None,
    }
}

// ---------------------------------------------------------------- normal

fn handle_normal_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            if !app.searching {
                app.toggle_focus();
            }
        }
        // Esc backs out one step and never quits; use `/quit`.
        KeyCode::Esc => {
            if app.cancel_archive() {
                return;
            }
            if app.searching {
                app.end_search();
            }
        }
        // `/` opens the command palette (search, settings, …).
        KeyCode::Char('/') => app.open_slash(),
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.mode = Mode::Help;
        }
        _ => match app.focus {
            Focus::Tasks => task_key(app, key),
            Focus::Sidebar => sidebar_key(app, key),
        },
    }
}

fn task_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let meta = key.modifiers.contains(KeyModifiers::SUPER);

    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl && !alt => {
            if app.searching {
                app.info("Leave search (Esc) before adding a task");
                return;
            }
            app.open_new_task();
        }
        KeyCode::Char('f') | KeyCode::Char('F') if ctrl && !alt => {
            app.cycle_importance(app.task_index);
        }
        KeyCode::Enter => app.open_edit_task(),
        KeyCode::Char(' ') => app.toggle_done(app.task_index),
        KeyCode::Up if alt && !ctrl && !meta => {
            app.move_task_order(-1);
        }
        KeyCode::Down if alt && !ctrl && !meta => {
            app.move_task_order(1);
        }
        KeyCode::Up => app.navigate_vertical(-1),
        KeyCode::Down => app.navigate_vertical(1),
        KeyCode::PageUp => app.select_first_task(),
        KeyCode::PageDown => app.select_last_task(),
        // The panels sit side by side, so the arrows that point at them
        // are what moves between them.
        KeyCode::Left => {
            let _ = app.set_focus(Focus::Sidebar);
        }
        KeyCode::Backspace => {
            if let Some(id) = app.selected_task().map(|task| task.id.clone()) {
                let confirm = Confirm::DeleteTask(id.clone());
                if app.awaiting(confirm.clone()) {
                    if app.delete_task_by_id(&id) {
                        app.info("Task deleted");
                    }
                } else {
                    app.ask_confirm(confirm, "Press Backspace again to delete this task");
                }
            }
        }
        // Type-to-jump: plain characters fuzzy-select a row (no mode).
        KeyCode::Char(c) if !ctrl && !alt && !meta && !c.is_control() => {
            app.typeahead_jump(c);
        }
        _ => {}
    }
}

fn sidebar_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let meta = key.modifiers.contains(KeyModifiers::SUPER);

    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl && !alt => app.open_new_category(),
        // Enter opens whatever is selected, and a category opens into
        // the same kind of dialog a task does.
        KeyCode::Enter => app.open_edit_category(),
        KeyCode::Right => {
            let _ = app.set_focus(Focus::Tasks);
        }
        KeyCode::Up if alt && !ctrl && !meta => {
            app.move_category_order(-1);
        }
        KeyCode::Down if alt && !ctrl && !meta => {
            app.move_category_order(1);
        }
        KeyCode::Up => app.navigate_vertical(-1),
        KeyCode::Down => app.navigate_vertical(1),
        KeyCode::PageUp => app.select_category(0),
        KeyCode::PageDown => app.select_last_category(),
        KeyCode::Backspace => {
            if app.is_all_view() {
                return;
            }
            let id = app.current_category_id().to_string();
            let confirm = Confirm::DeleteCategory(id.clone());
            if app.awaiting(confirm.clone()) {
                let count = app.category_progress(&id).1;
                if app.delete_category_by_id(&id) {
                    app.info(format!(
                        "Category deleted; {count} task(s) kept as Uncategorized"
                    ));
                }
            } else {
                let count = app.category_progress(app.current_category_id()).1;
                app.ask_confirm(
                    confirm,
                    format!(
                        "Press Backspace again to delete this category; {count} task(s) will be kept as Uncategorized"
                    ),
                );
            }
        }
        // Type-to-jump: plain characters fuzzy-select a category (no mode).
        KeyCode::Char(c) if !ctrl && !alt && !meta && !c.is_control() => {
            app.typeahead_jump(c);
        }
        _ => {}
    }
}

// ----------------------------------------------------------- text editing

/// macOS Option and Linux Alt both show up as [`KeyModifiers::ALT`].
/// Ctrl is the common non-Mac habit for the same motions.
fn word_mod(key: KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
}

/// Shared bindings for every one-line editor.
fn edit_line(input: &mut TextInput, key: KeyEvent) -> bool {
    let Some(action) = text_edit_action(key) else {
        return false;
    };
    action.apply_line(input);
    true
}

/// The same bindings as [`edit_line`], for the multi-line block editors:
/// a task's description and a category's description. Adds ↑/↓ across blocks.
/// The `/` menu is handled by the caller before this runs.
fn edit_description(description: &mut crate::description::DescriptionEditor, key: KeyEvent) {
    match key.code {
        KeyCode::Up => description.up(),
        KeyCode::Down => description.down(),
        _ => {
            if let Some(action) = text_edit_action(key) {
                action.apply_description(description);
            }
        }
    }
}

/// The category dialog: a name and a structured, text-only description.
fn handle_category_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('s')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app
            .category_form
            .as_ref()
            .is_some_and(|form| form.description.menu.is_some())
        {
            app.error("Choose or dismiss the description command before saving");
            return;
        }
        app.submit_category_form();
        return;
    }

    if is_undo_chord(key) {
        if let Some(form) = &mut app.category_form
            && form.undo()
        {
            app.info("Undo");
        }
        return;
    }
    if is_redo_chord(key) {
        if let Some(form) = &mut app.category_form
            && form.redo()
        {
            app.info("Redo");
        }
        return;
    }

    // Slash menu owns arrows / Enter while open on the description.
    if let Some(form) = app
        .category_form
        .as_mut()
        .filter(|form| form.on_description && form.description.menu.is_some())
    {
        let outcome = {
            // Structural apply (bullet etc.) needs a checkpoint first.
            if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
                form.before_edit(EditKind::Atomic);
            }
            description_menu_key(&mut form.description, key)
        };
        match outcome {
            MenuKey::Ignored => {}
            MenuKey::Handled => return,
            MenuKey::Request(request) => {
                finish_description_command(app, request);
                return;
            }
        }
    }

    match key.code {
        KeyCode::Esc => {
            let _ = request_close_form(app, OpenForm::Category, FormCloseSource::Escape);
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if let Some(form) = &mut app.category_form {
                form.description.close_menu();
                form.toggle_field();
            }
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL) =>
        {
            let url = app
                .category_form
                .as_ref()
                .filter(|form| form.on_description)
                .and_then(|form| form.description.link_url_at_cursor());
            if let Some(url) = url {
                open_link(app, &url);
            }
        }
        KeyCode::Enter => {
            let Some(form) = &mut app.category_form else {
                return;
            };
            if form.on_description {
                form.before_edit(EditKind::Atomic);
                let _ = form.description.newline();
            } else {
                form.toggle_field();
            }
        }
        _ => {
            let Some(form) = &mut app.category_form else {
                return;
            };
            if form.on_description {
                if let Some(mut kind) = content_edit_kind(key) {
                    if form.description.has_selection() {
                        kind = EditKind::Atomic;
                    }
                    form.before_edit(kind);
                } else {
                    form.break_coalesce();
                }
                edit_description(&mut form.description, key);
            } else if let Some(mut kind) = content_edit_kind(key) {
                if form.name.has_selection() {
                    kind = EditKind::Atomic;
                }
                form.before_edit(kind);
                edit_line(&mut form.name, key);
            } else {
                form.break_coalesce();
                edit_line(&mut form.name, key);
            }
        }
    }
}

/// The `/` command palette above the status bar.
fn cycle_index(index: &mut usize, count: usize, delta: isize) {
    if count > 0 {
        *index = ((*index as isize + delta).rem_euclid(count as isize)) as usize;
    }
}

fn handle_slash_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => close_slash(app),
        // Backspace on empty (or past the last char) drops the leading `/`.
        KeyCode::Backspace if app.input.is_empty() => close_slash(app),
        KeyCode::Up => {
            let n = crate::slash::matching(&app.input.value()).len();
            cycle_index(&mut app.slash_index, n, -1);
        }
        KeyCode::Down | KeyCode::Tab => {
            let n = crate::slash::matching(&app.input.value()).len();
            cycle_index(&mut app.slash_index, n, 1);
        }
        KeyCode::Enter => {
            let query = app.input.value();
            let matches = crate::slash::matching(&query);
            let cmd = matches.get(app.slash_index).copied();
            close_slash(app);
            if let Some(cmd) = cmd {
                run_slash(app, cmd, &query);
            }
        }
        _ => {
            if edit_line(&mut app.input, key) {
                app.slash_index = 0;
                app.clamp_slash_index();
            }
        }
    }
}

fn close_slash(app: &mut App) {
    app.mode = Mode::Normal;
    app.input = TextInput::default();
    app.slash_index = 0;
}

/// Live search after choosing Search from the palette.
fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.input = TextInput::default();
            app.end_search();
        }
        KeyCode::Enter => {
            // Keep the current query; just leave the typing field.
            app.mode = Mode::Normal;
            app.input = TextInput::default();
            // Keep searching/search_query so the list stays narrowed until Esc.
            if app.search_query.is_empty() {
                app.end_search();
            }
        }
        _ => {
            if edit_line(&mut app.input, key) {
                app.update_search();
            }
        }
    }
}

fn run_slash(app: &mut App, cmd: crate::slash::SlashCommand, query: &str) {
    use crate::slash::{SlashCommand, args_for};
    match cmd {
        SlashCommand::Search => {
            let q = args_for(cmd, query);
            app.start_search(&q);
        }
        SlashCommand::Settings => {
            app.settings_index = 0;
            app.mode = Mode::Settings;
        }
        SlashCommand::Labels => app.open_labels(),
        SlashCommand::Help => {
            app.help_scroll = 0;
            app.mode = Mode::Help;
        }
        SlashCommand::WhatsNew => app.mode = Mode::WhatsNew,
        SlashCommand::CopyTitle => match app.selected_task() {
            Some(task) => {
                finish_copy(
                    app,
                    crate::description::CopyPayload::Text(task.title.clone()),
                );
            }
            None => app.info("No task selected"),
        },
        SlashCommand::CopyTask => match app.selected_task() {
            Some(task) => {
                let text = task_clipboard_text(task);
                finish_copy(app, crate::description::CopyPayload::Text(text));
            }
            None => app.info("No task selected"),
        },
        SlashCommand::Export => {
            let argument = args_for(cmd, query);
            if !argument.is_empty() {
                app.error("Usage: /export");
                return;
            }
            app.start_export_archive();
        }
        SlashCommand::Import => {
            let argument = args_for(cmd, query);
            if argument.is_empty() {
                app.error("Usage: /import <FILE>");
            } else {
                app.start_import_archive(std::path::PathBuf::from(argument));
            }
        }
        SlashCommand::Done => {
            let _ = app.toggle_hide_done();
        }
        SlashCommand::Purge => {
            let ids = app.purge_candidate_ids();
            if ids.is_empty() {
                app.info("No done tasks to purge");
            } else {
                let count = ids.len();
                app.ask_confirm(
                    Confirm::Purge(ids),
                    format!("Press Enter to purge {count} done task(s)"),
                );
            }
        }
        SlashCommand::Update => app.start_update_install(),
        SlashCommand::Quit => app.request_quit(),
    }
}

// ------------------------------------------------------------ task dialog

/// Tab and the mouse move between fields. Ctrl+S saves; Enter acts on the
/// focused field (and starts a new block in the description).
fn handle_form_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Saving resolves the picker, but never guesses what an open description command
    // or full-screen preview was meant to do.
    if matches!(key.code, KeyCode::Char('s')) && ctrl {
        if app.form.as_ref().is_some_and(|form| form.preview) {
            app.error("Close the image preview before saving");
            return;
        }
        if app
            .form
            .as_ref()
            .is_some_and(|form| form.description.menu.is_some())
        {
            app.error("Choose or dismiss the description command before saving");
            return;
        }
        if let Some(form) = &mut app.form
            && form.picker.is_some()
        {
            form.take_due_picker();
        }
        app.submit_form();
        return;
    }

    // Esc peels one layer: preview → picker → slash menu → leave the form.
    // (Handled below in that order; bare Esc closes the dialog only when
    // none of those overlays are open.)

    // The image preview: Esc closes; Space / Enter toggles GIF pause.
    if app.form.as_ref().is_some_and(|f| f.preview) {
        match key.code {
            KeyCode::Esc => {
                // Drop frames/protocol first so the next draw cannot spend
                // another encode tick on this preview.
                if let Some(form) = &mut app.form {
                    form.close_image_preview();
                }
                app.images.clear_preview();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(form) = &mut app.form {
                    form.preview_click();
                }
            }
            _ => {}
        }
        return;
    }

    // Ctrl+Z / Ctrl+Shift+Z (Ctrl+Y) — form-wide undo/redo. The image
    // preview above owns its keys until explicitly closed.
    if is_undo_chord(key) {
        if let Some(form) = &mut app.form
            && form.undo()
        {
            app.info("Undo");
        }
        return;
    }
    if is_redo_chord(key) {
        if let Some(form) = &mut app.form
            && form.redo()
        {
            app.info("Redo");
        }
        return;
    }

    // The date/time picker owns Tab and the arrows while it is open.
    if app.form.as_ref().is_some_and(|f| f.picker.is_some()) {
        handle_picker_key(app, key);
        return;
    }

    if app
        .form
        .as_ref()
        .is_some_and(|form| form.label_picker_open())
    {
        handle_label_picker_key(app, key);
        return;
    }

    // Slash menu: Esc closes the menu only (not the whole dialog).
    if app
        .form
        .as_ref()
        .is_some_and(|f| f.description.menu.is_some())
        && handle_menu_key(app, key)
    {
        return;
    }

    match key.code {
        KeyCode::Esc => {
            let _ = request_close_form(app, OpenForm::Task, FormCloseSource::Escape);
        }
        KeyCode::Tab => {
            if let Some(form) = &mut app.form {
                form.focus_next();
            }
        }
        KeyCode::BackTab => {
            if let Some(form) = &mut app.form {
                form.focus_prev();
            }
        }
        // Enter never closes the dialog — it opens or adds whatever the
        // focused field holds. Ctrl+S is what saves.
        // ⌘Enter / Ctrl+Enter on a link opens it in the browser.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL) =>
        {
            let url = app
                .form
                .as_ref()
                .filter(|f| f.field == Field::Description)
                .and_then(|f| f.description.link_url_at_cursor());
            if let Some(url) = url {
                open_link(app, &url);
            }
        }
        KeyCode::Enter => {
            let Some(form) = &mut app.form else { return };
            match form.field {
                Field::Title | Field::Category | Field::Importance => form.focus_next(),
                Field::Labels => form.open_label_picker(),
                Field::Due => form.open_due_picker(),
                // On a picture there is nothing to type, so Enter is
                // what opens it.
                Field::Description if form.description.selected_image().is_some() => {
                    if let Some(err) = form.open_image_preview() {
                        app.error(err);
                    }
                }
                Field::Description => {
                    form.before_edit(EditKind::Atomic);
                    let _ = form.description.newline();
                }
            }
        }
        _ => {
            let Some(form) = &mut app.form else { return };
            match form.field {
                // Ctrl+D ticks a to-do off; everything else is ordinary
                // block editing.
                Field::Description
                    if ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) =>
                {
                    form.before_edit(EditKind::Atomic);
                    form.description.toggle();
                }
                Field::Description => {
                    if let Some(mut kind) = content_edit_kind(key) {
                        if form.description.has_selection() {
                            kind = EditKind::Atomic;
                        }
                        form.before_edit(kind);
                    } else {
                        form.break_coalesce();
                    }
                    edit_description(&mut form.description, key);
                }
                // Category is a bounded selector. All tasks is deliberately
                // absent; Backspace/Delete returns the task to Uncategorized.
                Field::Category => match key.code {
                    KeyCode::Left | KeyCode::Up => form.cycle_category(-1),
                    KeyCode::Right | KeyCode::Down | KeyCode::Char(' ') => form.cycle_category(1),
                    KeyCode::Backspace | KeyCode::Delete => form.clear_category(),
                    _ => form.break_coalesce(),
                },
                Field::Labels => match key.code {
                    KeyCode::Char(' ') => form.open_label_picker(),
                    KeyCode::Backspace | KeyCode::Delete => form.clear_labels(),
                    _ => form.break_coalesce(),
                },
                // Nothing to type here: the arrows and the digits set
                // how many flags the task carries.
                Field::Importance => match key.code {
                    KeyCode::Left | KeyCode::Down => {
                        form.set_importance(form.importance.saturating_sub(1))
                    }
                    KeyCode::Right | KeyCode::Up | KeyCode::Char(' ') => form.cycle_importance(),
                    KeyCode::Backspace | KeyCode::Delete => form.set_importance(0),
                    KeyCode::Char(c) if c.is_ascii_digit() => form.set_importance(c as u8 - b'0'),
                    _ => form.break_coalesce(),
                },
                // Due is picker-only — no free typing, so any character
                // opens the calendar instead of landing in the field.
                Field::Due => match key.code {
                    KeyCode::Char(_) => form.open_due_picker(),
                    KeyCode::Backspace | KeyCode::Delete => form.clear_due(),
                    _ => form.break_coalesce(),
                },
                // Arrow keys only ever move the cursor; Tab, Shift+Tab
                // and the mouse are what change fields.
                Field::Title => {
                    if let Some(mut kind) = content_edit_kind(key) {
                        if form.title.has_selection() {
                            kind = EditKind::Atomic;
                        }
                        form.before_edit(kind);
                    } else {
                        form.break_coalesce();
                    }
                    edit_line(&mut form.title, key);
                }
            }
        }
    }
}

fn handle_label_picker_key(app: &mut App, key: KeyEvent) {
    if let KeyCode::Char(c) = key.code
        && c != ' '
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        && !c.is_control()
    {
        app.typeahead_jump(c);
        return;
    }
    app.clear_typeahead();

    let manage = app
        .form
        .as_ref()
        .is_some_and(|form| form.label_picker_manage_selected());
    if manage && matches!(key.code, KeyCode::Esc) {
        if let Some(form) = &mut app.form {
            form.close_label_picker();
        }
        return;
    }
    if manage && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
        app.open_labels_from_form();
        return;
    }
    let Some(form) = &mut app.form else { return };
    match key.code {
        KeyCode::Esc | KeyCode::Enter => form.close_label_picker(),
        KeyCode::Up => form.move_label_picker(-1),
        KeyCode::Down | KeyCode::Tab => form.move_label_picker(1),
        KeyCode::BackTab => form.move_label_picker(-1),
        KeyCode::PageUp => form.move_label_picker(-8),
        KeyCode::PageDown => form.move_label_picker(8),
        KeyCode::Home => form.select_first_label(),
        KeyCode::End => form.select_last_label(),
        KeyCode::Char(' ') => {
            if let Err(error) = form.toggle_current_label() {
                form.error = Some(error.to_string());
            } else {
                form.error = None;
            }
        }
        _ => {}
    }
}

/// Date + time picker: Tab moves Calendar → Hour → Minute; arrows adjust
/// the focused part; Enter writes the value back into Due.
fn handle_picker_key(app: &mut App, key: KeyEvent) {
    use crate::duepicker::PickerFocus;

    let Some(form) = &mut app.form else { return };
    match key.code {
        KeyCode::Esc => {
            form.picker = None;
            return;
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            form.clear_due();
            return;
        }
        KeyCode::Enter => {
            form.take_due_picker();
            return;
        }
        _ => {}
    }

    let Some(picker) = &mut form.picker else {
        return;
    };
    match key.code {
        KeyCode::Tab => picker.focus_next(),
        KeyCode::BackTab => picker.focus_prev(),
        KeyCode::Char('t') => {
            picker.today();
            picker.now_time();
        }
        KeyCode::Left => match picker.focus {
            PickerFocus::Calendar => picker.move_days(-1),
            PickerFocus::Hour => picker.bump_hour(-1),
            PickerFocus::Minute => picker.bump_minute(-5),
        },
        KeyCode::Right => match picker.focus {
            PickerFocus::Calendar => picker.move_days(1),
            PickerFocus::Hour => picker.bump_hour(1),
            PickerFocus::Minute => picker.bump_minute(5),
        },
        KeyCode::Up => match picker.focus {
            PickerFocus::Calendar => picker.move_days(-7),
            PickerFocus::Hour => picker.bump_hour(1),
            PickerFocus::Minute => picker.bump_minute(5),
        },
        KeyCode::Down => match picker.focus {
            PickerFocus::Calendar => picker.move_days(7),
            PickerFocus::Hour => picker.bump_hour(-1),
            PickerFocus::Minute => picker.bump_minute(-5),
        },
        KeyCode::PageUp => match picker.focus {
            PickerFocus::Calendar => picker.move_months(-1),
            PickerFocus::Hour => picker.bump_hour(1),
            PickerFocus::Minute => picker.bump_minute(15),
        },
        KeyCode::PageDown => match picker.focus {
            PickerFocus::Calendar => picker.move_months(1),
            PickerFocus::Hour => picker.bump_hour(-1),
            PickerFocus::Minute => picker.bump_minute(-15),
        },
        // Space on the clock → now; digits type hour/minute directly.
        KeyCode::Char(' ') if picker.focus != PickerFocus::Calendar => picker.now_time(),
        KeyCode::Char(c) if c.is_ascii_digit() => picker.type_digit(c as u8 - b'0'),
        _ => {}
    }
}

/// Returns true when the key belonged to the open slash menu.
fn handle_menu_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(form) = app
        .form
        .as_mut()
        .filter(|form| form.description.menu.is_some())
    else {
        return false;
    };
    // Split the borrow: menu keys only need the description; clipboard work needs App.
    let outcome = {
        // Applying a command (Enter/Tab) mutates structure — checkpoint first.
        if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
            form.before_edit(EditKind::Atomic);
        }
        description_menu_key(&mut form.description, key)
    };
    match outcome {
        MenuKey::Ignored => false,
        MenuKey::Handled => true,
        MenuKey::Request(request) => {
            finish_description_command(app, request);
            true
        }
    }
}

enum MenuKey {
    Ignored,
    Handled,
    Request(crate::description::CommandRequest),
}

fn description_menu_key(
    description: &mut crate::description::DescriptionEditor,
    key: KeyEvent,
) -> MenuKey {
    match key.code {
        KeyCode::Up => {
            description.menu_prev();
            MenuKey::Handled
        }
        KeyCode::Down => {
            description.menu_next();
            MenuKey::Handled
        }
        KeyCode::Esc => {
            description.close_menu();
            MenuKey::Handled
        }
        KeyCode::Tab | KeyCode::Enter => match description.menu_selected() {
            Some(command) => match description.apply(command) {
                Some(request) => MenuKey::Request(request),
                None => MenuKey::Handled,
            },
            None => {
                description.close_menu();
                MenuKey::Handled
            }
        },
        _ => MenuKey::Ignored,
    }
}

/// Title, then description as plain text (same export as description `/copy`).
fn task_clipboard_text(task: &crate::model::Task) -> String {
    use crate::model::Block;

    // Tasks are already persisted as typed blocks. Formatting them must not
    // send stored text back through the editor's path-adoption logic, where
    // filesystem contents could silently reinterpret it as a picture.
    let mut number = 0usize;
    let description = task
        .description
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => {
                number = 0;
                (!text.trim().is_empty()).then(|| text.clone())
            }
            Block::Todo { text, done } => {
                number = 0;
                let mark = if *done { "[✓]" } else { "[ ]" };
                Some(format!("{mark} {text}"))
            }
            Block::Bullet { text } => {
                number = 0;
                Some(format!("- {text}"))
            }
            Block::Number { text } => {
                number += 1;
                Some(format!("{number}. {text}"))
            }
            Block::Link { url } => {
                number = 0;
                (!url.trim().is_empty()).then(|| url.clone())
            }
            Block::Image { .. } => {
                number = 0;
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if description.is_empty() {
        task.title.clone()
    } else {
        format!("{}\n\n{description}", task.title)
    }
}

fn finish_description_command(app: &mut App, request: crate::description::CommandRequest) {
    match request {
        crate::description::CommandRequest::Copy(payload) => finish_copy(app, payload),
        crate::description::CommandRequest::Paste => paste_from_clipboard(app),
    }
}

#[derive(Debug)]
struct ClipboardContent {
    image: Option<arboard::ImageData<'static>>,
    text: Option<String>,
}

fn resolve_clipboard_content(
    image: Result<arboard::ImageData<'static>, arboard::Error>,
    text: Result<String, arboard::Error>,
) -> Result<Option<ClipboardContent>, String> {
    let mut errors = Vec::new();
    let image = match image {
        Ok(image) => Some(image),
        Err(arboard::Error::ContentNotAvailable) => None,
        Err(error) => {
            errors.push(format!("image ({error})"));
            None
        }
    };
    let text = match text {
        Ok(text) if !text.is_empty() => Some(text),
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => None,
        Err(error) => {
            errors.push(format!("text ({error})"));
            None
        }
    };
    if image.is_some() || text.is_some() {
        Ok(Some(ClipboardContent { image, text }))
    } else if errors.is_empty() {
        Ok(None)
    } else {
        Err(format!("could not read clipboard {}", errors.join(" or ")))
    }
}

fn read_clipboard_content(
    clipboard: &mut arboard::Clipboard,
) -> Result<Option<ClipboardContent>, String> {
    resolve_clipboard_content(clipboard.get_image(), clipboard.get_text())
}

fn paste_from_clipboard(app: &mut App) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            app.error(format!("Could not paste: {error}"));
            return;
        }
    };
    let content = match read_clipboard_content(&mut clipboard) {
        Ok(Some(content)) => content,
        Ok(None) => {
            app.info("Nothing to paste");
            return;
        }
        Err(error) => {
            app.error(format!("Could not paste: {error}"));
            return;
        }
    };

    paste_clipboard_content(app, content);
}

fn paste_clipboard_content(app: &mut App, content: ClipboardContent) {
    let ClipboardContent { image, text } = content;
    let pasted_text = text.is_some();
    let (pasted_image, image_error, category_ignored_image) = match app.mode {
        Mode::TaskForm => {
            let Some(form) = app
                .form
                .as_mut()
                .filter(|form| form.field == Field::Description)
            else {
                debug_assert!(false, "paste requires an active task description");
                return;
            };
            // When both representations exist, keep their deterministic
            // visual order: clipboard text first, then its image.
            if let Some(text) = text {
                form.description.insert_str(&text);
            }
            let (pasted_image, image_error) = match image.map(crate::image::stage_clipboard_image) {
                Some(Ok(image)) => {
                    let inserted = form.insert_temporary_image(image);
                    (
                        inserted,
                        (!inserted).then(|| "this field is full".to_string()),
                    )
                }
                Some(Err(error)) => (false, Some(error)),
                None => (false, None),
            };
            (pasted_image, image_error, false)
        }
        Mode::CategoryForm => {
            let Some(form) = app
                .category_form
                .as_mut()
                .filter(|form| form.on_description)
            else {
                debug_assert!(false, "paste requires an active category description");
                return;
            };
            if let Some(text) = text {
                form.description.insert_str(&text);
            }
            (false, None, image.is_some())
        }
        _ => {
            debug_assert!(false, "paste requires an active description editor");
            return;
        }
    };

    if let Some(error) = image_error {
        if pasted_text {
            app.error(format!("Pasted text, but could not paste image: {error}"));
        } else {
            app.error(format!("Could not paste image: {error}"));
        }
        return;
    }
    match (pasted_text, pasted_image) {
        (true, true) => app.info("Pasted text and image from clipboard"),
        (true, false) if category_ignored_image => {
            app.info("Pasted text; category descriptions accept text only")
        }
        (true, false) => app.info("Pasted text from clipboard"),
        (false, true) => app.info("Pasted image from clipboard"),
        (false, false) if category_ignored_image => {
            app.info("Category descriptions accept text only")
        }
        (false, false) => app.info("Nothing to paste"),
    }
}

fn finish_copy(app: &mut App, payload: crate::description::CopyPayload) {
    match payload {
        crate::description::CopyPayload::Text(text) => {
            if text.is_empty() {
                app.info("Nothing to copy");
                return;
            }
            match copy_text(&text) {
                Ok(ClipboardTarget::System) => app.info("Copied text to clipboard"),
                Ok(ClipboardTarget::Terminal) => app.info("Copied text through the terminal"),
                Err(err) => app.error(format!("Could not copy: {err}")),
            }
        }
        crate::description::CopyPayload::Image(path) => match copy_image_file(&path) {
            Ok(()) => app.info("Copied image to clipboard"),
            Err(err) => app.error(format!("Could not copy image: {err}")),
        },
        crate::description::CopyPayload::All(lines) => {
            if lines.is_empty() {
                app.info("Nothing to copy");
                return;
            }
            match copy_all(&lines) {
                Ok(ClipboardTarget::System) => app.info("Copied text and pictures"),
                Ok(ClipboardTarget::Terminal) => app.info("Copied plain text through the terminal"),
                Err(err) => app.error(format!("Could not copy: {err}")),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardTarget {
    System,
    Terminal,
}

const MAX_OSC52_RAW_BYTES: usize = 64 * 1024;
const MAX_OSC52_ENCODED_BYTES: usize = 80 * 1024;
const MAX_RICH_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

fn copy_text(text: &str) -> Result<ClipboardTarget, String> {
    copy_with_terminal_fallback(text, || {
        arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text))
    })
}

fn copy_with_terminal_fallback(
    text: &str,
    system_copy: impl FnOnce() -> Result<(), arboard::Error>,
) -> Result<ClipboardTarget, String> {
    match system_copy() {
        Ok(()) => Ok(ClipboardTarget::System),
        Err(system_error) => osc52_copy(text).map_err(|terminal_error| {
            format!("system clipboard: {system_error}; terminal clipboard: {terminal_error}")
        }),
    }
}

fn osc52_copy(text: &str) -> Result<ClipboardTarget, String> {
    use std::io::Write;

    let sequence = osc52_sequence(text)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())?;
    Ok(ClipboardTarget::Terminal)
}

fn osc52_sequence(text: &str) -> Result<String, String> {
    use base64::Engine;

    if text.len() > MAX_OSC52_RAW_BYTES {
        return Err(format!(
            "OSC 52 text is {} bytes; raw limit is {MAX_OSC52_RAW_BYTES} bytes",
            text.len()
        ));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_BYTES {
        return Err(format!(
            "OSC 52 payload is {} bytes; encoded limit is {MAX_OSC52_ENCODED_BYTES} bytes",
            encoded.len()
        ));
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

/// Decode a description image file and put its pixels on the system clipboard.
fn copy_image_file(path: &std::path::Path) -> Result<(), String> {
    let rgba = crate::image::load_dynamic(path)?.into_rgba8();
    let (width, height) = rgba.dimensions();
    let data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: rgba.into_raw().into(),
    };
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_image(data))
        .map_err(|e| e.to_string())
}

/// Put the whole description on the clipboard as HTML (with embedded images)
/// plus a plain-text fallback. Notes, browsers, and mail clients can
/// paste the rich form; terminals get the text.
fn copy_all(lines: &[crate::description::CopyLine]) -> Result<ClipboardTarget, String> {
    let (plain, html) = build_clipboard_payload(lines, MAX_RICH_CLIPBOARD_BYTES);

    copy_with_terminal_fallback(&plain, || {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_html(html.as_str(), Some(plain.as_str())))
    })
}

fn build_clipboard_payload(
    lines: &[crate::description::CopyLine],
    rich_budget: usize,
) -> (String, String) {
    build_clipboard_payload_with(lines, rich_budget, image_data_url)
}

fn build_clipboard_payload_with(
    lines: &[crate::description::CopyLine],
    rich_budget: usize,
    mut load_image: impl FnMut(&std::path::Path, usize) -> Result<String, String>,
) -> (String, String) {
    use crate::description::CopyLine;

    const IMAGE_PREFIX: &str = r#"<div><img src=""#;
    const IMAGE_SUFFIX: &str = r#"" /></div>"#;
    let mut html = String::new();
    let mut plain = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            plain.push('\n');
        }
        match line {
            CopyLine::Text(text) => {
                plain.push_str(text);
                push_rich_fragment(
                    &mut html,
                    &format!("<div>{}</div>", escape_html(text)),
                    rich_budget,
                );
            }
            CopyLine::Link(url) => {
                plain.push_str(url);
                let label = escape_html(url);
                let fragment = match crate::open::normalize_url(url) {
                    Some(url) => {
                        let href = escape_html(&url);
                        format!("<div><a href=\"{href}\">{label}</a></div>")
                    }
                    None => format!("<div>{label}</div>"),
                };
                push_rich_fragment(&mut html, &fragment, rich_budget);
            }
            CopyLine::Image(path) => {
                let label = format!("[image: {}]", path.display());
                plain.push_str(&label);
                let url_budget = rich_budget
                    .saturating_sub(html.len())
                    .saturating_sub(IMAGE_PREFIX.len() + IMAGE_SUFFIX.len());
                let image = load_image(path, url_budget)
                    .ok()
                    .filter(|url| url.len() <= url_budget)
                    .map(|url| format!("{IMAGE_PREFIX}{url}{IMAGE_SUFFIX}"));
                let fragment = image.unwrap_or_else(|| {
                    // Keep layout order without letting data URLs consume an
                    // unbounded clipboard string.
                    format!("<div>{}</div>", escape_html(&label))
                });
                push_rich_fragment(&mut html, &fragment, rich_budget);
            }
        }
    }
    (plain, html)
}

fn push_rich_fragment(html: &mut String, fragment: &str, budget: usize) {
    if html.len().saturating_add(fragment.len()) <= budget {
        html.push_str(fragment);
    }
}

fn image_data_url(path: &std::path::Path, url_budget: usize) -> Result<String, String> {
    use base64::Engine;
    use image::ImageEncoder;
    use std::io::Write;

    const PREFIX: &str = "data:image/png;base64,";
    let encoded_budget = url_budget
        .checked_sub(PREFIX.len())
        .ok_or_else(|| "rich clipboard image budget is exhausted".to_string())?;
    // Base64 expands three bytes into four. Keep the encoded URL inside the
    // caller's remaining rich-payload budget before allocating its String.
    let png_budget = (encoded_budget / 4) * 3;
    if png_budget == 0 {
        return Err("rich clipboard image budget is exhausted".to_string());
    }

    struct BoundedPng {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for BoundedPng {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(buf.len()) > self.limit {
                return Err(std::io::Error::other(
                    "encoded image exceeds rich clipboard budget",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let rgba = crate::image::load_dynamic(path)?.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut png = BoundedPng {
        bytes: Vec::new(),
        limit: png_budget,
    };
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png.bytes);
    let url = format!("{PREFIX}{b64}");
    if url.len() > url_budget {
        return Err("encoded image exceeds rich clipboard budget".to_string());
    }
    Ok(url)
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

// -------------------------------------------------------------- settings

fn handle_settings_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Up => {
            app.settings_index = app.settings_index.saturating_sub(1);
        }
        KeyCode::Down => {
            app.settings_index = (app.settings_index + 1).min(crate::app::SETTINGS_ITEMS.len() - 1);
        }
        KeyCode::Right | KeyCode::Tab => app.cycle_setting(app.settings_index, 1),
        KeyCode::Left | KeyCode::BackTab => app.cycle_setting(app.settings_index, -1),
        _ => {}
    }
}

fn handle_labels_key(app: &mut App, key: KeyEvent) {
    if app.label_editor.is_some() {
        match key.code {
            KeyCode::Esc => app.cancel_label_editor(),
            KeyCode::Enter => app.submit_label_editor(),
            KeyCode::Char('s') | KeyCode::Char('S')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                app.submit_label_editor()
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if let Some(editor) = &mut app.label_editor {
                    editor.color_focused = !editor.color_focused;
                }
                app.label_error = None;
                app.dirty = true;
            }
            KeyCode::Left | KeyCode::Right
                if app
                    .label_editor
                    .as_ref()
                    .is_some_and(|editor| editor.color_focused) =>
            {
                if let Some(editor) = &mut app.label_editor {
                    editor.move_color(if key.code == KeyCode::Left { -1 } else { 1 });
                }
                app.label_error = None;
                app.dirty = true;
            }
            _ => {
                if let Some(editor) = &mut app.label_editor
                    && !editor.color_focused
                {
                    edit_line(&mut editor.name, key);
                }
                app.label_error = None;
                app.dirty = true;
            }
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.close_labels(),
        KeyCode::Left => app.move_label_selection(-1),
        KeyCode::Right => app.move_label_selection(1),
        KeyCode::Up => move_label_selection_row(app, false),
        KeyCode::Down => move_label_selection_row(app, true),
        KeyCode::PageUp => app.move_label_selection(-10),
        KeyCode::PageDown => app.move_label_selection(10),
        KeyCode::Home => app.select_label(0),
        KeyCode::End => app.select_label(app.labels.len().saturating_sub(1)),
        KeyCode::Char('a') | KeyCode::Char('A')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.begin_new_label()
        }
        KeyCode::Enter => app.begin_rename_label(),
        KeyCode::Backspace => {
            let Some(label) = app.selected_label().cloned() else {
                return;
            };
            let confirm = Confirm::DeleteLabel(label.id.clone());
            if app.awaiting(confirm.clone()) {
                if app.delete_label_by_id(&label.id) {
                    app.info(format!("Label {} deleted and unassigned", label.name));
                }
            } else {
                app.ask_confirm(
                    confirm,
                    format!(
                        "Press Backspace again to delete {} and remove it from every task",
                        label.name
                    ),
                );
            }
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                && !c.is_control() =>
        {
            app.typeahead_jump(c);
        }
        _ => {}
    }
}

fn move_label_selection_row(app: &mut App, down: bool) {
    let Some((_, selected)) = app
        .areas
        .label_hits
        .iter()
        .find(|(index, _)| *index == app.label_index)
        .copied()
    else {
        app.move_label_selection(if down { 1 } else { -1 });
        return;
    };
    let selected_center = selected.x.saturating_add(selected.width / 2);
    let candidate = app
        .areas
        .label_hits
        .iter()
        .filter(|(_, rect)| {
            if down {
                rect.y > selected.y
            } else {
                rect.y < selected.y
            }
        })
        .min_by_key(|(_, rect)| {
            let row_distance = selected.y.abs_diff(rect.y);
            let center = rect.x.saturating_add(rect.width / 2);
            (row_distance, selected_center.abs_diff(center))
        })
        .map(|(index, _)| *index);
    if let Some(index) = candidate {
        app.select_label(index);
    } else {
        app.move_label_selection(if down { 1 } else { -1 });
    }
}

#[derive(Clone, Copy)]
enum FormCloseSource {
    Escape,
    OutsideClick,
}

impl FormCloseSource {
    const fn discard_prompt(self) -> &'static str {
        match self {
            Self::Escape => "Unsaved changes · press Esc again to discard",
            Self::OutsideClick => "Unsaved changes · press Esc to discard",
        }
    }
}

/// Close a form only when there is no content to lose, or after the same
/// entity-bound discard action is explicitly confirmed with Esc.
#[derive(Clone, Copy)]
enum OpenForm {
    Task,
    Category,
}

fn request_close_form(app: &mut App, form: OpenForm, source: FormCloseSource) -> bool {
    let state = match form {
        OpenForm::Task => app
            .form
            .as_ref()
            .map(|form| (form.is_dirty(), Confirm::DiscardTask(form.editing.clone()))),
        OpenForm::Category => app.category_form.as_ref().map(|form| {
            (
                form.is_dirty(),
                Confirm::DiscardCategory(form.editing.clone()),
            )
        }),
    };
    let Some((dirty, confirm)) = state else {
        return true;
    };
    let confirmed = !dirty || app.awaiting(confirm.clone());
    if confirmed {
        match form {
            OpenForm::Task => app.close_form(),
            OpenForm::Category => app.close_category_form(),
        }
        return true;
    }
    app.ask_confirm(confirm, source.discard_prompt());
    false
}

// ------------------------------------------------------------------ mouse

fn handle_mouse(app: &mut App, m: MouseEvent) {
    app.cancel_pending();
    if app.mode == Mode::Slash {
        handle_slash_mouse(app, m);
        return;
    }
    if app.mode == Mode::Labels {
        handle_labels_mouse(app, m);
        return;
    }
    if app.mode == Mode::TaskForm {
        // The full-size image is modal over the panels. Route it before
        // hit-testing the list beneath, or a preview click can close the form.
        if app.form.as_ref().is_some_and(|form| form.preview) {
            handle_form_mouse(app, m);
            return;
        }
        // A clean editor may yield to the underlying panels. Dirty content
        // stays modal; Esc is the explicit discard path.
        if click_on_panels(app, m) {
            if !request_close_form(app, OpenForm::Task, FormCloseSource::OutsideClick) {
                return;
            }
        } else {
            handle_form_mouse(app, m);
            return;
        }
    }
    if app.mode == Mode::CategoryForm {
        if click_on_panels(app, m) {
            if !request_close_form(app, OpenForm::Category, FormCloseSource::OutsideClick) {
                return;
            }
        } else {
            if m.kind == MouseEventKind::Down(MouseButton::Left)
                && app
                    .category_form
                    .as_ref()
                    .is_some_and(|form| form.description.menu.is_some())
            {
                match click_form_slash_menu(app, OpenForm::Category, m.column, m.row) {
                    MenuClick::Handled => return,
                    MenuClick::Miss => {}
                }
            }
            let clicked_link = if let (MouseEventKind::Down(MouseButton::Left), Some(form)) =
                (m.kind, &mut app.category_form)
            {
                if contains(form.name_area, m.column, m.row) {
                    form.set_description_focus(false);
                    form.name
                        .set_cursor_from_col((m.column - form.name_area.x) as usize);
                    None
                } else if contains(form.description_area, m.column, m.row) {
                    form.set_description_focus(true);
                    let row = m.row - form.description_area.y;
                    let column = (m.column - form.description_area.x) as usize;
                    let url = form.description.link_url_at_position(row, column);
                    form.description.click(row, column).then_some(url).flatten()
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(url) = clicked_link {
                open_link(app, &url);
            }
            return;
        }
    }
    if app.mode.is_overlay() {
        return;
    }
    match m.kind {
        // The wheel works on the list under the pointer, not on whichever
        // panel holds the keyboard focus — so you can spin through
        // categories without first clicking into them. Focus stays put.
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta = if m.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            if contains(app.areas.tasks, m.column, m.row) {
                app.move_task_selection(delta);
            } else if contains(app.areas.sidebar, m.column, m.row) && !app.searching {
                // Same reason clicking a category is blocked mid-search:
                // picking one would silently drop the query.
                app.move_category_selection(delta);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let (x, y) = (m.column, m.row);
            let sidebar = app.areas.sidebar;
            let tasks = app.areas.tasks;
            if contains(app.areas.command_bar, x, y) {
                focus_command_bar(app, x);
            } else if contains(sidebar, x, y) {
                if app.searching {
                    return;
                }
                let _ = app.set_focus(Focus::Sidebar);
                let row = app.cat_state.offset() + (y - sidebar.y) as usize;
                if row >= app.categories.len() {
                    return;
                }
                app.select_category(row);
                if clicked_again(app, ClickTarget::Sidebar, row) {
                    app.open_edit_category();
                }
            } else if contains(tasks, x, y) {
                let _ = app.set_focus(Focus::Tasks);
                let visual = app.task_state.offset() + (y - tasks.y) as usize;
                let Some(row) = app.task_at_visual_row(visual) else {
                    // Separator / empty — no task under the pointer.
                    return;
                };
                // The checkbox and the flags toggle when clicked
                // directly; the flags are the last column, so everything
                // from their first cell rightwards counts as them.
                let on_flags = app.areas.flag_x.is_some_and(|at| x >= at);
                let on_done = app
                    .areas
                    .done_x
                    .is_some_and(|at| x >= at && x < at + crate::ui::DONE_MARK_WIDTH);
                if on_flags {
                    app.cycle_importance(row);
                } else if on_done {
                    app.toggle_done(row);
                } else {
                    // Anywhere else selects, and selecting twice in
                    // quick succession opens the task.
                    app.select_task(row);
                    if clicked_again(app, ClickTarget::Tasks, row) {
                        app.open_edit_task();
                    }
                }
            } else if contains(app.areas.preview, x, y) && app.selected_task().is_some() {
                // Click the permanent preview to edit the selected task.
                let _ = app.set_focus(Focus::Tasks);
                app.open_edit_task();
            }
        }
        _ => {}
    }
}

fn handle_labels_mouse(app: &mut App, mouse: MouseEvent) {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if app.label_editor.is_some() {
        let swatch = app
            .areas
            .label_color_hits
            .iter()
            .find(|(_, area)| contains(*area, mouse.column, mouse.row))
            .map(|(color, _)| *color);
        if let Some(editor) = &mut app.label_editor {
            if let Some(color) = swatch {
                editor.color = color;
                editor.color_focused = true;
                app.label_error = None;
                app.dirty = true;
            } else if contains(app.areas.label_name_input, mouse.column, mouse.row) {
                editor.color_focused = false;
                editor.name.set_cursor_from_col(
                    mouse.column.saturating_sub(app.areas.label_name_input.x) as usize,
                );
                app.dirty = true;
            }
        }
        return;
    }
    let Some(row) = app
        .areas
        .label_hits
        .iter()
        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
        .map(|(index, _)| *index)
    else {
        return;
    };
    app.select_label(row);
    if clicked_again(app, ClickTarget::Labels, row) {
        app.begin_rename_label();
    }
}

fn handle_slash_mouse(app: &mut App, mouse: MouseEvent) {
    let rect = app.areas.slash_menu;
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            if contains(rect, mouse.column, mouse.row) =>
        {
            let count = crate::slash::matching(&app.input.value()).len();
            if count == 0 {
                return;
            }
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            cycle_index(&mut app.slash_index, count, delta);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if contains(app.areas.command_bar, mouse.column, mouse.row) {
                set_command_bar_cursor(app, mouse.column);
            } else if contains(rect, mouse.column, mouse.row)
                && mouse.row > rect.y
                && mouse.row + 1 < rect.bottom()
            {
                let row = (mouse.row - rect.y - 1) as usize;
                let index = app.areas.slash_menu_start + row;
                let query = app.input.value();
                let commands = crate::slash::matching(&query);
                if let Some(command) = commands.get(index).copied() {
                    app.slash_index = index;
                    close_slash(app);
                    run_slash(app, command, &query);
                }
            } else {
                close_slash(app);
            }
        }
        _ => {}
    }
}

/// Give the bottom command bar the same input mode as its keyboard entry
/// point. A locked search resumes editing instead of being silently cleared.
fn focus_command_bar(app: &mut App, x: u16) {
    match app.mode {
        Mode::Slash | Mode::Search => {}
        Mode::Normal if app.searching => app.resume_search(),
        Mode::Normal => app.open_slash(),
        _ => return,
    }
    set_command_bar_cursor(app, x);
}

fn set_command_bar_cursor(app: &mut App, x: u16) {
    // The visible slash occupies the first cell of the command field.
    let col = x.saturating_sub(app.areas.command_bar.x).saturating_sub(1) as usize;
    app.input.set_cursor_from_col(col);
    app.dirty = true;
}

/// True when a left-click lands on the sidebar or task list (and not on
/// an open form control that happens to sit in those coordinates).
fn click_on_panels(app: &App, m: MouseEvent) -> bool {
    if m.kind != MouseEventKind::Down(MouseButton::Left) {
        return false;
    }
    let (x, y) = (m.column, m.row);
    if !contains(app.areas.sidebar, x, y) && !contains(app.areas.tasks, x, y) {
        return false;
    }
    // Prefer form chrome when it overlaps the panels (modal / picker).
    if let Some(form) = &app.form {
        if contains(form.form_area, x, y) {
            return false;
        }
        if form.areas.field_at(x, y).is_some() {
            return false;
        }
        if form.picker.as_ref().is_some_and(|p| p.contains(x, y)) {
            return false;
        }
        if form
            .label_picker_area()
            .is_some_and(|area| contains(area, x, y))
        {
            return false;
        }
        if form
            .description_menu_area
            .is_some_and(|r| contains(r, x, y))
        {
            return false;
        }
    }
    if let Some(form) = &app.category_form
        && (contains(form.form_area, x, y)
            || contains(form.name_area, x, y)
            || contains(form.description_area, x, y)
            || form
                .description_menu_area
                .is_some_and(|r| contains(r, x, y)))
    {
        return false;
    }
    true
}

/// Clicking a field focuses it and puts the cursor where the pointer
/// landed. Clicks on the task list or sidebar leave the dialog (see
/// [`handle_mouse`]); other outside clicks are ignored. Double-click on
/// a picture opens it, same as Enter. The due picker also takes clicks
/// and scroll.
fn handle_form_mouse(app: &mut App, m: MouseEvent) {
    // The label picker owns wheel movement while the pointer is over it.
    if matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && app.form.as_ref().is_some_and(|form| {
        form.label_picker_area()
            .is_some_and(|area| contains(area, m.column, m.row))
    }) {
        app.clear_typeahead();
        let delta = if m.kind == MouseEventKind::ScrollUp {
            -1
        } else {
            1
        };
        if let Some(form) = &mut app.form {
            form.move_label_picker(delta);
        }
        return;
    }

    // Scroll over the open date/time picker.
    if matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && app.form.as_ref().is_some_and(|f| f.picker.is_some())
    {
        let up = matches!(m.kind, MouseEventKind::ScrollUp);
        if let Some(form) = &mut app.form
            && let Some(picker) = &mut form.picker
        {
            let _ = picker.scroll(m.column, m.row, up);
        }
        return;
    }

    // Scroll over the open description `/` menu moves the selection.
    if matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && app
        .form
        .as_ref()
        .is_some_and(|f| f.description.menu.is_some())
    {
        let up = matches!(m.kind, MouseEventKind::ScrollUp);
        if let Some(form) = &mut app.form {
            if up {
                form.description.menu_prev();
            } else {
                form.description.menu_next();
            }
        }
        return;
    }

    if m.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    // Click in the image preview: pause/resume GIF (does not close).
    if app.form.as_ref().is_some_and(|f| f.preview) {
        if let Some(form) = &mut app.form {
            form.preview_click();
        }
        return;
    }

    // Consume picker chrome so re-clicking Labels cannot dismiss and
    // immediately reopen the overlay.
    if app
        .form
        .as_ref()
        .is_some_and(|form| form.label_picker_open())
    {
        app.clear_typeahead();
        let inside = app.form.as_ref().is_some_and(|form| {
            form.label_picker_area()
                .is_some_and(|area| contains(area, m.column, m.row))
        });
        if inside {
            let row = app
                .form
                .as_ref()
                .and_then(|form| form.label_picker_row_at(m.column, m.row));
            if let Some(index) = row {
                let manage = if let Some(form) = &mut app.form {
                    form.select_label_picker(index);
                    form.label_picker_manage_selected()
                } else {
                    false
                };
                if manage {
                    app.open_labels_from_form();
                } else if let Some(form) = &mut app.form {
                    if let Err(error) = form.toggle_current_label() {
                        form.error = Some(error.to_string());
                    } else {
                        form.error = None;
                    }
                }
            }
            return;
        }
        if let Some(form) = &mut app.form {
            form.close_label_picker();
        }
    }

    // Clicks on the date/time picker (days, hour, minute).
    if app.form.as_ref().is_some_and(|f| f.picker.is_some()) {
        let Some(form) = &mut app.form else { return };
        let handled = form
            .picker
            .as_mut()
            .is_some_and(|p| p.click(m.column, m.row));
        if handled {
            return;
        }
        // Click outside the picker closes it (unless reopening Due).
        if !form.areas.due.contains(ratatui::layout::Position {
            x: m.column,
            y: m.row,
        }) {
            form.picker = None;
        }
    }

    // Description `/` menu: click a row to run it; click elsewhere closes the
    // menu only (dialog stays open). Handled before description.click, which
    // would otherwise dismiss the menu without selecting anything.
    if app
        .form
        .as_ref()
        .is_some_and(|f| f.description.menu.is_some())
    {
        match click_form_slash_menu(app, OpenForm::Task, m.column, m.row) {
            MenuClick::Handled => return,
            MenuClick::Miss => {
                // Fall through: place the cursor / change field, menu
                // closes via description.click or set_field.
            }
        }
    }

    enum AfterClick {
        None,
        OpenUrl(String),
        PreviewErr(String),
    }
    let after = {
        let Some(form) = &mut app.form else { return };
        let Some(field) = form.areas.field_at(m.column, m.row) else {
            // Click outside the dialog fields — do not keep a pending
            // double-click that could open a picture on the next hit.
            form.last_description_click = None;
            return;
        };
        // Leaving Due dismisses the calendar so keys go to the new field.
        form.set_field(field);
        let last_description_click = form.last_description_click.take();

        let area = form.areas.rect(field);
        let col = (m.column - area.x) as usize;
        let row = (m.row - area.y) as usize;
        match field {
            Field::Title => {
                form.title.set_cursor_from_col(col);
                AfterClick::None
            }
            Field::Due => {
                form.open_due_picker();
                AfterClick::None
            }
            Field::Category => {
                form.cycle_category(1);
                AfterClick::None
            }
            Field::Labels => {
                form.open_label_picker();
                AfterClick::None
            }
            Field::Description => {
                // Resolve the link against the painted glyphs before click()
                // moves the cursor; blank row padding is not a hit target.
                let clicked_link = form.description.link_url_at_position(row as u16, col);
                let hit = form.description.click(row as u16, col);
                if !hit {
                    AfterClick::None
                } else if let Some(url) = clicked_link {
                    AfterClick::OpenUrl(url)
                } else if form.description.selected_image().is_some() {
                    // Pictures are letterboxed — only the drawn box counts.
                    // Gutter clicks must not select the picture or insert a
                    // blank line (←/→ are what create a caret next to it).
                    let line = form.description.cursor_line();
                    if !form.image_hit_at(line, m.column, m.row) {
                        form.description.abandon_image_selection();
                        AfterClick::None
                    } else {
                        let now = Instant::now();
                        let again = last_description_click.is_some_and(|(at, last)| {
                            last == line && now.duration_since(at) < DOUBLE_CLICK
                        });
                        if again {
                            match form.open_image_preview() {
                                Some(err) => AfterClick::PreviewErr(err),
                                None => AfterClick::None,
                            }
                        } else {
                            form.last_description_click = Some((now, line));
                            AfterClick::None
                        }
                    }
                } else {
                    AfterClick::None
                }
            }
            Field::Importance => {
                form.cycle_importance();
                AfterClick::None
            }
        }
    };
    match after {
        AfterClick::None => {}
        AfterClick::OpenUrl(url) => open_link(app, &url),
        AfterClick::PreviewErr(err) => app.error(err),
    }
}

enum MenuClick {
    /// Click was on the menu (selected a row or the chrome).
    Handled,
    /// Click missed the menu rect entirely.
    Miss,
}

enum MenuHit {
    Handled,
    Command {
        index: usize,
        command: crate::description::Command,
    },
    Miss,
}

fn slash_menu_hit(
    description: &crate::description::DescriptionEditor,
    rect: Option<ratatui::layout::Rect>,
    x: u16,
    y: u16,
) -> MenuHit {
    let Some(rect) = rect else {
        return MenuHit::Miss;
    };
    if !contains(rect, x, y) {
        return MenuHit::Miss;
    }

    // Rows sit inside the border: top border at rect.y, first command at y+1.
    let commands = description.menu_commands();
    if commands.is_empty() || y <= rect.y || y >= rect.bottom().saturating_sub(1) {
        return MenuHit::Handled;
    }
    let index = (y - rect.y - 1) as usize;
    match commands.get(index).copied() {
        Some(command) => MenuHit::Command { index, command },
        None => MenuHit::Handled,
    }
}

/// Hit-test an open form's description `/` dropdown. Clicking a command row runs it.
fn click_form_slash_menu(app: &mut App, form_kind: OpenForm, x: u16, y: u16) -> MenuClick {
    let hit = match form_kind {
        OpenForm::Task => app.form.as_ref().map_or(MenuHit::Miss, |form| {
            slash_menu_hit(&form.description, form.description_menu_area, x, y)
        }),
        OpenForm::Category => app.category_form.as_ref().map_or(MenuHit::Miss, |form| {
            slash_menu_hit(&form.description, form.description_menu_area, x, y)
        }),
    };
    let (index, command) = match hit {
        MenuHit::Miss => return MenuClick::Miss,
        MenuHit::Handled => return MenuClick::Handled,
        MenuHit::Command { index, command } => (index, command),
    };
    let request = match form_kind {
        OpenForm::Task => {
            let Some(form) = app.form.as_mut() else {
                return MenuClick::Miss;
            };
            if let Some(menu) = &mut form.description.menu {
                menu.index = index;
            }
            form.before_edit(EditKind::Atomic);
            form.description.apply(command)
        }
        OpenForm::Category => {
            let Some(form) = app.category_form.as_mut() else {
                return MenuClick::Miss;
            };
            if let Some(menu) = &mut form.description.menu {
                menu.index = index;
            }
            form.before_edit(EditKind::Atomic);
            form.description.apply(command)
        }
    };
    if let Some(request) = request {
        finish_description_command(app, request);
    }
    MenuClick::Handled
}

/// Whether this click lands on the row the last one did, soon enough to
/// count as a double click. Records the click either way.
fn clicked_again(app: &mut App, target: ClickTarget, row: usize) -> bool {
    let now = Instant::now();
    let again = app.last_click.is_some_and(|(at, last_target, last_row)| {
        last_target == target && last_row == row && now.duration_since(at) < DOUBLE_CLICK
    });
    app.last_click = (!again).then_some((now, target, row));
    again
}

fn contains(area: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    area.contains(ratatui::layout::Position { x, y })
}

fn open_link(app: &mut App, url: &str) {
    match crate::open::open_url(url) {
        Ok(()) => app.info(format!("Opened {url}")),
        Err(error) => app.error(error),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;

    use crate::description::CopyLine;

    use super::{
        ClipboardContent, MAX_OSC52_ENCODED_BYTES, MAX_OSC52_RAW_BYTES,
        build_clipboard_payload_with, osc52_sequence, paste_clipboard_content,
        resolve_clipboard_content, task_clipboard_text,
    };

    #[test]
    fn clipboard_keeps_image_and_text_when_both_are_available() {
        let content = resolve_clipboard_content(
            Ok(arboard::ImageData {
                width: 1,
                height: 1,
                bytes: Cow::Owned(vec![1, 2, 3, 255]),
            }),
            Ok("image alt text".into()),
        )
        .expect("read clipboard")
        .expect("clipboard content");

        assert!(content.image.is_some());
        assert_eq!(content.text.as_deref(), Some("image alt text"));
    }

    #[test]
    fn clipboard_text_is_used_when_no_image_format_is_available() {
        let content = resolve_clipboard_content(
            Err(arboard::Error::ContentNotAvailable),
            Ok("clipboard text".into()),
        )
        .expect("read clipboard")
        .expect("clipboard content");

        assert!(content.image.is_none());
        assert_eq!(content.text.as_deref(), Some("clipboard text"));
    }

    fn mixed_clipboard_content() -> ClipboardContent {
        ClipboardContent {
            image: Some(arboard::ImageData {
                width: 1,
                height: 1,
                bytes: Cow::Owned(vec![10, 20, 30, 255]),
            }),
            text: Some("clipboard text".into()),
        }
    }

    fn assert_text_then_image(blocks: &[crate::model::Block]) -> PathBuf {
        assert!(matches!(
            blocks.first(),
            Some(crate::model::Block::Text { text }) if text == "clipboard text"
        ));
        let Some(crate::model::Block::Image { attachment_id }) = blocks.get(1) else {
            panic!("clipboard image must follow its text representation");
        };
        PathBuf::from(attachment_id)
    }

    #[test]
    fn mixed_clipboard_content_is_inserted_into_a_task_description() {
        let logical_path = std::env::temp_dir().join(format!(
            "mach-mixed-task-clipboard-{}",
            uuid::Uuid::new_v4()
        ));
        let store = crate::store::Store::open_in_memory_with_paths(logical_path).unwrap();
        let mut app = crate::app::App::with_store("test", store).unwrap();
        app.open_new_task();
        app.form.as_mut().unwrap().field = crate::form::Field::Description;

        paste_clipboard_content(&mut app, mixed_clipboard_content());

        let path = assert_text_then_image(&app.form.as_ref().unwrap().description.value());
        assert!(path.is_file());
        drop(app);
        assert!(!path.exists());
    }

    #[test]
    fn category_description_uses_only_text_from_mixed_clipboard_content() {
        let logical_path = std::env::temp_dir().join(format!(
            "mach-mixed-category-clipboard-{}",
            uuid::Uuid::new_v4()
        ));
        let store = crate::store::Store::open_in_memory_with_paths(logical_path).unwrap();
        let mut app = crate::app::App::with_store("test", store).unwrap();
        app.open_new_category();
        app.category_form
            .as_mut()
            .unwrap()
            .set_description_focus(true);

        paste_clipboard_content(&mut app, mixed_clipboard_content());

        assert_eq!(
            app.category_form.as_ref().unwrap().description.value(),
            vec![crate::model::Block::text("clipboard text")]
        );
    }

    #[test]
    fn task_copy_preserves_stored_text_that_names_an_existing_image() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mut task = crate::model::Task::new("Keep the path", 0, None, "");
        task.description = vec![crate::model::Block::text(path)];

        assert_eq!(
            task_clipboard_text(&task),
            format!("Keep the path\n\n{path}")
        );
    }

    #[test]
    fn terminal_clipboard_fallback_preserves_utf8_text() {
        assert_eq!(osc52_sequence("买菜").unwrap(), "\u{1b}]52;c;5Lmw6I+c\u{7}");
    }

    #[test]
    fn terminal_clipboard_rejects_oversized_raw_and_encoded_payloads() {
        let raw = osc52_sequence(&"x".repeat(MAX_OSC52_RAW_BYTES + 1)).unwrap_err();
        assert!(raw.contains("raw limit"), "{raw}");

        let encoded_input = "x".repeat(62 * 1024);
        assert!(encoded_input.len() <= MAX_OSC52_RAW_BYTES);
        let encoded = osc52_sequence(&encoded_input).unwrap_err();
        assert!(encoded.contains("encoded limit"), "{encoded}");
        assert!(MAX_OSC52_ENCODED_BYTES < encoded_input.len() * 4 / 3 + 4);
    }

    #[test]
    fn rich_clipboard_budget_replaces_an_oversized_image_but_keeps_plain_text() {
        let lines = vec![
            CopyLine::Text("before".into()),
            CopyLine::Image(PathBuf::from("huge.png")),
            CopyLine::Text("after".into()),
        ];
        let budget = 128;
        let (plain, html) = build_clipboard_payload_with(&lines, budget, |_, _| {
            Ok(format!("data:image/png;base64,{}", "A".repeat(256)))
        });

        assert_eq!(plain, "before\n[image: huge.png]\nafter");
        assert!(html.contains("[image: huge.png]"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.len() <= budget);
    }

    #[test]
    fn rich_clipboard_only_links_to_approved_url_schemes() {
        let lines = vec![
            CopyLine::Link("example.com/?a=1&b=2".into()),
            CopyLine::Link("javascript:alert(1)".into()),
        ];

        let (plain, html) = build_clipboard_payload_with(&lines, 1024, |_, _| unreachable!());

        assert_eq!(plain, "example.com/?a=1&b=2\njavascript:alert(1)");
        assert!(
            html.contains("href=\"https://example.com/?a=1&amp;b=2\""),
            "{html}"
        );
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(html.contains("<div>javascript:alert(1)</div>"), "{html}");
    }
}
