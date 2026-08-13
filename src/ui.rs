//! All drawing: a Categories panel on the left, a Tasks panel on the
//! right, and a status line along the bottom. Each panel is a rounded
//! block whose border lights up when it holds focus.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Gauge, List, ListItem, Padding, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table,
};
use ratatui_image::{Resize, StatefulImage};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, MessageKind, Mode, SETTINGS_ITEMS, UpdateActivity};
use crate::banner;
use crate::due;
use crate::form::Field;
use crate::model::LabelColor;
use crate::theme::Theme;

/// Outer width of the sidebar, borders and padding included.
pub const SIDEBAR_WIDTH: u16 = 26;
/// `[ ]` / `[✓]` in the task list and description subtasks.
pub const DONE_MARK_WIDTH: u16 = 3;
/// Right column shorter than this → no bottom preview (list only).
const PREVIEW_SPLIT_MIN: u16 = 16;
/// Minimum height of the list half when the preview is below.
const LIST_MIN: u16 = 6;
/// Minimum height of the preview / docked editor half (bottom layout).
const PREVIEW_MIN: u16 = 8;
/// Minimum list width when the preview sits to the right.
const LIST_WIDTH_MIN: u16 = 24;
/// Minimum preview width when docked on the right.
const PREVIEW_WIDTH_MIN: u16 = 28;
/// Whole right column narrower than this → no side preview.
const PREVIEW_SIDE_MIN: u16 = LIST_WIDTH_MIN + PREVIEW_WIDTH_MIN + 1;
pub const MIN_TERMINAL_WIDTH: u16 = 60;
pub const MIN_TERMINAL_HEIGHT: u16 = 16;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Every frame owns its hit targets. Hidden overlays and undersized
    // terminals must never retain clickable geometry from an older frame.
    app.areas = crate::app::Areas::default();
    if let Some(form) = &mut app.form {
        form.areas = crate::form::FieldAreas::default();
        form.form_area = Rect::ZERO;
        form.description_menu_area = None;
        form.image_hits.clear();
        if let Some(picker) = &mut form.picker {
            picker.layout = crate::duepicker::PickerLayout::default();
        }
    }
    if let Some(form) = &mut app.category_form {
        form.form_area = Rect::ZERO;
        form.name_area = Rect::ZERO;
        form.description_area = Rect::ZERO;
        form.description_menu_area = None;
    }

    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let p = Paragraph::new(format!(
            "too small · need {MIN_TERMINAL_WIDTH}×{MIN_TERMINAL_HEIGHT}"
        ))
        .centered();
        f.render_widget(p, area);
        return;
    }

    let theme = app.theme();
    let [content, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);
    // The panels sit against each other: two borders is already a
    // divider, a gap on top of that is just slack.
    // A column of air between the panels keeps each one's focus colour
    // unambiguous.
    let [sidebar, right] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
            .spacing(1)
            .areas(content);

    let mut modal_task_form = false;
    draw_sidebar(f, app, &theme, sidebar);
    if let Some((list, preview_rect)) =
        split_tasks_and_preview(right, &app.settings.preview_position)
    {
        app.areas.preview = preview_rect;
        draw_tasks(f, app, &theme, list);
        match app.mode {
            Mode::TaskForm => match docked_task_form_layout(preview_rect) {
                Some(layout) => draw_task_form(f, app, &theme, preview_rect, layout),
                None => {
                    draw_task_preview(f, app, &theme, preview_rect);
                    modal_task_form = true;
                }
            },
            _ => draw_task_preview(f, app, &theme, preview_rect),
        }
    } else {
        app.areas.preview = Rect::ZERO;
        draw_tasks(f, app, &theme, right);
        if app.mode == Mode::TaskForm {
            modal_task_form = true;
        }
    }
    draw_status(f, app, &theme, status);
    // Palette floats above the status bar.
    if app.mode == Mode::Slash {
        draw_slash_palette(f, app, &theme, status);
    }

    match app.mode {
        Mode::Help => draw_help(f, app, &theme, area),
        Mode::Settings => draw_settings(f, app, &theme, area),
        Mode::Labels => draw_labels(f, app, &theme, area),
        Mode::Welcome => draw_welcome(f, app, &theme, area),
        Mode::WhatsNew => draw_whats_new(f, &theme, area),
        Mode::CategoryForm => draw_category_form(f, app, &theme, area),
        Mode::TaskForm if modal_task_form => {
            // Draw the fallback last, over the intact panels and task preview.
            draw_task_form(f, app, &theme, area, TaskFormLayout::Modal);
        }
        Mode::TaskForm => {} // Already drawn in the task preview pane.
        _ => {}
    }
}

/// Split the right column into task list + preview when there is room.
/// `position` is `"bottom"` (default) or `"right"`. Falls back to bottom
/// when a side-by-side split will not fit, then to no preview.
fn split_tasks_and_preview(right: Rect, position: &str) -> Option<(Rect, Rect)> {
    if position == "right"
        && let Some(pair) = split_preview_right(right)
    {
        return Some(pair);
    }
    split_preview_bottom(right)
}

fn split_preview_bottom(right: Rect) -> Option<(Rect, Rect)> {
    if right.height < PREVIEW_SPLIT_MIN {
        return None;
    }
    let [list, preview] = Layout::vertical([
        Constraint::Min(LIST_MIN),
        Constraint::Length((right.height / 2).max(PREVIEW_MIN)),
    ])
    .spacing(0)
    .areas(right);
    if list.height < LIST_MIN || preview.height < PREVIEW_MIN {
        return None;
    }
    Some((list, preview))
}

fn split_preview_right(right: Rect) -> Option<(Rect, Rect)> {
    if right.width < PREVIEW_SIDE_MIN || right.height < PREVIEW_MIN {
        return None;
    }
    let preview_w = (right.width / 2).max(PREVIEW_WIDTH_MIN);
    let [list, preview] = Layout::horizontal([
        Constraint::Min(LIST_WIDTH_MIN),
        Constraint::Length(preview_w),
    ])
    .spacing(1)
    .areas(right);
    if list.width < LIST_WIDTH_MIN || preview.width < PREVIEW_WIDTH_MIN {
        return None;
    }
    Some((list, preview))
}

// --------------------------------------------------------- task dialog

const TASK_FORM_WIDE_CHROME: u16 = 9;
const TASK_FORM_COMPACT_CHROME: u16 = 18;
const TASK_FORM_MIN_DESCRIPTION_HEIGHT: u16 = 3;
const TASK_FORM_WIDE_MIN_WIDTH: u16 = 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskFormLayout {
    DockedWide,
    DockedCompact,
    Modal,
}

impl TaskFormLayout {
    fn is_docked(self) -> bool {
        !matches!(self, Self::Modal)
    }

    fn is_compact(self) -> bool {
        matches!(self, Self::DockedCompact)
    }
}

fn docked_task_form_layout(area: Rect) -> Option<TaskFormLayout> {
    if area.width >= TASK_FORM_WIDE_MIN_WIDTH
        && area.height >= TASK_FORM_WIDE_CHROME + TASK_FORM_MIN_DESCRIPTION_HEIGHT
    {
        Some(TaskFormLayout::DockedWide)
    } else if area.width >= PREVIEW_WIDTH_MIN
        && area.height >= TASK_FORM_COMPACT_CHROME + TASK_FORM_MIN_DESCRIPTION_HEIGHT
    {
        Some(TaskFormLayout::DockedCompact)
    } else {
        None
    }
}

/// Title, category/labels/due/flags metadata, then the description: a free stack of prose,
/// to-dos and pictures with a `/` menu for making new ones.
///
/// Docked layouts fill the permanent task preview pane. The modal layout is
/// centered over `area` when that pane cannot expose every field honestly.
fn draw_task_form(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, layout: TaskFormLayout) {
    // Disjoint borrows: the form owns the fields, the store owns the
    // decoded images.
    let App {
        form,
        images: store,
        ..
    } = app;
    let Some(form) = form.as_mut() else { return };

    let rect = if layout.is_docked() {
        area
    } else {
        let width = 92.min(area.width.saturating_sub(4));
        let description_height = area
            .height
            .saturating_sub(TASK_FORM_WIDE_CHROME)
            .clamp(TASK_FORM_MIN_DESCRIPTION_HEIGHT, 22);
        centered(
            area,
            width,
            (TASK_FORM_WIDE_CHROME + description_height).min(area.height),
        )
    };
    form.form_area = rect;
    let h_pad = if layout.is_docked() { 1 } else { 2 };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(
            format!(" {} ", form.title_text()),
            theme.accent_text().bold(),
        ))
        .padding(Padding::new(h_pad, h_pad, 0, 0));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let (title_box, category_box, labels_box, due_box, importance_box, description_box, hint) =
        if layout.is_compact() {
            let [title, category, labels, due, importance, description, hint] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(TASK_FORM_MIN_DESCRIPTION_HEIGHT),
                Constraint::Length(1),
            ])
            .areas(inner);
            (title, category, labels, due, importance, description, hint)
        } else {
            let [title, metadata, description, hint] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(TASK_FORM_MIN_DESCRIPTION_HEIGHT),
                Constraint::Length(1),
            ])
            .areas(inner);
            // Category and labels share the flexible space; Due fits a
            // formatted date+time and Flags fits ⚑⚑⚑.
            let [category, labels, due, importance] = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Fill(1),
                Constraint::Length(20),
                Constraint::Length(9),
            ])
            .spacing(1)
            .areas(metadata);
            (title, category, labels, due, importance, description, hint)
        };

    // --- title ----------------------------------------------------------
    let focused = form.field == Field::Title;
    let box_inner = render_field_box(f, field_block("Title", focused, None, theme), title_box);
    form.areas.title = box_inner;
    draw_text_input(
        f,
        &mut form.title,
        box_inner,
        "what needs doing?",
        focused,
        theme,
    );

    // --- category -------------------------------------------------------
    let focused = form.field == Field::Category;
    let box_inner = render_field_box(
        f,
        field_block("Category", focused, None, theme),
        category_box,
    );
    form.areas.category = box_inner;
    let category = format!("‹ {} ›", form.category_label());
    f.render_widget(
        Paragraph::new(truncate(&category, box_inner.width as usize)),
        box_inner,
    );

    // --- labels ---------------------------------------------------------
    // The field is a summary; Enter opens the complete bounded picker.
    let focused = form.field == Field::Labels;
    let box_inner = render_field_box(f, field_block("Labels", focused, None, theme), labels_box);
    form.areas.labels = labels_box;
    let labels = form
        .selected_labels()
        .into_iter()
        .map(|(name, color)| LabelToken::new(name, color))
        .collect::<Vec<_>>();
    if labels.is_empty() {
        render_or_placeholder(f, box_inner, "", "↵ choose", theme);
    } else {
        let shown = compact_badge_tokens(&labels, box_inner.width as usize);
        f.render_widget(
            Paragraph::new(label_badges_line(&shown, theme, false)),
            box_inner,
        );
    }

    // --- due -------------------------------------------------------------
    // Picker-only: show the value, no text cursor (Enter / click opens UI).
    // Store the outer box so the calendar left-aligns with the Due border.
    let focused = form.field == Field::Due;
    let box_inner = render_field_box(f, field_block("Due", focused, None, theme), due_box);
    form.areas.due = due_box;
    let view = form.due.visible(box_inner.width as usize);
    render_or_placeholder(f, box_inner, &view.text, "↵ Enter", theme);

    // --- importance ---------------------------------------------------------
    let focused = form.field == Field::Importance;
    let box_inner = render_field_box(
        f,
        field_block("Flags", focused, None, theme),
        importance_box,
    );
    form.areas.importance = box_inner;
    let marks = crate::model::importance_marks(form.importance);
    if marks.is_empty() {
        render_or_placeholder(f, box_inner, "", "→", theme);
    } else {
        f.render_widget(
            Paragraph::new(Line::styled(marks, Style::new().fg(theme.error_color()))),
            box_inner,
        );
    }

    // --- description --------------------------------------------------------------
    let focused = form.field == Field::Description;
    let (done, total) = form.description.progress();
    let progress = (total > 0).then(|| format!("{done}/{total}"));
    let box_inner = render_field_box(
        f,
        field_block("Description", focused, progress, theme),
        description_box,
    );
    form.areas.description = box_inner;
    let overlay = f.area();
    let image_occlusion = task_form_image_occlusion(form, overlay);
    draw_description(f, form, store, theme, box_inner, focused, image_occlusion);
    scrollbar(
        f,
        theme,
        description_box,
        form.description.content_height(),
        box_inner.height as usize,
        form.description.scroll(),
        focused,
    );

    // --- error or key hints ---------------------------------------------
    let footer = match &form.error {
        Some(error) => Line::styled(
            truncate(error, hint.width as usize),
            Style::new()
                .fg(theme.error_color())
                .add_modifier(Modifier::BOLD),
        ),
        None => Line::styled(
            match layout {
                TaskFormLayout::DockedWide => "/ commands · Ctrl+Z undo · Ctrl+S save · Esc list",
                TaskFormLayout::DockedCompact => "/ · Ctrl+S save · Esc list",
                TaskFormLayout::Modal => "/ commands · Ctrl+Z undo · Ctrl+S save · Esc cancel",
            },
            Style::new().fg(theme.muted_color()),
        ),
    };
    f.render_widget(Paragraph::new(footer), hint);

    // Drawn last so it sits over the description box below it.
    // Picker/image lightbox use the full frame so they are not clipped.
    if let Some(picker) = form.picker.as_mut() {
        draw_due_picker(f, theme, picker, form.areas.due, overlay);
    }
    if form.label_picker_open() {
        draw_label_picker(f, theme, form, form.areas.labels, overlay);
    }

    // Preview the picture the cursor is on, or the first one otherwise.
    if form.preview
        && let Some(path) = form
            .description
            .selected_image()
            .or_else(|| form.description.images().first().cloned())
    {
        draw_image_preview(f, store, form, theme, &path, overlay);
    }
}

/// Read-only view of the selected task in the permanent preview pane.
fn draw_task_preview(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let focused = false;
    let block = panel("Task preview", focused, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(task) = app.selected_task().cloned() else {
        app.invalidate_preview();
        let style = Style::new().fg(theme.muted_color());
        draw_box(f, inner, "Select a task · Enter to edit", style);
        return;
    };

    // One owned snapshot avoids a second selection lookup and lets the image
    // cache and preview editor be borrowed independently below.
    let image_paths: Vec<_> = task
        .description
        .iter()
        .filter_map(|block| match block {
            crate::model::Block::Image { attachment_id } => Some(app.images.resolve(attachment_id)),
            _ => None,
        })
        .collect();
    let todo = crate::model::todo_progress(&task);
    let labels = app
        .labels
        .iter()
        .filter(|label| task.label_ids.contains(&label.id))
        .map(LabelToken::from)
        .collect::<Vec<_>>();
    let title = task.title;
    let done = task.done;
    let due_s = due::display(&task.due, &app.settings.date_format);
    let importance = task.importance;
    let description_empty = task.description.is_empty();

    // Prefetch description pictures so they appear on the next frames.
    app.images.prefetch(image_paths);

    let flags = crate::model::importance_marks(importance);
    let mut meta = String::new();
    if !due_s.is_empty() {
        meta.push_str(&due_s);
    }
    if !flags.is_empty() {
        if !meta.is_empty() {
            meta.push_str("  ");
        }
        meta.push_str(&flags);
    }
    if let Some((d, t)) = todo {
        if !meta.is_empty() {
            meta.push_str("  ");
        }
        meta.push_str(&format!("{d}/{t}"));
    }

    let title_style = if done {
        Style::new()
            .fg(theme.muted_color())
            .add_modifier(Modifier::CROSSED_OUT | Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };

    let label_lines = wrapped_label_badges(&labels, inner.width as usize, theme, done);
    let label_height = u16::try_from(label_lines.len()).unwrap_or(u16::MAX);
    let meta_height = u16::from(!meta.is_empty());
    let [title_row, meta_row, labels_area, description_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(meta_height),
        Constraint::Length(label_height),
        Constraint::Min(0),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(Line::styled(
            truncate(&title, title_row.width as usize),
            title_style,
        )),
        title_row,
    );
    if meta_height > 0 {
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate(&meta, meta_row.width as usize),
                Style::new().fg(theme.muted_color()),
            )),
            meta_row,
        );
    }
    if label_height > 0 {
        f.render_widget(Paragraph::new(label_lines), labels_area);
    }

    if description_area.height == 0 {
        return;
    }
    if description_empty {
        f.render_widget(
            Paragraph::new(Line::styled(
                "Enter to edit",
                Style::new().fg(theme.muted_color()),
            )),
            description_area,
        );
        return;
    }

    app.ensure_preview();
    let App {
        images: store,
        preview_form,
        ..
    } = app;
    if let Some(paint) = preview_form.as_mut() {
        draw_description(f, paint, store, theme, description_area, false, None);
        if paint.description.content_height() > usize::from(description_area.height)
            && description_area.height > 0
        {
            let indicator = Rect {
                y: description_area.bottom() - 1,
                height: 1,
                ..description_area
            };
            f.render_widget(
                Paragraph::new(Line::styled(
                    "↓ more · Enter to edit",
                    Style::new()
                        .fg(theme.muted_color())
                        .add_modifier(Modifier::BOLD),
                )),
                indicator,
            );
        }
    }
}

/// One field of a dialog: a rounded box with its name on the border.
fn field_block<'a>(
    label: &'a str,
    focused: bool,
    note: Option<String>,
    theme: &Theme,
) -> Block<'a> {
    // Thick glyphs (┃/━) — terminal bold barely changes box lines.
    let (border, label_style) = if focused {
        (theme.accent_text(), theme.accent_text().bold())
    } else {
        (
            Style::new().fg(theme.muted_color()),
            Style::new()
                .fg(theme.muted_color())
                .add_modifier(Modifier::BOLD),
        )
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(border)
        .title(Span::styled(format!(" {label} "), label_style))
        .padding(Padding::horizontal(1));
    if let Some(note) = note {
        block = block.title_top(
            Line::styled(format!(" {note} "), Style::new().fg(theme.muted_color())).right_aligned(),
        );
    }
    block
}

/// The category dialog: the same shape as a task's, with a name and a
/// note about what the category is for.
fn draw_category_form(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let Some(form) = &mut app.category_form else {
        return;
    };
    // Borders (2), name box (3), hint (1).
    const CHROME: u16 = 6;
    let text_height = area.height.saturating_sub(CHROME).clamp(3, 12);
    let width = 72.min(area.width.saturating_sub(4));
    let rect = centered(area, width, (CHROME + text_height).min(area.height));
    form.form_area = rect;

    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(
            format!(" {} ", form.title_text()),
            theme.accent_text().bold(),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let [name_box, text_box, hint] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(text_height),
        Constraint::Length(1),
    ])
    .areas(inner);

    let focused = !form.on_description;
    let box_inner = render_field_box(f, field_block("Name", focused, None, theme), name_box);
    form.name_area = box_inner;
    draw_text_input(
        f,
        &mut form.name,
        box_inner,
        "What to call it",
        focused,
        theme,
    );

    let focused = form.on_description;
    let box_inner = render_field_box(
        f,
        field_block("Description", focused, None, theme),
        text_box,
    );
    form.description_area = box_inner;
    let (lines, cursor) = form
        .description
        .layout(box_inner.width as usize, box_inner.height);
    if form.description.is_empty() && form.description.menu.is_none() {
        render_or_placeholder(f, box_inner, "", "Press / for commands", theme);
    }
    for placed in lines {
        if matches!(placed.block, crate::description::Painted::Text { .. }) {
            draw_placed_text(f, theme, box_inner, &placed);
        }
    }
    if let (true, Some((row, col))) = (focused, cursor) {
        f.set_cursor_position((
            box_inner.x.saturating_add(col),
            box_inner.y.saturating_add(row),
        ));
    }
    if focused {
        form.description_menu_area = slash_menu_rect(&form.description, box_inner, cursor);
        draw_slash_menu(f, &form.description, theme, box_inner, cursor);
    }
    scrollbar(
        f,
        theme,
        text_box,
        form.description.content_height(),
        box_inner.height as usize,
        form.description.scroll(),
        focused,
    );

    let footer = match &form.error {
        Some(error) => Line::styled(
            truncate(error, hint.width as usize),
            Style::new()
                .fg(theme.error_color())
                .add_modifier(Modifier::BOLD),
        ),
        None => Line::styled(
            "/ commands · Ctrl+Z undo · Ctrl+S save · Esc cancel",
            Style::new().fg(theme.muted_color()),
        ),
    };
    f.render_widget(Paragraph::new(footer), hint);
}

/// The stack of blocks, plus the `/` menu when it is open.
fn draw_description(
    f: &mut Frame,
    form: &mut crate::form::TaskForm,
    store: &mut crate::image::ImageStore,
    theme: &Theme,
    area: Rect,
    focused: bool,
    external_occlusion: Option<Rect>,
) {
    let crate::form::TaskForm {
        description,
        description_scroll,
        description_menu_area,
        image_hits,
        image_occlusions,
        image_layout,
        ..
    } = form;
    draw_block_editor(
        f,
        description,
        store,
        theme,
        area,
        focused,
        description_scroll,
        description_menu_area,
        image_hits,
        image_occlusions,
        image_layout,
        external_occlusion,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_block_editor(
    f: &mut Frame,
    editor: &mut crate::description::DescriptionEditor,
    store: &mut crate::image::ImageStore,
    theme: &Theme,
    area: Rect,
    focused: bool,
    previous_scroll: &mut usize,
    menu_area: &mut Option<Rect>,
    image_hits: &mut Vec<(usize, Rect)>,
    previous_image_occlusions: &mut Vec<Rect>,
    previous_image_layout: &mut Vec<(std::path::PathBuf, u16, u16)>,
    external_occlusion: Option<Rect>,
) {
    if editor.is_empty() && editor.menu.is_none() {
        render_or_placeholder(f, area, "", "Press / for commands", theme);
    }
    let (blocks, cursor) = editor.layout(area.width as usize, area.height);
    let scroll = editor.scroll();
    let image_layout: Vec<_> = blocks
        .iter()
        .filter_map(|placed| match &placed.block {
            crate::description::Painted::Image(path) => Some((path.clone(), placed.y, placed.rows)),
            crate::description::Painted::Text { .. } => None,
        })
        .collect();
    let menu_rect = slash_menu_rect(editor, area, cursor);
    *menu_area = menu_rect;
    let image_occlusions = [menu_rect, external_occlusion]
        .into_iter()
        .flatten()
        .filter(|rect| rect.width > 0 && rect.height > 0 && rects_overlap(*rect, area))
        .collect::<Vec<_>>();
    // Graphics protocols ignore cell Clear. Drop placements when overlays,
    // scrolling, or image geometry changes so the next get re-emits cleanly.
    // Decoded pixels stay in RAM; only the terminal encoding is rebuilt.
    if *previous_image_occlusions != image_occlusions
        || *previous_scroll != scroll
        || *previous_image_layout != image_layout
    {
        store.clear_cache();
        f.render_widget(Clear, area);
    }
    *previous_image_occlusions = image_occlusions.clone();
    *previous_scroll = scroll;
    *previous_image_layout = image_layout;
    // Only hide images an overlay actually covers. Graphics protocols cannot
    // be "punched" cleanly, so an overlapping image becomes a compact marker.
    image_hits.clear();
    for placed in blocks {
        match &placed.block {
            crate::description::Painted::Image(path) => {
                let row = Rect {
                    y: area.y.saturating_add(placed.y),
                    height: placed.rows,
                    ..area
                };
                let covered = image_occlusions
                    .iter()
                    .any(|occlusion| rects_overlap(*occlusion, row));
                // Frame + type label only while the description field owns focus
                // and the cursor is on this picture — not when the dialog
                // opens on Title with the cursor still sitting on line 0.
                let show_frame = focused && placed.selected;
                if covered {
                    f.render_widget(Clear, row);
                    let hit = letterbox_rect(row, 4, 3);
                    draw_image_placeholder(f, theme, hit, show_frame);
                    image_hits.push((placed.line, hit));
                } else if let Some(hit) = draw_image(f, store, theme, path, row, show_frame) {
                    image_hits.push((placed.line, hit));
                }
            }
            crate::description::Painted::Text { .. } => {
                draw_placed_text(f, theme, area, &placed);
            }
        }
    }
    if focused && let Some((row, col)) = cursor {
        f.set_cursor_position((area.x.saturating_add(col), area.y.saturating_add(row)));
    }

    draw_slash_menu(f, editor, theme, area, cursor);
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// Screen rect of the open `/` dropdown, if any.
fn slash_menu_rect(
    description: &crate::description::DescriptionEditor,
    area: Rect,
    cursor: Option<(u16, u16)>,
) -> Option<Rect> {
    description.menu.as_ref()?;
    let commands = description.menu_commands();
    if commands.is_empty() {
        return None;
    }
    let width = 48.min(area.width);
    let height = u16::try_from(commands.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let cursor_row = cursor.map(|(row, _)| row).unwrap_or(0);
    let below = area.y.saturating_add(cursor_row).saturating_add(1);
    let y = if area.bottom().saturating_sub(below) >= height {
        below
    } else {
        area.y.saturating_add(cursor_row).saturating_sub(height)
    };
    Some(Rect {
        x: area.x.saturating_add(
            cursor
                .map(|(_, col)| col)
                .unwrap_or(0)
                .min(area.width.saturating_sub(width)),
        ),
        y,
        width,
        height,
    })
}

/// Soft-wrapped text / list / link block (description and category description).
fn draw_placed_text(f: &mut Frame, theme: &Theme, area: Rect, placed: &crate::description::Placed) {
    let crate::description::Painted::Text { rows, kind } = &placed.block else {
        return;
    };
    let indent = kind.indent();
    let max_rows = placed.rows as usize;
    for (i, wr) in rows.iter().enumerate().take(max_rows) {
        let y = area
            .y
            .saturating_add(placed.y)
            .saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let base = match kind {
            crate::description::TextKind::Link => Style::new()
                .fg(theme.accent)
                .add_modifier(Modifier::UNDERLINED),
            crate::description::TextKind::Todo { done: true } => Style::new()
                .fg(theme.muted_color())
                .add_modifier(Modifier::CROSSED_OUT),
            _ => Style::new(),
        };
        let description = line_with_selection(&wr.text, wr.sel, base, theme);
        let line = if i == 0 {
            match kind {
                crate::description::TextKind::Todo { done: true } => Line::from(
                    [
                        vec![Span::styled("[✓] ", Style::new().fg(theme.success_color()))],
                        description.spans,
                    ]
                    .concat(),
                ),
                crate::description::TextKind::Todo { done: false } => Line::from(
                    [
                        vec![Span::styled("[ ] ", Style::new().fg(theme.muted_color()))],
                        description.spans,
                    ]
                    .concat(),
                ),
                crate::description::TextKind::Bullet => Line::from(
                    [
                        vec![Span::styled("• ", Style::new().fg(theme.muted_color()))],
                        description.spans,
                    ]
                    .concat(),
                ),
                crate::description::TextKind::Number(n) => Line::from(
                    [
                        vec![Span::styled(
                            format!("{n}. "),
                            Style::new().fg(theme.muted_color()),
                        )],
                        description.spans,
                    ]
                    .concat(),
                ),
                crate::description::TextKind::Link => Line::from(
                    [
                        vec![Span::styled("↗ ", Style::new().fg(theme.muted_color()))],
                        description.spans,
                    ]
                    .concat(),
                ),
                crate::description::TextKind::Plain => description,
            }
        } else if indent > 0 {
            // Continuation rows line up under the text, past the prefix.
            Line::from([vec![Span::raw(" ".repeat(indent))], description.spans].concat())
        } else {
            description
        };
        f.render_widget(Paragraph::new(line), row);
    }
}

/// Compact cell stand-in when the `/` menu covers an image slot.
fn draw_image_placeholder(f: &mut Frame, theme: &Theme, area: Rect, selected: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rect = Rect { height: 1, ..area };
    let style = if selected {
        theme.accent_text()
    } else {
        Style::new().fg(theme.muted_color())
    };
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(Line::styled(" [image] ", style)), rect);
}

/// Full-size stand-in while a description/preview image is loading or failed.
enum ImageSlotKind<'a> {
    Loading,
    Broken { detail: &'a str },
}

/// Letterbox a content box into `area` (after a 1-cell frame margin),
/// matching how real pictures are laid out. `aspect_w` / `aspect_h` are
/// relative; unknown images use 4×3.
fn letterbox_rect(area: Rect, aspect_w: u16, aspect_h: u16) -> Rect {
    if area.width < 3 || area.height < 3 {
        return area;
    }
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let aw = u32::from(aspect_w.max(1));
    let ah = u32::from(aspect_h.max(1));
    let iw = u32::from(inner.width);
    let ih = u32::from(inner.height);
    let (pw, ph) = if iw * ah <= ih * aw {
        let pw = iw;
        let ph = (iw * ah / aw).clamp(1, ih);
        (pw as u16, ph as u16)
    } else {
        let ph = ih;
        let pw = (ih * aw / ah).clamp(1, iw);
        (pw as u16, ph as u16)
    };
    centered(inner, pw, ph)
}

/// Outer area used by preview stand-ins (inner content + 1-cell frame).
fn preview_slot_area(inner: Rect) -> Rect {
    Rect {
        x: inner.x.saturating_sub(1),
        y: inner.y.saturating_sub(1),
        width: inner.width.saturating_add(2),
        height: inner.height.saturating_add(2),
    }
}

fn draw_image_slot(
    f: &mut Frame,
    theme: &Theme,
    area: Rect,
    kind: ImageSlotKind<'_>,
    selected: bool,
) {
    if area.width < 3 || area.height < 2 {
        return;
    }
    let border = if selected {
        theme.accent_text()
    } else {
        Style::new().fg(theme.muted_color())
    };
    let (icon, title, title_style, detail) = match kind {
        ImageSlotKind::Loading => ("▢", "loading", Style::new().fg(theme.muted_color()), None),
        ImageSlotKind::Broken { detail } => (
            "✕",
            "broken image",
            Style::new().fg(theme.error_color()),
            Some(detail),
        ),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    // Vertical centre: pad, icon, title, optional detail.
    let content_rows: u16 = if detail.is_some() { 3 } else { 2 };
    let pad = inner.height.saturating_sub(content_rows) / 2;
    for _ in 0..pad {
        lines.push(Line::raw(""));
    }
    lines.push(
        Line::from(Span::styled(
            truncate(icon, inner.width as usize),
            title_style,
        ))
        .centered(),
    );
    lines.push(
        Line::from(Span::styled(
            truncate(title, inner.width as usize),
            title_style,
        ))
        .centered(),
    );
    if let Some(d) = detail {
        let d = d.trim();
        if !d.is_empty() {
            lines.push(
                Line::from(Span::styled(
                    truncate(d, inner.width as usize),
                    Style::new().fg(theme.muted_color()),
                ))
                .centered(),
            );
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The `/` menu floats under the line being typed on.
fn draw_slash_menu(
    f: &mut Frame,
    description: &crate::description::DescriptionEditor,
    theme: &Theme,
    area: Rect,
    cursor: Option<(u16, u16)>,
) {
    let Some(menu) = &description.menu else {
        return;
    };
    let Some(rect) = slash_menu_rect(description, area, cursor) else {
        return;
    };
    let commands = description.menu_commands();
    // Inner width of a bordered block (no horizontal padding).
    let row_width = rect.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = commands
        .iter()
        .enumerate()
        .map(|(i, command)| {
            let selected = i == menu.index.min(commands.len() - 1);
            dropdown_row(
                theme,
                selected,
                &format!("{:<14}", command.label()),
                description.command_hint(*command),
                row_width,
            )
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(
            format!(" /{} ", menu.query),
            Style::new().fg(theme.muted_color()),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn task_form_image_occlusion(form: &crate::form::TaskForm, area: Rect) -> Option<Rect> {
    if form.picker.is_some() {
        Some(due_picker_rect(form.areas.due, area))
    } else if form.label_picker_open() {
        let total_rows = form.label_choices().count().saturating_add(1);
        Some(label_picker_rect(total_rows, form.areas.labels, area))
    } else {
        None
    }
}

const DUE_PICKER_CAL_COLS: u16 = 21;
const DUE_PICKER_HEIGHT: u16 = 13;

fn due_picker_rect(field: Rect, area: Rect) -> Rect {
    let width = (DUE_PICKER_CAL_COLS + 2).max(field.width).min(area.width);
    let below = field.bottom();
    Rect {
        x: field.x.min(area.right().saturating_sub(width)),
        y: if area.bottom().saturating_sub(below) >= DUE_PICKER_HEIGHT {
            below
        } else {
            field.y.saturating_sub(DUE_PICKER_HEIGHT)
        },
        width,
        height: DUE_PICKER_HEIGHT,
    }
}

/// Calendar + clock, dropped under the due field. Date and time are both
/// set here — the Due field itself is not typed into.
fn draw_due_picker(
    f: &mut Frame,
    theme: &Theme,
    picker: &mut crate::duepicker::DuePicker,
    field: Rect,
    area: Rect,
) {
    use crate::duepicker::{PickerFocus, PickerLayout};

    let Some(day) = crate::duepicker::to_time_date(picker.day) else {
        return;
    };
    let mut events = ratatui::widgets::calendar::CalendarEventStore::today(
        Style::new().fg(theme.success_color()),
    );
    // Underlined rather than filled, to match the task list.
    events.add(day, theme.selection().add_modifier(Modifier::UNDERLINED));

    // Monthly needs 21 columns (` Su Mo …` / 7×3-wide day cells). Borders
    // add 2; keep the panel at least that wide so headers and days line up,
    // even when the Due field itself is narrower.
    let rect = due_picker_rect(field, area);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title_bottom(
            Line::styled(" Tab · clear(x) ", Style::new().fg(theme.muted_color())).left_aligned(),
        );
    f.render_widget(Clear, rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Calendar (8) + blank gap (1) + clock (1).
    let [cal_area, _gap, time_area] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let cal_area = Rect {
        width: DUE_PICKER_CAL_COLS.min(cal_area.width),
        ..cal_area
    };
    let time_area = Rect {
        width: DUE_PICKER_CAL_COLS.min(time_area.width),
        ..time_area
    };

    // Month header (1) + weekdays (1) + day grid — matches Monthly's layout.
    let days = Rect {
        x: cal_area.x,
        y: cal_area.y.saturating_add(2),
        width: cal_area.width,
        height: cal_area.height.saturating_sub(2),
    };

    let calendar = ratatui::widgets::calendar::Monthly::new(day, events)
        .show_month_header(theme.accent_text().add_modifier(Modifier::BOLD))
        .show_weekdays_header(Style::new().fg(theme.muted_color()))
        .show_surrounding(
            Style::new()
                .fg(theme.muted_color())
                .add_modifier(Modifier::DIM),
        );
    f.render_widget(calendar, cal_area);

    // Clock only — no "Time" label — centered under the calendar.
    let hour = format!("{:02}", picker.hour);
    let minute = format!("{:02}", picker.minute);
    let unit = |label: &str, on: bool| {
        if on {
            Span::styled(
                label.to_string(),
                theme.selection().add_modifier(Modifier::UNDERLINED),
            )
        } else {
            Span::styled(label.to_string(), Style::new())
        }
    };
    let time_line = Line::from(vec![
        unit(&hour, picker.focus == PickerFocus::Hour),
        Span::styled(":", Style::new().fg(theme.muted_color())),
        unit(&minute, picker.focus == PickerFocus::Minute),
    ])
    .centered();
    f.render_widget(Paragraph::new(time_line), time_area);

    // Hit targets for "HH" and "MM" within the centered "HH:MM" (5 cells).
    let clock_w = 5u16;
    let clock_x = time_area
        .x
        .saturating_add(time_area.width.saturating_sub(clock_w) / 2);
    picker.layout = PickerLayout {
        frame: rect,
        days,
        hour: Rect {
            x: clock_x,
            y: time_area.y,
            width: 2,
            height: 1,
        },
        minute: Rect {
            x: clock_x.saturating_add(3),
            y: time_area.y,
            width: 2,
            height: 1,
        },
        time_row: time_area,
    };
}

/// Bounded, scrolling task-label selector. Selection changes remain in the
/// task draft; dismissing the overlay does not save the form.
fn label_picker_rect(total_rows: usize, field: Rect, area: Rect) -> Rect {
    let desired_rows = total_rows.clamp(1, 8) as u16;
    let desired_height = desired_rows.saturating_add(2).min(area.height);
    let width = field.width.min(area.width);
    let below = field.bottom();
    let below_space = area.bottom().saturating_sub(below);
    let above_space = field.y.saturating_sub(area.y);
    let place_below = below_space >= 3 || below_space >= above_space;
    let available_height = if place_below {
        below_space
    } else {
        above_space
    };
    let height = desired_height.min(available_height);
    Rect {
        x: field.x.min(area.right().saturating_sub(width)),
        y: if place_below {
            below
        } else {
            field.y.saturating_sub(height)
        },
        width,
        height,
    }
}

fn draw_label_picker(
    f: &mut Frame,
    theme: &Theme,
    form: &mut crate::form::TaskForm,
    field: Rect,
    area: Rect,
) {
    let choices = form
        .label_choices()
        .map(|(_, name, color, selected)| (name.to_string(), color, selected))
        .collect::<Vec<_>>();
    let total_rows = choices.len().saturating_add(1);
    let selected = form
        .label_picker
        .as_ref()
        .map(|picker| picker.index)
        .unwrap_or_default()
        .min(total_rows.saturating_sub(1));
    let rect = label_picker_rect(total_rows, field, area);
    let width = rect.width;
    let footer = if let Some(error) = &form.error {
        Line::styled(
            format!(" {} ", truncate(error, width.saturating_sub(4) as usize)),
            Style::new()
                .fg(theme.error_color())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Line::styled(" Space toggle ", Style::new().fg(theme.muted_color()))
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title_bottom(footer.right_aligned());
    let inner = block.inner(rect);
    let visible = inner.height as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(total_rows.saturating_sub(visible));
    form.set_label_picker_layout(rect, start);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let row_width = inner.width as usize;
    let lines = (start..total_rows)
        .take(visible)
        .map(|index| {
            let mut line = if let Some((name, color, checked)) = choices.get(index) {
                let marker = if *checked { "[✓]" } else { "[ ]" };
                let name = truncate(name, row_width.saturating_sub(6));
                let used = marker.width().saturating_add(3 + name.width());
                Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled("■", theme.label_swatch(*color)),
                    Span::raw(" "),
                    Span::raw(name),
                    Span::raw(" ".repeat(row_width.saturating_sub(used))),
                ])
            } else {
                let available = row_width.saturating_sub(6);
                let label = if "Manage labels ↵".width() <= available {
                    "Manage labels ↵"
                } else {
                    "Manage ↵"
                };
                let content = truncate(label, available);
                let padding = " ".repeat(row_width.saturating_sub(6 + content.width()));
                Line::from(vec![
                    Span::raw("      "),
                    Span::raw(content),
                    Span::raw(padding),
                ])
            };
            if index == selected {
                line = line.style(theme.selection());
            }
            line
        })
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), inner);
    paint_scrollbar(f, theme, rect, total_rows, visible, start, true, 1);
}

/// A description image at whatever size the screen allows.
fn draw_image_preview(
    f: &mut Frame,
    store: &mut crate::image::ImageStore,
    form: &crate::form::TaskForm,
    theme: &Theme,
    path: &std::path::Path,
    area: Rect,
) {
    let rect = centered(
        area,
        (u32::from(area.width) * 9 / 10) as u16,
        (u32::from(area.height) * 9 / 10) as u16,
    );
    let title = truncate(
        &path.file_name().unwrap_or_default().to_string_lossy(),
        rect.width.saturating_sub(10) as usize,
    );
    let kind = crate::image::type_label(path);
    let anim_note = form
        .gif
        .as_ref()
        .map(|(_, g)| g)
        .filter(|g| g.is_animated())
        .map(|g| format!(" · {}/{}", g.frame_number(), g.frame_count()))
        .unwrap_or_default();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(
            format!(" {title} "),
            theme.accent_text().bold(),
        ))
        .title_top(
            Line::styled(
                format!(" {kind}{anim_note} "),
                Style::new().fg(theme.muted_color()),
            )
            .right_aligned(),
        )
        .title_bottom(
            Line::styled(
                match form.gif.as_ref().map(|(_, g)| g) {
                    Some(g) if g.is_animated() && g.is_paused() => {
                        " Esc closes · click/space resume "
                    }
                    Some(g) if g.is_animated() => " Esc closes · click/space pause ",
                    _ => " Esc closes ",
                },
                Style::new().fg(theme.muted_color()),
            )
            .right_aligned(),
        );
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    // Preview has its own chrome; no selection frame margin.
    if let Some((_, gif)) = form.gif.as_ref() {
        match store.preview_frame(gif) {
            Ok(protocol) => {
                let _ = render_protocol(f, protocol, inner, theme, None);
            }
            Err(err) => {
                let slot = letterbox_rect(preview_slot_area(inner), 4, 3);
                draw_image_slot(
                    f,
                    theme,
                    slot,
                    ImageSlotKind::Broken { detail: &err },
                    false,
                );
            }
        }
    } else {
        match store.get_preview(path) {
            crate::image::ImageReady::Ready(protocol) => {
                let _ = render_protocol(f, protocol, inner, theme, None);
            }
            crate::image::ImageReady::Loading => {
                // Loading means not cached yet — aspect unknown.
                let slot = letterbox_rect(preview_slot_area(inner), 4, 3);
                draw_image_slot(f, theme, slot, ImageSlotKind::Loading, false);
            }
            crate::image::ImageReady::Failed(err) => {
                let slot = letterbox_rect(preview_slot_area(inner), 4, 3);
                draw_image_slot(
                    f,
                    theme,
                    slot,
                    ImageSlotKind::Broken { detail: &err },
                    false,
                );
            }
        }
    }
}

/// Draws a decoded image, or a loading / broken stand-in sized like the
/// picture. Returns the screen rect of the picture (hit target).
fn draw_image(
    f: &mut Frame,
    store: &mut crate::image::ImageStore,
    theme: &Theme,
    path: &std::path::Path,
    area: Rect,
    selected: bool,
) -> Option<Rect> {
    if area.width < 3 || area.height < 3 {
        return None;
    }
    // Room for the frame is always left, so selecting a picture does not
    // change its size.
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    match store.get(path) {
        crate::image::ImageReady::Ready(protocol) => Some(render_protocol(
            f,
            protocol,
            inner,
            theme,
            selected.then_some(path),
        )),
        crate::image::ImageReady::Loading => {
            // Not in cache yet — aspect unknown until decode finishes.
            let slot = letterbox_rect(area, 4, 3);
            draw_image_slot(f, theme, slot, ImageSlotKind::Loading, selected);
            Some(slot)
        }
        crate::image::ImageReady::Failed(err) => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(err.as_str());
            let slot = letterbox_rect(area, 4, 3);
            draw_image_slot(
                f,
                theme,
                slot,
                ImageSlotKind::Broken { detail: name },
                selected,
            );
            Some(slot)
        }
    }
}

/// Paints the protocol and returns the interactive rect (frame when
/// selected, otherwise the picture itself).
fn render_protocol(
    f: &mut Frame,
    protocol: &mut ratatui_image::protocol::StatefulProtocol,
    inner: Rect,
    theme: &Theme,
    frame: Option<&std::path::Path>,
) -> Rect {
    // Scale (not Fit): Fit never grows past the source pixel size, so a
    // 1920px image on a large terminal only fills part of the preview.
    // Scale keeps aspect ratio and uses the full cell area.
    let size = protocol.size_for(Resize::Scale(None), inner.as_size());
    let picture = centered(
        inner,
        size.width.min(inner.width),
        size.height.min(inner.height),
    );
    f.render_stateful_widget(
        StatefulImage::default().resize(Resize::Scale(None)),
        picture,
        protocol,
    );
    let hit = if let Some(path) = frame {
        let border = Rect {
            x: picture.x.saturating_sub(1),
            y: picture.y.saturating_sub(1),
            width: picture.width.saturating_add(2),
            height: picture.height.saturating_add(2),
        };
        let kind = crate::image::type_label(path);
        f.render_widget(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(theme.accent_text())
                .title_top(
                    Line::styled(format!(" {kind} "), Style::new().fg(theme.muted_color()))
                        .right_aligned(),
                ),
            border,
        );
        border
    } else {
        picture
    };
    if let Some(Err(err)) = protocol.last_encoding_result() {
        let line = Line::styled(
            truncate(&format!("image: {err}"), inner.width as usize),
            Style::new().fg(theme.error_color()),
        );
        f.render_widget(Paragraph::new(line), inner);
    }
    hit
}

fn render_field_box(f: &mut Frame, block: Block, area: Rect) -> Rect {
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// Draws `text`, or a dim hint at what belongs there when it is empty.
/// Split `text` into spans, washing the selection with the theme accent.
fn line_with_selection(
    text: &str,
    sel: Option<(u16, u16)>,
    base: Style,
    theme: &Theme,
) -> Line<'static> {
    let Some((a, b)) = sel else {
        return Line::from(Span::styled(text.to_string(), base));
    };
    let a = a as usize;
    let b = b as usize;
    if a >= b {
        return Line::from(Span::styled(text.to_string(), base));
    }
    let sel_style = theme.selection();
    let mut spans = Vec::new();
    let mut col = 0usize;
    let mut chunk = String::new();
    let mut chunk_in_sel = false;
    let flush = |spans: &mut Vec<Span<'static>>, chunk: &mut String, in_sel: bool| {
        if chunk.is_empty() {
            return;
        }
        let style = if in_sel { sel_style } else { base };
        spans.push(Span::styled(std::mem::take(chunk), style));
    };
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        let in_sel = col >= a && col < b;
        if !chunk.is_empty() && in_sel != chunk_in_sel {
            flush(&mut spans, &mut chunk, chunk_in_sel);
        }
        chunk_in_sel = in_sel;
        chunk.push_str(grapheme);
        col += w;
    }
    flush(&mut spans, &mut chunk, chunk_in_sel);
    Line::from(spans)
}

fn render_or_placeholder(f: &mut Frame, area: Rect, text: &str, placeholder: &str, theme: &Theme) {
    let line = if text.is_empty() {
        Line::styled(
            truncate(placeholder, area.width as usize),
            Style::new()
                .fg(theme.muted_color())
                .add_modifier(Modifier::DIM),
        )
    } else {
        Line::raw(text.to_string())
    };
    f.render_widget(Paragraph::new(line), area);
}

#[derive(Clone)]
struct LabelToken {
    name: String,
    color: Option<LabelColor>,
}

impl LabelToken {
    fn new(name: &str, color: LabelColor) -> Self {
        Self {
            name: name.to_string(),
            color: Some(color),
        }
    }

    fn remainder(hidden: usize) -> Self {
        Self {
            name: format!("+{hidden}"),
            color: None,
        }
    }
}

impl From<&crate::model::Label> for LabelToken {
    fn from(label: &crate::model::Label) -> Self {
        Self::new(&label.name, label.color)
    }
}

fn label_badges_width(labels: &[LabelToken]) -> usize {
    labels
        .iter()
        .map(|label| {
            label
                .name
                .width()
                .saturating_add(if label.color.is_some() { 2 } else { 0 })
        })
        .sum::<usize>()
        .saturating_add(labels.len().saturating_sub(1))
}

fn label_badges_spans(labels: &[LabelToken], theme: &Theme, done: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(labels.len().saturating_mul(2));
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        match label.color {
            Some(color) => spans.push(Span::styled(
                format!(" {} ", label.name),
                theme.label_badge(color, done),
            )),
            None => spans.push(Span::styled(
                label.name.clone(),
                Style::new().fg(theme.muted_color()),
            )),
        }
    }
    spans
}

fn label_badges_line(labels: &[LabelToken], theme: &Theme, done: bool) -> Line<'static> {
    Line::from(label_badges_spans(labels, theme, done))
}

fn wrapped_label_badges(
    labels: &[LabelToken],
    width: usize,
    theme: &Theme,
    done: bool,
) -> Vec<Line<'static>> {
    if labels.is_empty() || width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut row = Vec::new();
    let mut row_width = 0usize;
    for label in labels {
        let name = truncate(&label.name, width.saturating_sub(2));
        let badge_width = name.width().saturating_add(2);
        let gap = usize::from(!row.is_empty());
        if !row.is_empty() && row_width.saturating_add(gap + badge_width) > width {
            lines.push(label_badges_line(&row, theme, done));
            row.clear();
            row_width = 0;
        }
        row_width = row_width
            .saturating_add(usize::from(!row.is_empty()))
            .saturating_add(badge_width);
        row.push(LabelToken {
            name,
            color: label.color,
        });
    }
    if !row.is_empty() {
        lines.push(label_badges_line(&row, theme, done));
    }
    lines
}

fn draw_text_input(
    f: &mut Frame,
    input: &mut crate::text_input::TextInput,
    area: Rect,
    placeholder: &str,
    focused: bool,
    theme: &Theme,
) {
    let view = input.visible(area.width as usize);
    if view.text.is_empty() {
        render_or_placeholder(f, area, "", placeholder, theme);
    } else {
        f.render_widget(
            Paragraph::new(line_with_selection(
                &view.text,
                view.sel_cols,
                Style::new(),
                theme,
            )),
            area,
        );
    }
    if focused {
        f.set_cursor_position((area.x.saturating_add(view.cursor_col), area.y));
    }
}

/// A panel: thick border glyphs, title in the top-left, accent colour
/// while focused. (Terminal bold barely thickens box-drawing chars.)
fn panel<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    field_block(title, focused, None, theme)
}

/// Panel scrollbar (right border). Accent when focused, grey otherwise.
fn scrollbar(
    f: &mut Frame,
    theme: &Theme,
    area: Rect,
    total: usize,
    visible: usize,
    offset: usize,
    focused: bool,
) {
    paint_scrollbar(f, theme, area, total, visible, offset, focused, 1);
}

#[allow(clippy::too_many_arguments)]
fn paint_scrollbar(
    f: &mut Frame,
    theme: &Theme,
    area: Rect,
    total: usize,
    visible: usize,
    offset: usize,
    focused: bool,
    vertical_margin: u16,
) {
    // Ratatui's thumb hits the end only when `position == content_length - 1`.
    // List/table `offset` runs 0..=(total - visible), so content_length must
    // be that range's size (max_offset + 1), not the raw row count — otherwise
    // the thumb stops short when you are already on the last row.
    let max_offset = total.saturating_sub(visible);
    if max_offset == 0 || area.height <= vertical_margin.saturating_mul(2) {
        return;
    }
    let mut state = ScrollbarState::new(max_offset + 1).position(offset.min(max_offset));
    let style = if focused {
        theme.accent_text()
    } else {
        Style::new().fg(theme.muted_color())
    };
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .symbols(ratatui::symbols::scrollbar::VERTICAL)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(style)
            .track_style(style),
        area.inner(Margin {
            horizontal: 0,
            vertical: vertical_margin,
        }),
        &mut state,
    );
}

// --------------------------------------------------------------- sidebar

fn draw_sidebar(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let chrome_focus = focused && !app.mode.command_bar_focused();
    let block = panel("Categories", chrome_focus, theme);
    let inner = block.inner(area);
    if inner.height == 0 || inner.width == 0 {
        f.render_widget(block, area);
        return;
    }

    let (list_area, hint_area) = if chrome_focus && inner.height > 1 {
        let [list_area, hint_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        (list_area, Some(hint_area))
    } else {
        (inner, None)
    };
    app.areas.sidebar = list_area;

    let width = inner.width as usize;
    let scores: Vec<String> = app
        .categories
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let (done, total) = app.category_progress_at(index);
            if app.settings.hide_done {
                (total - done).to_string()
            } else {
                format!("{done}/{total}")
            }
        })
        .collect();
    let count_width = scores.iter().map(|s| s.width()).max().unwrap_or(3).max(3);
    let name_field = width.saturating_sub(count_width + 1);
    let items: Vec<ListItem> = app
        .categories
        .iter()
        .zip(scores.iter())
        .map(|(cat, score)| {
            let count = format!("{score:>count_width$}");
            let name = truncate(&cat.name, name_field);
            let pad = " ".repeat(width.saturating_sub(name.width() + count.width()));
            ListItem::new(Line::from(vec![
                Span::raw(name),
                Span::raw(pad),
                Span::styled(count, Style::new().fg(theme.muted_color())),
            ]))
        })
        .collect();
    let rows = items.len();

    app.cat_state.select(Some(app.cat_index));
    let list = List::new(items).highlight_style(if focused {
        theme.selection()
    } else {
        theme.selection_unfocused()
    });
    f.render_widget(block, area);
    f.render_stateful_widget(list, list_area, &mut app.cat_state);
    if let Some(hint_area) = hint_area {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "⌥↑↓ reorder",
                Style::new().fg(theme.muted_color()),
            )))
            .right_aligned(),
            hint_area,
        );
    }

    let scrollbar_area = Rect {
        height: list_area.height.saturating_add(2).min(area.height),
        ..area
    };
    scrollbar(
        f,
        theme,
        scrollbar_area,
        rows,
        list_area.height as usize,
        app.cat_state.offset(),
        chrome_focus,
    );
}

// ----------------------------------------------------------------- tasks

fn draw_tasks(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Tasks;
    // While the task editor is open, dim panel chrome (border / scrollbar)
    // but keep the selected-row wash so the edited task stays visible.
    let chrome_focus = focused && app.mode != Mode::TaskForm && !app.mode.command_bar_focused();
    // The sidebar already says which category is showing; only a search
    // needs spelling out up here.
    let mut block = panel("Tasks", chrome_focus, theme);
    // Search spells out what matched; category notes stay in the editor.
    if app.searching {
        let context = format!(" search: {} · {} found ", app.search_query, app.view.len());
        block = block
            .title_top(Line::styled(context, Style::new().fg(theme.muted_color())).right_aligned());
    }
    let inner = block.inner(area);
    app.areas.tasks = inner;
    if inner.height == 0 || inner.width == 0 {
        f.render_widget(block, area);
        return;
    }

    if app.view.is_empty() {
        f.render_widget(block, area);
        let text = if app.searching {
            banner::NO_SEARCH_RESULTS
        } else {
            banner::EMPTY_TASKS
        };
        let style = if chrome_focus {
            theme.accent_text()
        } else {
            Style::new().fg(theme.muted_color())
        };
        draw_box(f, inner, text, style);
        return;
    }

    // Category is shown as a section header row in All Tasks / search, not a
    // per-task suffix. Flags keep a fixed right edge; due dates and description
    // markers live inside each task's content cell so metadata on one task
    // cannot shorten every other title in the list.
    let flags_width = crate::model::MAX_IMPORTANCE as usize;
    let today = chrono::Local::now().date_naive();

    // Preserve a useful title at narrow widths. Flags stay aligned when the
    // panel can afford them; row-local metadata decides independently whether
    // its complete value fits beside that task's title.
    const TITLE_MIN: usize = 8;
    let available = inner.width as usize;
    let flags_visible = DONE_MARK_WIDTH as usize + 1 + TITLE_MIN + 1 + flags_width <= available;
    let mut widths = vec![
        Constraint::Length(DONE_MARK_WIDTH), // [ ] / [✓]
        Constraint::Fill(1),                 // title + this task's metadata
    ];
    if flags_visible {
        widths.push(Constraint::Length(flags_width as u16));
    }
    let column_gaps = widths.len().saturating_sub(1);
    let content_width = available
        .saturating_sub(DONE_MARK_WIDTH as usize)
        .saturating_sub(column_gaps)
        .saturating_sub(if flags_visible { flags_width } else { 0 });
    let rows: Vec<Row> = app
        .list_rows
        .iter()
        .map(|row| match row {
            // Placeholder — the full-width rule is painted after the table
            // so column gaps cannot break the line or the title.
            crate::app::TaskListRow::Separator { .. } => {
                Row::new(std::iter::repeat_n(Cell::new(""), widths.len()))
            }
            crate::app::TaskListRow::Task(view_idx) => {
                let task = &app.tasks[app.view[*view_idx]];
                task_row(
                    TaskPresentation::new(task, &app.labels, &app.settings.date_format, today),
                    theme,
                    (*view_idx == app.task_index).then(|| {
                        if chrome_focus {
                            theme.selection()
                        } else {
                            theme.selection_unfocused()
                        }
                    }),
                    content_width,
                    flags_visible,
                )
            }
        })
        .collect();
    // Selected-row wash stays on during edit; bold only when the list has chrome focus.
    let table = Table::new(rows, widths).block(block).column_spacing(1);

    // Remember where the markers ended up, so a click can find them. The
    // flags sit at the right edge, the tick at the left.
    app.areas.done_x = Some(inner.x);
    app.areas.flag_x = flags_visible.then_some(inner.right().saturating_sub(flags_width as u16));

    let vis = app.selected_visual_row();
    app.task_state.select(vis);
    // Table does `start = offset.min(selected)`, so scrolling up to the first
    // task of a group lands on that task and hides the section header above
    // it. Pull offset back onto the header first so the header stays in view.
    if let Some(vis) = vis {
        pin_section_header(app, vis);
    }
    f.render_stateful_widget(table, area, &mut app.task_state);

    // Full-width category rules on top of separator placeholder rows.
    let offset = app.task_state.offset();
    let rule_style = Style::new().fg(theme.muted_color());
    for (vis_i, row) in app.list_rows.iter().enumerate().skip(offset) {
        let y = inner
            .y
            .saturating_add(u16::try_from(vis_i - offset).unwrap_or(u16::MAX));
        if y >= inner.bottom() {
            break;
        }
        let crate::app::TaskListRow::Separator { title } = row else {
            continue;
        };
        // Align the name with task titles (after `[ ]` + column gap).
        let title_x = (DONE_MARK_WIDTH + 1) as usize;
        let line = category_rule(title, inner.width as usize, title_x);
        f.render_widget(
            Paragraph::new(Span::styled(line, rule_style)),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }

    scrollbar(
        f,
        theme,
        area,
        app.list_rows.len(),
        inner.height as usize,
        app.task_state.offset(),
        chrome_focus,
    );
}

/// If `vis` is the first task under a section header, do not let the table
/// scroll that header off the top of the viewport.
fn pin_section_header(app: &mut App, vis: usize) {
    if vis == 0 {
        return;
    }
    let header = vis - 1;
    if !matches!(
        app.list_rows.get(header),
        Some(crate::app::TaskListRow::Separator { .. })
    ) {
        return;
    }
    if app.task_state.offset() > header {
        *app.task_state.offset_mut() = header;
    }
}

/// The markers shown between a task's title and its due date.
fn extras(task: &crate::model::Task) -> String {
    let mut has_prose_or_image = false;
    let mut done = 0usize;
    let mut total = 0usize;
    for block in &task.description {
        match block {
            crate::model::Block::Todo { done: is_done, .. } => {
                total += 1;
                done += usize::from(*is_done);
            }
            block if !block.is_empty() => has_prose_or_image = true,
            _ => {}
        }
    }
    match (has_prose_or_image, total) {
        (true, 0) => "≡".to_string(),
        (true, _) => format!("≡ {done}/{total}"),
        (false, 0) => String::new(),
        (false, _) => format!("{done}/{total}"),
    }
}

/// Owned display data derived once for one task during a frame.
struct TaskPresentation<'a> {
    title: &'a str,
    labels: Vec<LabelToken>,
    extras: String,
    due: String,
    flags: String,
    done: bool,
}

impl<'a> TaskPresentation<'a> {
    fn new(
        task: &'a crate::model::Task,
        labels: &[crate::model::Label],
        date_format: &str,
        today: chrono::NaiveDate,
    ) -> Self {
        Self {
            title: &task.title,
            labels: labels
                .iter()
                .filter(|label| task.label_ids.contains(&label.id))
                .map(LabelToken::from)
                .collect(),
            extras: extras(task),
            due: due::display_compact_at(&task.due, date_format, today),
            flags: crate::model::importance_marks(task.importance),
            done: task.done,
        }
    }
}

/// Full-width rule with the category name aligned to the title column:
/// `─── Mach ────────────────` (space before the name, same column as titles).
fn category_rule(title: &str, width: usize, title_x: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // One space before the name so it does not touch the rule; the name
    // still starts at `title_x` like task titles after `[ ] `.
    let label = format!(" {title} ");
    let label_w = label.width();
    let pad = title_x.saturating_sub(1).min(width);
    if pad + label_w >= width {
        let head = "─".repeat(pad);
        return truncate(&format!("{head}{label}"), width);
    }
    format!(
        "{}{label}{}",
        "─".repeat(pad),
        "─".repeat(width - pad - label_w)
    )
}

fn task_row(
    presentation: TaskPresentation<'_>,
    theme: &Theme,
    selection: Option<Style>,
    content_width: usize,
    flags_visible: bool,
) -> Row<'static> {
    let done = presentation.done;
    let selected = selection.is_some();
    // A finished task is muted — but not on the selected row (even when
    // Categories has focus), where the tick and strikethrough say enough.
    // Due colour belongs to the due label rather than tinting the whole title.
    let title_style = if done && !selected {
        Style::new().fg(theme.muted_color())
    } else {
        theme.plain()
    };
    let title_style = if done {
        title_style.add_modifier(Modifier::CROSSED_OUT)
    } else {
        title_style
    };

    let mut cells = Vec::with_capacity(5);
    let (mark, mark_style) = if done {
        ("[✓]", Style::new().fg(theme.success_color()))
    } else {
        ("[ ]", Style::new().fg(theme.muted_color()))
    };
    cells.push(Cell::new(mark).style(mark_style));
    let metadata_style = if done {
        Style::new()
            .fg(theme.muted_color())
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::new().fg(theme.muted_color())
    };
    let due_style = if done {
        title_style
    } else {
        Style::new().fg(theme.accent)
    };
    cells.push(Cell::new(task_content_line(
        &presentation,
        TaskContentStyles {
            title: title_style,
            extras: metadata_style,
            due: due_style,
        },
        theme,
        content_width,
    )));
    if flags_visible {
        let flag_style = if done {
            metadata_style
        } else {
            Style::new().fg(theme.error_color())
        };
        cells.push(Cell::new(Text::from(
            Line::from(Span::styled(presentation.flags, flag_style)).right_aligned(),
        )));
    }
    let row = Row::new(cells);
    if let Some(style) = selection {
        row.style(style)
    } else {
        row
    }
}

/// Build one task's content cell with row-local metadata at its right edge.
/// Due is the highest-priority suffix. Labels then use whole tokens and a
/// `+N` remainder; description/progress joins only when every value fits
/// while retaining a recognisable title.
#[derive(Clone, Copy)]
struct TaskContentStyles {
    title: Style,
    extras: Style,
    due: Style,
}

fn task_content_line(
    presentation: &TaskPresentation<'_>,
    styles: TaskContentStyles,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    const TITLE_MIN: usize = 8;
    const META_GAP: usize = 1;

    let title_floor = presentation.title.width().min(TITLE_MIN);
    let mut show_due = false;
    let mut show_extras = false;
    let mut shown_labels = Vec::new();
    let mut metadata_width = 0;

    if !presentation.due.is_empty() && title_floor + META_GAP + presentation.due.width() <= width {
        show_due = true;
        metadata_width = presentation.due.width();
    }

    if !presentation.labels.is_empty() {
        let reserved = title_floor
            .saturating_add(META_GAP)
            .saturating_add(metadata_width)
            .saturating_add(usize::from(metadata_width > 0));
        shown_labels = compact_badge_tokens(&presentation.labels, width.saturating_sub(reserved));
        if !shown_labels.is_empty() {
            metadata_width = metadata_width
                .saturating_add(usize::from(metadata_width > 0))
                .saturating_add(label_badges_width(&shown_labels));
        }
    }
    if !presentation.extras.is_empty() {
        let joined_width = if metadata_width == 0 {
            presentation.extras.width()
        } else {
            presentation.extras.width() + META_GAP + metadata_width
        };
        if title_floor + META_GAP + joined_width <= width {
            show_extras = true;
            metadata_width = joined_width;
        }
    }

    if metadata_width == 0 {
        return Line::from(Span::styled(
            truncate(presentation.title, width),
            styles.title,
        ));
    }

    let title_width = width.saturating_sub(META_GAP + metadata_width);
    let title = truncate(presentation.title, title_width);
    let padding = width.saturating_sub(title.width() + metadata_width);
    let mut spans = vec![
        Span::styled(title, styles.title),
        Span::raw(" ".repeat(padding)),
    ];
    if !shown_labels.is_empty() {
        spans.extend(label_badges_spans(&shown_labels, theme, presentation.done));
        if show_extras || show_due {
            spans.push(Span::raw(" "));
        }
    }
    if show_extras {
        spans.push(Span::styled(presentation.extras.clone(), styles.extras));
        if show_due {
            spans.push(Span::raw(" "));
        }
    }
    if show_due {
        spans.push(Span::styled(presentation.due.clone(), styles.due));
    }
    Line::from(spans)
}

fn compact_badge_tokens(labels: &[LabelToken], width: usize) -> Vec<LabelToken> {
    for shown in (0..=labels.len()).rev() {
        let hidden = labels.len() - shown;
        let mut parts = labels[..shown].to_vec();
        if hidden > 0 {
            parts.push(LabelToken::remainder(hidden));
        }
        if label_badges_width(&parts) <= width {
            return parts;
        }
    }
    Vec::new()
}

// ------------------------------------------------------------ status bar

fn draw_status(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    // The bar is a panel like the others, minus the name: while a
    // command or a search is being typed it is what has focus.
    let typing = matches!(app.mode, Mode::Slash | Mode::Search);
    let update_activity = (!typing).then(|| app.update_activity()).flatten();
    let archive_activity = (!typing && app.message.is_none())
        .then(|| app.archive_activity_text())
        .flatten();
    let downloading = matches!(update_activity, Some(UpdateActivity::Downloading(_)));
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(if typing {
            theme.accent_text()
        } else {
            Style::new().fg(theme.muted_color())
        })
        .padding(if downloading {
            Padding::ZERO
        } else {
            Padding::horizontal(1)
        });
    let inner = block.inner(area);
    f.render_widget(block, area);
    let area = inner;

    if let Some(UpdateActivity::Downloading(progress)) = update_activity {
        draw_download_progress(f, progress, theme, area);
        return;
    }

    let right = Line::from(Span::styled(
        due::now_string(&app.settings.date_format),
        Style::new().fg(theme.muted_color()),
    ));
    // A transient message owns the status row. Its result or recovery detail
    // is more useful for these few seconds than a clock that is always there
    // otherwise. Persistent update notices still share the row with the time.
    let show_clock = typing || update_activity.is_some() || app.message.is_none();
    let right_width = if show_clock { right.width() as u16 } else { 0 };
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_width.min(area.width)),
    ])
    .areas(area);
    // The clock is display-only, but it is still part of the command bar's
    // mouse target. A click there focuses the input and lands at its end.
    app.areas.command_bar = area;
    if show_clock {
        f.render_widget(Paragraph::new(right), right_area);
    }

    let field = left_area.width.saturating_sub(2) as usize;
    let left = match app.mode {
        Mode::Slash | Mode::Search => {
            let view = app.input.visible(field);
            f.set_cursor_position((
                left_area
                    .x
                    .saturating_add(1)
                    .saturating_add(view.cursor_col),
                left_area.y,
            ));
            let description = line_with_selection(&view.text, view.sel_cols, Style::new(), theme);
            Line::from(
                [
                    vec![Span::styled("/", theme.accent_text())],
                    description.spans,
                ]
                .concat(),
            )
        }
        _ => {
            if let Some(text) = archive_activity.as_deref() {
                Line::from(Span::styled(truncate(text, field), theme.accent_text()))
            } else if update_activity == Some(UpdateActivity::Checking) {
                Line::from(Span::styled("Checking for updates…", theme.accent_text()))
            } else {
                match app.status_message() {
                    Some((text, kind)) => {
                        let style = match kind {
                            MessageKind::Error => Style::new()
                                .fg(theme.error_color())
                                .add_modifier(Modifier::BOLD),
                            MessageKind::Info => theme.plain(),
                        };
                        Line::from(Span::styled(truncate(text, field), style))
                    }
                    None => {
                        let hint = if app.searching {
                            format!("search: {} · Esc clears", app.search_query)
                        } else {
                            "/ commands".to_string()
                        };
                        if (left_area.width as usize) >= hint.width() + 2 {
                            Line::from(Span::styled(hint, Style::new().fg(theme.muted_color())))
                        } else {
                            Line::raw("")
                        }
                    }
                }
            }
        }
    };
    f.render_widget(Paragraph::new(left), left_area);
}

fn draw_download_progress(
    f: &mut Frame,
    progress: crate::update::DownloadProgress,
    theme: &Theme,
    area: Rect,
) {
    let Some(total) = progress.total.filter(|total| *total > 0) else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "Downloading update… {}",
                    readable_bytes(progress.downloaded)
                ),
                theme.accent_text(),
            )))
            .centered(),
            area,
        );
        return;
    };
    let ratio = progress.downloaded.min(total) as f64 / total as f64;
    let percent = (ratio * 100.0).round() as u64;
    let label = format!("Downloading update {percent}%");
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(label)
            .use_unicode(true)
            .style(Style::new().fg(theme.muted_color()))
            .gauge_style(theme.accent_text().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn readable_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Dropdown of `/` commands, drawn upward from the status bar.
fn draw_slash_palette(f: &mut Frame, app: &mut App, theme: &Theme, status: Rect) {
    let query = app.input.value();
    let commands = crate::slash::matching(&query);
    if commands.is_empty() {
        return;
    }
    let desired_width = commands
        .iter()
        .map(|command| format!(" /{:<15}{} ", command.usage(), command.hint()).width() as u16)
        .max()
        .unwrap_or(22)
        .saturating_add(3);
    let width = desired_width.min(status.width.saturating_sub(2)).max(24);
    let height = u16::try_from(commands.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(status.y.max(3));
    let rect = Rect {
        x: status.x,
        y: status.y.saturating_sub(height),
        width,
        height,
    };
    app.areas.slash_menu = rect;
    let visible_rows = usize::from(height.saturating_sub(2));
    let selected_index = app.slash_index.min(commands.len() - 1);
    let start = selected_index
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(commands.len().saturating_sub(visible_rows));
    app.areas.slash_menu_start = start;
    let row_width = width.saturating_sub(2) as usize;
    let lines: Vec<Line> = commands
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, cmd)| {
            let selected = index == selected_index;
            dropdown_row(
                theme,
                selected,
                &format!("/{:<15}", cmd.usage()),
                cmd.hint(),
                row_width,
            )
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(
            format!(" /{} ", query),
            Style::new().fg(theme.muted_color()),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// One row of a small dropdown: no leading arrow; selection wash runs
/// the full inner width so the bar reaches the right border.
fn dropdown_row(
    theme: &Theme,
    selected: bool,
    label: &str,
    hint: &str,
    row_width: usize,
) -> Line<'static> {
    let label_part = truncate(&format!(" {label} "), row_width);
    let hint_space = row_width.saturating_sub(label_part.width());
    let hint_part = if hint_space == 0 {
        String::new()
    } else {
        format!("{} ", truncate(hint, hint_space - 1))
    };
    let used = label_part.width() + hint_part.width();
    let pad = " ".repeat(row_width.saturating_sub(used));

    let (label_style, hint_style, pad_style) = if selected {
        let selection = theme.selection();
        (selection, selection, selection)
    } else {
        (
            Style::new(),
            Style::new().fg(theme.muted_color()),
            Style::new(),
        )
    };
    Line::from(vec![
        Span::styled(label_part, label_style),
        Span::styled(hint_part, hint_style),
        Span::styled(pad, pad_style),
    ])
}

// -------------------------------------------------------------- overlays

fn draw_help(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    const COLUMN_WIDTH: usize = 40;
    const WIDE_WIDTH: u16 = COLUMN_WIDTH as u16 * 2 + 7;
    const NARROW_WIDTH: u16 = 58;

    let wide = area.width >= WIDE_WIDTH;
    let width = if wide {
        WIDE_WIDTH
    } else {
        NARROW_WIDTH.min(area.width)
    };
    let mut lines = wordmark_lines(theme, width);
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    let row_style = |heading| {
        if heading {
            theme.accent_text().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        }
    };
    if wide {
        for banner::HelpRow {
            left,
            right,
            heading,
        } in banner::HELP_COLUMNS
        {
            let style = row_style(heading);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{left:<COLUMN_WIDTH$}"), style),
                Span::styled(right, style),
            ]));
        }
        lines.push(Line::raw(""));
    } else {
        // Stack the paired sections only when two readable columns do not fit.
        for side in 0..2 {
            for banner::HelpRow {
                left,
                right,
                heading,
            } in banner::HELP_COLUMNS
            {
                let text = if side == 0 { left } else { right };
                if text.is_empty() {
                    lines.push(Line::raw(""));
                    continue;
                }
                let prefix = if heading { "" } else { "  " };
                lines.push(Line::styled(format!("{prefix}{text}"), row_style(heading)));
            }
            lines.push(Line::raw(""));
        }
    }
    let store = format!("Data store: {}", app.data_dir().display());
    lines.push(
        Line::styled(
            truncate(&store, width.saturating_sub(4) as usize),
            Style::new().fg(theme.muted_color()),
        )
        .centered(),
    );
    lines.push(Line::styled(banner::HELP_FOOTER, theme.accent_text()).centered());

    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let rect = centered(area, width, height);
    let viewport = rect.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(viewport);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let title = Line::from(vec![
        Span::raw(" mach "),
        Span::styled(
            format!("v{} ", crate::VERSION),
            Style::new().fg(theme.muted_color()),
        ),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .title(title)
        .border_style(theme.accent_text())
        .padding(ratatui::widgets::Padding::horizontal(1));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.help_scroll.min(u16::MAX as usize) as u16, 0)),
        rect,
    );
}

fn draw_settings(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in SETTINGS_ITEMS.iter().enumerate() {
        let selected = i == app.settings_index;
        let value = app.setting_value(i);
        let marker = if selected { "❯ " } else { "  " };
        let name_style = if selected {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, theme.accent_text()),
            Span::styled(format!("{item:<14}"), name_style),
            Span::styled(value, theme.accent_text()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "↑↓ select · ←→ change · Esc close",
        Style::new().fg(theme.muted_color()),
    ));

    let width = 48.min(area.width);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let rect = centered(area, width, height);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .title(Line::from(" Settings "))
        .border_style(theme.accent_text())
        .padding(ratatui::widgets::Padding::horizontal(2));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_labels(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let editing = app.label_editor.is_some();
    let width = 48.min(area.width);
    let content_width = width.saturating_sub(4);
    let flow = label_flow_layout(&app.labels, content_width);
    let flow_rows = flow.last().map_or(1, |(_, rect)| rect.y.saturating_add(1));
    let desired_rows = flow_rows.clamp(3, 10);
    let height = desired_rows
        .saturating_add(if editing { 10 } else { 2 })
        .min(area.height);
    let rect = centered(area, width, height);
    let hint = if let Some(error) = &app.label_error {
        Line::styled(
            format!(" {} ", truncate(error, width.saturating_sub(4) as usize)),
            Style::new()
                .fg(theme.error_color())
                .add_modifier(Modifier::BOLD),
        )
    } else if editing {
        Line::styled(" Ctrl+S save ", Style::new().fg(theme.muted_color()))
    } else {
        Line::styled(
            " Ctrl+A new · Backspace delete ",
            Style::new().fg(theme.muted_color()),
        )
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .title(Span::styled(" Labels ", theme.accent_text().bold()))
        .title_bottom(hint.right_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (list_area, input_area) = if editing {
        let [list, input] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(8)]).areas(inner);
        (list, Some(input))
    } else {
        (inner, None)
    };

    if app.labels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                "No labels yet",
                Style::new().fg(theme.muted_color()),
            )),
            list_area,
        );
    } else {
        let selected = app.label_index.min(app.labels.len() - 1);
        let visible_rows = list_area.height;
        let selected_row = flow.get(selected).map_or(0, |(_, badge)| badge.y);
        let start_row = selected_row
            .saturating_add(1)
            .saturating_sub(visible_rows)
            .min(flow_rows.saturating_sub(visible_rows));
        for (index, (name, badge)) in flow.iter().enumerate() {
            if badge.y < start_row || badge.y >= start_row.saturating_add(visible_rows) {
                continue;
            }
            let screen = Rect {
                x: list_area.x.saturating_add(badge.x),
                y: list_area.y.saturating_add(badge.y - start_row),
                width: badge.width,
                height: 1,
            };
            app.areas.label_hits.push((index, screen));
            let style = if index == selected {
                theme.label_focus()
            } else {
                theme.label_badge(app.labels[index].color, false)
            };
            f.render_widget(
                Paragraph::new(Line::styled(format!(" {name} "), style)),
                screen,
            );
        }
        paint_scrollbar(
            f,
            theme,
            rect,
            flow_rows as usize,
            visible_rows as usize,
            start_row as usize,
            true,
            1,
        );
    }

    if let Some(input_area) = input_area
        && let Some(editor) = &mut app.label_editor
    {
        let label = if editor.editing_id.is_some() {
            "Edit label"
        } else {
            "New label"
        };
        let editor_inner = render_field_box(
            f,
            field_block(label, true, None, theme).padding(Padding::ZERO),
            input_area,
        );
        let [name_box, color_box] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).areas(editor_inner);
        let name_area = render_field_box(
            f,
            field_block("Name", !editor.color_focused, None, theme),
            name_box,
        );
        let color_area = render_field_box(
            f,
            field_block("Color", editor.color_focused, None, theme),
            color_box,
        );
        app.areas.label_name_input = name_area;
        draw_text_input(
            f,
            &mut editor.name,
            name_area,
            "name without #",
            !editor.color_focused,
            theme,
        );

        const SWATCH_SLOT_WIDTH: u16 = 3;
        let palette_width = SWATCH_SLOT_WIDTH * LabelColor::SWATCHES.len() as u16;
        if color_area.width >= palette_width {
            let ring_style = if editor.color_focused {
                theme.accent_text().bold()
            } else {
                Style::new().fg(theme.muted_color())
            };
            let mut x = color_area
                .x
                .saturating_add(color_area.width.saturating_sub(palette_width) / 2);
            for color in LabelColor::SWATCHES {
                let slot = Rect {
                    x,
                    y: color_area.y,
                    width: SWATCH_SLOT_WIDTH,
                    height: 1,
                };
                app.areas.label_color_hits.push((color, slot));
                let selected = color == editor.color;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(if selected { "[" } else { " " }, ring_style),
                        Span::styled("■", theme.label_swatch(color)),
                        Span::styled(if selected { "]" } else { " " }, ring_style),
                    ])),
                    slot,
                );
                x = x.saturating_add(SWATCH_SLOT_WIDTH);
            }
        }
    }
}

fn label_flow_layout(labels: &[crate::model::Label], width: u16) -> Vec<(String, Rect)> {
    if width == 0 {
        return Vec::new();
    }
    let mut x: u16 = 0;
    let mut y: u16 = 0;
    labels
        .iter()
        .map(|label| {
            let name = truncate(&label.name, width.saturating_sub(2) as usize);
            let badge_width = u16::try_from(name.width())
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .min(width);
            if x > 0 && x.saturating_add(1).saturating_add(badge_width) > width {
                x = 0;
                y = y.saturating_add(1);
            } else if x > 0 {
                x = x.saturating_add(1);
            }
            let rect = Rect {
                x,
                y,
                width: badge_width,
                height: 1,
            };
            x = x.saturating_add(badge_width);
            (name, rect)
        })
        .collect()
}

fn draw_welcome(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let mut lines = wordmark_lines(theme, area.width);
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.push(
        Line::styled(
            format!("Welcome to mach v{}", crate::VERSION),
            Style::new().add_modifier(Modifier::BOLD),
        )
        .centered(),
    );
    lines.push(Line::raw(""));
    lines.push(Line::raw("Written in Rust with ratatui.").centered());
    let storage = format!("Your tasks stay local in {}.", app.data_dir().display());
    lines.push(Line::raw(storage.clone()).centered());
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            "Press Enter to start · /help for the key list",
            Style::new().fg(theme.muted_color()),
        )
        .centered(),
    );

    let width = u16::try_from(storage.width())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .max(50)
        .min(area.width);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let rect = centered(area, width, height);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text());
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn wordmark_lines(theme: &Theme, available_width: u16) -> Vec<Line<'static>> {
    if available_width < banner::BANNER_WIDTH + 8 {
        return Vec::new();
    }
    banner::BANNER
        .iter()
        .map(|row| Line::styled(*row, theme.accent_text()).centered())
        .collect()
}

fn draw_whats_new(f: &mut Frame, theme: &Theme, area: Rect) {
    const OVERLAY_WIDTH: u16 = 62;
    const BULLET_PREFIX: &str = "• ";
    const DESCRIPTION_PREFIX: &str = "  ";
    const RELEASE_NOTES_LABEL: &str = "Full release notes:";
    const CONTINUE_HINT: &str = "Press Enter or Esc to continue";

    let heading = format!("What's new in mach v{}", crate::VERSION);
    let release_url = format!("github.com/Q1CHENL/mach/releases/tag/v{}", crate::VERSION);
    let width = OVERLAY_WIDTH.min(area.width);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.accent_text())
        .padding(Padding::horizontal(2));
    let content_width = usize::from(block.inner(Rect::new(0, 0, width, area.height)).width);
    let description_width = content_width.saturating_sub(DESCRIPTION_PREFIX.width());

    let mut lines = vec![
        Line::styled(heading, Style::new().add_modifier(Modifier::BOLD)).centered(),
        Line::raw(""),
    ];
    for (index, (title, description)) in banner::WHATS_NEW.into_iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(BULLET_PREFIX, theme.accent_text()),
            Span::styled(title, Style::new().add_modifier(Modifier::BOLD)),
        ]));
        let graphemes = description
            .graphemes(true)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.extend(
            crate::text_input::wrap_breaks(&graphemes, description_width)
                .into_iter()
                .map(|(start, end)| {
                    let text = graphemes[start..end].concat();
                    Line::raw(format!("{DESCRIPTION_PREFIX}{}", text.trim_end()))
                }),
        );
        if index + 1 < banner::WHATS_NEW.len() {
            lines.push(Line::raw(""));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(RELEASE_NOTES_LABEL, Style::new().fg(theme.muted_color())).centered());
    lines.push(Line::styled(release_url, Style::new().fg(theme.muted_color())).centered());
    lines.push(Line::styled(CONTINUE_HINT, Style::new().fg(theme.muted_color())).centered());

    if lines.len().saturating_add(2) > usize::from(area.height) {
        lines.retain(|line| line.width() > 0);
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let rect = centered(area, width, height);
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// ----------------------------------------------------------------- utils

fn draw_box(f: &mut Frame, area: Rect, text: &str, style: Style) {
    let width = u16::try_from(text.width())
        .unwrap_or(u16::MAX)
        .saturating_add(8)
        .min(area.width);
    let rect = centered(area, width, 3);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(style);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(Line::styled(text.to_string(), style))
            .centered()
            .block(block),
        rect,
    );
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x.saturating_add((area.width - width) / 2),
        y: area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    }
}

/// Cut a string to a display width without splitting a grapheme cluster.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let w = grapheme.width();
        if used + w > width {
            break;
        }
        used += w;
        out.push_str(grapheme);
    }
    out
}
