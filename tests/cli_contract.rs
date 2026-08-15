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
fn plain_output_escapes_bidirectional_formatting_controls() {
    let dir = TempDir::new("terminal-bidi-controls");
    let title = "safe\u{202e}spoof";
    let added = mach(dir.path(), &["--json", "add", title]);
    assert!(added.status.success());

    let listed = mach(dir.path(), &["list"]);
    assert!(listed.status.success());
    let listed = String::from_utf8(listed.stdout).expect("UTF-8 list output");
    assert!(!listed.contains('\u{202e}'));
    assert!(listed.contains("safe\\u{202e}spoof"));
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
fn task_description_is_the_only_public_cli_and_json_name() {
    let dir = TempDir::new("task-description-name");
    let output = mach(
        dir.path(),
        &["--json", "add", "named cleanly", "--description", "details"],
    );

    assert!(
        output.status.success(),
        "--description was rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["description"], "details");
    assert!(task.get("body").is_none());

    let old_name = mach(
        dir.path(),
        &["--json", "add", "old name", "--body", "details"],
    );
    assert!(!old_name.status.success());
    let error: serde_json::Value = serde_json::from_slice(&old_name.stdout).unwrap();
    assert_eq!(error["kind"], "usage");
}

#[test]
fn documented_hyphen_leading_description_value_is_accepted_normally() {
    let dir = TempDir::new("hyphen-leading-description");
    let output = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "bullet description",
            "--description",
            "- first",
        ],
    );

    assert!(
        output.status.success(),
        "documented bullet markup was rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["description"], "- first");
}

#[test]
fn missing_description_value_does_not_consume_the_next_flag_or_mutate() {
    let dir = TempDir::new("missing-description-before-flag");
    let added = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "keep me",
            "--description",
            "original description",
            "--due",
            "2026-08-20",
        ],
    );
    assert!(added.status.success());
    let task: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let id = task["id"].as_str().unwrap();

    let edited = mach(
        dir.path(),
        &["--json", "edit", id, "--description", "--clear-due"],
    );

    assert!(!edited.status.success());
    let error: serde_json::Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(error["kind"], "usage");
    let shown = mach(dir.path(), &["--json", "show", id]);
    assert!(shown.status.success());
    let unchanged: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(unchanged["description"], "original description");
    assert_eq!(unchanged["due"], "2026-08-20");
}

#[test]
fn explicit_equals_preserves_option_shaped_description_text() {
    let dir = TempDir::new("option-shaped-description");
    let output = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "literal option",
            "--description=--clear-due",
        ],
    );

    assert!(
        output.status.success(),
        "explicit description value was rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["description"], "--clear-due");
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
            "--description",
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
    assert_eq!(task["labels"], serde_json::json!([]));
    assert_eq!(task["due"], "2099-12-31 09:30");
    assert_eq!(task["subtasks_total"], 2);
    assert!(
        task["description"]
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
fn ensure_commands_create_or_return_exact_name_identities_and_reject_metadata_conflicts() {
    let dir = TempDir::new("cli-ensure");

    let category = mach(
        dir.path(),
        &[
            "--json",
            "categories",
            "ensure",
            "Café",
            "--description",
            "Projects",
        ],
    );
    assert!(category.status.success());
    let category: serde_json::Value = serde_json::from_slice(&category.stdout).unwrap();
    assert_eq!(category["created"], true);
    assert_eq!(category["category"]["name"], "Café");
    assert_eq!(category["category"]["description"], "Projects");
    let category_id = category["category"]["id"].as_str().unwrap();

    let category_again = mach(
        dir.path(),
        &[
            "--json",
            "categories",
            "ensure",
            "CAFE\u{301}",
            "--description",
            "Projects",
        ],
    );
    assert!(category_again.status.success());
    let category_again: serde_json::Value = serde_json::from_slice(&category_again.stdout).unwrap();
    assert_eq!(category_again["created"], false);
    assert_eq!(category_again["category"]["id"], category_id);
    assert_eq!(category_again["category"]["name"], "Café");

    let category_conflict = mach(
        dir.path(),
        &[
            "--json",
            "categories",
            "ensure",
            "café",
            "--description",
            "Different",
        ],
    );
    assert!(!category_conflict.status.success());
    let category_conflict: serde_json::Value =
        serde_json::from_slice(&category_conflict.stdout).unwrap();
    assert_eq!(category_conflict["kind"], "conflict");

    let label = mach(
        dir.path(),
        &["--json", "labels", "ensure", "Maße", "--color", "red"],
    );
    assert!(label.status.success());
    let label: serde_json::Value = serde_json::from_slice(&label.stdout).unwrap();
    assert_eq!(label["created"], true);
    assert_eq!(label["label"]["name"], "Maße");
    assert_eq!(label["label"]["color"], "red");
    let label_id = label["label"]["id"].as_str().unwrap();

    let label_again = mach(
        dir.path(),
        &["--json", "labels", "ensure", "MASSE", "--color", "red"],
    );
    assert!(label_again.status.success());
    let label_again: serde_json::Value = serde_json::from_slice(&label_again.stdout).unwrap();
    assert_eq!(label_again["created"], false);
    assert_eq!(label_again["label"]["id"], label_id);

    let label_conflict = mach(
        dir.path(),
        &["--json", "labels", "ensure", "masse", "--color", "blue"],
    );
    assert!(!label_conflict.status.success());
    let label_conflict: serde_json::Value = serde_json::from_slice(&label_conflict.stdout).unwrap();
    assert_eq!(label_conflict["kind"], "conflict");

    assert!(
        mach(dir.path(), &["labels", "add", "Backend"])
            .status
            .success()
    );
    let exact_not_prefix = mach(dir.path(), &["--json", "labels", "ensure", "Back"]);
    assert!(exact_not_prefix.status.success());
    let exact_not_prefix: serde_json::Value =
        serde_json::from_slice(&exact_not_prefix.stdout).unwrap();
    assert_eq!(exact_not_prefix["created"], true);
    assert_eq!(exact_not_prefix["label"]["name"], "Back");
}

#[test]
fn label_crud_preserves_identity_reports_counts_and_only_unassigns_tasks() {
    let dir = TempDir::new("cli-label-crud");

    let bug = mach(
        dir.path(),
        &["--json", "labels", "add", "Bug", "--color", "red"],
    );
    assert!(bug.status.success());
    let bug: serde_json::Value = serde_json::from_slice(&bug.stdout).unwrap();
    let bug_id = bug["id"].as_str().unwrap().to_string();
    assert_eq!(bug["name"], "Bug");
    assert_eq!(bug["color"], "red");

    let backend = mach(dir.path(), &["--json", "labels", "add", "Backend"]);
    assert!(backend.status.success());
    let backend: serde_json::Value = serde_json::from_slice(&backend.stdout).unwrap();
    let backend_id = backend["id"].as_str().unwrap().to_string();

    let first = mach(
        dir.path(),
        &[
            "--json", "add", "first", "--label", "bug", "--label", "Back",
        ],
    );
    assert!(first.status.success());
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_id = first["id"].as_str().unwrap().to_string();

    let second = mach(dir.path(), &["--json", "add", "second", "--label", "Bug"]);
    assert!(second.status.success());
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    let second_id = second["id"].as_str().unwrap().to_string();
    assert!(mach(dir.path(), &["done", &second_id]).status.success());

    let listed = mach(dir.path(), &["--json", "labels"]);
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        listed,
        serde_json::json!({
            "labels": [
                {"id": bug_id, "name": "Bug", "color": "red", "total": 2, "done": 1},
                {"id": backend_id, "name": "Backend", "color": "orange", "total": 1, "done": 0},
            ]
        })
    );
    let plain_labels = mach(dir.path(), &["labels"]);
    let plain_labels = String::from_utf8(plain_labels.stdout).unwrap();
    assert!(plain_labels.contains("Bug  red  1/2"));
    assert!(plain_labels.contains("Backend  orange  0/1"));
    assert!(!plain_labels.contains("#Bug"));
    assert!(!plain_labels.contains("#Backend"));

    let renamed = mach(
        dir.path(),
        &[
            "--json", "labels", "edit", "bug", "--name", "Defect", "--color", "indigo",
        ],
    );
    assert!(renamed.status.success());
    let renamed: serde_json::Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(renamed["id"], bug_id);
    assert_eq!(renamed["name"], "Defect");
    assert_eq!(renamed["color"], "indigo");

    let recolored = mach(
        dir.path(),
        &["--json", "labels", "edit", "Back", "--color", "cyan"],
    );
    assert!(recolored.status.success());
    let recolored: serde_json::Value = serde_json::from_slice(&recolored.stdout).unwrap();
    assert_eq!(recolored["name"], "Backend");
    assert_eq!(recolored["color"], "cyan");

    let deleted = mach(dir.path(), &["--json", "labels", "delete", "Back"]);
    assert!(deleted.status.success());
    let deleted: serde_json::Value = serde_json::from_slice(&deleted.stdout).unwrap();
    assert_eq!(deleted["id"], backend_id);
    assert_eq!(deleted["deleted"], "Backend");
    assert_eq!(deleted["tasks_unassigned"], 1);

    let shown = mach(dir.path(), &["--json", "show", &first_id]);
    assert!(shown.status.success(), "deleting a label deleted its task");
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        shown["labels"],
        serde_json::json!([{"id": bug_id, "name": "Defect", "color": "indigo"}])
    );
}

#[test]
fn repeatable_label_filters_are_conjunctive_and_compose_with_task_filters() {
    let dir = TempDir::new("cli-label-filter");
    for label in ["Backend", "Bug", "Release"] {
        assert!(mach(dir.path(), &["labels", "add", label]).status.success());
    }
    for category in ["Work", "Home"] {
        assert!(
            mach(dir.path(), &["categories", "add", category])
                .status
                .success()
        );
    }

    let add = |title: &str, category: &str, labels: &[&str]| {
        let mut arguments = vec!["--json", "add", title, "--category", category];
        for label in labels {
            arguments.extend(["--label", label]);
        }
        let output = mach(dir.path(), &arguments);
        assert!(
            output.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };

    let both_open = add("both open", "Work", &["Bug", "Back", "bug"]);
    let bug_only = add("bug only", "Work", &["Bug"]);
    let backend_only = add("backend only", "Work", &["Backend"]);
    let both_done = add("both done", "Home", &["Backend", "Bug"]);
    assert!(
        mach(dir.path(), &["done", both_done["id"].as_str().unwrap()])
            .status
            .success()
    );

    let labels = both_open["labels"].as_array().unwrap();
    assert_eq!(
        labels.len(),
        2,
        "duplicate --label values must be idempotent"
    );
    assert_eq!(labels[0]["name"], "Backend", "use global label order");
    assert_eq!(labels[1]["name"], "Bug");

    let filtered = mach(
        dir.path(),
        &["--json", "list", "--label", "bug", "--label", "back"],
    );
    assert!(filtered.status.success());
    let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(
        filtered
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["title"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["both open", "both done"]
    );

    let open_work = mach(
        dir.path(),
        &[
            "--json",
            "list",
            "--label",
            "bug",
            "--label",
            "back",
            "--open",
            "--category",
            "Work",
        ],
    );
    let open_work: serde_json::Value = serde_json::from_slice(&open_work.stdout).unwrap();
    assert_eq!(open_work.as_array().unwrap().len(), 1);
    assert_eq!(open_work[0]["id"], both_open["id"]);

    let plain = mach(dir.path(), &["list", "--label", "Bug", "--open"]);
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("Backend Bug"));
    assert!(!plain.contains("#Backend"));
    assert!(!plain.contains("#Bug"));
    assert!(plain.contains("both open"));
    assert!(plain.contains("bug only"));
    assert!(!plain.contains("backend only"));

    let shown = mach(dir.path(), &["show", both_open["id"].as_str().unwrap()]);
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains("labels:     Backend Bug"));

    assert!(bug_only["labels"].is_array());
    assert!(backend_only["labels"].is_array());
}

#[test]
fn list_query_searches_task_text_and_labels_and_composes_with_existing_filters() {
    let dir = TempDir::new("cli-list-query");
    for label in ["Bug", "Backend"] {
        assert!(mach(dir.path(), &["labels", "add", label]).status.success());
    }
    for category in ["Work", "Home"] {
        assert!(
            mach(dir.path(), &["categories", "add", category])
                .status
                .success()
        );
    }

    let matching = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "Release API",
            "--description",
            "Handles Maße correctly",
            "--category",
            "Work",
            "--label",
            "Bug",
            "--label",
            "Backend",
        ],
    );
    assert!(matching.status.success());
    let matching: serde_json::Value = serde_json::from_slice(&matching.stdout).unwrap();

    let done = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "Maße in title",
            "--category",
            "Work",
            "--label",
            "Bug",
            "--label",
            "Backend",
        ],
    );
    let done: serde_json::Value = serde_json::from_slice(&done.stdout).unwrap();
    assert!(
        mach(dir.path(), &["done", done["id"].as_str().unwrap()])
            .status
            .success()
    );

    assert!(
        mach(
            dir.path(),
            &[
                "add",
                "Home match",
                "--description",
                "masse",
                "--category",
                "Home",
                "--label",
                "Bug",
                "--label",
                "Backend",
            ],
        )
        .status
        .success()
    );
    let label_only = mach(
        dir.path(),
        &[
            "--json",
            "add",
            "No text match",
            "--category",
            "Work",
            "--label",
            "Bug",
            "--label",
            "Backend",
        ],
    );
    assert!(label_only.status.success());
    let label_only: serde_json::Value = serde_json::from_slice(&label_only.stdout).unwrap();

    let filtered = mach(
        dir.path(),
        &[
            "--json",
            "list",
            "--query",
            "MASSE",
            "--category",
            "Work",
            "--label",
            "Bug",
            "--label",
            "Backend",
            "--open",
        ],
    );
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stdout)
    );
    let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["id"], matching["id"]);

    let title_match = mach(dir.path(), &["--json", "list", "--query", "release"]);
    let title_match: serde_json::Value = serde_json::from_slice(&title_match.stdout).unwrap();
    assert_eq!(title_match.as_array().unwrap().len(), 1);
    assert_eq!(title_match[0]["id"], matching["id"]);

    let label_name_only = mach(dir.path(), &["--json", "list", "--query", "Backend"]);
    let label_name_only: serde_json::Value =
        serde_json::from_slice(&label_name_only.stdout).unwrap();
    assert!(
        label_name_only
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == label_only["id"])
    );

    let empty = mach(dir.path(), &["--json", "list", "--query", "   "]);
    assert!(!empty.status.success());
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty["kind"], "validation");
}

#[test]
fn label_edits_are_idempotent_and_invalid_sets_do_not_partially_mutate() {
    let dir = TempDir::new("cli-label-edits");
    for label in ["Bug", "Backend", "Build", "Release"] {
        assert!(mach(dir.path(), &["labels", "add", label]).status.success());
    }
    let unresolved_add = mach(
        dir.path(),
        &["--json", "add", "must not exist", "--label", "Missing"],
    );
    assert!(!unresolved_add.status.success());
    let tasks = mach(dir.path(), &["--json", "list"]);
    let tasks: serde_json::Value = serde_json::from_slice(&tasks.stdout).unwrap();
    assert_eq!(tasks, serde_json::json!([]));

    let added = mach(dir.path(), &["--json", "add", "task", "--label", "Bug"]);
    assert!(added.status.success());
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let id = added["id"].as_str().unwrap();

    let expanded = mach(
        dir.path(),
        &[
            "--json",
            "edit",
            id,
            "--add-label",
            "bug",
            "--add-label",
            "backend",
            "--remove-label",
            "Release",
        ],
    );
    assert!(expanded.status.success());
    let expanded: serde_json::Value = serde_json::from_slice(&expanded.stdout).unwrap();
    assert_eq!(
        expanded["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Bug", "Backend"]
    );

    let replaced = mach(
        dir.path(),
        &[
            "--json",
            "edit",
            id,
            "--clear-labels",
            "--add-label",
            "Release",
        ],
    );
    assert!(replaced.status.success());
    let replaced: serde_json::Value = serde_json::from_slice(&replaced.stdout).unwrap();
    assert_eq!(replaced["labels"][0]["name"], "Release");
    assert_eq!(replaced["labels"].as_array().unwrap().len(), 1);

    for invalid in [
        vec!["--add-label", "Release", "--remove-label", "release"],
        vec!["--clear-labels", "--remove-label", "Release"],
        vec!["--add-label", "b"],
        vec!["--add-label", "Missing"],
    ] {
        let mut arguments = vec!["--json", "edit", id];
        arguments.extend(invalid);
        let output = mach(dir.path(), &arguments);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let error: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("one JSON error document");
        assert_eq!(error["ok"], false);

        let shown = mach(dir.path(), &["--json", "show", id]);
        let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
        assert_eq!(shown["labels"], replaced["labels"]);
    }
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
fn archive_round_trip_preserves_tasks_categories_labels_and_images() {
    let source = TempDir::new("archive-source");
    let destination = TempDir::new("archive-destination");
    let output = TempDir::new("archive-output");
    let archive = output.path().join("tasks.mach");
    let screenshot = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/screenshot.png");
    let description = format!(
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
    assert!(
        mach(source.path(), &["labels", "add", "Portable"])
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
            "--label",
            "Portable",
            "--description",
            &description,
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
    assert_eq!(exported["labels"], 1);
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
    assert_eq!(imported["labels_added"], 1);
    assert_eq!(imported["images_added"], 1);

    let source_store = Store::open(source.path()).unwrap();
    let source_snapshot = source_store.snapshot().unwrap();
    let destination_store = Store::open(destination.path()).unwrap();
    let destination_snapshot = destination_store.snapshot().unwrap();
    assert_eq!(destination_snapshot.categories, source_snapshot.categories);
    assert_eq!(destination_snapshot.labels, source_snapshot.labels);
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
    let description = format!("[image:{}]", screenshot.display());

    let added = mach(
        source.path(),
        &["--json", "add", "image", "--description", &description],
    );
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
