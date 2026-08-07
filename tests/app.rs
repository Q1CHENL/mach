//! End-to-end checks against a throwaway schema-3 data directory.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use mach::app::App;
use mach::form::TaskDraft;
use mach::model::Block;

/// `store::paths()` resolves once per process, so every test in this file
/// shares one directory. The lock keeps them from running concurrently.
static DATA_DIR: Mutex<()> = Mutex::new(());

fn setup() -> (PathBuf, MutexGuard<'static, ()>) {
    let guard = DATA_DIR.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("mach-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // SAFETY: no other thread is running while the lock is held.
    unsafe { std::env::set_var("MACH_DIR", &dir) };

    fs::write(
        dir.join("categories.json"),
        r#"{
          "schema": 3,
          "categories": [
            {"id":"c-work","name":"Work","description":""},
            {"id":"c-home","name":"Home","description":""}
          ]
        }"#,
    )
    .unwrap();
    fs::write(
        dir.join("tasks.json"),
        r#"{
          "schema": 3,
          "tasks": [
            {
              "id":"t-done",
              "title":"done task",
              "body":[],
              "due":"",
              "created":"2024-01-01 00:00:00",
              "done":true,
              "importance":0,
              "category_id":"c-work"
            },
            {
              "id":"t-open",
              "title":"open task",
              "body":[],
              "due":"2030-01-02",
              "created":"2024-01-02 00:00:00",
              "done":false,
              "importance":1
            }
          ]
        }"#,
    )
    .unwrap();
    (dir, guard)
}

#[test]
fn reads_writes_and_edits_a_data_directory() {
    let (dir, _guard) = setup();
    let mut app = App::new("test");

    assert_eq!(app.tasks.len(), 2);
    assert!(app.tasks[0].done);
    assert_eq!(app.tasks[0].due, "");
    // All Tasks (virtual) + Work + Home
    assert_eq!(app.categories.len(), 3);
    assert!(app.categories[0].is_all());
    assert_eq!(app.task_count(), 2, "All Tasks shows everything");

    assert!(
        app.create_task(&TaskDraft::new("nope")).is_none(),
        "cannot file a task under All Tasks"
    );
    app.select_category(1); // Work
    let id = app
        .create_task(&TaskDraft {
            title: "draft the release note [2030-05-06 07:08]".into(),
            due: String::new(),
            importance: 2,
            body: vec![
                Block::text("with notes"),
                Block::todo("step one", true),
                Block::todo("step two", false),
            ],
        })
        .expect("task created");
    let added = app.selected_task().expect("new task is selected");
    assert_eq!(added.title, "draft the release note");
    assert_eq!(added.due, "2030-05-06 07:08");
    assert_eq!(added.body[0], Block::text("with notes"));
    assert_eq!(
        added.category_id.as_deref(),
        Some(app.categories[1].id.as_str())
    );
    assert_eq!(mach::model::todo_progress(added), Some((1, 2)));
    assert!(mach::model::has_prose_or_image(added));

    let pos = app.task_index;
    app.toggle_done(pos);
    app.cycle_importance(pos);
    let task = app.visible_task(pos).unwrap();
    assert!(task.done);
    assert_eq!(task.importance, 3, "two flags, stepped up to three");
    assert_eq!(app.done_count(), 2);

    app.update_task(
        &id,
        &TaskDraft {
            title: "renamed task".into(),
            due: "09:00".into(),
            ..TaskDraft::default()
        },
    );
    let task = app.visible_task(pos).unwrap();
    assert_eq!(task.title, "renamed task");
    assert_eq!(task.due, "09:00");
    assert!(task.body.is_empty(), "the body can be cleared");

    let saved = fs::read_to_string(dir.join("tasks.json")).unwrap();
    assert!(
        saved.contains("\"schema\":3") || saved.contains("\"schema\": 3"),
        "schema version is written"
    );
    assert!(saved.contains("\"title\""), "title is stored");

    let before = app.tasks.len();
    let deleted_title = app.visible_task(pos).unwrap().title.clone();
    app.delete_task(pos);
    assert_eq!(app.tasks.len(), before - 1);
    assert!(!app.tasks.iter().any(|t| t.title == deleted_title));
    assert!(
        !dir.join("purged.json").exists(),
        "delete does not write a trash file"
    );

    assert_eq!(app.purge(), 1);
    assert!(app.tasks.iter().all(|t| !t.done));

    let reloaded = App::new("test");
    assert_eq!(reloaded.tasks.len(), app.tasks.len());
    assert_eq!(reloaded.tasks[0].title, app.tasks[0].title);
    // Only the three real files; no trash / backup beside them.
    let mut written: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    assert_eq!(written, ["categories.json", "settings.json", "tasks.json"]);
}

#[test]
fn filters_by_category_and_sorts_as_configured() {
    let (_dir, _guard) = setup();
    let mut app = App::new("test");

    app.select_category(1); // Work
    assert!(!app.is_all_view());
    assert_eq!(app.task_count(), 1);
    assert_eq!(app.selected_task().unwrap().title, "done task");

    app.create_task(&TaskDraft::new("work item"));
    assert_eq!(
        app.selected_task().unwrap().category_id.as_deref(),
        Some(app.categories[1].id.as_str())
    );
    assert_eq!(app.task_count(), 2);

    // Sort applies inside a category.
    app.select_category(1); // Work: done task + work item
    let work_item = app
        .view
        .iter()
        .position(|&i| app.tasks[i].title == "work item")
        .unwrap();
    app.cycle_importance(work_item);
    app.settings.sort = "important".into();
    app.rebuild_view();
    assert_eq!(
        app.visible_task(0).unwrap().title,
        "work item",
        "flagged task sorts first inside the category"
    );

    app.settings.sort = "done".into();
    app.rebuild_view();
    let last = app.task_count() - 1;
    assert!(app.visible_task(last).unwrap().done, "done sink to the end");

    // All Tasks stacks categories (Work, then Home, then uncategorized).
    app.settings.sort = "manual".into();
    app.select_category(0);
    app.rebuild_view();
    let titles: Vec<&str> = (0..app.task_count())
        .map(|i| app.visible_task(i).unwrap().title.as_str())
        .collect();
    let work_at = titles.iter().position(|t| *t == "work item").unwrap();
    let open_at = titles.iter().position(|t| *t == "open task").unwrap();
    assert!(
        work_at < open_at,
        "Work group before uncategorized: {titles:?}"
    );

    // Due sort inside Work: dated "work item" stays with Work; open task
    // keeps its own due date in the uncategorized bucket.
    app.settings.sort = "due".into();
    app.rebuild_view();
    assert!(
        app.view
            .iter()
            .any(|&i| app.tasks[i].title == "open task" && app.tasks[i].due == "2030-01-02")
    );

    app.search_query = "task".to_string();
    app.searching = true;
    app.rebuild_view();
    assert_eq!(app.task_count(), 2);

    app.end_search();
    app.select_category(2); // Home
    let id = app
        .create_task(&TaskDraft {
            title: "plain title".into(),
            body: vec![Block::text("buried in the body")],
            ..TaskDraft::default()
        })
        .unwrap();
    app.select_category(0);
    app.search_query = "buried".to_string();
    app.searching = true;
    app.rebuild_view();
    assert_eq!(app.task_count(), 1);
    assert_eq!(app.selected_task().unwrap().id, id);

    app.end_search();
    assert_eq!(app.task_count(), 4);
}

#[test]
fn selection_follows_the_task_when_sort_reorders() {
    let (_dir, _guard) = setup();
    let mut app = App::new("test");
    app.settings.sort = "done".into();
    app.rebuild_view();

    let pos = app
        .view
        .iter()
        .position(|&i| !app.tasks[i].done)
        .expect("an open task");
    app.task_index = pos;
    let id = app.selected_task().unwrap().id.clone();

    app.toggle_done(pos);
    assert_eq!(
        app.selected_task().map(|t| t.id.as_str()),
        Some(id.as_str()),
        "selection should stay on the same task after it moves in the view"
    );
    assert!(app.selected_task().unwrap().done);

    app.cycle_importance(app.task_index);
    assert_eq!(
        app.selected_task().map(|t| t.id.as_str()),
        Some(id.as_str()),
        "importance changes must not drop the selection either"
    );
}

#[test]
fn deleting_a_category_takes_its_tasks_without_renumbering() {
    let (_dir, _guard) = setup();
    let mut app = App::new("test");
    app.select_category(1); // Work
    let work_id = app.categories[1].id.clone();
    let home_id = app.categories[2].id.clone();
    assert_eq!(app.category_progress(&work_id).1, 1);

    app.delete_category();
    // Virtual All + Home
    assert_eq!(app.categories.len(), 2);
    assert!(app.categories.iter().all(|c| c.name != "Work"));
    assert!(app.tasks.iter().all(|t| t.title != "done task"));
    // Home keeps its uuid.
    assert_eq!(app.categories[1].id, home_id);

    app.select_category(0);
    app.delete_category();
    assert_eq!(app.categories.len(), 2, "All Tasks cannot be deleted");
}

#[test]
fn hide_done_and_purge_follow_the_category_view() {
    let (_dir, _guard) = setup();
    let mut app = App::new("test");
    // Seed: open + done in Work, open elsewhere.
    let total = app.tasks.len();
    let done_n = app.tasks.iter().filter(|t| t.done).count();
    assert!(done_n >= 1);

    assert!(app.toggle_hide_done(), "first toggle hides");
    assert_eq!(app.task_count(), total - done_n);
    assert!(app.tasks.iter().any(|t| t.done), "still on disk");
    assert!(!app.toggle_hide_done(), "second toggle shows again");

    app.select_category(1); // Work has the done task
    let work_id = app.categories[1].id.clone();
    let work_done = app
        .tasks
        .iter()
        .filter(|t| t.done && t.category_id.as_deref() == Some(work_id.as_str()))
        .count();
    assert_eq!(app.purge(), work_done, "category purge only that cat");
    assert!(
        app.tasks
            .iter()
            .filter(|t| t.category_id.as_deref() == Some(work_id.as_str()))
            .all(|t| !t.done)
    );

    // Done elsewhere remains until All Tasks purge.
    app.select_category(0);
    if let Some(pos) = app.view.iter().position(|&i| !app.tasks[i].done) {
        app.toggle_done(pos);
    }
    let n = app.purge();
    assert!(n >= 1);
    assert!(app.tasks.iter().all(|t| !t.done));
}
