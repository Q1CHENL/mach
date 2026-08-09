//! Every screen is drawn once against an in-memory terminal.
//!
//! The drawing code does its own arithmetic on rects, widths and scroll
//! offsets, so a bad subtraction panics rather than misdraws. These
//! render each mode at a few sizes — including ones barely big enough —
//! and assert the frame comes out with something recognisable on it.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use mach::app::{App, Focus, Mode};
use mach::form::CategoryForm;
use mach::model::{Block, Category, Task};
use mach::store::Store;
use mach::ui;

/// Draw `app` at `width` x `height` and hand back the finished cells.
fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| ui::draw(frame, app))
        .expect("draw must not panic");
    terminal.backend().buffer().clone()
}

/// The screen as text, one line per row.
fn render(app: &mut App, width: u16, height: u16) -> String {
    let buffer = draw(app, width, height);
    (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// Whether the cells spelling out `needle` are all bold — how the help
/// page marks a section heading. Panics if the text is not on screen.
fn is_bold(buffer: &Buffer, needle: &str) -> bool {
    let (x0, y) = find_cells(buffer, needle);
    (x0..x0 + needle.chars().count() as u16)
        .map(|x| &buffer[(x, y)])
        .filter(|cell| !cell.symbol().trim().is_empty())
        .all(|cell| cell.modifier.contains(Modifier::BOLD))
}

/// Top-left cell of the run spelling out `needle`.
fn find_cells(buffer: &Buffer, needle: &str) -> (u16, u16) {
    let width = needle.chars().count() as u16;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width.saturating_sub(width) {
            let run: String = (x..x + width).map(|x| buffer[(x, y)].symbol()).collect();
            if run == needle {
                return (x, y);
            }
        }
    }
    panic!("{needle:?} is not on screen:\n{}", buffer_text(buffer));
}

fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An app with two categories and a few tasks, backed by an independent
/// in-memory store for each test.
fn sample_app() -> App {
    let mut store = Store::open_in_memory_with_paths(
        std::env::temp_dir().join(format!("mach-render-test-{}", uuid::Uuid::new_v4())),
    )
    .unwrap();
    let categories = vec![
        Category {
            id: "c-work".into(),
            name: "Work".into(),
            description: "what pays".into(),
        },
        Category {
            id: "c-home".into(),
            name: "Home".into(),
            description: String::new(),
        },
    ];
    let mut flagged = Task::new(
        "ship the release",
        3,
        Some("c-work".into()),
        "2030-01-02 09:00",
    );
    flagged.body = vec![
        Block::text("with a note"),
        Block::todo("step one", true),
        Block::todo("step two", false),
        Block::link("https://example.com"),
        Block::bullet("a point"),
        Block::number("first"),
    ];
    let mut done = Task::new("已完成的任务", 0, Some("c-home".into()), "");
    done.done = true;
    let tasks = vec![flagged, done, Task::new("no category", 1, None, "09:00")];
    store
        .update(|data| {
            data.categories = categories;
            data.tasks = tasks;
            Ok(())
        })
        .unwrap();
    let mut app = App::with_store("test", store).unwrap();
    // A fresh store has no recorded version, so the welcome splash is up;
    // tests that want it set the mode themselves.
    app.mode = Mode::Normal;
    app
}

#[test]
fn draws_the_main_screen() {
    let mut app = sample_app();
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("Categories"), "{screen}");
    assert!(screen.contains("Tasks"), "{screen}");
    assert!(screen.contains("ship the release"), "{screen}");
    assert!(screen.contains("Work"), "{screen}");
    // Flags, tick marks and the due date all share the row.
    assert!(screen.contains("[✓]") && screen.contains("[ ]"), "{screen}");
    assert!(screen.contains("⚑"), "{screen}");
}

#[test]
fn draws_every_overlay() {
    for (mode, expect) in [
        (Mode::Help, "MOVING AROUND"),
        (Mode::Settings, "Sort"),
        (Mode::Welcome, "Welcome to mach"),
        (Mode::WhatsNew, "What's new in mach"),
        (Mode::Slash, "search tasks"),
    ] {
        let mut app = sample_app();
        app.mode = mode;
        let screen = render(&mut app, 100, 30);
        assert!(
            screen.contains(expect),
            "{mode:?} missing {expect:?}:\n{screen}"
        );
    }
}

#[test]
fn help_marks_both_section_headings() {
    let mut app = sample_app();
    app.mode = Mode::Help;
    let buffer = draw(&mut app, 110, 40);
    // "COMMANDS  (press /)" has lowercase inside it, so a heading cannot
    // be recognised by its casing — it has to be marked as one.
    assert!(is_bold(&buffer, "MOVING AROUND"), "first heading not bold");
    assert!(
        is_bold(&buffer, "TASKS & CATEGORIES"),
        "first heading not bold"
    );
    assert!(
        is_bold(&buffer, "COMMANDS  (press /)"),
        "second heading not bold"
    );
    assert!(is_bold(&buffer, "TASK PREVIEW"), "second heading not bold");
    // An ordinary row is plain, so the checks above mean something.
    assert!(!is_bold(&buffer, "/quit"), "body row should not be bold");
}

#[test]
fn draws_the_task_dialog_with_a_body() {
    let mut app = sample_app();
    app.open_edit_task();
    // Tall enough that the docked preview shows the full body stack.
    let screen = render(&mut app, 100, 48);
    assert!(screen.contains("Edit task"), "{screen}");
    assert!(
        screen.contains("Title") && screen.contains("Due"),
        "{screen}"
    );
    assert!(screen.contains("ship the release"), "{screen}");
    // Body blocks keep their markers (docked under the task list).
    assert!(screen.contains("[✓] step one"), "{screen}");
    assert!(screen.contains("• a point"), "{screen}");
    assert!(screen.contains("1. first"), "{screen}");
    assert!(screen.contains("↗ https://example.com"), "{screen}");
    assert!(
        screen.contains("Task preview") || screen.contains("Esc list"),
        "{screen}"
    );
}

#[test]
fn narrow_side_preview_uses_a_stacked_in_place_editor() {
    let mut app = sample_app();
    app.settings.preview_position = "right".into();
    app.open_edit_task();

    let buffer = draw(&mut app, 90, 30);
    let preview = app.areas.preview;
    let (edit_x, _) = find_cells(&buffer, " Edit task ");
    assert!(
        edit_x >= preview.x && edit_x < preview.right(),
        "editor should stay inside the task preview: preview={preview:?}\n{}",
        buffer_text(&buffer)
    );

    let (_, title_y) = find_cells(&buffer, " Title ");
    let (_, category_y) = find_cells(&buffer, " Category ");
    let (_, due_y) = find_cells(&buffer, " Due ");
    let (_, flags_y) = find_cells(&buffer, " Flags ");
    assert!(
        title_y < category_y && category_y < due_y && due_y < flags_y,
        "compact metadata fields should stack vertically\n{}",
        buffer_text(&buffer)
    );
}

#[test]
fn modal_task_editor_keeps_the_task_preview_visible_behind_it() {
    let mut app = sample_app();
    app.settings.preview_position = "right".into();
    app.tasks[0].title = format!("{}UNDERLAY", " ".repeat(32));
    app.invalidate_preview();
    app.rebuild_view();
    app.open_edit_task();

    let buffer = draw(&mut app, 120, 16);
    let preview = app.areas.preview;
    let (edit_x, _) = find_cells(&buffer, " Edit task ");
    assert!(edit_x < preview.x, "fixture should use the modal fallback");

    // The modal ends before the right edge. A marker placed at the far end
    // of the underlying preview title must remain visible beside it.
    let edge_start = buffer.area.width.saturating_sub(14);
    let mut right_edge = String::new();
    for y in 0..buffer.area.height {
        for x in edge_start..buffer.area.width {
            right_edge.push_str(buffer[(x, y)].symbol());
        }
        right_edge.push('\n');
    }
    assert!(
        right_edge.contains("UNDERLAY"),
        "modal fallback should preserve the task preview\n{}",
        buffer_text(&buffer)
    );
}

#[test]
fn draws_the_new_task_dialog_and_its_calendar() {
    let mut app = sample_app();
    app.select_category(1);
    app.open_new_task();
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("New task"), "{screen}");
    assert!(
        screen.contains("Category") && screen.contains("Work"),
        "{screen}"
    );

    app.form.as_mut().unwrap().open_due_picker();
    let screen = render(&mut app, 100, 30);
    assert!(
        screen.contains("Su") && screen.contains("Sa"),
        "weekday header:\n{screen}"
    );
    assert!(screen.contains("clear(x)"), "{screen}");
}

#[test]
fn draws_the_category_dialog() {
    let mut app = sample_app();
    app.focus = Focus::Sidebar;
    app.select_category(1);
    app.open_edit_category();
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("Edit category"), "{screen}");
    assert!(
        screen.contains("Work") && screen.contains("what pays"),
        "{screen}"
    );

    app.category_form = Some(CategoryForm::new());
    assert!(render(&mut app, 100, 30).contains("New category"));
}

#[test]
fn draws_search_and_its_empty_result() {
    let mut app = sample_app();
    app.start_search("ship");
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("ship the release"), "{screen}");
    assert!(
        !screen.contains("no category"),
        "other tasks filtered out:\n{screen}"
    );

    app.start_search("nothing matches this");
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("No tasks found"), "{screen}");
}

#[test]
fn draws_an_empty_store() {
    let mut app = sample_app();
    app.tasks.clear();
    app.rebuild_view();
    assert!(render(&mut app, 100, 30).contains("No active tasks"));
}

#[test]
fn survives_sizes_from_tiny_to_wide() {
    // Below the floor the UI says so instead of laying anything out.
    let mut app = sample_app();
    assert!(render(&mut app, 20, 5).contains("too small"));
    assert!(render(&mut app, 30, 8).contains("60×16"));

    // At and above the honest floor every mode remains operable.
    for (width, height) in [(60, 16), (80, 24), (200, 60)] {
        for mode in [
            Mode::Normal,
            Mode::Help,
            Mode::Settings,
            Mode::Welcome,
            Mode::WhatsNew,
            Mode::Slash,
            Mode::TaskForm,
            Mode::CategoryForm,
        ] {
            let mut app = sample_app();
            match mode {
                Mode::TaskForm => app.open_edit_task(),
                Mode::CategoryForm => {
                    app.select_category(1);
                    app.open_edit_category();
                }
                _ => app.mode = mode,
            }
            render(&mut app, width, height);
        }
    }
}

#[test]
fn minimum_size_keeps_both_primary_panels_visible() {
    let mut app = sample_app();
    let screen = render(&mut app, 60, 16);
    assert!(screen.contains("Categories"), "{screen}");
    assert!(screen.contains("Tasks"), "{screen}");
    assert!(screen.contains("ship the re"), "{screen}");
}

#[test]
fn wide_help_uses_paired_columns_and_content_height() {
    let mut app = sample_app();
    app.mode = Mode::Help;
    let buffer = draw(&mut app, 120, 40);

    let (_, title_y) = find_cells(&buffer, " mach ");
    let (_, moving_y) = find_cells(&buffer, "MOVING AROUND");
    let (_, tasks_y) = find_cells(&buffer, "TASKS & CATEGORIES");

    assert_eq!(moving_y, tasks_y, "wide help sections must share a row");
    assert!(title_y > 0, "wide help must not fill the terminal height");
}

#[test]
fn narrow_help_scrolls_to_every_section() {
    let mut app = sample_app();
    app.mode = Mode::Help;
    let top = render(&mut app, 80, 24);
    assert!(top.contains("MOVING AROUND"), "{top}");

    app.help_scroll = usize::MAX;
    let bottom = render(&mut app, 80, 24);
    assert!(bottom.contains("PREVIEW"), "{bottom}");
    assert!(bottom.contains("github.com/Q1CHENL/mach"), "{bottom}");
}

#[test]
fn hidden_geometry_is_cleared_on_an_undersized_frame() {
    let mut app = sample_app();
    let _ = render(&mut app, 100, 30);
    assert!(!app.areas.tasks.is_empty());
    assert!(!app.areas.command_bar.is_empty());

    let _ = render(&mut app, 20, 5);
    assert!(app.areas.tasks.is_empty());
    assert!(app.areas.sidebar.is_empty());
    assert!(app.areas.command_bar.is_empty());
    assert!(app.areas.slash_menu.is_empty());
}

#[test]
fn a_clipped_read_only_preview_says_that_more_content_exists() {
    let mut app = sample_app();
    app.tasks[0].body = (0..40)
        .map(|index| Block::text(&format!("preview line {index}")))
        .collect();
    app.invalidate_preview();
    app.rebuild_view();

    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("↓ more · Enter to edit"), "{screen}");
}

#[test]
fn survives_every_sort_order() {
    for sort in mach::settings::SORTS {
        let mut app = sample_app();
        app.settings.sort = sort.to_string();
        app.rebuild_view();
        assert!(
            render(&mut app, 100, 30).contains("ship the release"),
            "sort {sort}"
        );
    }
}

// ------------------------------------------------------- picture geometry

/// A picture is letterboxed into its box, never cropped.
///
/// The size is worked out in cells and the terminal is then told to draw
/// `cells × cell_size` pixels there, so this has to come out to a rect that
/// fits — anything larger is clipped by the terminal, which reads as the
/// right and bottom of the picture being cut off.
#[test]
fn a_picture_is_letterboxed_into_its_box_never_cropped() {
    use image::DynamicImage;
    use ratatui::layout::{Rect, Size};
    use ratatui_image::picker::Picker;
    use ratatui_image::{FontSize, Resize};

    let cell = FontSize::new(8, 16);
    // Wider than any box below, so a crop would be obvious.
    let (iw, ih) = (1873u32, 1050u32);
    #[allow(deprecated, reason = "the only way to pin a cell size for a test")]
    let picker = Picker::from_fontsize(cell);
    let protocol = picker.new_resize_protocol(DynamicImage::new_rgba8(iw, ih));

    for box_size in [
        Size::new(128, 55),
        Size::new(120, 50),
        Size::new(200, 60),
        Size::new(60, 40),
        Size::new(10, 4),
    ] {
        let fitted = protocol.size_for(Resize::Scale(None), box_size);
        let area = Rect {
            x: 0,
            y: 0,
            width: box_size.width,
            height: box_size.height,
        };
        let picture = ui::centered(
            area,
            fitted.width.min(area.width),
            fitted.height.min(area.height),
        );

        assert!(
            picture.width <= area.width && picture.height <= area.height,
            "{box_size:?}: picture {picture:?} spills out of its box"
        );
        assert!(
            picture.right() <= area.right() && picture.bottom() <= area.bottom(),
            "{box_size:?}: picture {picture:?} would be clipped"
        );

        // Aspect ratio survives, to within the cell the size is rounded up to.
        let want_h = (box_size.width as f32 * cell.width as f32 * ih as f32)
            / (iw as f32 * cell.height as f32);
        let capped = want_h.min(box_size.height as f32);
        assert!(
            (picture.height as f32 - capped).abs() <= 1.5,
            "{box_size:?}: {} rows, expected about {capped:.1}",
            picture.height
        );
    }
}

#[test]
fn categories_scrollbar_track_uses_accent_when_focused() {
    let mut app = sample_app();
    app.focus = Focus::Sidebar;
    for i in 0..40 {
        app.categories.push(Category {
            id: format!("c-{i}"),
            name: format!("Cat{i}"),
            description: String::new(),
        });
    }
    app.rebuild_view();
    let buffer = draw(&mut app, 100, 20);
    let accent = app.theme().accent;
    // Right border column of the Categories panel (SIDEBAR_WIDTH = 26).
    let x = mach::ui::SIDEBAR_WIDTH - 1;
    let mut found_track = false;
    for y in 2..18 {
        let cell = &buffer[(x, y)];
        let sym = cell.symbol();
        if matches!(sym, "│" | "┃" | "║") {
            found_track = true;
            assert_eq!(
                cell.style().fg,
                Some(accent),
                "track at ({x},{y}) should be accent, got {:?} sym={sym:?}",
                cell.style().fg
            );
        }
        if sym == "█" || sym == "▐" {
            assert_eq!(cell.style().fg, Some(accent), "thumb should be accent");
        }
    }
    assert!(
        found_track,
        "expected a scrollbar track on the categories border"
    );
}

#[test]
fn preview_can_sit_on_the_right() {
    let mut app = sample_app();
    app.settings.preview_position = "right".into();
    // Wide + tall enough for a side preview.
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("Task preview"), "{screen}");
    assert!(screen.contains("ship the release"), "{screen}");
    // Tasks and Task preview titles should appear on the same band (top),
    // not only with Task preview under a short task list.
    let lines: Vec<&str> = screen.lines().collect();
    let tasks_y = lines
        .iter()
        .position(|l| l.contains("Tasks"))
        .expect("Tasks");
    let preview_y = lines
        .iter()
        .position(|l| l.contains("Task preview"))
        .expect("Task preview");
    assert!(
        preview_y.abs_diff(tasks_y) <= 2,
        "preview should be beside tasks, not far below: tasks={tasks_y} preview={preview_y}\n{screen}"
    );
}
