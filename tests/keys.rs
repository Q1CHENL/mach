//! Key and mouse handling, driven through the real event entry point.

use std::process::Command;

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use mach::app::{App, Confirm, Focus, Mode};
use mach::form::{TaskDraft, TaskForm};
use mach::input::handle_event;
use mach::model::{Block, Category, LabelColor, Task};
use mach::store::Store;

mod common;
use common::TempDir;
#[path = "common/render.rs"]
mod render_common;
use render_common::{buffer_text, draw, find_cells, move_mouse};

fn app() -> App {
    let mut store = Store::open_in_memory_with_paths(
        std::env::temp_dir().join(format!("mach-keys-test-{}", uuid::Uuid::new_v4())),
    )
    .unwrap();
    let categories = vec![
        Category {
            id: "c-work".into(),
            name: "Work".into(),
            description: String::new(),
        },
        Category {
            id: "c-home".into(),
            name: "Home".into(),
            description: String::new(),
        },
    ];
    let tasks = vec![
        Task::new("first", 0, Some("c-work".into()), ""),
        Task::new("second", 0, Some("c-work".into()), ""),
        Task::new("third", 0, Some("c-home".into()), ""),
    ];
    store
        .update(|data| {
            data.categories = categories;
            data.tasks = tasks;
            data.settings.sort = "manual".into();
            Ok(())
        })
        .unwrap();
    let mut app = App::with_store("test", store).unwrap();
    // A fresh store has no recorded version, so the welcome splash is up
    // and would swallow the first key of every test.
    app.mode = Mode::Normal;
    app
}

fn overflowing_app() -> App {
    let mut store = Store::open_in_memory_with_paths(
        std::env::temp_dir().join(format!("mach-keys-overflow-test-{}", uuid::Uuid::new_v4())),
    )
    .unwrap();
    let categories = (0..20)
        .map(|index| Category {
            id: format!("c-{index:02}"),
            name: format!("Category {index:02}"),
            description: String::new(),
        })
        .collect::<Vec<_>>();
    let last_category = categories.last().unwrap().id.clone();
    let tasks = (0..20)
        .map(|index| {
            let title = format!("task {index:02}");
            Task::new(&title, 0, Some(last_category.clone()), "")
        })
        .collect::<Vec<_>>();
    store
        .update(|data| {
            data.categories = categories;
            data.tasks = tasks;
            data.settings.sort = "manual".into();
            Ok(())
        })
        .unwrap();
    let mut app = App::with_store("test", store).unwrap();
    app.mode = Mode::Normal;
    app
}

struct FileApp {
    app: App,
    dir: TempDir,
}

fn file_app() -> FileApp {
    let dir = TempDir::new("keys-file-test");
    let store = Store::open(dir.path()).unwrap();
    let mut app = App::with_store("test", store).unwrap();
    app.mode = Mode::Normal;
    FileApp { app, dir }
}

#[test]
fn settings_can_enable_essential_hints_and_persist_the_choice() {
    let mut fixture = file_app();
    fixture.app.mode = Mode::Settings;
    fixture.app.settings_index = 4;

    press(&mut fixture.app, KeyCode::Right, KeyModifiers::NONE);

    assert_eq!(fixture.app.settings.hint_level, "essential");
    let persisted = Store::open(fixture.dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(persisted.settings.hint_level, "essential");
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_event(app, Event::Key(KeyEvent::new(code, modifiers)));
}

fn run_slash_command(app: &mut App, command: &str) {
    press(app, KeyCode::Char('/'), KeyModifiers::NONE);
    for c in command.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

fn click(app: &mut App, column: u16, row: u16) {
    handle_event(
        app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

fn release_click(app: &mut App, column: u16, row: u16) {
    handle_event(
        app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

fn repeat(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_event(
        app,
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        }),
    );
}

fn ctrl_c(app: &mut App) {
    press(app, KeyCode::Char('c'), KeyModifiers::CONTROL);
}

fn message(app: &App) -> String {
    app.message
        .as_ref()
        .map(|m| m.text.clone())
        .unwrap_or_default()
}

#[test]
fn ignored_terminal_events_do_not_request_a_redraw() {
    let mut app = app();
    let release = KeyEvent {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    };

    assert!(!handle_event(&mut app, Event::Key(release)));
    assert!(!handle_event(&mut app, Event::FocusGained));
    assert!(!handle_event(&mut app, Event::FocusLost));
    assert!(!handle_event(&mut app, Event::Paste(String::new())));
    assert!(!handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
    ));
}

#[test]
fn stateful_terminal_events_request_a_redraw() {
    let mut app = app();

    assert!(handle_event(&mut app, Event::Resize(120, 40)));
    assert!(handle_event(
        &mut app,
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    ));
}

#[test]
fn mouse_motion_redraws_only_when_the_hover_target_changes() {
    let mut app = app();
    let buffer = draw(&mut app, 100, 30);
    let (title_x, row) = find_cells(&buffer, "first");
    assert!(
        app.areas.tasks.contains((title_x, row).into()),
        "the test task must be in the clickable task list"
    );
    let selected = app.task_index;
    let focus = app.focus;
    let done = app.selected_task().unwrap().done;

    assert!(
        move_mouse(&mut app, title_x, row),
        "entering a task redraws"
    );
    assert!(
        !move_mouse(&mut app, title_x + 1, row),
        "motion within the same task target is free"
    );

    let done_x = app.areas.done_x.unwrap();
    assert!(
        !move_mouse(&mut app, done_x + 1, row),
        "one task row is one visual hover target"
    );
    assert!(move_mouse(&mut app, 0, 0), "leaving every target redraws");
    assert!(!move_mouse(&mut app, 0, 1), "empty-space motion is free");

    assert!(move_mouse(&mut app, title_x, row));
    app.mode = Mode::Help;
    let _ = draw(&mut app, 100, 30);
    assert!(
        !move_mouse(&mut app, title_x + 1, row),
        "a new frame must re-resolve a stationary pointer against its final targets"
    );

    assert_eq!(app.task_index, selected, "hover cannot change selection");
    assert_eq!(app.focus, focus, "hover cannot take keyboard focus");
    assert_eq!(
        app.selected_task().unwrap().done,
        done,
        "hover cannot invoke the target"
    );
}

#[test]
fn form_chrome_stays_unpainted_while_due_dates_are_individual_targets() {
    let mut app = app();
    app.select_category(1);
    app.tasks
        .iter_mut()
        .find(|task| task.title == "first")
        .unwrap()
        .due = "2030-01-02".into();
    app.open_edit_task();
    let _ = draw(&mut app, 100, 40);
    let title = app.form.as_ref().unwrap().areas.title;
    let category = app.form.as_ref().unwrap().areas.category;
    let field = app.form.as_ref().unwrap().field;

    assert!(move_mouse(&mut app, title.x, title.y));
    assert!(!move_mouse(&mut app, title.x.saturating_add(1), title.y));
    assert!(
        !move_mouse(&mut app, category.x, category.y),
        "form fields share one non-painting occlusion target"
    );
    assert_eq!(app.form.as_ref().unwrap().field, field);

    app.form.as_mut().unwrap().open_due_picker();
    let _ = draw(&mut app, 100, 40);
    let picker = app.form.as_ref().unwrap().picker.as_ref().unwrap();
    let days = picker.layout.days;
    let hour = picker.layout.hour;
    let minute = picker.layout.minute;
    let day = picker.day;
    let focus = picker.focus;

    assert!(
        move_mouse(&mut app, days.x, days.y),
        "entering a calendar date redraws"
    );
    assert!(
        !move_mouse(&mut app, days.x.saturating_add(1), days.y),
        "moving within picker chrome does not redraw"
    );
    assert!(
        move_mouse(&mut app, days.x.saturating_add(3), days.y),
        "each calendar date is a distinct hover target"
    );
    assert!(
        move_mouse(&mut app, hour.x, hour.y),
        "leaving a date for picker chrome removes its hover"
    );
    assert!(
        !move_mouse(&mut app, minute.x, minute.y),
        "hour and minute remain part of the non-painting picker chrome"
    );
    let picker = app.form.as_ref().unwrap().picker.as_ref().unwrap();
    assert_eq!(picker.day, day);
    assert_eq!(picker.focus, focus);
}

// ------------------------------------------------------------------ quit

#[test]
fn ctrl_c_takes_two_presses_and_says_so_in_between() {
    let mut app = app();

    ctrl_c(&mut app);
    assert!(!app.should_quit, "one press must not quit");
    assert!(
        message(&app).contains("again to quit"),
        "the status bar has to say a second press is armed, got {:?}",
        message(&app)
    );

    ctrl_c(&mut app);
    assert!(app.should_quit, "the second press quits");
}

#[test]
fn cmd_c_never_quits() {
    // ⌘C is Copy on macOS. Two of them must do nothing but try to copy.
    let mut app = app();
    press(&mut app, KeyCode::Char('c'), KeyModifiers::SUPER);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::SUPER);
    assert!(!app.should_quit);
    assert!(app.pending.is_none());
}

#[test]
fn cmd_c_without_a_selection_does_not_type_into_a_form() {
    let mut app = app();
    app.open_new_task();

    press(&mut app, KeyCode::Char('c'), KeyModifiers::SUPER);

    assert_eq!(app.form.as_ref().unwrap().title.value(), "");
}

#[test]
fn ctrl_shift_c_never_quits() {
    // Ctrl+Shift+C is Copy in many terminals. With no selection it must
    // remain a no-op instead of arming mach's Ctrl+C quit chord.
    let mut app = app();
    let copy = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    press(&mut app, KeyCode::Char('C'), copy);
    press(&mut app, KeyCode::Char('C'), copy);
    assert!(!app.should_quit);
    assert!(app.pending.is_none());
}

#[test]
fn ctrl_c_does_not_quit_out_of_a_dialog() {
    // Esc backs out of a dialog; quitting from one would throw away
    // whatever had been typed.
    for mode in [Mode::TaskForm, Mode::Slash, Mode::Search, Mode::Help] {
        let mut app = app();
        app.mode = mode;
        if mode == Mode::TaskForm {
            app.form = Some(TaskForm::new());
        }
        ctrl_c(&mut app);
        ctrl_c(&mut app);
        assert!(!app.should_quit, "{mode:?} must not quit on Ctrl+C");
    }
}

#[test]
fn any_other_key_calls_off_the_quit() {
    let mut app = app();
    ctrl_c(&mut app);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.pending, None, "the offer lapses");
    assert!(message(&app).is_empty(), "and so does the prompt");

    ctrl_c(&mut app);
    assert!(
        !app.should_quit,
        "this is a first press again, not a second"
    );
}

#[test]
fn the_quit_offer_lapses_on_its_own() {
    let mut app = app();
    ctrl_c(&mut app);
    assert!(app.awaiting(Confirm::Quit));

    // Wind the deadline back to simulate the prompt having faded.
    let (armed, _) = app.pending.expect("armed");
    app.pending = Some((armed, std::time::Instant::now()));
    assert!(!app.awaiting(Confirm::Quit), "the window has closed");

    ctrl_c(&mut app);
    assert!(
        !app.should_quit,
        "a stray Ctrl+C long after the prompt faded must re-arm, not quit"
    );
    assert!(
        app.awaiting(Confirm::Quit),
        "it counts as a fresh first press"
    );
}

#[test]
fn an_armed_delete_cannot_hide_behind_a_quit_prompt() {
    let mut app = app();
    let before = app.tasks.len();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(app.pending.is_some());

    ctrl_c(&mut app);
    assert!(app.awaiting(Confirm::Quit), "quit takes over");

    // This Backspace is a first press again — it must not land on a task.
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before, "nothing was deleted");
    assert!(app.pending.is_some());
}

#[test]
fn backspace_twice_still_deletes() {
    let mut app = app();
    let before = app.tasks.len();
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before - 1);
}

#[test]
fn key_repeat_cannot_complete_a_destructive_confirmation() {
    let mut app = app();
    let before = app.tasks.len();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    repeat(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before, "holding Backspace must not delete");
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before - 1, "a second press still confirms");

    ctrl_c(&mut app);
    repeat(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!app.should_quit, "holding Ctrl+C must not quit");
    ctrl_c(&mut app);
    assert!(app.should_quit, "a second Ctrl+C press still confirms");
}

#[test]
fn paste_cancels_a_pending_destructive_confirmation() {
    let mut app = app();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    handle_event(&mut app, Event::Paste("unrelated input".into()));

    assert!(app.pending.is_none());
    assert!(message(&app).is_empty());
}

#[test]
fn delete_confirmation_cannot_be_retargeted_to_another_task() {
    let mut app = app();
    app.select_category(0);
    app.select_task(0);
    let first = app.selected_task().unwrap().id.clone();
    let second = app.visible_task(1).unwrap().id.clone();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    app.select_task(1);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);

    assert!(app.tasks.iter().any(|task| task.id == first));
    assert!(app.tasks.iter().any(|task| task.id == second));
    assert_eq!(
        app.tasks.len(),
        3,
        "the new target must be armed, not deleted"
    );
}

#[test]
fn selection_change_clears_a_cancelled_confirmation_prompt() {
    let mut app = app();
    app.select_category(1);
    app.select_task(0);

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(message(&app).contains("again"));

    app.select_task(1);
    assert!(app.pending.is_none());
    assert!(message(&app).is_empty());
}

#[test]
fn a_non_confirmation_message_disarms_a_hidden_destructive_action() {
    let mut app = app();
    let task_id = app.selected_task().unwrap().id.clone();

    app.ask_confirm(
        Confirm::DeleteTask(task_id),
        "Press Backspace again to delete",
    );
    app.info("Update check finished");

    assert!(app.pending.is_none());
    assert_eq!(message(&app), "Update check finished");
}

#[test]
fn an_expired_confirmation_prompt_disarms_its_action() {
    let mut app = app();
    let task_id = app.selected_task().unwrap().id.clone();

    app.ask_confirm(
        Confirm::DeleteTask(task_id),
        "Press Backspace again to delete",
    );
    app.message.as_mut().unwrap().until = std::time::Instant::now();

    assert!(app.expire_message());
    assert!(app.pending.is_none());
}

#[test]
fn deleting_a_category_says_and_does_keep_its_tasks_uncategorized() {
    let mut app = app();
    app.focus = Focus::Sidebar;
    app.select_category(1);
    let work_id = app.current_category_id().to_string();
    let retained: Vec<String> = app
        .tasks
        .iter()
        .filter(|task| task.category_id.as_deref() == Some(work_id.as_str()))
        .map(|task| task.id.clone())
        .collect();
    let before = app.tasks.len();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    let prompt = message(&app).to_lowercase();
    assert!(prompt.contains("kept") && prompt.contains("uncategorized"));
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);

    assert_eq!(app.tasks.len(), before);
    for id in retained {
        assert_eq!(
            app.tasks
                .iter()
                .find(|task| task.id == id)
                .unwrap()
                .category_id,
            None
        );
    }
}

#[test]
fn purge_names_the_count_and_requires_an_explicit_second_step() {
    let mut app = app();
    app.select_category(0);
    app.select_task(0);
    app.toggle_done(0);
    let before = app.tasks.len();

    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "purge".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.tasks.len(), before, "the palette Enter only arms purge");
    assert!(
        message(&app).contains('1'),
        "the confirmation must name the count"
    );

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.tasks.len(),
        before - 1,
        "the explicit confirmation purges"
    );
}

#[test]
fn slash_done_keeps_a_store_failure_visible() {
    let mut fixture = file_app();
    let observer = rusqlite::Connection::open(fixture.dir.path().join("mach.db")).unwrap();
    observer.execute("DELETE FROM app_state", []).unwrap();

    press(&mut fixture.app, KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "done".chars() {
        press(&mut fixture.app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut fixture.app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(
        message(&fixture.app).contains("Could not update settings"),
        "the persistence error must not be replaced by a false success: {:?}",
        message(&fixture.app)
    );
}

#[test]
fn slash_hints_toggles_and_persists_the_resulting_level() {
    let mut fixture = file_app();

    run_slash_command(&mut fixture.app, "hints");

    assert_eq!(fixture.app.settings.hint_level, "essential");
    assert_eq!(message(&fixture.app), "Hints: Essential");
    let persisted = Store::open(fixture.dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(persisted.settings.hint_level, "essential");

    run_slash_command(&mut fixture.app, "hints");

    assert_eq!(fixture.app.settings.hint_level, "all");
    assert_eq!(message(&fixture.app), "Hints: All");
    let persisted = Store::open(fixture.dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(persisted.settings.hint_level, "all");
}

#[test]
fn slash_hints_keeps_a_store_failure_visible() {
    let mut fixture = file_app();
    let observer = rusqlite::Connection::open(fixture.dir.path().join("mach.db")).unwrap();
    observer.execute("DELETE FROM app_state", []).unwrap();

    run_slash_command(&mut fixture.app, "hints");

    assert_eq!(fixture.app.settings.hint_level, "all");
    assert!(
        message(&fixture.app).contains("Could not update settings"),
        "the persistence error must not be replaced by a false success: {:?}",
        message(&fixture.app)
    );
}

#[test]
fn slash_hints_rejects_arguments_without_toggling() {
    let mut fixture = file_app();

    run_slash_command(&mut fixture.app, "hints essential");

    assert_eq!(fixture.app.settings.hint_level, "all");
    assert_eq!(message(&fixture.app), "Usage: /hints");
    let persisted = Store::open(fixture.dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(persisted.settings.hint_level, "all");
}

#[test]
fn task_delete_keeps_a_store_failure_visible() {
    let mut fixture = file_app();
    fixture
        .app
        .create_task(&mach::form::TaskDraft::new("keep me"))
        .unwrap();
    let observer = rusqlite::Connection::open(fixture.dir.path().join("mach.db")).unwrap();
    observer.execute("DELETE FROM app_state", []).unwrap();

    press(&mut fixture.app, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut fixture.app, KeyCode::Backspace, KeyModifiers::NONE);

    assert!(
        message(&fixture.app).contains("Could not delete task"),
        "the persistence error must not be replaced by a false success: {:?}",
        message(&fixture.app)
    );
    assert!(fixture.app.tasks.iter().any(|task| task.title == "keep me"));
}

// ----------------------------------------------------------- type-to-jump

#[test]
fn typing_jumps_to_the_best_matching_task() {
    let mut app = app();
    app.focus = Focus::Tasks;
    app.select_category(0); // All Tasks
    app.select_task(0);
    assert_eq!(app.selected_task().map(|t| t.title.as_str()), Some("first"));

    press(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
    assert_eq!(
        app.selected_task().map(|t| t.title.as_str()),
        Some("second")
    );

    press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(
        app.selected_task().map(|t| t.title.as_str()),
        Some("second")
    );
    // List is not filtered — still every task in the view.
    assert_eq!(app.view.len(), 3);
}

#[test]
fn typing_jumps_to_the_best_matching_category() {
    let mut app = app();
    app.focus = Focus::Sidebar;
    app.select_category(0);
    assert_eq!(app.categories[app.cat_index].name, "All tasks");

    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(app.categories[app.cat_index].name, "Home");

    press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
    assert_eq!(app.categories[app.cat_index].name, "Home");
    assert_eq!(app.categories.len(), 3, "categories are not filtered");
}

#[test]
fn typing_jumps_to_the_best_matching_label() {
    let mut app = app();
    app.create_label("bug").unwrap();
    app.create_label("backend").unwrap();
    app.create_label("release").unwrap();
    app.open_labels();
    assert_eq!(
        app.selected_label().map(|label| label.name.as_str()),
        Some("bug")
    );

    press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert_eq!(
        app.selected_label().map(|label| label.name.as_str()),
        Some("release")
    );

    press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(
        app.selected_label().map(|label| label.name.as_str()),
        Some("release")
    );
    assert_eq!(app.labels.len(), 3, "labels are not filtered");
}

#[test]
fn typing_jumps_to_the_best_matching_label_in_task_form_picker() {
    let mut app = app();
    app.create_label("bug").unwrap();
    app.create_label("backend").unwrap();
    app.create_label("release").unwrap();
    app.focus = Focus::Tasks;
    press(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
    assert_eq!(
        app.selected_task().map(|task| task.title.as_str()),
        Some("second")
    );
    app.open_edit_task();
    app.form
        .as_mut()
        .unwrap()
        .set_field(mach::form::Field::Labels);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert_eq!(app.form.as_ref().unwrap().label_picker.unwrap().index, 2);

    press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(app.form.as_ref().unwrap().label_picker.unwrap().index, 2);

    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    let form = app.form.as_ref().unwrap();
    assert_eq!(form.label_picker.unwrap().index, 1);
    assert_eq!(form.label_choices().count(), 3, "labels are not filtered");
    assert!(
        form.label_ids().is_empty(),
        "typing does not toggle a label"
    );
}

#[test]
fn ctrl_a_opens_new_task_and_plain_a_does_not() {
    let mut app = app();
    app.select_category(1); // Work
    app.focus = Focus::Tasks;

    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_ne!(app.mode, Mode::TaskForm, "plain a is type-to-jump");

    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, Mode::TaskForm);
}

#[test]
fn ctrl_f_cycles_importance() {
    let mut app = app();
    app.focus = Focus::Tasks;
    app.select_category(1);
    app.select_task(0);
    assert_eq!(app.selected_task().map(|t| t.importance), Some(0));

    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert_eq!(app.selected_task().map(|t| t.importance), Some(1));
}

#[test]
fn slash_free_text_is_not_search() {
    let mut app = app();
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Slash);
    for c in "milk".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    assert!(!app.searching, "unknown slash text must not start search");
}

#[test]
fn slash_export_rejects_a_path_argument() {
    let mut app = app();
    let output = TempDir::new("keys-export-argument");
    let archive = output.path().join("not-allowed.mach");

    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for character in format!("export {}", archive.display()).chars() {
        press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(message(&app), "Usage: /export");
    assert!(!archive.exists());
}

#[test]
fn slash_import_uses_the_specified_archive_path() {
    let mut source = file_app();
    source
        .app
        .create_task(&TaskDraft::new("portable from TUI"))
        .expect("create source task");
    let archive_dir = TempDir::new("keys-import-archive");
    let archive = archive_dir.path().join("tasks.mach");
    let exported = Command::new(env!("CARGO_BIN_EXE_mach"))
        .arg("--dir")
        .arg(source.dir.path())
        .arg("export")
        .arg(&archive)
        .output()
        .expect("export archive through CLI");
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );

    let mut destination = file_app();
    let lock = rusqlite::Connection::open(destination.dir.path().join("mach.db"))
        .expect("open destination database lock");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold destination write lock");
    press(&mut destination.app, KeyCode::Char('/'), KeyModifiers::NONE);
    for character in format!("import {}", archive.display()).chars() {
        press(
            &mut destination.app,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }
    let started = std::time::Instant::now();
    press(&mut destination.app, KeyCode::Enter, KeyModifiers::NONE);
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "slash import blocked the input path for {elapsed:?}"
    );
    press(&mut destination.app, KeyCode::Char('/'), KeyModifiers::NONE);
    assert_eq!(destination.app.mode, Mode::Slash);
    press(&mut destination.app, KeyCode::Esc, KeyModifiers::NONE);
    lock.execute_batch("ROLLBACK")
        .expect("release destination write lock");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while destination.app.tasks.is_empty() && std::time::Instant::now() < deadline {
        destination.app.poll_external_changes();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        destination.app.tasks.len(),
        1,
        "{}",
        message(&destination.app)
    );
    assert_eq!(destination.app.tasks[0].title, "portable from TUI");
}

#[test]
fn click_on_panels_does_not_discard_a_dirty_task_form() {
    let mut app = app();
    lay_out(&mut app);
    app.select_category(1);
    app.open_edit_task();
    app.form.as_mut().unwrap().title.insert('!');
    assert_eq!(app.mode, Mode::TaskForm);

    handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    );
    release_click(&mut app, 30, 3);
    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.is_some());
    assert_eq!(
        message(&app),
        "Unsaved changes · press Esc to discard",
        "an outside click is the first discard request, not a repeated Esc"
    );
}

#[test]
fn modal_task_form_chrome_owns_clicks_over_the_task_panel() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_task();
    draw(&mut app, 120, 16);
    let point = ratatui::layout::Position { x: 30, y: 1 };
    assert!(
        app.areas.tasks.contains(point),
        "fixture must overlap the task panel"
    );
    assert!(
        app.form
            .as_ref()
            .unwrap()
            .areas
            .field_at(point.x, point.y)
            .is_none(),
        "fixture must hit modal chrome, not a field"
    );

    click(&mut app, point.x, point.y);

    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.is_some());
}

#[test]
fn clicking_the_visible_panel_outside_a_clean_modal_still_leaves_the_form() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_task();
    draw(&mut app, 120, 16);
    let point = ratatui::layout::Position { x: 110, y: 2 };
    assert!(
        app.areas.tasks.contains(point),
        "fixture must hit the task panel"
    );

    click(&mut app, point.x, point.y);

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.form.is_none());
}

#[test]
fn click_on_panels_does_not_discard_a_dirty_category_form() {
    let mut app = app();
    lay_out(&mut app);
    app.select_category(1);
    app.open_edit_category();
    app.category_form.as_mut().unwrap().name.insert('!');
    assert_eq!(app.mode, Mode::CategoryForm);

    handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(app.mode, Mode::CategoryForm);
    assert!(app.category_form.is_some());
    assert_eq!(message(&app), "Unsaved changes · press Esc to discard");
}

#[test]
fn modal_category_form_chrome_owns_clicks_over_the_task_panel() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_category();
    draw(&mut app, 120, 16);
    let point = ratatui::layout::Position { x: 30, y: 1 };
    assert!(
        app.areas.tasks.contains(point),
        "fixture must overlap the task panel"
    );
    let form = app.category_form.as_ref().unwrap();
    assert!(!form.name_area.contains(point));
    assert!(!form.description_area.contains(point));

    click(&mut app, point.x, point.y);

    assert_eq!(app.mode, Mode::CategoryForm);
    assert!(app.category_form.is_some());
}

#[test]
fn category_description_slash_menu_rows_are_clickable() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_category();
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    draw(&mut app, 100, 30);

    let menu_area = app
        .category_form
        .as_ref()
        .unwrap()
        .description_menu_area
        .expect("menu layout");
    assert!(
        app.category_form
            .as_ref()
            .unwrap()
            .description
            .menu
            .is_some(),
        "slash menu must be open"
    );
    handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: menu_area.x + 1,
            row: menu_area.y + 1,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(
        app.category_form
            .as_ref()
            .unwrap()
            .description
            .menu
            .is_some(),
        "scrolling over a menu row must not activate it"
    );
    click(&mut app, menu_area.x + 1, menu_area.y + 1);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);

    let form = app.category_form.as_ref().unwrap();
    assert_eq!(form.description.plain_value(), "- x");
    assert!(form.description.menu.is_none());
}

#[test]
fn escape_requires_confirmation_before_discarding_a_dirty_task_form() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_task();
    app.form.as_mut().unwrap().title.insert('!');

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::TaskForm);
    assert_eq!(
        message(&app),
        "Unsaved changes · press Esc again to discard"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn new_task_from_all_opens_with_uncategorized_selected() {
    let mut app = app();
    app.select_category(0);

    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::TaskForm);
    let form = app.form.as_ref().unwrap();
    assert_eq!(form.category_id(), None);
    assert_eq!(form.category_label(), "Uncategorized");
}

#[test]
fn task_form_category_dropdown_typeahead_moves_then_commits_the_selection() {
    let mut app = app();
    app.select_category(1);
    let id = app.selected_task().unwrap().id.clone();
    app.open_edit_task();
    let form = app.form.as_mut().unwrap();
    assert_eq!(form.category_id(), Some("c-work"));
    form.set_field(mach::form::Field::Category);

    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    let form = app.form.as_ref().unwrap();
    assert!(form.category_picker_open());
    assert_eq!(form.category_picker.unwrap().index, 2);
    assert_eq!(
        form.category_id(),
        Some("c-work"),
        "type-to-jump only moves the pending dropdown selection"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let form = app.form.as_ref().unwrap();
    assert!(!form.category_picker_open());
    assert_eq!(form.category_id(), Some("c-work"));

    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.form.as_ref().unwrap().category_picker_open());
    assert_eq!(app.form.as_ref().unwrap().category_id(), Some("c-home"));
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        app.tasks
            .iter()
            .find(|task| task.id == id)
            .unwrap()
            .category_id
            .as_deref(),
        Some("c-home")
    );
}

#[test]
fn task_form_category_dropdown_supports_click_commit_and_outside_cancel() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_task();
    draw(&mut app, 100, 30);

    let category = app.form.as_ref().unwrap().areas.category;
    click(&mut app, category.x, category.y);
    assert!(app.form.as_ref().unwrap().category_picker_open());
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);

    let title = app.form.as_ref().unwrap().areas.title;
    click(&mut app, title.x, title.y);
    let form = app.form.as_ref().unwrap();
    assert!(!form.category_picker_open());
    assert_eq!(form.category_id(), Some("c-work"));

    click(&mut app, category.x, category.y);
    draw(&mut app, 100, 30);
    let picker = app.form.as_ref().unwrap().category_picker_area().unwrap();
    click(&mut app, picker.x + 1, picker.y + 3);
    let form = app.form.as_ref().unwrap();
    assert!(!form.category_picker_open());
    assert_eq!(form.category_id(), Some("c-home"));
}

#[test]
fn task_form_label_picker_toggles_multiple_labels_without_closing() {
    let mut app = app();
    let bug = app.create_label("bug").unwrap();
    let backend = app.create_label("backend").unwrap();
    app.select_category(1);
    app.open_edit_task();
    app.form
        .as_mut()
        .unwrap()
        .set_field(mach::form::Field::Labels);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.form.as_ref().unwrap().label_picker_open());
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(app.form.as_ref().unwrap().label_picker_open());
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(app.form.as_ref().unwrap().label_picker_open());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.form.as_ref().unwrap().label_picker_open());
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.selected_task().unwrap().label_ids, vec![bug, backend]);
}

#[test]
fn task_form_label_picker_supports_click_scroll_and_outside_close() {
    let mut app = app();
    for index in 0..10 {
        app.create_label(&format!("label-{index}")).unwrap();
    }
    app.select_category(1);
    app.open_edit_task();
    app.form
        .as_mut()
        .unwrap()
        .set_field(mach::form::Field::Labels);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    draw(&mut app, 60, 16);

    let picker = app.form.as_ref().unwrap().label_picker_area().unwrap();
    click(&mut app, picker.x + 1, picker.y + 2);
    let form = app.form.as_ref().unwrap();
    assert!(
        form.label_picker_open(),
        "clicking a row keeps the picker open"
    );
    assert_eq!(
        form.selected_labels()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["label-1"]
    );

    scroll(
        &mut app,
        MouseEventKind::ScrollDown,
        picker.x + 1,
        picker.y + 1,
    );
    assert_eq!(app.form.as_ref().unwrap().label_picker.unwrap().index, 2);

    let title = app.form.as_ref().unwrap().areas.title;
    click(&mut app, title.x, title.y);
    assert!(!app.form.as_ref().unwrap().label_picker_open());
}

#[test]
fn label_picker_manage_row_returns_to_the_draft_with_refreshed_labels() {
    let mut app = app();
    let bug = app.create_label("bug").unwrap();
    app.select_category(1);
    app.open_edit_task();
    app.form
        .as_mut()
        .unwrap()
        .set_field(mach::form::Field::Labels);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Labels);

    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    for character in "backend".chars() {
        press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert_eq!(app.mode, Mode::TaskForm);
    let form = app.form.as_ref().unwrap();
    assert_eq!(form.label_ids(), &[bug]);
    assert_eq!(
        form.label_choices()
            .map(|(_, name, _, _)| name)
            .collect::<Vec<_>>(),
        vec!["bug", "backend"]
    );

    app.form.as_mut().unwrap().open_label_picker();
    let buffer = draw(&mut app, 100, 30);
    let (x, y) = find_cells(&buffer, "Manage");
    click(&mut app, x, y);
    assert_eq!(app.mode, Mode::Labels);
}

#[test]
fn slash_labels_manager_creates_renames_and_deletes_a_global_label() {
    let mut app = app();
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for character in "labels".chars() {
        press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Labels);

    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    for character in "bug".chars() {
        press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(
        app.labels
            .iter()
            .map(|label| label.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bug"]
    );
    let label_id = app.labels[0].id.clone();
    let task_id = app.selected_task().unwrap().id.clone();
    app.set_task_labels(&task_id, vec![label_id]).unwrap();

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
    assert_eq!(app.label_editor.as_ref().unwrap().color, LabelColor::Red);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(app.label_editor.as_ref().unwrap().color_focused);
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(app.labels[0].name, "bug!");
    assert_eq!(app.labels[0].color, LabelColor::Brown);

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(
        message(&app),
        "Press Backspace again to delete bug! and remove it from every task"
    );
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert!(app.labels.is_empty());
    assert!(app.selected_task().unwrap().label_ids.is_empty());
    assert_eq!(message(&app), "Label bug! deleted and unassigned");
}

#[test]
fn image_preview_owns_undo_keys_until_it_is_closed() {
    let mut app = app();
    app.open_new_task();
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    let form = app.form.as_mut().unwrap();
    form.description
        .insert_block(mach::model::Block::image("preview.png"));
    form.description.up();
    assert!(form.open_image_preview().is_none());
    assert!(form.preview);

    press(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);

    let form = app.form.as_ref().unwrap();
    assert!(form.preview, "Ctrl+Z must not escape the image overlay");
    assert_eq!(
        form.title.value(),
        "x",
        "the hidden form must not be edited"
    );
}

#[test]
fn double_clicking_a_label_starts_renaming_that_label() {
    let mut app = app();
    app.create_label("bug").unwrap();
    app.create_label("backend").unwrap();
    app.open_labels();
    let buffer = draw(&mut app, 100, 30);
    let (x, y) = find_cells(&buffer, "backend");

    click(&mut app, x, y);
    assert_eq!(app.label_index, 1);
    assert!(app.label_editor.is_none(), "the first click only selects");

    click(&mut app, x, y);
    let editor = app.label_editor.as_ref().expect("rename input");
    assert_eq!(
        editor.editing_id.as_deref(),
        Some(app.labels[1].id.as_str())
    );
    assert_eq!(editor.name.value(), "backend");

    draw(&mut app, 100, 30);
    let (_, green) = app
        .areas
        .label_color_hits
        .iter()
        .find(|(color, _)| *color == LabelColor::Green)
        .copied()
        .expect("green swatch");
    click(&mut app, green.x.saturating_add(green.width / 2), green.y);
    assert_eq!(app.label_editor.as_ref().unwrap().color, LabelColor::Green);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(app.labels[1].color, LabelColor::Green);
}

#[test]
fn image_preview_owns_clicks_over_the_underlying_panels() {
    let mut fixture = file_app();
    let mut draft = mach::form::TaskDraft::new("picture");
    draft.description = vec![mach::model::Block::image(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/screenshot.png"
    ))];
    fixture.app.create_task(&draft).unwrap();
    fixture.app.open_edit_task();
    assert!(
        fixture
            .app
            .form
            .as_mut()
            .unwrap()
            .open_image_preview()
            .is_none()
    );
    lay_out(&mut fixture.app);

    handle_event(
        &mut fixture.app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    );

    assert_eq!(fixture.app.mode, Mode::TaskForm);
    assert!(fixture.app.form.as_ref().is_some_and(|form| form.preview));
}

#[test]
fn option_arrows_reorder_tasks_without_changing_selection_identity() {
    let mut app = app();
    app.select_category(1);
    app.select_task(0);
    let id = app.selected_task().unwrap().id.clone();

    press(&mut app, KeyCode::Down, KeyModifiers::ALT);

    assert_eq!(app.task_index, 1);
    assert_eq!(app.selected_task().unwrap().id, id);
}

#[test]
fn option_arrows_reorder_categories_but_keep_all_fixed() {
    let mut app = app();
    app.focus = Focus::Sidebar;
    app.select_category(1);
    let id = app.current_category_id().to_string();

    press(&mut app, KeyCode::Down, KeyModifiers::ALT);

    assert!(app.categories[0].is_all());
    assert_eq!(app.cat_index, 2);
    assert_eq!(app.current_category_id(), id);
}

#[test]
fn ctrl_s_commits_the_open_due_picker_before_saving() {
    let mut app = app();
    app.select_category(1);
    app.open_new_task();
    let form = app.form.as_mut().unwrap();
    form.title.insert_str("picker task");
    form.open_due_picker();
    form.picker.as_mut().unwrap().move_days(2);
    let expected = form.picker.as_ref().unwrap().value();

    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.selected_task().unwrap().due, expected);
}

#[test]
fn ctrl_s_commits_the_open_category_dropdown_before_saving() {
    let mut app = app();
    app.select_category(1);
    app.open_new_task();
    let form = app.form.as_mut().unwrap();
    form.title.insert_str("category picker task");
    form.open_category_picker();
    form.move_category_picker(1);

    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        app.tasks
            .iter()
            .find(|task| task.title == "category picker task")
            .unwrap()
            .category_id
            .as_deref(),
        Some("c-home")
    );
}

#[test]
fn ctrl_s_rejects_an_unresolved_description_command() {
    let mut app = app();
    app.select_category(1);
    app.open_new_task();
    let form = app.form.as_mut().unwrap();
    form.title.insert_str("menu task");
    form.field = mach::form::Field::Description;
    for c in "/todo".chars() {
        form.description.insert(c);
    }

    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.as_ref().unwrap().description.menu.is_some());
    assert!(message(&app).contains("command"));
}

#[test]
fn locked_search_cannot_move_focus_to_the_sidebar() {
    let mut app = app();
    app.start_search("first");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.searching);

    press(&mut app, KeyCode::Left, KeyModifiers::NONE);

    assert!(app.searching);
    assert_eq!(app.focus, Focus::Tasks);
}

#[test]
fn key_repeat_events_keep_navigation_responsive() {
    let mut app = app();
    app.select_category(0);
    app.select_task(0);
    handle_event(
        &mut app,
        Event::Key(KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        }),
    );
    assert_eq!(app.task_index, 1);
}

#[test]
fn launch_overlays_do_not_consume_the_first_command_key() {
    for mode in [Mode::Welcome, Mode::WhatsNew] {
        let mut app = app();
        app.mode = mode;
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(app.mode, Mode::Slash);
    }
}

#[test]
fn typeahead_query_is_reset_when_focus_changes() {
    let mut app = app();
    app.tasks
        .push(Task::new("zebra", 0, Some("c-home".into()), ""));
    app.tasks
        .push(Task::new("other", 0, Some("c-home".into()), ""));
    app.rebuild_view();
    app.focus = Focus::Sidebar;
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(app.categories[app.cat_index].name, "Home");
    app.select_task(2);

    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('z'), KeyModifiers::NONE);

    assert_eq!(
        app.selected_task().map(|task| task.title.as_str()),
        Some("zebra")
    );
}

#[test]
fn command_bar_click_opens_the_palette_and_positions_its_cursor() {
    let mut app = app();
    app.focus = Focus::Sidebar;
    app.areas.command_bar = Rect {
        x: 2,
        y: 14,
        width: 40,
        height: 1,
    };

    click(&mut app, 8, 14);
    assert_eq!(app.mode, Mode::Slash);

    for c in "help".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    click(&mut app, 5, 14);
    assert_eq!(
        app.mode,
        Mode::Slash,
        "clicking the input must not close it"
    );
    assert_eq!(app.input.cursor(), 2);
}

#[test]
fn command_bar_click_resumes_a_locked_search() {
    let mut app = app();
    app.start_search("i");
    app.select_task(1);
    let selected = app.selected_task().unwrap().id.clone();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.searching);
    app.areas.command_bar = Rect {
        x: 2,
        y: 14,
        width: 40,
        height: 1,
    };

    click(&mut app, 8, 14);

    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.input.value(), "i");
    assert_eq!(app.input.cursor(), 1);
    assert_eq!(app.selected_task().map(|task| &task.id), Some(&selected));
}

#[test]
fn command_bar_clock_is_clickable_and_places_the_cursor_at_the_end() {
    let mut app = app();
    app.focus = Focus::Sidebar;
    let (width, height) = (100, 30);
    draw(&mut app, width, height);

    // The clock is right-aligned inside the bottom status bar.
    click(&mut app, width - 3, height - 2);
    assert_eq!(app.mode, Mode::Slash);

    for c in "help".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    click(&mut app, width - 3, height - 2);

    assert_eq!(
        app.mode,
        Mode::Slash,
        "the clock belongs to the command bar"
    );
    assert_eq!(app.input.cursor(), 4);
}

#[test]
fn top_level_command_palette_rows_are_clickable() {
    let mut app = app();
    app.open_slash();
    app.areas.slash_menu = Rect {
        x: 0,
        y: 2,
        width: 40,
        height: 11,
    };
    handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(app.mode, Mode::Search, "the first palette row is Search");
}

// ----------------------------------------------------------------- wheel

/// Put the two panels somewhere known, as a frame would.
fn lay_out(app: &mut App) {
    app.areas.sidebar = Rect {
        x: 1,
        y: 1,
        width: 24,
        height: 10,
    };
    app.areas.tasks = Rect {
        x: 27,
        y: 1,
        width: 40,
        height: 10,
    };
}

fn scroll(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    handle_event(
        app,
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

fn long_description() -> Vec<Block> {
    (0..24)
        .map(|index| Block::text(&format!("description line {index}")))
        .collect()
}

#[test]
fn wheel_over_task_preview_scrolls_the_description_not_the_task_list() {
    let mut app = app();
    app.tasks[0].description = long_description();
    draw(&mut app, 120, 30);
    let area = app.areas.preview_description;
    assert!(
        area.width > 0 && area.height > 0,
        "preview body must be visible"
    );
    let selected = app.task_index;

    scroll(&mut app, MouseEventKind::ScrollDown, area.x + 1, area.y + 1);

    assert_eq!(
        app.task_index, selected,
        "preview scrolling keeps selection"
    );
    assert!(
        app.preview_form.as_ref().unwrap().description.scroll() > 0,
        "the preview description moves"
    );

    for _ in 0..20 {
        scroll(&mut app, MouseEventKind::ScrollDown, area.x + 1, area.y + 1);
    }
    let screen = draw(&mut app, 120, 30);
    find_cells(&screen, "description line 23");
}

#[test]
fn bottom_control_scrolls_overflowing_preview_without_opening_the_editor() {
    let mut app = app();
    app.tasks[0].description = long_description();
    app.settings.hint_level = "essential".into();

    let initial = draw(&mut app, 120, 30);
    let (x, y) = find_cells(&initial, "Bottom ↓");
    assert!(!app.areas.preview_bottom.is_empty());
    let initial_style = initial[(x, y)].style();
    assert!(
        matches!(initial_style.bg, Some(Color::Indexed(240 | 250)))
            || initial_style.add_modifier.contains(Modifier::REVERSED)
    );
    assert!(move_mouse(&mut app, x, y));
    let hovered = draw(&mut app, 120, 30);
    assert_ne!(hovered[(x, y)].style(), initial_style);

    let viewport_height = usize::from(app.areas.preview_description.height);
    click(&mut app, x, y);

    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.form.is_none(),
        "the control must not open the task editor"
    );
    let description = &app.preview_form.as_ref().unwrap().description;
    assert_eq!(
        description.scroll(),
        description.content_height().saturating_sub(viewport_height)
    );

    let at_bottom = buffer_text(&draw(&mut app, 120, 30));
    assert!(!at_bottom.contains("Bottom ↓"), "{at_bottom}");
    assert!(app.areas.preview_bottom.is_empty());
}

#[test]
fn bottom_control_jumps_to_the_last_category_without_opening_it() {
    let mut app = overflowing_app();
    app.select_last_task();
    app.focus = Focus::Sidebar;

    let initial = draw(&mut app, 80, 16);
    let (x, y) = find_cells(&initial, "Bottom ↓");
    assert!(app.areas.sidebar_bottom.contains((x, y).into()));
    click(&mut app, x, y);

    assert_eq!(app.focus, Focus::Sidebar);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.category_form.is_none());
    assert_eq!(app.cat_index, app.categories.len() - 1);
    assert_eq!(app.categories[app.cat_index].name, "Category 19");
    let _ = draw(&mut app, 80, 16);
    assert!(app.areas.sidebar_bottom.is_empty());
}

#[test]
fn bottom_control_jumps_to_the_last_task_without_opening_it() {
    let mut app = overflowing_app();
    app.select_last_category();
    app.focus = Focus::Tasks;

    let initial = draw(&mut app, 80, 16);
    let (x, y) = find_cells(&initial, "Bottom ↓");
    assert!(app.areas.tasks_bottom.contains((x, y).into()));
    click(&mut app, x, y);

    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.form.is_none());
    assert_eq!(app.task_index, app.view.len() - 1);
    assert_eq!(app.selected_task().unwrap().title, "task 19");
    let _ = draw(&mut app, 80, 16);
    assert!(app.areas.tasks_bottom.is_empty());
}

#[test]
fn bottom_control_scrolls_the_task_editor_without_moving_its_caret() {
    let mut app = app();
    app.tasks[0].description = long_description();
    app.open_edit_task();

    let initial = draw(&mut app, 120, 30);
    let (x, y) = find_cells(&initial, "Bottom ↓");
    let form = app.form.as_ref().unwrap();
    assert!(!form.areas.description_bottom.is_empty());
    let field = form.field;
    let cursor = form.description.cursor_line();
    let dirty = form.is_dirty();
    let viewport_height = usize::from(form.areas.description.height);

    click(&mut app, x, y);

    let form = app.form.as_ref().unwrap();
    assert_eq!(app.mode, Mode::TaskForm);
    assert_eq!(form.field, field);
    assert_eq!(form.description.cursor_line(), cursor);
    assert_eq!(form.is_dirty(), dirty);
    assert_eq!(
        form.description.scroll(),
        form.description
            .content_height()
            .saturating_sub(viewport_height)
    );

    let at_bottom = buffer_text(&draw(&mut app, 120, 30));
    assert!(!at_bottom.contains("Bottom ↓"), "{at_bottom}");
    assert!(
        app.form
            .as_ref()
            .unwrap()
            .areas
            .description_bottom
            .is_empty()
    );
}

#[test]
fn wheel_over_task_form_description_preserves_field_and_caret() {
    let mut app = app();
    app.tasks[0].description = long_description();
    app.open_edit_task();
    draw(&mut app, 120, 30);
    let area = app.form.as_ref().unwrap().areas.description;
    let field = app.form.as_ref().unwrap().field;
    let caret = app.form.as_ref().unwrap().description.cursor_line();

    scroll(&mut app, MouseEventKind::ScrollDown, area.x + 1, area.y + 1);

    let form = app.form.as_ref().unwrap();
    assert_eq!(form.field, field, "wheel scrolling does not steal focus");
    assert_eq!(
        form.description.cursor_line(),
        caret,
        "wheel scrolling does not move the caret"
    );
    assert!(form.description.scroll() > 0);
}

#[test]
fn wheel_over_category_description_scrolls_without_stealing_focus() {
    let mut app = app();
    app.categories
        .iter_mut()
        .find(|category| category.id == "c-work")
        .unwrap()
        .description = (0..24)
        .map(|index| format!("category line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.select_category(1);
    app.open_edit_category();
    draw(&mut app, 100, 24);
    let area = app.category_form.as_ref().unwrap().description_area;
    assert!(!app.category_form.as_ref().unwrap().on_description);

    scroll(&mut app, MouseEventKind::ScrollDown, area.x + 1, area.y + 1);

    let form = app.category_form.as_ref().unwrap();
    assert!(!form.on_description, "wheel scrolling does not steal focus");
    assert!(form.description.scroll() > 0);
}

#[test]
fn the_wheel_follows_the_pointer_not_the_focus() {
    let mut app = app();
    lay_out(&mut app);
    app.focus = Focus::Tasks;
    app.select_task(0);
    let task_before = app.task_index;

    // Over the sidebar: the categories move, even though Tasks has focus.
    scroll(&mut app, MouseEventKind::ScrollDown, 5, 3);
    assert_eq!(app.cat_index, 1, "the category under the pointer scrolled");
    assert_eq!(app.focus, Focus::Tasks, "scrolling does not steal focus");

    // Over the tasks: the task list moves.
    app.select_task(0);
    scroll(&mut app, MouseEventKind::ScrollDown, 30, 3);
    assert_eq!(app.task_index, task_before + 1);
}

#[test]
fn the_wheel_over_the_sidebar_no_longer_moves_tasks() {
    let mut app = app();
    lay_out(&mut app);
    app.focus = Focus::Tasks;
    app.select_category(1);
    app.select_task(0);

    scroll(&mut app, MouseEventKind::ScrollDown, 5, 3);
    assert_eq!(
        app.task_index, 0,
        "the task selection is not what the pointer was over"
    );
}

#[test]
fn the_wheel_outside_both_panels_does_nothing() {
    let mut app = app();
    lay_out(&mut app);
    let (cat, task) = (app.cat_index, app.task_index);
    scroll(&mut app, MouseEventKind::ScrollDown, 200, 200);
    assert_eq!((app.cat_index, app.task_index), (cat, task));
}
