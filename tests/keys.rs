//! Key and mouse handling, driven through the real event entry point.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use mach::app::{App, Confirm, Focus, Mode};
use mach::form::TaskForm;
use mach::input::handle_event;
use mach::model::{Category, Task};

/// `App::new` reads and writes the data directory, so send it somewhere
/// throwaway before the first call.
fn app() -> App {
    static REDIRECT_DATA_DIR: std::sync::Once = std::sync::Once::new();
    REDIRECT_DATA_DIR.call_once(|| {
        let dir = std::env::temp_dir().join(format!("mach-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        mach::store::set_data_dir(dir);
    });

    let mut app = App::new("test");
    // A fresh data dir means no recorded version, so the welcome splash
    // is up and would swallow the first key of every test.
    app.mode = Mode::Normal;
    app.categories = vec![
        Category::all_tasks(),
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
    app.tasks = vec![
        Task::new("first", 0, Some("c-work".into()), ""),
        Task::new("second", 0, Some("c-work".into()), ""),
        Task::new("third", 0, Some("c-home".into()), ""),
    ];
    app.settings.sort = "manual".into();
    app.rebuild_view();
    app
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_event(app, Event::Key(KeyEvent::new(code, modifiers)));
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
    assert!(app.awaiting(Confirm::Delete));

    ctrl_c(&mut app);
    assert!(app.awaiting(Confirm::Quit), "quit takes over");

    // This Backspace is a first press again — it must not land on a task.
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before, "nothing was deleted");
    assert!(app.awaiting(Confirm::Delete));
}

#[test]
fn backspace_twice_still_deletes() {
    let mut app = app();
    let before = app.tasks.len();
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.tasks.len(), before - 1);
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
fn click_on_panels_closes_task_form() {
    let mut app = app();
    lay_out(&mut app);
    app.select_category(1);
    app.open_edit_task();
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
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.form.is_none());
}

#[test]
fn click_on_panels_closes_category_form() {
    let mut app = app();
    lay_out(&mut app);
    app.select_category(1);
    app.open_edit_category();
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
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.category_form.is_none());
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
