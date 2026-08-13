//! App behavior against an independent in-memory store per test.

use mach::app::{App, Confirm, Mode};
use mach::form::TaskDraft;
use mach::model::{Block, Category, Task};
use mach::store::{CategoryPatch, RelativePosition, Store, TaskPatch};

mod common;
use common::TempDir;

fn seed(store: &mut Store) {
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
    let mut done = Task::new("done task", 0, Some("c-work".into()), "");
    done.id = "t-done".into();
    done.created = "2024-01-01 00:00:00".into();
    done.done = true;
    let mut open = Task::new("open task", 1, None, "2030-01-02");
    open.id = "t-open".into();
    open.created = "2024-01-02 00:00:00".into();
    store
        .update(|data| {
            data.categories = categories;
            data.tasks = vec![done, open];
            Ok(())
        })
        .unwrap();
}

fn setup() -> App {
    let mut store = Store::open_in_memory_with_paths(
        std::env::temp_dir().join(format!("mach-app-test-{}", uuid::Uuid::new_v4())),
    )
    .unwrap();
    seed(&mut store);
    let mut app = App::with_store("test", store).unwrap();
    app.mode = Mode::Normal;
    app
}

struct OnDiskPair {
    app: App,
    external: Store,
    dir: TempDir,
}

fn on_disk_pair() -> OnDiskPair {
    let dir = TempDir::new("app-agents");
    let mut initial = Store::open(dir.path()).unwrap();
    seed(&mut initial);
    drop(initial);
    let mut app = App::with_store("test", Store::open(dir.path()).unwrap()).unwrap();
    app.mode = Mode::Normal;
    let external = Store::open(dir.path()).unwrap();
    OnDiskPair { app, external, dir }
}

fn replace_form_title(app: &mut App, title: &str) {
    let input = &mut app.form.as_mut().expect("task form open").title;
    input.home();
    input.delete_to_end();
    input.insert_str(title);
}

#[test]
fn constructing_tui_state_does_not_consume_the_launch_screen() {
    let dir = TempDir::new("launch-pending");
    let app = App::with_store("0.4.0", Store::open(dir.path()).unwrap()).unwrap();
    assert_eq!(app.mode, Mode::Welcome);
    drop(app);

    let snapshot = Store::open(dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(
        snapshot.settings.last_run_version, None,
        "building App before terminal setup must not consume Welcome"
    );
}

#[test]
fn loads_and_mutates_a_store_snapshot() {
    let mut app = setup();

    assert_eq!(app.tasks.len(), 2);
    assert!(app.tasks[0].done);
    assert_eq!(app.tasks[0].due, "");
    // All Tasks (virtual) + Work + Home
    assert_eq!(app.categories.len(), 3);
    assert!(app.categories[0].is_all());
    assert_eq!(app.task_count(), 2, "All Tasks shows everything");

    let uncategorized = app
        .create_task(&TaskDraft::new("from all"))
        .expect("All view creates an Uncategorized task");
    assert_eq!(
        app.tasks
            .iter()
            .find(|task| task.id == uncategorized)
            .unwrap()
            .category_id,
        None
    );
    app.select_category(1); // Work
    let work_id = app.categories[1].id.clone();
    let id = app
        .create_task(&TaskDraft {
            title: "draft the release note [2030-05-06 07:08]".into(),
            category_id: Some(work_id.clone()),
            label_ids: Vec::new(),
            due: String::new(),
            importance: 2,
            description: vec![
                Block::text("with notes"),
                Block::todo("step one", true),
                Block::todo("step two", false),
            ],
        })
        .expect("task created");
    let added = app.selected_task().expect("new task is selected");
    assert_eq!(added.title, "draft the release note");
    assert_eq!(added.due, "2030-05-06 07:08");
    assert_eq!(added.description[0], Block::text("with notes"));
    assert_eq!(added.category_id.as_deref(), Some(work_id.as_str()));
    assert_eq!(mach::model::todo_progress(added), Some((1, 2)));
    assert!(mach::model::has_prose_or_image(added));

    let pos = app.task_index;
    app.toggle_done(pos);
    app.cycle_importance(app.task_index);
    let task = app.selected_task().unwrap();
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
    let task = app.tasks.iter().find(|task| task.id == id).unwrap();
    assert_eq!(task.title, "renamed task");
    assert!(task.due.ends_with("09:00"), "{}", task.due);
    assert!(
        task.description.is_empty(),
        "the description can be cleared"
    );
    assert_eq!(task.category_id, None, "editing can move a task");

    let before = app.tasks.len();
    app.delete_task_by_id(&id);
    assert_eq!(app.tasks.len(), before - 1);
    assert!(!app.tasks.iter().any(|task| task.id == id));
    assert_eq!(app.purge(), 1);
    assert!(app.tasks.iter().all(|t| !t.done));
}

#[test]
fn search_uses_the_same_unicode_caseless_identity_as_typeahead_and_categories() {
    let mut app = setup();
    app.create_task(&TaskDraft::new("Maße"))
        .expect("create Unicode title");
    let mut description_task = TaskDraft::new("accent note");
    description_task.description = vec![Block::text("Cafe\u{301}")];
    app.create_task(&description_task)
        .expect("create decomposed description");

    app.start_search("MASSE");
    assert_eq!(app.task_count(), 1);
    assert_eq!(app.selected_task().unwrap().title, "Maße");

    app.start_search("Café");
    assert_eq!(app.task_count(), 1);
    assert_eq!(app.selected_task().unwrap().title, "accent note");
}

#[test]
fn filters_by_category_and_sorts_as_configured() {
    let mut app = setup();

    app.select_category(1); // Work
    assert!(!app.is_all_view());
    assert_eq!(app.task_count(), 1);
    assert_eq!(app.selected_task().unwrap().title, "done task");

    let work_id = app.categories[1].id.clone();
    app.create_task(&TaskDraft {
        title: "work item".into(),
        category_id: Some(work_id.clone()),
        ..TaskDraft::default()
    });
    assert_eq!(
        app.selected_task().unwrap().category_id.as_deref(),
        Some(work_id.as_str())
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
    let home_id = app.categories[2].id.clone();
    let id = app
        .create_task(&TaskDraft {
            title: "plain title".into(),
            category_id: Some(home_id),
            description: vec![Block::text("buried in the description")],
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
    let mut app = setup();
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
fn every_view_rebuild_preserves_the_selected_task_by_id() {
    let mut app = setup();
    app.settings.sort = "manual".into();
    app.settings.hide_done = false;
    app.select_category(0);
    app.rebuild_view();

    let selected = app
        .tasks
        .iter()
        .find(|task| !task.done)
        .expect("open task")
        .id
        .clone();
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == selected)
        .unwrap();
    app.select_task(pos);

    app.settings.sort = "important".into();
    app.rebuild_view();
    assert_eq!(
        app.selected_task().map(|task| task.id.as_str()),
        Some(selected.as_str())
    );

    app.settings.hide_done = true;
    app.rebuild_view();
    assert_eq!(
        app.selected_task().map(|task| task.id.as_str()),
        Some(selected.as_str())
    );
}

#[test]
fn deleting_a_category_keeps_its_tasks_uncategorized_without_renumbering() {
    let mut app = setup();
    app.select_category(1); // Work
    let work_id = app.categories[1].id.clone();
    let home_id = app.categories[2].id.clone();
    assert_eq!(app.category_progress(&work_id).1, 1);

    app.delete_category();
    // Virtual All + Home
    assert_eq!(app.categories.len(), 2);
    assert!(app.categories.iter().all(|c| c.name != "Work"));
    let retained = app
        .tasks
        .iter()
        .find(|task| task.title == "done task")
        .expect("category tasks are retained");
    assert_eq!(retained.category_id, None);
    // Home keeps its uuid.
    assert_eq!(app.categories[1].id, home_id);

    app.select_category(0);
    app.delete_category();
    assert_eq!(app.categories.len(), 2, "All Tasks cannot be deleted");
}

#[test]
fn hide_done_and_purge_follow_the_category_view() {
    let mut app = setup();
    // Seed: open + done in Work, open elsewhere.
    let total = app.tasks.len();
    let done_n = app.tasks.iter().filter(|t| t.done).count();
    assert!(done_n >= 1);

    assert_eq!(app.toggle_hide_done(), Some(true), "first toggle hides");
    assert_eq!(app.task_count(), total - done_n);
    assert!(app.tasks.iter().any(|t| t.done), "still on disk");
    assert_eq!(
        app.toggle_hide_done(),
        Some(false),
        "second toggle shows again"
    );

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

#[test]
fn external_commit_refreshes_without_losing_selected_task_identity() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    let selected = app.selected_task().unwrap().id.clone();

    external
        .update(|data| {
            data.edit_task(
                &selected,
                TaskPatch {
                    title: Some("agent-renamed".into()),
                    ..TaskPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();

    assert!(app.poll_external_changes());
    assert_eq!(app.selected_task().unwrap().id, selected);
    assert_eq!(app.selected_task().unwrap().title, "agent-renamed");
}

#[test]
fn external_refresh_waits_while_the_label_manager_owns_an_edit() {
    let mut pair = on_disk_pair();
    pair.app.open_labels();
    pair.app.begin_new_label();
    pair.app
        .label_editor
        .as_mut()
        .unwrap()
        .name
        .insert_str("typed draft");
    pair.external
        .update(|data| {
            data.create_label("external")?;
            Ok(())
        })
        .unwrap();

    assert!(!pair.app.poll_external_changes());
    assert_eq!(
        pair.app.label_editor.as_ref().unwrap().name.value(),
        "typed draft"
    );
    assert!(pair.app.labels.is_empty());

    pair.app.close_labels();
    assert!(pair.app.poll_external_changes());
    assert_eq!(pair.app.labels[0].name, "external");
}

#[test]
fn repeated_external_poll_errors_do_not_disarm_a_confirmation() {
    let mut pair = on_disk_pair();
    let database = pair.dir.path().join("mach.db");
    let app = &mut pair.app;
    let observer = rusqlite::Connection::open(database).unwrap();
    observer.execute("DROP TABLE app_state", []).unwrap();
    drop(observer);

    assert!(app.poll_external_changes(), "the first failure is reported");
    app.ask_confirm(Confirm::Quit, "Press Ctrl+C again to quit");
    assert!(app.awaiting(Confirm::Quit));

    let redrawn = app.poll_external_changes();
    assert!(
        !redrawn,
        "an unchanged polling failure must not redraw or replace the message: {:?}",
        app.message.as_ref().map(|message| message.text.as_str())
    );
    assert!(
        app.awaiting(Confirm::Quit),
        "background polling must not cancel an unrelated confirmation"
    );
}

#[test]
fn form_save_preserves_an_external_done_toggle() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    app.open_edit_task();
    external
        .update(|data| {
            data.toggle_task_done("t-open")?;
            Ok(())
        })
        .unwrap();
    replace_form_title(app, "human title");

    app.submit_form();

    assert!(
        app.form.is_none(),
        "unrelated external fields do not conflict"
    );
    let saved = app.tasks.iter().find(|task| task.id == "t-open").unwrap();
    assert_eq!(saved.title, "human title");
    assert!(saved.done, "the external toggle must survive form save");
}

#[test]
fn form_save_merges_disjoint_human_and_agent_fields() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    app.open_edit_task();
    external
        .update(|data| {
            data.edit_task(
                "t-open",
                TaskPatch {
                    title: Some("agent title".into()),
                    ..TaskPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    app.form
        .as_mut()
        .expect("task form open")
        .description
        .insert_str("human description");

    app.submit_form();

    assert!(app.form.is_none(), "disjoint fields should merge");
    let saved = app.tasks.iter().find(|task| task.id == "t-open").unwrap();
    assert_eq!(saved.title, "agent title");
    assert_eq!(saved.description, vec![Block::text("human description")]);
}

#[test]
fn form_save_merges_label_edits_with_an_external_title_change() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    let release = app.create_label("release").expect("create release");
    let backend = app.create_label("backend").expect("create backend");
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    app.open_edit_task();

    external
        .update(|data| {
            data.edit_task(
                "t-open",
                TaskPatch {
                    title: Some("agent title".into()),
                    ..TaskPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    let form = app.form.as_mut().expect("task form open");
    form.toggle_label(&release).unwrap();
    form.toggle_label(&backend).unwrap();

    app.submit_form();

    assert!(app.form.is_none(), "disjoint fields should merge");
    let saved = app.tasks.iter().find(|task| task.id == "t-open").unwrap();
    assert_eq!(saved.title, "agent title");
    assert_eq!(saved.label_ids, vec![release, backend]);
}

#[test]
fn form_save_accepts_a_convergent_external_edit() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    app.open_edit_task();
    replace_form_title(app, "shared title");
    external
        .update(|data| {
            data.edit_task(
                "t-open",
                TaskPatch {
                    title: Some("shared title".into()),
                    ..TaskPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();

    app.submit_form();

    assert!(
        app.form.is_none(),
        "identical human and agent edits have already converged"
    );
    assert_eq!(
        app.tasks
            .iter()
            .find(|task| task.id == "t-open")
            .unwrap()
            .title,
        "shared title"
    );
}

#[test]
fn task_form_adopts_paths_against_its_store_before_taking_the_dirty_baseline() {
    let dir = TempDir::new("form-image-root");
    let mut store = Store::open(dir.path()).unwrap();
    std::fs::create_dir_all(store.images_dir()).unwrap();
    std::fs::write(store.images_dir().join("active-root.png"), b"fixture").unwrap();
    let mut task = Task::new("picture path", 0, None, "");
    task.description = vec![Block::text("active-root.png")];
    store
        .update(|data| {
            data.tasks.push(task);
            Ok(())
        })
        .unwrap();

    let mut app = App::with_store("test", store).unwrap();
    app.mode = Mode::Normal;
    app.open_edit_task();
    let form = app.form.as_ref().expect("task form open");
    assert_eq!(
        form.description.value(),
        vec![Block::image("active-root.png")]
    );
    assert!(
        !form.is_dirty(),
        "path adoption during construction belongs in the initial baseline"
    );
}

#[test]
fn external_title_edit_returns_stale_entity_without_discarding_typed_form() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let pos = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == "t-open")
        .unwrap();
    app.select_task(pos);
    app.open_edit_task();
    external
        .update(|data| {
            data.edit_task(
                "t-open",
                TaskPatch {
                    title: Some("agent title".into()),
                    ..TaskPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    replace_form_title(app, "human title");

    app.submit_form();

    let form = app.form.as_ref().expect("stale form stays open");
    assert_eq!(form.title.value(), "human title");
    assert!(
        form.error
            .as_deref()
            .is_some_and(|error| error.contains("changed since it was loaded")),
        "{:?}",
        form.error
    );
    assert_eq!(
        external
            .snapshot()
            .unwrap()
            .tasks
            .iter()
            .find(|task| task.id == "t-open")
            .unwrap()
            .title,
        "agent title"
    );
}

#[test]
fn task_reorder_keeps_its_entity_target_across_an_external_insertion() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(0);
    let a = app.create_task(&TaskDraft::new("a")).unwrap();
    let b = app.create_task(&TaskDraft::new("b")).unwrap();
    let c = app.create_task(&TaskDraft::new("c")).unwrap();
    let b_position = app
        .view
        .iter()
        .position(|index| app.tasks[*index].id == b)
        .unwrap();
    app.select_task(b_position);

    external
        .update(|data| {
            let inserted = data.create_task("inserted", Vec::new(), "", 0, None)?;
            data.move_task_relative(&inserted.id, &a, RelativePosition::Before)?;
            Ok(())
        })
        .unwrap();

    assert!(app.move_task_order(1));
    let snapshot = external.snapshot().unwrap();
    let b_position = snapshot.tasks.iter().position(|task| task.id == b).unwrap();
    let c_position = snapshot.tasks.iter().position(|task| task.id == c).unwrap();
    assert_eq!(b_position, c_position + 1, "b must remain targeted after c");
}

#[test]
fn category_reorder_keeps_its_entity_target_across_an_external_insertion() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    let work_id = app.categories[1].id.clone();
    let home_id = app.categories[2].id.clone();
    app.select_category(2);

    external
        .update(|data| {
            let inserted = data.create_category("Inserted", "")?;
            data.move_category(&inserted.id, 0)?;
            Ok(())
        })
        .unwrap();

    assert!(app.move_category_order(-1));
    let snapshot = external.snapshot().unwrap();
    let work_position = snapshot
        .categories
        .iter()
        .position(|category| category.id == work_id)
        .unwrap();
    let home_position = snapshot
        .categories
        .iter()
        .position(|category| category.id == home_id)
        .unwrap();
    assert_eq!(home_position + 1, work_position);
}

#[test]
fn category_form_merges_disjoint_human_and_agent_fields() {
    let mut pair = on_disk_pair();
    let (app, external) = (&mut pair.app, &mut pair.external);
    app.select_category(1);
    app.open_edit_category();
    external
        .update(|data| {
            data.edit_category(
                "c-work",
                CategoryPatch {
                    name: Some("Agent work".into()),
                    ..CategoryPatch::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    app.category_form
        .as_mut()
        .expect("category form open")
        .description
        .insert_str("human description");

    app.submit_category_form();

    assert!(app.category_form.is_none(), "disjoint fields should merge");
    let saved = app
        .categories
        .iter()
        .find(|category| category.id == "c-work")
        .unwrap();
    assert_eq!(saved.name, "Agent work");
    assert_eq!(saved.description, "human description");
}
