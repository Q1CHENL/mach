use std::path::{Path, PathBuf};

use mach::model::{Category, LabelColor, Task};
use mach::store::{LabelPatch, PurgeScope, RelativePosition, Store, StoreError, TaskPatch};
use sha2::{Digest, Sha256};

mod common;
use common::TempDir;

fn write_test_png(path: &Path, color: [u8; 4]) {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
    image.save(path).expect("write test PNG");
}

#[test]
fn stores_are_independent_and_reentrant() {
    let first_dir = TempDir::new("store-first");
    let second_dir = TempDir::new("store-second");
    let mut first = Store::open(first_dir.path()).expect("open first store");
    let second = Store::open(second_dir.path()).expect("open second store");

    first
        .update(|data| {
            data.tasks.push(Task::new("only first", 0, None, ""));
            Ok(())
        })
        .expect("write first store");

    assert_eq!(first.snapshot().unwrap().tasks.len(), 1);
    assert!(second.snapshot().unwrap().tasks.is_empty());
    assert_ne!(first.database_path(), second.database_path());
}

#[test]
fn immediate_transactions_preserve_concurrent_updates() {
    let dir = TempDir::new("store-concurrency");
    Store::open(dir.path()).expect("initialize store");
    let path = dir.path().to_path_buf();

    let workers: Vec<_> = (0..16)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).expect("open worker store");
                store
                    .update(|data| {
                        data.tasks
                            .push(Task::new(&format!("task {i}"), 0, None, ""));
                        Ok(())
                    })
                    .expect("commit worker update");
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker did not panic");
    }

    let store = Store::open(dir.path()).expect("reopen store");
    let data = store.snapshot().expect("read final snapshot");
    assert_eq!(data.tasks.len(), 16);
}

#[test]
fn snapshots_never_mix_revision_and_rows_from_different_commits() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = TempDir::new("snapshot-coherence");
    Store::open(dir.path()).expect("initialize store");
    let finished = Arc::new(AtomicBool::new(false));
    let writer_finished = Arc::clone(&finished);
    let writer_path = dir.path().to_path_buf();
    let writer = std::thread::spawn(move || {
        let mut store = Store::open(writer_path).expect("open writer");
        for index in 0..128 {
            store
                .update(|data| {
                    data.create_task(format!("task {index}"), Vec::new(), "", 0, None)?;
                    Ok(())
                })
                .expect("writer commit");
        }
        writer_finished.store(true, Ordering::Release);
    });

    let reader = Store::open(dir.path()).expect("open reader");
    while !finished.load(Ordering::Acquire) {
        let snapshot = reader.snapshot().expect("coherent snapshot");
        assert_eq!(snapshot.revision as usize, snapshot.tasks.len());
    }
    writer.join().expect("writer did not panic");
    let snapshot = reader.snapshot().expect("final snapshot");
    assert_eq!(snapshot.revision, 128);
    assert_eq!(snapshot.tasks.len(), 128);
}

#[test]
fn revision_increments_once_per_commit_and_not_on_failure() {
    let dir = TempDir::new("store-revision");
    let mut store = Store::open(dir.path()).expect("open store");
    assert_eq!(store.revision().unwrap(), 0);

    store
        .update(|data| {
            data.create_category("Work", "")?;
            Ok(())
        })
        .expect("first update");
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.snapshot().unwrap().revision, 1);

    let error = store
        .update(|data| {
            data.create_category("work", "duplicate")?;
            Ok(())
        })
        .expect_err("duplicate update must fail");
    assert!(matches!(error, StoreError::Validation(_)));
    assert_eq!(store.revision().unwrap(), 1);

    store
        .update(|data| {
            data.update_settings(|settings| settings.hide_done = true)?;
            Ok(())
        })
        .expect("second committed update");
    assert_eq!(store.revision().unwrap(), 2);
}

#[test]
fn ensure_only_advances_the_revision_when_it_creates_an_entity() {
    let dir = TempDir::new("store-ensure-revision");
    let mut store = Store::open(dir.path()).expect("open store");

    let (category, created) = store
        .ensure_category("Café", Some("Projects".to_string()))
        .expect("create category");
    assert!(created);
    assert_eq!(store.revision().unwrap(), 1);

    let (same_category, created) = store
        .ensure_category("CAFE\u{301}", None)
        .expect("return category");
    assert!(!created);
    assert_eq!(same_category.id, category.id);
    assert_eq!(store.revision().unwrap(), 1);

    let error = store
        .ensure_category("café", Some("Different".to_string()))
        .expect_err("conflicting description");
    assert!(matches!(
        error,
        StoreError::MetadataConflict {
            entity: "category",
            field: "description",
            ..
        }
    ));
    assert_eq!(store.revision().unwrap(), 1);

    let (label, created) = store
        .ensure_label("Maße", Some(LabelColor::Red))
        .expect("create label");
    assert!(created);
    assert_eq!(store.revision().unwrap(), 2);

    let (same_label, created) = store
        .ensure_label("MASSE", Some(LabelColor::Red))
        .expect("return label");
    assert!(!created);
    assert_eq!(same_label.id, label.id);
    assert_eq!(store.revision().unwrap(), 2);

    let error = store
        .ensure_label("masse", Some(LabelColor::Blue))
        .expect_err("conflicting color");
    assert!(matches!(
        error,
        StoreError::MetadataConflict {
            entity: "label",
            field: "color",
            ..
        }
    ));
    assert_eq!(store.revision().unwrap(), 2);
}

#[test]
fn editing_one_task_does_not_rewrite_unchanged_rows() {
    let dir = TempDir::new("incremental-task-write");
    let mut store = Store::open(dir.path()).unwrap();
    let (changed_id, untouched_id) = store
        .update(|data| {
            let changed = data.create_task("changed", Vec::new(), "", 0, None)?;
            let untouched = data.create_task("untouched", Vec::new(), "", 0, None)?;
            Ok((changed.id, untouched.id))
        })
        .unwrap();
    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    observer
        .execute_batch(
            "
            CREATE TABLE write_audit (
                operation TEXT NOT NULL,
                task_id TEXT NOT NULL
            ) STRICT;
            CREATE TRIGGER audit_task_insert AFTER INSERT ON tasks BEGIN
                INSERT INTO write_audit VALUES ('insert', NEW.id);
            END;
            CREATE TRIGGER audit_task_update AFTER UPDATE ON tasks BEGIN
                INSERT INTO write_audit VALUES ('update', NEW.id);
            END;
            CREATE TRIGGER audit_task_delete AFTER DELETE ON tasks BEGIN
                INSERT INTO write_audit VALUES ('delete', OLD.id);
            END;
            ",
        )
        .unwrap();
    drop(observer);

    store
        .update(|data| data.set_task_done(&changed_id, true))
        .unwrap();

    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    let writes: Vec<(String, String)> = observer
        .prepare("SELECT operation, task_id FROM write_audit ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(writes, vec![("update".into(), changed_id)]);
    assert!(!writes.iter().any(|(_, id)| id == &untouched_id));
}

#[test]
fn editing_task_labels_only_writes_that_tasks_assignment_rows() {
    let dir = TempDir::new("incremental-task-label-write");
    let mut store = Store::open(dir.path()).unwrap();
    let (changed_id, untouched_id, first_id, second_id) = store
        .update(|data| {
            let first = data.create_label("first")?;
            let second = data.create_label("second")?;
            let changed = data.create_task("changed", Vec::new(), "", 0, None)?;
            let untouched = data.create_task("untouched", Vec::new(), "", 0, None)?;
            data.set_task_labels(&changed.id, vec![first.id.clone()])?;
            data.set_task_labels(&untouched.id, vec![first.id.clone()])?;
            Ok((changed.id, untouched.id, first.id, second.id))
        })
        .unwrap();
    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    observer
        .execute_batch(
            "
            CREATE TABLE label_write_audit (
                entity TEXT NOT NULL,
                operation TEXT NOT NULL,
                id TEXT NOT NULL
            ) STRICT;
            CREATE TRIGGER audit_task_label_only_update AFTER UPDATE ON tasks BEGIN
                INSERT INTO label_write_audit VALUES ('task', 'update', NEW.id);
            END;
            CREATE TRIGGER audit_label_insert AFTER INSERT ON labels BEGIN
                INSERT INTO label_write_audit VALUES ('label', 'insert', NEW.id);
            END;
            CREATE TRIGGER audit_label_update AFTER UPDATE ON labels BEGIN
                INSERT INTO label_write_audit VALUES ('label', 'update', NEW.id);
            END;
            CREATE TRIGGER audit_label_delete AFTER DELETE ON labels BEGIN
                INSERT INTO label_write_audit VALUES ('label', 'delete', OLD.id);
            END;
            CREATE TRIGGER audit_task_label_insert AFTER INSERT ON task_labels BEGIN
                INSERT INTO label_write_audit
                VALUES ('task_label', 'insert', NEW.task_id || ':' || NEW.label_id);
            END;
            CREATE TRIGGER audit_task_label_delete AFTER DELETE ON task_labels BEGIN
                INSERT INTO label_write_audit
                VALUES ('task_label', 'delete', OLD.task_id || ':' || OLD.label_id);
            END;
            ",
        )
        .unwrap();
    drop(observer);

    store
        .update(|data| data.set_task_labels(&changed_id, vec![second_id.clone()]))
        .unwrap();

    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    let writes: Vec<(String, String, String)> = observer
        .prepare("SELECT entity, operation, id FROM label_write_audit ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        writes,
        vec![
            (
                "task_label".into(),
                "delete".into(),
                format!("{changed_id}:{first_id}"),
            ),
            (
                "task_label".into(),
                "insert".into(),
                format!("{changed_id}:{second_id}"),
            ),
        ]
    );
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        snapshot.task(&untouched_id).unwrap().label_ids,
        vec![first_id]
    );
}

#[test]
fn settings_only_updates_do_not_touch_task_rows() {
    let dir = TempDir::new("incremental-settings-write");
    let mut store = Store::open(dir.path()).unwrap();
    let task_id = store
        .update(|data| data.create_task("untouched", Vec::new(), "", 0, None))
        .unwrap()
        .id;
    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    observer
        .execute_batch(
            "
            CREATE TABLE task_write_count (writes INTEGER NOT NULL) STRICT;
            INSERT INTO task_write_count VALUES (0);
            CREATE TRIGGER count_task_update AFTER UPDATE ON tasks BEGIN
                UPDATE task_write_count SET writes = writes + 1;
            END;
            CREATE TRIGGER count_task_insert AFTER INSERT ON tasks BEGIN
                UPDATE task_write_count SET writes = writes + 1;
            END;
            CREATE TRIGGER count_task_delete AFTER DELETE ON tasks BEGIN
                UPDATE task_write_count SET writes = writes + 1;
            END;
            ",
        )
        .unwrap();
    drop(observer);

    store
        .update(|data| data.update_settings(|settings| settings.hide_done = true))
        .unwrap();

    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    let writes: i64 = observer
        .query_row("SELECT writes FROM task_write_count", [], |row| row.get(0))
        .unwrap();
    assert_eq!(writes, 0);
    assert_eq!(store.snapshot().unwrap().tasks[0].id, task_id);
}

#[test]
fn compare_and_swap_rejects_a_stale_snapshot() {
    let dir = TempDir::new("store-conflict");
    let mut stale = Store::open(dir.path()).expect("open stale handle");
    let expected = stale.revision().unwrap();
    let mut writer = Store::open(dir.path()).expect("open writer");
    writer
        .update(|data| {
            data.create_task("external", Vec::new(), "", 0, None)?;
            Ok(())
        })
        .expect("external update");

    let error = stale
        .update_if_revision(expected, |data| {
            data.create_task("stale", Vec::new(), "", 0, None)?;
            Ok(())
        })
        .expect_err("stale update must not overwrite external data");
    assert!(matches!(
        error,
        StoreError::Conflict {
            expected: 0,
            actual: 1
        }
    ));
    let data = stale.snapshot().unwrap();
    assert_eq!(data.tasks.len(), 1);
    assert_eq!(data.tasks[0].title, "external");
}

#[test]
fn labels_use_stable_unicode_caseless_identities() {
    let dir = TempDir::new("label-identities");
    let mut store = Store::open(dir.path()).expect("open store");
    let (cafe, street) = store
        .update(|data| {
            let cafe = data.create_label("  Café  ")?;
            let street = data.create_label("Maße")?;
            Ok((cafe, street))
        })
        .expect("create labels");

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.label(&cafe.id).unwrap().name, "Café");
    assert_eq!(snapshot.resolve_label_id("Cafe\u{301}").unwrap(), cafe.id);
    assert_eq!(snapshot.resolve_label_id("MASSE").unwrap(), street.id);
    assert_eq!(snapshot.resolve_label_id("mass").unwrap(), street.id);

    let duplicate = store
        .update(|data| data.create_label("Cafe\u{301}"))
        .expect_err("canonically equivalent label names must conflict");
    assert!(matches!(duplicate, StoreError::Validation(_)));
    let presentation_syntax = store
        .update(|data| data.create_label("#bug"))
        .expect_err("the display prefix is not part of a stored label name");
    assert!(
        presentation_syntax
            .to_string()
            .contains("must not start with '#'")
    );
}

#[test]
fn label_identity_key_is_persisted_enforced_and_verified() {
    let dir = TempDir::new("label-identity-key");
    let mut store = Store::open(dir.path()).expect("open store");
    let label = store
        .update(|data| data.create_label("Maße"))
        .expect("create label");
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let stored_key: String = connection
        .query_row(
            "SELECT name_key FROM labels WHERE id = ?1",
            [&label.id],
            |row| row.get(0),
        )
        .expect("read the persisted Unicode identity key");
    assert_eq!(stored_key, mach::model::label_name_key("Maße"));

    let duplicate = connection.execute(
        "INSERT INTO labels(id, position, name, name_key, color)
         VALUES ('duplicate', 1, 'MASSE', ?1, 'red')",
        [&stored_key],
    );
    assert!(
        duplicate.is_err(),
        "SQLite must enforce the same Unicode identity as the application"
    );

    connection
        .execute(
            "UPDATE labels SET name_key = 'wrong' WHERE id = ?1",
            [&label.id],
        )
        .unwrap();
    drop(connection);
    let error = store
        .snapshot()
        .expect_err("a mismatched persisted identity key is corruption");
    assert!(matches!(error, StoreError::Corrupt(_)));
    assert!(error.to_string().contains("identity key"));
}

#[test]
fn label_color_is_persisted_constrained_and_verified() {
    let dir = TempDir::new("label-color-storage");
    let mut store = Store::open(dir.path()).expect("open store");
    let label = store
        .update(|data| data.create_label_with_color("Bug", LabelColor::Purple))
        .expect("create colored label");
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let stored_color: String = connection
        .query_row(
            "SELECT color FROM labels WHERE id = ?1",
            [&label.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_color, "purple");

    let invalid = connection.execute(
        "UPDATE labels SET color = 'violet' WHERE id = ?1",
        [&label.id],
    );
    assert!(invalid.is_err(), "SQLite must reject unknown label colors");

    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    connection
        .execute(
            "UPDATE labels SET color = 'violet' WHERE id = ?1",
            [&label.id],
        )
        .unwrap();
    drop(connection);

    let error = store
        .snapshot()
        .expect_err("an unknown persisted color is corruption");
    assert!(matches!(error, StoreError::Corrupt(_)));
    assert!(error.to_string().contains("unknown color"));
}

#[test]
fn label_rename_preserves_identity_and_delete_only_unassigns() {
    let dir = TempDir::new("label-lifecycle");
    let mut store = Store::open(dir.path()).expect("open store");
    let (task_id, first_id, second_id) = store
        .update(|data| {
            let first = data.create_label("backend")?;
            let second = data.create_label("release")?;
            let task = data.create_task("ship", Vec::new(), "", 0, None)?;
            data.set_task_labels(&task.id, vec![second.id.clone(), first.id.clone()])?;
            Ok((task.id, first.id, second.id))
        })
        .expect("create and assign labels");

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.label(&first_id).unwrap().color, LabelColor::Red);
    assert_eq!(
        snapshot.label(&second_id).unwrap().color,
        LabelColor::Orange
    );
    assert_eq!(
        snapshot.task(&task_id).unwrap().label_ids,
        vec![first_id.clone(), second_id.clone()],
        "task labels follow the store's deterministic global label order"
    );

    let renamed = store
        .update(|data| {
            data.edit_label(
                &first_id,
                LabelPatch {
                    name: Some("server".into()),
                    color: Some(LabelColor::Purple),
                },
            )
        })
        .expect("edit label name and color atomically");
    assert_eq!(renamed.id, first_id);
    assert_eq!(renamed.color, LabelColor::Purple);
    assert_eq!(
        store.snapshot().unwrap().task(&task_id).unwrap().label_ids[0],
        first_id
    );

    let deleted = store
        .update(|data| data.delete_label(&first_id))
        .expect("delete label");
    assert_eq!(deleted.id, first_id);
    let reopened = Store::open(dir.path()).expect("reopen store");
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.task(&task_id).unwrap().label_ids, vec![second_id]);
}

#[test]
fn automatic_label_colors_balance_across_the_full_editor_palette() {
    let mut store = Store::open_in_memory_with_paths(
        std::env::temp_dir().join(format!("mach-label-palette-{}", uuid::Uuid::new_v4())),
    )
    .unwrap();
    let colors = store
        .update(|data| {
            (0..13)
                .map(|index| {
                    data.create_label(format!("label-{index}"))
                        .map(|label| label.color)
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap();

    assert_eq!(
        colors,
        [
            LabelColor::Red,
            LabelColor::Orange,
            LabelColor::Yellow,
            LabelColor::Lime,
            LabelColor::Green,
            LabelColor::Teal,
            LabelColor::Cyan,
            LabelColor::Blue,
            LabelColor::Indigo,
            LabelColor::Purple,
            LabelColor::Pink,
            LabelColor::Brown,
            LabelColor::Red,
        ]
    );
}

#[test]
fn invalid_task_label_assignment_is_atomic() {
    let dir = TempDir::new("label-assignment-atomicity");
    let mut store = Store::open(dir.path()).expect("open store");
    let task = store
        .update(|data| data.create_task("task", Vec::new(), "", 0, None))
        .unwrap();
    let revision = store.revision().unwrap();

    let error = store
        .update(|data| data.set_task_labels(&task.id, vec!["missing".into()]))
        .expect_err("unknown labels must fail the whole transaction");
    assert!(matches!(error, StoreError::Validation(_)));
    assert_eq!(store.revision().unwrap(), revision);
    assert!(
        store
            .snapshot()
            .unwrap()
            .task(&task.id)
            .unwrap()
            .label_ids
            .is_empty()
    );
}

#[test]
fn label_count_and_per_task_limits_are_enforced_atomically() {
    let dir = TempDir::new("label-limits");
    let mut store = Store::open(dir.path()).expect("open store");
    let (task_id, label_ids) = store
        .update(|data| {
            let mut label_ids = Vec::new();
            for index in 0..mach::model::MAX_LABEL_COUNT {
                label_ids.push(data.create_label(format!("label {index}"))?.id);
            }
            let task = data.create_task("task", Vec::new(), "", 0, None)?;
            Ok((task.id, label_ids))
        })
        .expect("fill the bounded label collection");
    let revision = store.revision().unwrap();

    let too_many_global = store
        .update(|data| data.create_label("overflow"))
        .expect_err("the global label limit must be enforced");
    assert!(too_many_global.to_string().contains("label limit"));
    let too_many_assigned = store
        .update(|data| {
            data.set_task_labels(
                &task_id,
                label_ids[..mach::model::MAX_LABELS_PER_TASK + 1].to_vec(),
            )
        })
        .expect_err("the per-task label limit must be enforced");
    assert!(too_many_assigned.to_string().contains("label limit"));

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, revision);
    assert_eq!(snapshot.labels.len(), mach::model::MAX_LABEL_COUNT);
    assert!(snapshot.task(&task_id).unwrap().label_ids.is_empty());
}

#[test]
fn conditional_task_label_edit_preserves_unrelated_external_changes() {
    let dir = TempDir::new("conditional-label-edit");
    let mut form_store = Store::open(dir.path()).unwrap();
    let (expected, second_id) = form_store
        .update(|data| {
            let first = data.create_label("first")?;
            let second = data.create_label("second")?;
            let task = data.create_task("task", Vec::new(), "", 0, None)?;
            data.set_task_labels(&task.id, vec![first.id.clone()])?;
            Ok((data.task(&task.id)?.clone(), second.id))
        })
        .unwrap();
    let mut external = Store::open(dir.path()).unwrap();
    external
        .update(|data| data.set_task_done(&expected.id, true))
        .unwrap();

    let (_, committed) = form_store
        .update_with_snapshot(|data| {
            data.edit_task_if_unchanged(
                &expected,
                TaskPatch {
                    label_ids: Some(vec![second_id.clone()]),
                    ..TaskPatch::default()
                },
            )
        })
        .expect("unrelated done edit should be preserved");
    let task = committed.task(&expected.id).unwrap();
    assert!(task.done);
    assert_eq!(task.label_ids, vec![second_id]);

    let error = form_store
        .update(|data| {
            data.edit_task_if_unchanged(
                &expected,
                TaskPatch {
                    label_ids: Some(Vec::new()),
                    ..TaskPatch::default()
                },
            )
        })
        .expect_err("divergent edits to labels must conflict");
    assert!(matches!(error, StoreError::StaleEntity { .. }));
}

#[test]
fn legacy_json_is_migrated_once_without_changing_source_files() {
    let dir = TempDir::new("legacy-migration");
    let categories = r#"{
      "schema": 3,
      "categories": [{"id":"work","name":"Work","description":"notes"}]
    }"#;
    let tasks = r#"{
      "schema": 3,
      "tasks": [{
        "id":"task-1",
        "title":"migrated",
        "body":[],
        "due":"8-9",
        "created":"2026-08-08 10:00:00",
        "done":false,
        "importance":1,
        "category_id":"work"
      }]
    }"#;
    let settings = r#"{
      "date_format":"D-M-Y",
      "selected_color":"cyan",
      "sort":"due",
      "preview_position":"right",
      "hide_done":true,
      "last_run_version":"0.1.1"
    }"#;
    std::fs::write(dir.path().join("categories.json"), categories).unwrap();
    std::fs::write(dir.path().join("tasks.json"), tasks).unwrap();
    std::fs::write(dir.path().join("settings.json"), settings).unwrap();

    let store = Store::open(dir.path()).expect("migrate legacy JSON");
    let data = store.snapshot().expect("read migrated data");
    assert_eq!(data.categories.len(), 1);
    assert_eq!(data.tasks.len(), 1);
    assert_eq!(data.tasks[0].title, "migrated");
    assert_eq!(data.tasks[0].category_id.as_deref(), Some("work"));
    assert!(data.tasks[0].due.starts_with("20"), "due was not absolute");
    assert_eq!(data.settings.date_format, "D-M-Y");
    assert_eq!(data.settings.last_update_check_at, None);

    assert_eq!(
        std::fs::read_to_string(dir.path().join("categories.json")).unwrap(),
        categories
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tasks.json")).unwrap(),
        tasks
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        settings
    );

    drop(store);
    let reopened = Store::open(dir.path()).expect("reopen migrated store");
    assert_eq!(reopened.snapshot().unwrap().tasks.len(), 1);
}

#[test]
fn legacy_image_paths_become_content_addressed_attachments() {
    let dir = TempDir::new("legacy-image-migration");
    let source_dir = TempDir::new("legacy-image-source");
    let source = source_dir.path().join("legacy.png");
    write_test_png(&source, [12, 34, 56, 255]);
    let source_bytes = std::fs::read(&source).unwrap();
    let digest = format!("{:x}", Sha256::digest(&source_bytes));
    let legacy = serde_json::json!({
        "schema": 3,
        "tasks": [{
            "id": "task-with-image",
            "title": "migrated image",
            "body": [{"type": "image", "path": source}],
            "due": "",
            "created": "2026-08-08 10:00:00",
            "done": false,
            "importance": 0,
            "category_id": null
        }]
    });
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    let legacy_path = dir.path().join("tasks.json");
    std::fs::write(&legacy_path, &legacy_bytes).unwrap();

    let store = Store::open(dir.path()).expect("migrate legacy image");
    let snapshot = store.snapshot().unwrap();
    let block = serde_json::to_value(&snapshot.tasks[0].description[0]).unwrap();
    assert_eq!(block["attachment_id"], digest);
    assert!(block.get("path").is_none(), "legacy paths must not persist");

    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let attachment: (String, String, i64, String) = connection
        .query_row(
            "SELECT sha256, media_type, byte_len, storage_name FROM attachments WHERE id = ?1",
            [&digest],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(attachment.0, digest);
    assert_eq!(attachment.1, "image/png");
    assert_eq!(attachment.2, source_bytes.len() as i64);
    assert_eq!(attachment.3, format!("{digest}.png"));
    let reference: (String, i64, String) = connection
        .query_row(
            "SELECT task_id, block_index, attachment_id FROM task_description_attachments",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(reference, ("task-with-image".into(), 0, digest.clone()));
    assert_eq!(
        std::fs::read(dir.path().join("images").join(format!("{digest}.png"))).unwrap(),
        source_bytes
    );
    assert_eq!(std::fs::read(legacy_path).unwrap(), legacy_bytes);
}

#[test]
fn identical_image_sources_share_one_managed_attachment() {
    let dir = TempDir::new("attachment-dedup");
    let sources = TempDir::new("attachment-dedup-sources");
    let first = sources.path().join("first.png");
    let second = sources.path().join("second.png");
    write_test_png(&first, [90, 80, 70, 255]);
    std::fs::copy(&first, &second).unwrap();

    let mut store = Store::open(dir.path()).unwrap();
    store
        .update(|data| {
            data.create_task(
                "first",
                vec![mach::model::Block::image(&first.to_string_lossy())],
                "",
                0,
                None,
            )?;
            data.create_task(
                "second",
                vec![mach::model::Block::image(&second.to_string_lossy())],
                "",
                0,
                None,
            )?;
            Ok(())
        })
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    let first_ref = serde_json::to_value(&snapshot.tasks[0].description[0]).unwrap();
    let second_ref = serde_json::to_value(&snapshot.tasks[1].description[0]).unwrap();
    assert_eq!(first_ref["attachment_id"], second_ref["attachment_id"]);
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let attachments: i64 = connection
        .query_row("SELECT count(*) FROM attachments", [], |row| row.get(0))
        .unwrap();
    let references: i64 = connection
        .query_row(
            "SELECT count(*) FROM task_description_attachments",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attachments, 1);
    assert_eq!(references, 2);
}

#[test]
fn category_descriptions_treat_image_syntax_as_text_only() {
    let dir = TempDir::new("category-description-text-only");
    let source = dir.path().join("category-source.png");
    write_test_png(&source, [40, 80, 120, 255]);
    let description = format!("before\n![image]({})\nafter", source.display());
    let mut store = Store::open(dir.path()).unwrap();

    store
        .update(|data| data.create_category("Visual notes", description.clone()))
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.categories[0].description, description);
    assert!(snapshot.attachments().is_empty());
}

#[test]
fn deleting_one_shared_reference_keeps_the_managed_attachment() {
    let dir = TempDir::new("attachment-shared-delete");
    let sources = TempDir::new("attachment-shared-delete-sources");
    let source = sources.path().join("shared.png");
    write_test_png(&source, [70, 80, 90, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    let (first, second) = store
        .update(|data| {
            let first = data.create_task(
                "first shared reference",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )?;
            let second = data.create_task(
                "second shared reference",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )?;
            Ok((first, second))
        })
        .unwrap();
    let attachment = store.snapshot().unwrap().attachments()[0].clone();
    let managed = store.images_dir().join(&attachment.storage_name);

    store.update(|data| data.delete_task(&first.id)).unwrap();

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].id, second.id);
    assert_eq!(snapshot.attachments(), &[attachment]);
    assert!(managed.is_file());
}

#[test]
fn deleting_the_last_reference_reclaims_its_managed_attachment() {
    let dir = TempDir::new("attachment-reclaim");
    let source_dir = TempDir::new("attachment-reclaim-source");
    let source = source_dir.path().join("temporary.png");
    write_test_png(&source, [22, 44, 66, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    let task = store
        .update(|data| {
            data.create_task(
                "temporary image",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )
        })
        .unwrap();
    let attachment = store.snapshot().unwrap().attachments()[0].clone();
    let managed = store.images_dir().join(&attachment.storage_name);
    assert!(managed.is_file());

    store
        .update(|data| data.delete_task(&task.id))
        .expect("delete the attachment's last task reference");

    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.tasks.is_empty());
    assert!(snapshot.attachments().is_empty());
    assert!(
        !managed.exists(),
        "unreferenced managed image must not remain on disk"
    );
}

#[test]
fn failed_database_write_reclaims_new_managed_attachment_file() {
    let dir = TempDir::new("attachment-database-failure");
    let source_dir = TempDir::new("attachment-database-failure-source");
    let source = source_dir.path().join("rollback.png");
    write_test_png(&source, [11, 33, 55, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    observer
        .execute_batch(
            "CREATE TRIGGER reject_task_insert
             BEFORE INSERT ON tasks
             BEGIN
                 SELECT RAISE(ABORT, 'forced task insert failure');
             END;",
        )
        .unwrap();
    drop(observer);

    let error = store
        .update(|data| {
            data.create_task(
                "must roll back",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )
        })
        .expect_err("database failure must abort the attachment import");
    assert!(matches!(error, StoreError::Database(_)));

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.tasks.is_empty());
    assert!(snapshot.attachments().is_empty());
    if store.images_dir().exists() {
        assert_eq!(
            std::fs::read_dir(store.images_dir()).unwrap().count(),
            0,
            "a failed database mutation must not leave managed image bytes"
        );
    }
}

#[test]
fn failed_deduplicated_write_keeps_the_preexisting_managed_attachment() {
    let dir = TempDir::new("attachment-dedup-database-failure");
    let source_dir = TempDir::new("attachment-dedup-database-failure-source");
    let source = source_dir.path().join("shared.png");
    write_test_png(&source, [12, 34, 56, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    let original = store
        .update(|data| {
            data.create_task(
                "original",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )
        })
        .unwrap();
    let attachment = store.snapshot().unwrap().attachments()[0].clone();
    let managed = store.images_dir().join(&attachment.storage_name);
    let original_bytes = std::fs::read(&managed).unwrap();
    let observer = rusqlite::Connection::open(store.database_path()).unwrap();
    observer
        .execute_batch(
            "CREATE TRIGGER reject_deduplicated_task_insert
             BEFORE INSERT ON tasks
             BEGIN
                 SELECT RAISE(ABORT, 'forced deduplicated task failure');
             END;",
        )
        .unwrap();
    drop(observer);

    let error = store
        .update(|data| {
            data.create_task(
                "must roll back",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )
        })
        .expect_err("the second task insert must fail");
    assert!(matches!(error, StoreError::Database(_)));

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].id, original.id);
    assert_eq!(snapshot.attachments(), &[attachment]);
    assert_eq!(std::fs::read(managed).unwrap(), original_bytes);
}

#[test]
fn reopening_reconciles_only_managed_or_temporary_orphan_files() {
    let dir = TempDir::new("attachment-reopen-reconciliation");
    let source_dir = TempDir::new("attachment-reopen-reconciliation-source");
    let source = source_dir.path().join("legacy.png");
    write_test_png(&source, [21, 43, 65, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    let task = store
        .update(|data| {
            data.create_task(
                "legacy orphan",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )
        })
        .unwrap();
    let attachment = store.snapshot().unwrap().attachments()[0].clone();
    let legacy_managed = store.images_dir().join(&attachment.storage_name);
    let orphaned_managed = store.images_dir().join(format!("{}.png", "a".repeat(64)));
    let stale_temp = store
        .images_dir()
        .join(format!(".mach-attachment-{}.tmp", uuid::Uuid::new_v4()));
    let arbitrary = store.images_dir().join("keep-user-source.png");
    let lookalike_temp = store.images_dir().join(".mach-attachment-not-a-uuid.tmp");
    std::fs::write(&orphaned_managed, b"orphaned managed bytes").unwrap();
    std::fs::write(&stale_temp, b"stale temporary bytes").unwrap();
    std::fs::write(&arbitrary, b"user source bytes").unwrap();
    std::fs::write(&lookalike_temp, b"not a managed temp name").unwrap();
    let database = store.database_path().to_path_buf();
    drop(store);

    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();
    connection
        .execute("DELETE FROM tasks WHERE id = ?1", [&task.id])
        .unwrap();
    drop(connection);

    let reopened = Store::open(dir.path()).expect("reconcile attachment storage on reopen");
    let snapshot = reopened.snapshot().unwrap();
    assert!(snapshot.tasks.is_empty());
    assert!(snapshot.attachments().is_empty());
    assert!(!legacy_managed.exists());
    assert!(!orphaned_managed.exists());
    assert!(!stale_temp.exists());
    assert!(arbitrary.is_file());
    assert!(lookalike_temp.is_file());
}

#[test]
fn failed_attachment_import_does_not_commit_the_task() {
    let dir = TempDir::new("attachment-import-failure");
    let mut store = Store::open(dir.path()).unwrap();
    let error = store
        .update(|data| {
            data.create_task(
                "missing image",
                vec![mach::model::Block::image("missing.png")],
                "",
                0,
                None,
            )?;
            Ok(())
        })
        .expect_err("missing source must abort the transaction");
    assert!(matches!(error, StoreError::Io { .. }));
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.tasks.is_empty());
    assert!(snapshot.attachments().is_empty());
    assert!(!dir.path().join("images").exists());
}

#[test]
fn task_attachment_rows_must_match_description_json() {
    let dir = TempDir::new("attachment-reference-coherence");
    let source = dir.path().join("source.png");
    write_test_png(&source, [1, 2, 3, 255]);
    let mut store = Store::open(dir.path()).unwrap();
    store
        .update(|data| {
            data.create_task(
                "image",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )?;
            Ok(())
        })
        .unwrap();
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    connection
        .execute("DELETE FROM task_description_attachments", [])
        .unwrap();

    let error = store
        .snapshot()
        .expect_err("missing relationship must be corrupt");
    assert!(
        matches!(error, StoreError::Corrupt(message) if message.contains("attachment reference"))
    );
}

#[test]
fn malformed_legacy_json_is_a_typed_error_and_is_left_untouched() {
    let dir = TempDir::new("malformed-legacy");
    let malformed = br#"{"schema":3,"tasks":["#;
    let path = dir.path().join("tasks.json");
    std::fs::write(&path, malformed).unwrap();

    let error = match Store::open(dir.path()) {
        Err(error) => error,
        Ok(_) => panic!("malformed legacy data must fail"),
    };
    match error {
        StoreError::Json {
            path: error_path, ..
        } => assert_eq!(error_path, path),
        other => panic!("expected StoreError::Json, got {other:?}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), malformed);
}

#[test]
fn unusable_data_directory_is_an_error_instead_of_an_empty_store() {
    let parent = TempDir::new("unusable-data-dir");
    let blocker = parent.path().join("not-a-directory");
    std::fs::write(&blocker, "file").unwrap();

    let error = match Store::open(blocker.join("mach")) {
        Err(error) => error,
        Ok(_) => panic!("path cannot be created"),
    };
    assert!(matches!(error, StoreError::Io { .. }));
}

#[test]
fn database_v1_description_names_are_migrated_without_losing_tasks() {
    let dir = TempDir::new("database-v1-description-migration");
    let source = dir.path().join("v1-source.png");
    write_test_png(&source, [24, 68, 120, 255]);
    let mut store = Store::open(dir.path()).expect("initialize current database");
    let task = store
        .update(|data| {
            data.create_task(
                "keep description",
                vec![
                    mach::model::Block::text("legacy details"),
                    mach::model::Block::image(&source.to_string_lossy()),
                ],
                "",
                0,
                None,
            )
        })
        .unwrap();
    let before = store.snapshot().unwrap();
    let expected_description = before.tasks[0].description.clone();
    let expected_attachment = before.attachments()[0].clone();
    let database = store.database_path().to_path_buf();
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "ALTER TABLE tasks RENAME COLUMN description_json TO body_json",
            [],
        )
        .unwrap();
    let has_current_relation: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'task_description_attachments'
            )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_current_relation {
        connection
            .execute(
                "ALTER TABLE task_description_attachments RENAME TO task_attachments",
                [],
            )
            .unwrap();
    }
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS task_description_attachments_by_attachment;
             CREATE INDEX IF NOT EXISTS task_attachments_by_attachment
                 ON task_attachments(attachment_id);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(dir.path()).expect("migrate database v1");
    let snapshot = store.snapshot().expect("read migrated database");
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].id, task.id);
    assert_eq!(snapshot.tasks[0].description, expected_description);
    assert_eq!(snapshot.attachments(), &[expected_attachment]);
    drop(store);

    let connection = rusqlite::Connection::open(database).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(tasks)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(columns.iter().any(|column| column == "description_json"));
    assert!(!columns.iter().any(|column| column == "body_json"));
    let relations: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('task_description_attachments', 'task_attachments')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(relations, 1);
    let current_relation: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'task_description_attachments'
            )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(current_relation);
    let attachment_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM task_description_attachments",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attachment_rows, 1);
}

#[test]
fn database_v2_is_migrated_to_labels_without_changing_existing_data() {
    let dir = TempDir::new("database-v2-label-migration");
    let mut store = Store::open(dir.path()).expect("initialize current database");
    let (category, task) = store
        .update(|data| {
            let category = data.create_category("Work", "preserved")?;
            let task = data.create_task(
                "existing task",
                vec![mach::model::Block::text("existing description")],
                "2026-08-20 09:30",
                2,
                Some(category.id.clone()),
            )?;
            data.update_settings(|settings| settings.hide_done = true)?;
            Ok((category, task))
        })
        .expect("create v2-compatible data");
    let before = store.snapshot().unwrap();
    let database = store.database_path().to_path_buf();
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP INDEX task_labels_by_label;
             DROP TABLE task_labels;
             DROP TABLE labels;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(dir.path()).expect("migrate database v2 to v3");
    let after = store.snapshot().expect("read migrated database");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.categories, before.categories);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.settings, before.settings);
    assert!(after.labels.is_empty());
    assert_eq!(
        after.category(&category.id).unwrap().description,
        "preserved"
    );
    assert_eq!(after.task(&task.id).unwrap().importance, 2);
    drop(store);

    let connection = rusqlite::Connection::open(database).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    for table in ["labels", "task_labels"] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
                 )",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "migration did not create {table}");
    }
}

#[test]
fn unknown_database_schema_is_rejected_without_being_downgraded() {
    let dir = TempDir::new("future-schema");
    let database = dir.path().join("mach.db");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);

    let error = match Store::open(dir.path()) {
        Err(error) => error,
        Ok(_) => panic!("future schema must be rejected"),
    };
    assert!(matches!(
        error,
        StoreError::UnsupportedDatabaseSchema {
            found: 99,
            expected: 3,
            ..
        }
    ));
    let connection = rusqlite::Connection::open(database).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 99);
}

#[test]
fn non_contiguous_database_positions_are_reported_as_corruption() {
    let dir = TempDir::new("non-contiguous-positions");
    let mut store = Store::open(dir.path()).expect("open store");
    let second = store
        .update(|data| {
            data.create_task("first", Vec::new(), "", 0, None)?;
            data.create_task("second", Vec::new(), "", 0, None)
        })
        .unwrap();
    let database = store.database_path().to_path_buf();
    drop(store);

    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute("UPDATE tasks SET position = 7 WHERE id = ?1", [&second.id])
        .unwrap();
    drop(connection);

    let store = Store::open(dir.path()).expect("structurally valid SQLite still opens");
    let error = store
        .snapshot()
        .expect_err("non-contiguous application order must not be normalized silently");
    assert!(matches!(error, StoreError::Corrupt(_)));
    assert!(error.to_string().contains("task position"));
}

#[test]
fn in_memory_store_uses_explicit_paths_without_touching_them() {
    let logical = PathBuf::from("/logical/mach-test-data");
    let mut store = Store::open_in_memory_with_paths(&logical).expect("open memory store");
    assert_eq!(store.data_dir(), logical);
    assert_eq!(store.images_dir(), logical.join("images"));
    store
        .update(|data| {
            data.create_task("memory", Vec::new(), "", 0, None)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(store.snapshot().unwrap().tasks.len(), 1);
}

#[test]
fn in_memory_store_rejects_attachment_import_without_touching_its_logical_path() {
    let parent = TempDir::new("in-memory-attachment");
    let data_dir = parent.path().join("logical-data");
    let source = parent.path().join("source.png");
    write_test_png(&source, [8, 7, 6, 255]);
    let mut store = Store::open_in_memory_with_paths(&data_dir).unwrap();

    let error = store
        .update(|data| {
            data.create_task(
                "image",
                vec![mach::model::Block::image(&source.to_string_lossy())],
                "",
                0,
                None,
            )?;
            Ok(())
        })
        .expect_err("an ephemeral store has nowhere durable to own attachment bytes");

    assert!(
        matches!(error, StoreError::Validation(message) if message.contains("persistent store"))
    );
    assert!(!data_dir.exists());
    assert_eq!(store.revision().unwrap(), 0);
}

#[test]
fn validation_rejects_case_insensitive_duplicate_category_names_atomically() {
    let dir = TempDir::new("duplicate-categories");
    let mut store = Store::open(dir.path()).expect("open store");

    let error = store
        .update(|data| {
            data.categories.push(Category::new("Work"));
            data.categories.push(Category::new("work"));
            Ok(())
        })
        .expect_err("duplicate names must fail");
    assert!(error.to_string().contains("category"));
    assert!(error.to_string().contains("unique"));
    assert!(store.snapshot().unwrap().categories.is_empty());
}

#[test]
fn category_identity_uses_nfkc_and_full_unicode_case_folding() {
    let dir = TempDir::new("unicode-category-identity");
    let mut store = Store::open(dir.path()).expect("open store");
    let (cafe_id, street_id) = store
        .update(|data| {
            let cafe = data.create_category("Café", "")?;
            let street = data.create_category("Maße", "")?;
            Ok((cafe.id, street.id))
        })
        .expect("create Unicode category names");

    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        snapshot.resolve_category_id("Cafe\u{301}").unwrap(),
        cafe_id
    );
    assert_eq!(snapshot.resolve_category_id("MASSE").unwrap(), street_id);
    assert_eq!(snapshot.resolve_category_id("mass").unwrap(), street_id);

    let decomposed = store
        .update(|data| data.create_category("Cafe\u{301}", ""))
        .expect_err("canonically equivalent names must conflict");
    assert!(matches!(decomposed, StoreError::Validation(_)));
    let expanded = store
        .update(|data| data.create_category("MASSE", ""))
        .expect_err("full-fold equivalent names must conflict");
    assert!(matches!(expanded, StoreError::Validation(_)));
}

#[test]
fn category_identity_key_is_persisted_enforced_and_verified() {
    let dir = TempDir::new("category-identity-key");
    let mut store = Store::open(dir.path()).expect("open store");
    let category = store
        .update(|data| data.create_category("Maße", ""))
        .expect("create category");
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let stored_key: String = connection
        .query_row(
            "SELECT name_key FROM categories WHERE id = ?1",
            [&category.id],
            |row| row.get(0),
        )
        .expect("read the persisted Unicode identity key");
    assert_eq!(stored_key, mach::model::caseless_key("Maße"));

    let duplicate = connection.execute(
        "INSERT INTO categories(id, position, name, name_key, description)
         VALUES ('duplicate', 1, 'MASSE', ?1, '')",
        [&stored_key],
    );
    assert!(
        duplicate.is_err(),
        "SQLite must enforce the same Unicode identity as the application"
    );

    connection
        .execute(
            "UPDATE categories SET name_key = 'wrong' WHERE id = ?1",
            [&category.id],
        )
        .unwrap();
    drop(connection);
    let error = store
        .snapshot()
        .expect_err("a mismatched persisted identity key is corruption");
    assert!(matches!(error, StoreError::Corrupt(_)));
    assert!(error.to_string().contains("identity key"));
}

#[test]
fn shared_category_delete_preserves_tasks_as_uncategorized() {
    let dir = TempDir::new("domain-category-delete");
    let mut store = Store::open(dir.path()).expect("open store");
    store
        .update(|data| {
            let category = data.create_category("Work", "")?;
            data.create_task("keep", Vec::new(), "", 0, Some(category.id.clone()))?;
            data.delete_category(&category.id)?;
            Ok(())
        })
        .expect("delete category");

    let data = store.snapshot().unwrap();
    assert!(data.categories.is_empty());
    assert_eq!(data.tasks.len(), 1);
    assert_eq!(data.tasks[0].category_id, None);
}

#[test]
fn incremental_categories_support_name_swaps_and_fk_safe_replacement() {
    let dir = TempDir::new("incremental-category-write");
    let mut store = Store::open(dir.path()).expect("open store");
    let (alpha_id, beta_id, task_id) = store
        .update(|data| {
            let alpha = data.create_category("Alpha", "first")?;
            let beta = data.create_category("Beta", "second")?;
            let task =
                data.create_task("categorized", Vec::new(), "", 0, Some(alpha.id.clone()))?;
            Ok((alpha.id, beta.id, task.id))
        })
        .unwrap();

    store
        .update(|data| {
            data.categories[0].name = "Beta".into();
            data.categories[1].name = "Alpha".into();
            Ok(())
        })
        .expect("UNIQUE category names must allow a transactional swap");
    let swapped = store.snapshot().unwrap();
    assert_eq!(swapped.category(&alpha_id).unwrap().name, "Beta");
    assert_eq!(swapped.category(&beta_id).unwrap().name, "Alpha");

    let replacement_id = store
        .update(|data| {
            data.delete_category(&alpha_id)?;
            let replacement = data.create_category("Beta", "replacement")?;
            data.set_task_category(&task_id, Some(replacement.id.clone()))?;
            Ok(replacement.id)
        })
        .expect("replace category while preserving valid foreign keys");
    let replaced = store.snapshot().unwrap();
    assert!(
        replaced
            .categories
            .iter()
            .all(|category| category.id != alpha_id)
    );
    assert_eq!(
        replaced.task(&task_id).unwrap().category_id.as_deref(),
        Some(replacement_id.as_str())
    );
}

#[test]
fn shared_reorder_and_purge_enforce_category_boundaries() {
    let dir = TempDir::new("domain-reorder-purge");
    let mut store = Store::open(dir.path()).expect("open store");
    let (first, second, other, work_id) = store
        .update(|data| {
            let work = data.create_category("Work", "")?;
            let home = data.create_category("Home", "")?;
            let first = data.create_task("first", Vec::new(), "", 0, Some(work.id.clone()))?;
            let mut other = data.create_task("other", Vec::new(), "", 0, Some(home.id.clone()))?;
            let second = data.create_task("second", Vec::new(), "", 0, Some(work.id.clone()))?;
            other = data.set_task_done(&other.id, true)?;
            Ok((first, second, other, work.id))
        })
        .unwrap();

    store
        .update(|data| data.move_task_relative(&second.id, &first.id, RelativePosition::Before))
        .expect("same-category reorder");
    assert_eq!(store.snapshot().unwrap().tasks[0].id, second.id);
    let error = store
        .update(|data| data.move_task(&first.id, 2))
        .expect_err("cross-category reorder must fail");
    assert!(matches!(error, StoreError::Validation(_)));

    let removed = store
        .update(|data| data.purge_completed(&PurgeScope::Category(work_id)))
        .unwrap();
    assert!(removed.is_empty());
    let removed = store
        .update(|data| data.purge_completed(&PurgeScope::All))
        .unwrap();
    assert_eq!(removed, vec![other]);
}

#[test]
fn conditional_task_edit_preserves_unrelated_external_changes() {
    let dir = TempDir::new("conditional-edit");
    let mut form_store = Store::open(dir.path()).unwrap();
    let expected = form_store
        .update(|data| data.create_task("old title", Vec::new(), "", 0, None))
        .unwrap();
    let mut external = Store::open(dir.path()).unwrap();
    external
        .update(|data| data.set_task_done(&expected.id, true))
        .unwrap();

    let (_, committed) = form_store
        .update_with_snapshot(|data| {
            data.edit_task_if_unchanged(
                &expected,
                TaskPatch {
                    title: Some("new title".into()),
                    ..TaskPatch::default()
                },
            )
        })
        .expect("unrelated done change should be preserved");
    let task = committed.task(&expected.id).unwrap();
    assert_eq!(task.title, "new title");
    assert!(task.done);

    let error = form_store
        .update(|data| {
            data.edit_task_if_unchanged(
                &expected,
                TaskPatch {
                    title: Some("another title".into()),
                    ..TaskPatch::default()
                },
            )
        })
        .expect_err("edited field changed since form opened");
    assert!(matches!(error, StoreError::StaleEntity { .. }));
}

#[test]
fn validation_rejects_controls_and_invalid_creation_timestamps() {
    let dir = TempDir::new("invalid-fields");
    let mut store = Store::open(dir.path()).expect("open store");
    let control_error = store
        .update(|data| {
            data.create_task("escape\u{009b}sequence", Vec::new(), "", 0, None)?;
            Ok(())
        })
        .expect_err("terminal control must be rejected");
    assert!(matches!(control_error, StoreError::Validation(_)));

    let timestamp_error = store
        .update(|data| {
            let mut task = Task::new("bad time", 0, None, "");
            task.created = "not a timestamp".into();
            data.insert_task(task)?;
            Ok(())
        })
        .expect_err("invalid timestamp must be rejected");
    assert!(matches!(timestamp_error, StoreError::Validation(_)));

    assert!(store.snapshot().unwrap().tasks.is_empty());
}

#[test]
fn length_limits_count_user_perceived_graphemes() {
    let dir = TempDir::new("grapheme-limits");
    let mut store = Store::open(dir.path()).expect("open store");
    let grapheme = "e\u{301}";
    let accepted = grapheme.repeat(mach::model::MAX_TITLE_LEN);
    store
        .update(|data| {
            data.create_task(accepted, Vec::new(), "", 0, None)?;
            Ok(())
        })
        .expect("combined graphemes count as one character each");

    let too_long = grapheme.repeat(mach::model::MAX_TITLE_LEN + 1);
    let error = store
        .update(|data| {
            data.create_task(too_long, Vec::new(), "", 0, None)?;
            Ok(())
        })
        .expect_err("grapheme limit still applies");
    assert!(matches!(error, StoreError::Validation(_)));
    assert_eq!(store.snapshot().unwrap().tasks.len(), 1);
}

#[test]
fn byte_limits_reject_pathological_single_graphemes() {
    let dir = TempDir::new("byte-limits");
    let mut store = Store::open(dir.path()).expect("open store");
    let pathological = format!("e{}", "\u{301}".repeat(10_000));

    let error = store
        .update(|data| data.create_task(pathological, Vec::new(), "", 0, None))
        .expect_err("one grapheme must not bypass the storage byte budget");
    assert!(matches!(error, StoreError::Validation(_)));
    assert!(store.snapshot().unwrap().tasks.is_empty());
}

#[cfg(unix)]
#[test]
fn data_directory_and_database_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let parent = TempDir::new("private-store");
    let data_dir = parent.path().join("data");
    let store = Store::open(&data_dir).expect("open store");

    let dir_mode = std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
    let db_mode = std::fs::metadata(store.database_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(db_mode, 0o600);
}

#[cfg(unix)]
#[test]
fn existing_data_directory_permissions_are_not_rewritten() {
    use std::os::unix::fs::PermissionsExt;

    let data_dir = TempDir::new("existing-directory-mode");
    std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut store = Store::open(data_dir.path()).expect("open an existing data directory");
    store
        .update(|data| data.create_task("private", Vec::new(), "", 0, None))
        .expect("write through WAL");

    let dir_mode = std::fs::metadata(data_dir.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let db_mode = std::fs::metadata(store.database_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o755,
        "mach must not chmod a caller-owned directory"
    );
    assert_eq!(db_mode, 0o600, "the mach-owned database remains private");
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = store.database_path().as_os_str().to_os_string();
        sidecar.push(suffix);
        let mode = std::fs::metadata(PathBuf::from(sidecar))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "SQLite {suffix} sidecar must remain private");
    }
}

#[test]
fn oversized_legacy_json_is_rejected_before_deserialization() {
    let dir = TempDir::new("oversized-legacy-json");
    let tasks = dir.path().join("tasks.json");
    let file = std::fs::File::create(&tasks).unwrap();
    file.set_len(128 * 1024 * 1024 + 1).unwrap();

    let error = match Store::open(dir.path()) {
        Ok(_) => panic!("oversized legacy JSON must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::Validation(_)));
    assert!(error.to_string().contains("128 MiB"));
    assert_eq!(
        std::fs::metadata(tasks).unwrap().len(),
        128 * 1024 * 1024 + 1
    );
}
