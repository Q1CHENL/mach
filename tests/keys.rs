//! Key and mouse handling, driven through the real event entry point.

use std::process::Command;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;

use mach::app::{App, Confirm, Focus, Mode};
use mach::form::{TaskDraft, TaskForm};
use mach::input::handle_event;
use mach::model::{Category, Task};
use mach::store::Store;
use mach::ui;

mod common;
use common::TempDir;

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

fn file_app() -> (App, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("mach-keys-file-test-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir).unwrap();
    let mut app = App::with_store("test", store).unwrap();
    app.mode = Mode::Normal;
    (app, dir)
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_event(app, Event::Key(KeyEvent::new(code, modifiers)));
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

fn draw(app: &mut App, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| ui::draw(frame, app))
        .expect("draw must not panic");
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
    let (mut app, dir) = file_app();
    let observer = rusqlite::Connection::open(dir.join("mach.db")).unwrap();
    observer.execute("DELETE FROM app_state", []).unwrap();

    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "done".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(
        message(&app).contains("Could not update settings"),
        "the persistence error must not be replaced by a false success: {:?}",
        message(&app)
    );
    drop(observer);
    drop(app);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn task_delete_keeps_a_store_failure_visible() {
    let (mut app, dir) = file_app();
    app.create_task(&mach::form::TaskDraft::new("keep me"))
        .unwrap();
    let observer = rusqlite::Connection::open(dir.join("mach.db")).unwrap();
    observer.execute("DELETE FROM app_state", []).unwrap();

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);

    assert!(
        message(&app).contains("Could not delete task"),
        "the persistence error must not be replaced by a false success: {:?}",
        message(&app)
    );
    assert!(app.tasks.iter().any(|task| task.title == "keep me"));
    drop(observer);
    drop(app);
    let _ = std::fs::remove_dir_all(dir);
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
    let (mut source, source_dir) = file_app();
    source
        .create_task(&TaskDraft::new("portable from TUI"))
        .expect("create source task");
    let archive_dir = TempDir::new("keys-import-archive");
    let archive = archive_dir.path().join("tasks.mach");
    let exported = Command::new(env!("CARGO_BIN_EXE_mach"))
        .arg("--dir")
        .arg(&source_dir)
        .arg("export")
        .arg(&archive)
        .output()
        .expect("export archive through CLI");
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );

    let (mut destination, destination_dir) = file_app();
    let lock = rusqlite::Connection::open(destination_dir.join("mach.db"))
        .expect("open destination database lock");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold destination write lock");
    press(&mut destination, KeyCode::Char('/'), KeyModifiers::NONE);
    for character in format!("import {}", archive.display()).chars() {
        press(
            &mut destination,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }
    let started = std::time::Instant::now();
    press(&mut destination, KeyCode::Enter, KeyModifiers::NONE);
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "slash import blocked the input path for {elapsed:?}"
    );
    press(&mut destination, KeyCode::Char('/'), KeyModifiers::NONE);
    assert_eq!(destination.mode, Mode::Slash);
    press(&mut destination, KeyCode::Esc, KeyModifiers::NONE);
    lock.execute_batch("ROLLBACK")
        .expect("release destination write lock");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while destination.tasks.is_empty() && std::time::Instant::now() < deadline {
        destination.poll_external_changes();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(destination.tasks.len(), 1, "{}", message(&destination));
    assert_eq!(destination.tasks[0].title, "portable from TUI");

    drop(source);
    drop(destination);
    let _ = std::fs::remove_dir_all(source_dir);
    let _ = std::fs::remove_dir_all(destination_dir);
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
    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.is_some());
    assert!(message(&app).to_lowercase().contains("unsaved"));
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
    assert!(message(&app).to_lowercase().contains("unsaved"));
}

#[test]
fn escape_requires_confirmation_before_discarding_a_dirty_task_form() {
    let mut app = app();
    app.select_category(1);
    app.open_edit_task();
    app.form.as_mut().unwrap().title.insert('!');

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::TaskForm);
    assert!(message(&app).contains("again"));

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
fn task_form_can_move_an_existing_task_to_another_category() {
    let mut app = app();
    app.select_category(1);
    let id = app.selected_task().unwrap().id.clone();
    app.open_edit_task();
    let form = app.form.as_mut().unwrap();
    assert_eq!(form.category_id(), Some("c-work"));
    form.set_field(mach::form::Field::Category);

    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
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
fn image_preview_owns_undo_keys_until_it_is_closed() {
    let mut app = app();
    app.open_new_task();
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    let form = app.form.as_mut().unwrap();
    form.body
        .insert_block(mach::model::Block::image("preview.png"));
    form.body.up();
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
fn image_preview_owns_clicks_over_the_underlying_panels() {
    let (mut app, dir) = file_app();
    let mut draft = mach::form::TaskDraft::new("picture");
    draft.body = vec![mach::model::Block::image(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/screenshot.png"
    ))];
    app.create_task(&draft).unwrap();
    app.open_edit_task();
    assert!(app.form.as_mut().unwrap().open_image_preview().is_none());
    lay_out(&mut app);

    handle_event(
        &mut app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    );

    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.as_ref().is_some_and(|form| form.preview));
    drop(app);
    let _ = std::fs::remove_dir_all(dir);
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
fn ctrl_s_rejects_an_unresolved_body_command() {
    let mut app = app();
    app.select_category(1);
    app.open_new_task();
    let form = app.form.as_mut().unwrap();
    form.title.insert_str("menu task");
    form.field = mach::form::Field::Body;
    for c in "/todo".chars() {
        form.body.insert(c);
    }

    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, Mode::TaskForm);
    assert!(app.form.as_ref().unwrap().body.menu.is_some());
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
