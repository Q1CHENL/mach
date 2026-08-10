use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mach::model::Task;
use mach::store::Store;
use sha2::{Digest, Sha256};

mod common;
use common::TempDir;

fn mach(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mach"))
        .arg("--dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("run mach")
}

#[test]
fn non_terminal_tui_request_fails_before_initializing_the_store() {
    let dir = TempDir::new("non-terminal-tui");
    let output = mach(dir.path(), &[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 terminal error"),
        "mach: an interactive terminal is required on stdin and stdout; use a CLI subcommand for scripts\n"
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read temporary data directory")
            .count(),
        0,
        "a rejected TUI request must not create or migrate user data"
    );
}

#[test]
fn json_errors_are_one_document_on_stdout_and_exit_nonzero() {
    let dir = TempDir::new("json-error");
    let output = mach(dir.path(), &["--json", "show", "missing"]);

    assert!(
        !output.status.success(),
        "an error must have a nonzero status"
    );
    assert!(
        output.stderr.is_empty(),
        "JSON mode must not mix human stderr into the machine contract: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("exactly one JSON error document");
    assert_eq!(value["ok"], false);
    assert!(value["error"].is_string());
}

#[test]
fn plain_output_does_not_emit_user_supplied_terminal_controls() {
    let dir = TempDir::new("terminal-controls");
    let title = "safe\u{1b}]52;c;SGVsbG8=\u{7}\nnext";
    let output = mach(dir.path(), &["--json", "add", title]);

    assert!(
        !output.status.success(),
        "single-line controls must be rejected"
    );
    assert!(!output.stdout.contains(&0x1b), "raw ESC reached stdout");
    assert!(!output.stdout.contains(&0x07), "raw BEL reached stdout");
    assert!(output.stderr.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON error document");
    assert_eq!(value["ok"], false);
}

#[test]
fn plain_cli_output_is_unstyled_for_pipes() {
    let dir = TempDir::new("plain-no-ansi");
    let added = mach(dir.path(), &["--json", "add", "plain", "--importance", "2"]);
    let task: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let id = task["id"].as_str().unwrap();
    assert!(mach(dir.path(), &["done", id]).status.success());

    let listed = mach(dir.path(), &["list"]);
    assert!(listed.status.success());
    assert!(!listed.stdout.contains(&0x1b));
}

#[test]
fn clap_errors_are_json_when_json_was_requested() {
    let dir = TempDir::new("json-clap-error");
    let output = mach(dir.path(), &["--json", "not-a-command"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON clap error document");
    assert_eq!(value["ok"], false);
    assert_eq!(value["kind"], "usage");
}

#[test]
fn alternate_text_inputs_conflict_instead_of_silently_overriding_each_other() {
    let dir = TempDir::new("ambiguous-text-input");
    let add = mach(
        dir.path(),
        &["--json", "add", "positional", "--title", "flag"],
    );
    assert_eq!(add.status.code(), Some(2));
    let error: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    assert_eq!(error["kind"], "usage");

    let task = mach(dir.path(), &["--json", "add", "parent"]);
    let task: serde_json::Value = serde_json::from_slice(&task.stdout).unwrap();
    let id = task["id"].as_str().unwrap();
    let subtask = mach(
        dir.path(),
        &[
            "--json",
            "subtasks",
            id,
            "add",
            "positional",
            "--text",
            "flag",
        ],
    );
    assert_eq!(subtask.status.code(), Some(2));
    let error: serde_json::Value = serde_json::from_slice(&subtask.stdout).unwrap();
    assert_eq!(error["kind"], "usage");
}

#[test]
fn plain_clap_errors_escape_untrusted_controls() {
    let dir = TempDir::new("plain-clap-controls");
    let output = mach(dir.path(), &["bad\u{1b}]52;c;x\u{7}\u{009b}\nnext"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.contains(&0x1b));
    assert!(!output.stderr.contains(&0x07));
    let error = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(!error.contains('\u{009b}'));
    assert_eq!(error.lines().count(), 1, "argument injected an error line");
}

#[test]
fn version_is_one_json_document_when_requested() {
    let dir = TempDir::new("json-version");
    let output = mach(dir.path(), &["--json", "--version"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON version document");
    assert_eq!(value["ok"], true);
    assert!(value["version"].is_string());
}

#[test]
fn plain_version_is_one_conventional_line() {
    let dir = TempDir::new("plain-version");
    let output = mach(dir.path(), &["--version"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("mach v{}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn closed_stdout_pipe_is_a_normal_exit() {
    let dir = TempDir::new("broken-pipe");
    let mut store = Store::open(dir.path()).unwrap();
    store
        .update(|data| {
            for index in 0..512 {
                data.tasks.push(Task::new(
                    &format!("task-{index}-{}", "x".repeat(180)),
                    0,
                    None,
                    "",
                ));
            }
            Ok(())
        })
        .unwrap();
    drop(store);

    let mut child = Command::new(env!("CARGO_BIN_EXE_mach"))
        .arg("--dir")
        .arg(dir.path())
        .args(["--json", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mach");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait for mach");
    assert!(
        output.status.success(),
        "broken pipe should be normal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn category_delete_keeps_tasks_uncategorized() {
    let dir = TempDir::new("category-delete");
    assert!(
        mach(dir.path(), &["categories", "add", "Work"])
            .status
            .success()
    );
    let added = mach(
        dir.path(),
        &["--json", "add", "keep me", "--category", "Work"],
    );
    assert!(added.status.success());
    let task: serde_json::Value = serde_json::from_slice(&added.stdout).expect("task JSON");
    let id = task["id"].as_str().expect("task id");

    assert!(
        mach(dir.path(), &["categories", "delete", "Work"])
            .status
            .success()
    );
    let shown = mach(dir.path(), &["--json", "show", id]);
    assert!(shown.status.success(), "category deletion removed the task");
    let task: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("task JSON");
    assert!(task["category"]["id"].is_null());
    assert!(
        task["category"]["name"].is_null(),
        "machine output must not use a presentation glyph as a null sentinel"
    );
}

#[test]
fn empty_repeatable_subtasks_are_rejected_instead_of_succeeding_silently() {
    let dir = TempDir::new("empty-repeatable-subtask");
    let output = mach(dir.path(), &["--json", "add", "parent", "--subtask", "   "]);

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["kind"], "validation");

    let listed = mach(dir.path(), &["--json", "list"]);
    let tasks: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(tasks, serde_json::json!([]));
}

#[test]
fn documented_hyphen_leading_body_value_is_accepted_normally() {
    let dir = TempDir::new("hyphen-leading-body");
    let output = mach(
        dir.path(),
        &["--json", "add", "bullet body", "--body", "- first"],
    );

    assert!(
        output.status.success(),
        "documented bullet markup was rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["body"], "- first");
}

#[test]
fn missing_body_value_does_not_consume_the_next_flag_or_mutate() {
    let dir = TempDir::new("missing-body-before-flag");
    let added = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "keep me",
            "--body",
            "original body",
            "--due",
            "2026-08-20",
        ],
    );
    assert!(added.status.success());
    let task: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let id = task["id"].as_str().unwrap();

    let edited = mach(dir.path(), &["--json", "edit", id, "--body", "--clear-due"]);

    assert!(!edited.status.success());
    let error: serde_json::Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(error["kind"], "usage");
    let shown = mach(dir.path(), &["--json", "show", id]);
    assert!(shown.status.success());
    let unchanged: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(unchanged["body"], "original body");
    assert_eq!(unchanged["due"], "2026-08-20");
}

#[test]
fn explicit_equals_preserves_option_shaped_body_text() {
    let dir = TempDir::new("option-shaped-body");
    let output = mach(
        dir.path(),
        &["--json", "add", "literal option", "--body=--clear-due"],
    );

    assert!(
        output.status.success(),
        "explicit body value was rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["body"], "--clear-due");
}

#[test]
fn concurrent_cli_adds_do_not_lose_updates() {
    let dir = TempDir::new("cli-concurrency");
    assert!(mach(dir.path(), &["--json", "list"]).status.success());
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_mach"));
    let path = dir.path().to_path_buf();
    let workers: Vec<_> = (0..16)
        .map(|index| {
            let executable = executable.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                Command::new(executable)
                    .arg("--dir")
                    .arg(path)
                    .arg("--json")
                    .arg("add")
                    .arg(format!("task-{index}"))
                    .output()
                    .expect("run concurrent add")
            })
        })
        .collect();
    for worker in workers {
        let output = worker.join().expect("worker did not panic");
        assert!(
            output.status.success(),
            "concurrent add failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .expect("each add emits one JSON document");
    }

    let output = mach(dir.path(), &["--json", "list"]);
    assert!(output.status.success());
    let tasks: serde_json::Value = serde_json::from_slice(&output.stdout).expect("task list JSON");
    assert_eq!(tasks.as_array().expect("task array").len(), 16);
}

#[test]
fn task_category_and_subtask_workflow_keeps_one_json_contract() {
    let dir = TempDir::new("cli-workflow");
    let images = dir.path().join("images");
    std::fs::create_dir(&images).unwrap();
    let source = images.join("foo.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([4, 3, 2, 255]))
        .save(&source)
        .unwrap();
    let attachment_id = format!("{:x}", Sha256::digest(std::fs::read(&source).unwrap()));
    let category = mach(
        dir.path(),
        &[
            "--json",
            "categories",
            "add",
            "Work",
            "--description",
            "Projects",
        ],
    );
    assert!(category.status.success());
    let category: serde_json::Value = serde_json::from_slice(&category.stdout).unwrap();
    assert_eq!(category["name"], "Work");

    let added = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "Ship it",
            "--category",
            "Wo",
            "--due",
            "2099-12-31",
            "--time",
            "09:30",
            "--body",
            "note\n[ ] first\n- bullet\n1. numbered\nhttps://example.com\n[image:foo.png]",
            "--subtask",
            "second",
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stdout)
    );
    let task: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["category"]["name"], "Work");
    assert_eq!(task["due"], "2099-12-31 09:30");
    assert_eq!(task["subtasks_total"], 2);
    assert!(
        task["body"]
            .as_str()
            .unwrap()
            .contains(&format!("[image:{attachment_id}]"))
    );

    let edited = mach(
        dir.path(),
        &[
            "--json",
            "edit",
            &id,
            "--title",
            "Ship safely",
            "--importance",
            "3",
            "--time",
            "10:00",
        ],
    );
    assert!(edited.status.success());
    let task: serde_json::Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(task["title"], "Ship safely");
    assert_eq!(task["importance"], 3);
    assert_eq!(task["due"], "2099-12-31 10:00");

    let toggled = mach(dir.path(), &["--json", "subtasks", &id, "toggle", "1"]);
    assert!(toggled.status.success());
    let toggled: serde_json::Value = serde_json::from_slice(&toggled.stdout).unwrap();
    assert_eq!(toggled["done"], true);

    let renamed = mach(
        dir.path(),
        &["--json", "subtasks", &id, "edit", "2", "second revised"],
    );
    assert!(renamed.status.success());
    let renamed: serde_json::Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(renamed["text"], "second revised");

    assert!(
        mach(dir.path(), &["--json", "categories", "delete", "Work"])
            .status
            .success()
    );
    let shown = mach(dir.path(), &["--json", "show", &id]);
    assert!(shown.status.success());
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert!(shown["category"]["id"].is_null());
    assert_eq!(shown["subtasks"][0]["done"], true);
    assert_eq!(shown["subtasks"][1]["text"], "second revised");
}

#[test]
fn trailing_inline_due_is_shared_by_cli_but_mid_title_brackets_are_literal() {
    let dir = TempDir::new("cli-inline-due");
    let added = mach(dir.path(), &["--json", "add", "pay rent [2099-01-02]"]);
    assert!(added.status.success());
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    assert_eq!(added["title"], "pay rent");
    assert_eq!(added["due"], "2099-01-02");

    let literal = mach(dir.path(), &["--json", "add", "plan [2099-01-03] launch"]);
    assert!(literal.status.success());
    let literal: serde_json::Value = serde_json::from_slice(&literal.stdout).unwrap();
    assert_eq!(literal["title"], "plan [2099-01-03] launch");
    assert_eq!(literal["due"], "");

    let id = added["id"].as_str().unwrap();
    let edited = mach(
        dir.path(),
        &["--json", "edit", id, "--title", "pay later [2099-02-03]"],
    );
    assert!(edited.status.success());
    let edited: serde_json::Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(edited["title"], "pay later");
    assert_eq!(edited["due"], "2099-02-03");
}

#[test]
fn malformed_legacy_store_error_uses_json_channel() {
    let dir = TempDir::new("cli-malformed-legacy");
    std::fs::write(dir.path().join("tasks.json"), "{broken").unwrap();

    let output = mach(dir.path(), &["--json", "list"]);
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["kind"], "legacy_json");
}

#[test]
fn move_and_purge_expose_manual_order_and_completed_cleanup() {
    let dir = TempDir::new("cli-move-purge");
    assert!(
        mach(dir.path(), &["categories", "add", "Work"])
            .status
            .success()
    );
    assert!(
        mach(dir.path(), &["categories", "add", "Home"])
            .status
            .success()
    );
    let add = |title: &str, category: &str| {
        let output = mach(
            dir.path(),
            &["--json", "add", title, "--category", category],
        );
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let first = add("first", "Work");
    let home = add("home", "Home");
    let second = add("second", "Work");

    let moved = mach(dir.path(), &["--json", "move", &second, "--before", &first]);
    assert!(moved.status.success());
    let moved: serde_json::Value = serde_json::from_slice(&moved.stdout).unwrap();
    assert_eq!(moved["relation"], "before");
    let listed = mach(dir.path(), &["--json", "list", "--category", "Work"]);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed[0]["title"], "second");
    assert_eq!(listed[1]["title"], "first");

    let cross_category = mach(dir.path(), &["--json", "move", &first, "--after", &home]);
    assert!(!cross_category.status.success());
    let error: serde_json::Value = serde_json::from_slice(&cross_category.stdout).unwrap();
    assert_eq!(error["kind"], "validation");

    assert!(mach(dir.path(), &["done", &first]).status.success());
    assert!(mach(dir.path(), &["done", &home]).status.success());
    let missing_interlock = mach(dir.path(), &["--json", "purge"]);
    assert_eq!(missing_interlock.status.code(), Some(2));
    let purged = mach(
        dir.path(),
        &["--json", "purge", "--done", "--category", "Work"],
    );
    assert!(purged.status.success());
    let purged: serde_json::Value = serde_json::from_slice(&purged.stdout).unwrap();
    assert_eq!(purged["count"], 1);

    let remaining = mach(dir.path(), &["--json", "list"]);
    let remaining: serde_json::Value = serde_json::from_slice(&remaining.stdout).unwrap();
    assert_eq!(remaining.as_array().unwrap().len(), 2);
    assert!(
        remaining
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == home)
    );

    let purged = mach(dir.path(), &["--json", "purge", "--done"]);
    assert!(purged.status.success());
    let purged: serde_json::Value = serde_json::from_slice(&purged.stdout).unwrap();
    assert_eq!(purged["count"], 1);
}

#[test]
fn archive_round_trip_preserves_tasks_categories_and_images() {
    let source = TempDir::new("archive-source");
    let destination = TempDir::new("archive-destination");
    let output = TempDir::new("archive-output");
    let archive = output.path().join("tasks.mach");
    let screenshot = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/screenshot.png");
    let body = format!(
        "notes\n[ ] open step\n- bullet\n1. numbered\nhttps://example.com\n[image:{}]",
        screenshot.display()
    );

    assert!(
        mach(
            source.path(),
            &["categories", "add", "Work", "--description", "work notes",],
        )
        .status
        .success()
    );
    assert!(
        mach(
            source.path(),
            &[
                "categories",
                "add",
                "Empty",
                "--description",
                "kept without tasks",
            ],
        )
        .status
        .success()
    );
    let added = mach(
        source.path(),
        &[
            "--json",
            "add",
            "portable task",
            "--category",
            "Work",
            "--body",
            &body,
            "--due",
            "2026-08-10",
            "--importance",
            "2",
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let task_id = added["id"].as_str().unwrap();
    assert!(mach(source.path(), &["done", task_id]).status.success());

    let exported = mach(
        source.path(),
        &["--json", "export", archive.to_str().unwrap()],
    );
    assert!(
        exported.status.success(),
        "export failed: stdout={} stderr={}",
        String::from_utf8_lossy(&exported.stdout),
        String::from_utf8_lossy(&exported.stderr)
    );
    let exported: serde_json::Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(exported["tasks"], 1);
    assert_eq!(exported["categories"], 2);
    assert_eq!(exported["images"], 1);

    let imported = mach(
        destination.path(),
        &["--json", "import", archive.to_str().unwrap()],
    );
    assert!(
        imported.status.success(),
        "import failed: stdout={} stderr={}",
        String::from_utf8_lossy(&imported.stdout),
        String::from_utf8_lossy(&imported.stderr)
    );
    let imported: serde_json::Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(imported["tasks_added"], 1);
    assert_eq!(imported["categories_added"], 2);
    assert_eq!(imported["images_added"], 1);

    let source_store = Store::open(source.path()).unwrap();
    let source_snapshot = source_store.snapshot().unwrap();
    let destination_store = Store::open(destination.path()).unwrap();
    let destination_snapshot = destination_store.snapshot().unwrap();
    assert_eq!(destination_snapshot.categories, source_snapshot.categories);
    assert_eq!(destination_snapshot.tasks, source_snapshot.tasks);
    assert_eq!(
        destination_snapshot.attachments(),
        source_snapshot.attachments()
    );
    let image = &destination_snapshot.attachments()[0];
    assert_eq!(
        std::fs::read(destination_store.images_dir().join(&image.storage_name)).unwrap(),
        std::fs::read(screenshot).unwrap()
    );
}

#[test]
fn archive_import_is_idempotent_and_rejects_conflicting_ids_atomically() {
    let source = TempDir::new("archive-merge-source");
    let destination = TempDir::new("archive-merge-destination");
    let output = TempDir::new("archive-merge-output");
    let archive = output.path().join("tasks.mach");

    let added = mach(source.path(), &["--json", "add", "from archive"]);
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let imported_id = added["id"].as_str().unwrap();
    assert!(
        mach(source.path(), &["export", archive.to_str().unwrap()],)
            .status
            .success()
    );
    assert!(
        mach(destination.path(), &["add", "already here"])
            .status
            .success()
    );

    let first = mach(
        destination.path(),
        &["--json", "import", archive.to_str().unwrap()],
    );
    assert!(first.status.success());
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["tasks_added"], 1);
    assert_eq!(first["tasks_unchanged"], 0);
    let after_first = Store::open(destination.path()).unwrap().snapshot().unwrap();

    let second = mach(
        destination.path(),
        &["--json", "import", archive.to_str().unwrap()],
    );
    assert!(second.status.success());
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["tasks_added"], 0);
    assert_eq!(second["tasks_unchanged"], 1);
    let after_second = Store::open(destination.path()).unwrap().snapshot().unwrap();
    assert_eq!(after_second.revision, after_first.revision);
    assert_eq!(after_second.tasks, after_first.tasks);

    assert!(
        mach(
            destination.path(),
            &["edit", imported_id, "--title", "changed locally"],
        )
        .status
        .success()
    );
    let before = Store::open(destination.path()).unwrap().snapshot().unwrap();
    let conflicted = mach(
        destination.path(),
        &["--json", "import", archive.to_str().unwrap()],
    );
    assert!(!conflicted.status.success());
    let error: serde_json::Value = serde_json::from_slice(&conflicted.stdout).unwrap();
    assert_eq!(error["kind"], "conflict");
    assert!(error["error"].as_str().unwrap().contains(imported_id));

    let after = Store::open(destination.path()).unwrap().snapshot().unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.categories, before.categories);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.attachments(), before.attachments());
}

#[test]
fn failed_archive_database_merge_does_not_install_image_files() {
    let source = TempDir::new("archive-rollback-source");
    let destination = TempDir::new("archive-rollback-destination");
    let output = TempDir::new("archive-rollback-output");
    let archive = output.path().join("tasks.mach");
    let screenshot = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/screenshot.png");
    let body = format!("[image:{}]", screenshot.display());

    let added = mach(source.path(), &["--json", "add", "image", "--body", &body]);
    assert!(added.status.success());
    assert!(
        mach(
            source.path(),
            &["export", archive.to_str().expect("UTF-8 archive path")],
        )
        .status
        .success()
    );
    assert!(
        mach(destination.path(), &["--json", "list"])
            .status
            .success()
    );
    let store = Store::open(destination.path()).unwrap();
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_imported_task
             BEFORE INSERT ON tasks
             BEGIN
                 SELECT RAISE(ABORT, 'forced archive merge failure');
             END;",
        )
        .unwrap();
    drop(connection);
    drop(store);

    let imported = mach(
        destination.path(),
        &["--json", "import", archive.to_str().unwrap()],
    );
    assert!(!imported.status.success());
    let error: serde_json::Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(error["kind"], "database");

    let store = Store::open(destination.path()).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.tasks.is_empty());
    assert!(snapshot.attachments().is_empty());
    if store.images_dir().exists() {
        assert_eq!(
            std::fs::read_dir(store.images_dir()).unwrap().count(),
            0,
            "failed archive merge must not leave managed image bytes"
        );
    }
}
