//! Command-line front end for humans and agents.
//!
//! Every invocation owns one [`Store`]. Read commands use one snapshot and
//! mutations execute one fresh read-modify-write transaction. JSON mode emits
//! exactly one document on stdout, including usage and runtime errors.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use chrono::{NaiveTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::VERSION;
use crate::model::{Block, Category, Task};
use crate::store::{CategoryPatch, PurgeScope, RelativePosition, Store, StoreError, TaskPatch};

/// Full CLI reference under `mach --help`.
const HELP: &str = "\
  list
    -c, --category NAME  only this category
    --open               only incomplete
    --done               only completed

  categories
    (no args)            list categories (done/total)
    add NAME
      -d, --description TEXT
    edit NAME            rename / set description
      -n, --name NEW
      -d, --description TEXT
      --clear-description
    delete NAME          delete category; tasks become uncategorized

  add [TITLE]
    -t, --title TITLE    title (required if no positional TITLE)
    -b, --body TEXT      body (newlines = lines; see BODY MARKUP)
    -d, --due DATE       YYYY-MM-DD | MM-DD | HH:MM | DATEThh:mm
    --time HH:MM         with --due, or alone = next occurrence
    -c, --category NAME  category (name or unique prefix)
    -i, --importance N   0–3 (default 0)
    --subtask TEXT       add subtask (repeatable)

  show ID                ID = uuid or unique prefix

  done ID
  undone ID

  delete ID

  move ID (--before | --after) TARGET
    reorder within the task's current category

  purge --done
    -c, --category NAME  only completed tasks in this category

  edit ID                only given flags change
    -t, --title TITLE
    -b, --body TEXT      replace entire body (BODY MARKUP; wipes old body)
    -d, --due DATE       date-only keeps existing time
    --time HH:MM         keeps existing date if no --due
    --clear-due          remove due date/time
    -c, --category NAME
    --clear-category     uncategorized
    -i, --importance N   0–3

  BODY MARKUP (add/edit --body, one block per line)
    plain text
    [ ] item / [x] item  subtask
    - item / • item      bullet
    1. item              numbered (any leading N.)
    https://…            link
    [image:PATH]         import an absolute path, or one relative to images/

  subtasks TASK
    (no subcommand)      list subtasks
    add [TEXT]
      -t, --text TEXT
      --done             create already checked
    done INDEX           INDEX = 1-based among checkboxes only
    undone INDEX
    toggle INDEX
    edit INDEX [TEXT]
      -t, --text TEXT
    delete INDEX         later indexes shift down

  export [FILE]          portable .mach archive (tasks, categories, images)
                         default: ./mach-export-YYYYMMDD-HHMMSS.mach

  import FILE            safely merge a .mach archive
                         identical records are skipped; conflicts abort

  update                 check GitHub for a newer release
    --install            verify SHA-256 and install release binary to ~/.local/bin

  (no command)           open TUI
  --json                 exactly one JSON document on stdout
  --dir PATH             data directory (global)

Data: --dir PATH  >  $MACH_DIR  >  ~/.mach
";

#[derive(Parser)]
#[command(
    name = "mach",
    about = concat!("mach v", env!("CARGO_PKG_VERSION")),
    disable_version_flag = true,
    color = clap::ColorChoice::Never,
    after_help = HELP,
)]
struct Cli {
    /// Show version
    #[arg(short = 'v', long = "version")]
    version: bool,

    /// Data directory (default ~/.mach; overrides $MACH_DIR)
    #[arg(long = "dir", value_name = "PATH", global = true)]
    dir: Option<PathBuf>,

    /// JSON stdout
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List tasks
    List {
        /// Category name / prefix
        #[arg(short = 'c', long = "category", value_name = "NAME")]
        category: Option<String>,
        /// Incomplete only
        #[arg(long, conflicts_with = "done")]
        open: bool,
        /// Done only
        #[arg(long)]
        done: bool,
    },
    /// List / add / edit / delete categories
    Categories {
        #[command(subcommand)]
        action: Option<CatAction>,
    },
    /// Add a task
    Add(AddArgs),
    /// Show task
    Show {
        /// Task id / prefix
        id: String,
    },
    /// Mark task done
    Done {
        /// Task id / prefix
        id: String,
    },
    /// Mark task not done
    Undone {
        /// Task id / prefix
        id: String,
    },
    /// Delete task
    Delete {
        /// Task id / prefix
        id: String,
    },
    /// Reorder a task within its category
    Move {
        /// Task id / prefix
        id: String,
        /// Place before this task id / prefix
        #[arg(
            long,
            value_name = "TARGET",
            conflicts_with = "after",
            required_unless_present = "after"
        )]
        before: Option<String>,
        /// Place after this task id / prefix
        #[arg(
            long,
            value_name = "TARGET",
            conflicts_with = "before",
            required_unless_present = "before"
        )]
        after: Option<String>,
    },
    /// Permanently remove completed tasks
    Purge {
        /// Required safety interlock: purge completed tasks only
        #[arg(long, required = true)]
        done: bool,
        /// Limit to one category
        #[arg(short = 'c', long = "category", value_name = "NAME")]
        category: Option<String>,
    },
    /// Edit task fields
    Edit(EditArgs),
    /// Subtasks on a task
    Subtasks {
        /// Parent task id / prefix
        task: String,
        #[command(subcommand)]
        action: Option<SubAction>,
    },
    /// Export tasks, categories, and images to a portable archive
    Export {
        /// Output file (default ./mach-export-YYYYMMDD-HHMMSS.mach)
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Safely merge a portable archive
    Import {
        /// Archive file
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Check GitHub for a newer release (optional install)
    Update {
        /// Verify SHA-256 and install the release binary to ~/.local/bin
        #[arg(long)]
        install: bool,
    },
}

#[derive(clap::Args)]
struct AddArgs {
    /// Title (or use --title)
    #[arg(value_name = "TITLE", conflicts_with = "title")]
    title_pos: Option<String>,
    /// Title
    #[arg(short = 't', long = "title")]
    title: Option<String>,
    /// Body text (newlines → lines)
    #[arg(short = 'b', long = "body")]
    body: Option<String>,
    /// Due date
    #[arg(short = 'd', long = "due", value_name = "DATE")]
    due: Option<String>,
    /// Due time HH:MM (with --due, or alone = next occurrence)
    #[arg(long = "time", value_name = "HH:MM")]
    time: Option<String>,
    /// Category
    #[arg(short = 'c', long = "category", value_name = "NAME")]
    category: Option<String>,
    /// Importance 0–3
    #[arg(
        short = 'i',
        long = "importance",
        value_name = "N",
        default_value_t = 0
    )]
    importance: u8,
    /// Subtask (repeatable)
    #[arg(long = "subtask", value_name = "TEXT")]
    subtasks: Vec<String>,
}

#[derive(clap::Args)]
struct EditArgs {
    /// Task id / prefix
    id: String,
    /// New title
    #[arg(short = 't', long = "title")]
    title: Option<String>,
    /// Replace body
    #[arg(short = 'b', long = "body")]
    body: Option<String>,
    /// Due date
    #[arg(short = 'd', long = "due", value_name = "DATE")]
    due: Option<String>,
    /// Due time HH:MM
    #[arg(long = "time", value_name = "HH:MM")]
    time: Option<String>,
    /// Clear due
    #[arg(long)]
    clear_due: bool,
    /// Set category
    #[arg(short = 'c', long = "category", value_name = "NAME")]
    category: Option<String>,
    /// Uncategorized
    #[arg(long = "clear-category")]
    clear_cat: bool,
    /// Importance 0–3
    #[arg(short = 'i', long = "importance", value_name = "N")]
    importance: Option<u8>,
}

#[derive(Subcommand)]
enum CatAction {
    /// List categories (default)
    List,
    /// Create category
    Add {
        /// Name
        name: String,
        /// Description
        #[arg(short = 'd', long = "description")]
        description: Option<String>,
    },
    /// Rename / set description
    Edit {
        /// Current name / prefix
        name: String,
        /// New name
        #[arg(short = 'n', long = "name", value_name = "NEW")]
        new_name: Option<String>,
        /// Description
        #[arg(short = 'd', long = "description")]
        description: Option<String>,
        /// Clear description
        #[arg(long = "clear-description")]
        clear_description: bool,
    },
    /// Delete category (tasks become uncategorized)
    Delete {
        /// Name / prefix
        name: String,
    },
}

#[derive(Subcommand)]
enum SubAction {
    /// List subtasks (default)
    List,
    /// Add subtask
    Add {
        /// Text (or --text)
        #[arg(value_name = "TEXT", conflicts_with = "text")]
        text_pos: Option<String>,
        /// Text
        #[arg(short = 't', long = "text")]
        text: Option<String>,
        /// Start done
        #[arg(long)]
        done: bool,
    },
    /// Mark subtask done
    Done {
        /// 1-based index
        index: usize,
    },
    /// Mark subtask not done
    Undone {
        /// 1-based index
        index: usize,
    },
    /// Toggle subtask
    Toggle {
        /// 1-based index
        index: usize,
    },
    /// Edit subtask text
    Edit {
        /// 1-based index
        index: usize,
        /// Text (or --text)
        #[arg(value_name = "TEXT", conflicts_with = "text")]
        text_pos: Option<String>,
        /// Text
        #[arg(short = 't', long = "text")]
        text: Option<String>,
    },
    /// Delete subtask
    Delete {
        /// 1-based index
        index: usize,
    },
}

#[derive(Debug)]
struct CliError {
    kind: &'static str,
    message: String,
}

impl CliError {
    fn validation(message: impl Into<String>) -> Self {
        Self {
            kind: "validation",
            message: message.into(),
        }
    }

    fn update(message: impl Into<String>) -> Self {
        Self {
            kind: "update",
            message: message.into(),
        }
    }
}

impl From<StoreError> for CliError {
    fn from(error: StoreError) -> Self {
        let kind = match &error {
            StoreError::Io { .. } => "io",
            StoreError::Json { .. } => "legacy_json",
            StoreError::Database(_) => "database",
            StoreError::UnsupportedLegacySchema { .. }
            | StoreError::UnsupportedDatabaseSchema { .. } => "schema",
            StoreError::Conflict { .. } | StoreError::StaleEntity { .. } => "conflict",
            StoreError::NotFound { .. } => "not_found",
            StoreError::Ambiguous { .. } => "ambiguous",
            StoreError::Validation(_) => "validation",
            StoreError::Corrupt(_) => "corrupt",
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

impl From<crate::archive::ArchiveError> for CliError {
    fn from(error: crate::archive::ArchiveError) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

enum Rendered {
    Json(Value),
    Plain(String),
}

impl Rendered {
    fn emit(self) -> io::Result<()> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        match self {
            Self::Json(value) => {
                serde_json::to_writer_pretty(&mut output, &value).map_err(|error| {
                    if let Some(kind) = error.io_error_kind() {
                        io::Error::new(kind, error)
                    } else {
                        io::Error::other(error)
                    }
                })?;
                output.write_all(b"\n")
            }
            Self::Plain(text) => output.write_all(text.as_bytes()),
        }
    }
}

fn rendered(json_mode: bool, value: Value, plain: String) -> Rendered {
    if json_mode {
        Rendered::Json(value)
    } else {
        Rendered::Plain(plain)
    }
}

pub fn run() {
    let arguments = normalize_documented_body_values(std::env::args_os().collect());
    let json_requested = requested_json(&arguments);
    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error) => emit_parse_error(error, json_requested),
    };
    let Cli {
        version,
        dir,
        json,
        command,
    } = cli;

    if version {
        let output = if json {
            Rendered::Json(json!({ "ok": true, "version": VERSION }))
        } else {
            Rendered::Plain(format!("mach v{VERSION}\n"))
        };
        emit_success(output);
        return;
    }

    let result = match command {
        Some(Command::Update { install }) => cmd_update(install, json),
        None if json => Err(CliError::validation(
            "--json requires a command or --version",
        )),
        None => crate::require_interactive_terminal()
            .map_err(terminal_error)
            .and_then(|()| Store::open_default(dir).map_err(CliError::from))
            .and_then(|store| {
                crate::run_tui(store).map_err(terminal_error)?;
                Ok(Rendered::Plain(String::new()))
            }),
        Some(command) => Store::open_default(dir)
            .map_err(CliError::from)
            .and_then(|mut store| dispatch(&mut store, command, json)),
    };

    match result {
        Ok(output) => emit_success(output),
        Err(error) => emit_runtime_error(error, json),
    }
}

/// Clap normally treats a separate leading-hyphen value as another option.
/// Preserve that unambiguous behavior except for the documented `- ` body
/// bullet; explicit `--body=...` remains the escape hatch for all other text.
fn normalize_documented_body_values(arguments: Vec<OsString>) -> Vec<OsString> {
    let mut normalized = Vec::with_capacity(arguments.len());
    let mut arguments = arguments.into_iter().peekable();
    let mut options = true;
    while let Some(argument) = arguments.next() {
        if options && argument == "--" {
            options = false;
            normalized.push(argument);
            continue;
        }
        let body_option = options && (argument == "--body" || argument == "-b");
        let documented_bullet = body_option
            && arguments
                .peek()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with("- "));
        if documented_bullet {
            let value = arguments.next().expect("peeked body value must exist");
            let mut combined = OsString::from("--body=");
            combined.push(value);
            normalized.push(combined);
        } else {
            normalized.push(argument);
        }
    }
    normalized
}

fn terminal_error(error: io::Error) -> CliError {
    CliError {
        kind: "terminal",
        message: error.to_string(),
    }
}

fn requested_json(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| argument.as_os_str() == "--json")
}

fn emit_parse_error(error: clap::Error, json_mode: bool) -> ! {
    let help = matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    );
    let exit_code = if help { 0 } else { error.exit_code() };
    if !json_mode {
        let write = if help {
            Rendered::Plain(error.to_string()).emit()
        } else {
            write_stderr(&format!("{}\n", terminal_text(&error.to_string())))
        };
        exit_after_write(write, exit_code);
    }
    let value = if help {
        json!({ "ok": true, "kind": "help", "help": error.to_string() })
    } else {
        json!({ "ok": false, "kind": "usage", "error": error.to_string() })
    };
    exit_after_write(Rendered::Json(value).emit(), exit_code);
}

fn emit_runtime_error(error: CliError, json_mode: bool) -> ! {
    let write = if json_mode {
        Rendered::Json(json!({
            "ok": false,
            "kind": error.kind,
            "error": error.message,
        }))
        .emit()
    } else {
        write_stderr(&format!("mach: {}\n", terminal_text(&error.message)))
    };
    exit_after_write(write, 1);
}

fn emit_success(output: Rendered) {
    if let Err(error) = output.emit() {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return;
        }
        let _ = write_stderr(&format!("mach: could not write output: {error}\n"));
        std::process::exit(1);
    }
}

fn write_stderr(text: &str) -> io::Result<()> {
    let stderr = io::stderr();
    stderr.lock().write_all(text.as_bytes())
}

fn exit_after_write(result: io::Result<()>, intended_code: i32) -> ! {
    match result {
        Ok(()) => std::process::exit(intended_code),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

fn dispatch(store: &mut Store, command: Command, json_mode: bool) -> Result<Rendered, CliError> {
    match command {
        Command::List {
            category,
            open,
            done,
        } => cmd_list(store, category.as_deref(), open, done, json_mode),
        Command::Categories { action } => match action {
            None | Some(CatAction::List) => cmd_categories_list(store, json_mode),
            Some(CatAction::Add { name, description }) => {
                cmd_category_add(store, &name, description.as_deref(), json_mode)
            }
            Some(CatAction::Edit {
                name,
                new_name,
                description,
                clear_description,
            }) => cmd_category_edit(
                store,
                &name,
                new_name.as_deref(),
                description.as_deref(),
                clear_description,
                json_mode,
            ),
            Some(CatAction::Delete { name }) => cmd_category_delete(store, &name, json_mode),
        },
        Command::Add(arguments) => cmd_add(store, &arguments, json_mode),
        Command::Show { id } => cmd_show(store, &id, json_mode),
        Command::Done { id } => cmd_set_done(store, &id, true, json_mode),
        Command::Undone { id } => cmd_set_done(store, &id, false, json_mode),
        Command::Delete { id } => cmd_delete(store, &id, json_mode),
        Command::Move { id, before, after } => {
            cmd_move(store, &id, before.as_deref(), after.as_deref(), json_mode)
        }
        Command::Purge { done, category } => cmd_purge(store, done, category.as_deref(), json_mode),
        Command::Edit(arguments) => cmd_edit(store, &arguments, json_mode),
        Command::Subtasks { task, action } => match action {
            None | Some(SubAction::List) => cmd_subtasks_list(store, &task, json_mode),
            Some(SubAction::Add {
                text_pos,
                text,
                done,
            }) => cmd_subtask_add(
                store,
                &task,
                &text.or(text_pos).unwrap_or_default(),
                done,
                json_mode,
            ),
            Some(SubAction::Done { index }) => {
                cmd_subtask_set_done(store, &task, index, Some(true), json_mode)
            }
            Some(SubAction::Undone { index }) => {
                cmd_subtask_set_done(store, &task, index, Some(false), json_mode)
            }
            Some(SubAction::Toggle { index }) => {
                cmd_subtask_set_done(store, &task, index, None, json_mode)
            }
            Some(SubAction::Edit {
                index,
                text_pos,
                text,
            }) => cmd_subtask_edit(
                store,
                &task,
                index,
                &text.or(text_pos).unwrap_or_default(),
                json_mode,
            ),
            Some(SubAction::Delete { index }) => cmd_subtask_delete(store, &task, index, json_mode),
        },
        Command::Export { file } => cmd_export(store, file.as_deref(), json_mode),
        Command::Import { file } => cmd_import(store, &file, json_mode),
        Command::Update { .. } => Err(CliError {
            kind: "internal",
            message: "update command crossed the data-command boundary".into(),
        }),
    }
}

fn cmd_export(
    store: &Store,
    path: Option<&std::path::Path>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let summary = crate::archive::export(store, path)?;
    let value = json!({
        "ok": true,
        "archive": summary.path.display().to_string(),
        "tasks": summary.tasks,
        "categories": summary.categories,
        "images": summary.images,
    });
    let contents =
        crate::archive::content_count_text(summary.tasks, summary.categories, summary.images);
    let plain = format!(
        "exported {contents} to {}\n",
        terminal_text(&summary.path.display().to_string())
    );
    Ok(rendered(json_mode, value, plain))
}

fn cmd_import(
    store: &mut Store,
    path: &std::path::Path,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let outcome = crate::archive::import(store, path)?;
    let summary = outcome.summary;
    let value = json!({
        "ok": true,
        "archive": summary.path.display().to_string(),
        "tasks_added": summary.tasks_added,
        "tasks_unchanged": summary.tasks_unchanged,
        "categories_added": summary.categories_added,
        "categories_unchanged": summary.categories_unchanged,
        "images_added": summary.images_added,
        "images_unchanged": summary.images_unchanged,
    });
    let added = crate::archive::content_count_text(
        summary.tasks_added,
        summary.categories_added,
        summary.images_added,
    );
    let unchanged = crate::archive::content_count_text(
        summary.tasks_unchanged,
        summary.categories_unchanged,
        summary.images_unchanged,
    );
    let plain = if summary.tasks_added + summary.categories_added + summary.images_added == 0 {
        format!("nothing imported; {unchanged} already present\n")
    } else {
        format!("imported {added}; {unchanged} already present\n")
    };
    Ok(rendered(json_mode, value, plain))
}

fn cmd_update(do_install: bool, json_mode: bool) -> Result<Rendered, CliError> {
    let now = Utc::now().timestamp();
    let mut update_state = crate::update_state::UpdateStateStore::open_default().ok();
    let lease = update_state
        .as_mut()
        .and_then(|store| store.claim_manual(now).ok());
    let checked = match crate::update::check_with_etag(None) {
        Ok(crate::update::CheckResponse::Modified { info, etag }) => (info, etag),
        Ok(crate::update::CheckResponse::NotModified) => {
            if let (Some(store), Some(lease)) = (update_state.as_mut(), lease.as_ref()) {
                let _ = store.finish_failure(lease, Utc::now().timestamp(), None);
            }
            return Err(CliError::update(
                "GitHub returned 304 without a conditional request",
            ));
        }
        Err(error) => {
            if let (Some(store), Some(lease)) = (update_state.as_mut(), lease.as_ref()) {
                let _ = store.finish_failure(lease, Utc::now().timestamp(), error.retry_at);
            }
            return Err(CliError::update(error.message));
        }
    };
    let (info, etag) = checked;
    let install = if do_install && info.newer {
        crate::update::install(&info).map(Some)
    } else {
        Ok(None)
    };
    if let (Some(store), Some(lease)) = (update_state.as_mut(), lease.as_ref()) {
        let _ = store.finish_modified(lease, Utc::now().timestamp(), etag.as_deref(), &info.latest);
    }
    let install = install.map_err(CliError::update)?;
    let install_disposition = install.as_ref().map(|result| match result.disposition {
        crate::update::InstallDisposition::Installed => "installed",
        crate::update::InstallDisposition::AlreadyCurrent => "already_current",
    });

    let value = json!({
        "ok": true,
        "current": info.current,
        "latest": info.latest,
        "newer": info.newer,
        "prerelease": info.prerelease,
        "url": info.release_url,
        "installed": install_disposition == Some("installed"),
        "install_disposition": install_disposition,
        "destination": install
            .as_ref()
            .map(|result| result.destination.display().to_string()),
        "tag": install.as_ref().map(|result| result.tag.as_str()).unwrap_or(&info.tag),
    });
    let mut plain = format!("{}\n", terminal_text(&info.summary()));
    if info.newer && install.is_none() {
        plain.push('\n');
        for line in info.install_hint().lines() {
            plain.push_str(&terminal_text(line));
            plain.push('\n');
        }
    }
    if do_install && install.is_none() {
        plain.push_str("Already up to date.\n");
    } else if let Some(result) = install {
        let message = match result.disposition {
            crate::update::InstallDisposition::Installed => format!(
                "Installed {} to {}. Restart mach to use the new build.\n",
                terminal_text(&result.tag),
                terminal_text(&result.destination.display().to_string())
            ),
            crate::update::InstallDisposition::AlreadyCurrent => format!(
                "Already installed {} at {}. Restart mach to use the new build.\n",
                terminal_text(&result.tag),
                terminal_text(&result.destination.display().to_string())
            ),
        };
        plain.push_str(&message);
    }
    Ok(rendered(json_mode, value, plain))
}

// ---------------------------------------------------------------- helpers

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            character if character.is_control() => {
                safe.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => safe.push(character),
        }
    }
    safe
}

fn body_from_text(text: &str) -> Vec<Block> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().map(line_to_block).collect()
}

fn line_to_block(line: &str) -> Block {
    let text = line.trim_end();
    if let Some(rest) = text.strip_prefix("[ ] ") {
        return Block::todo(rest, false);
    }
    if let Some(rest) = text
        .strip_prefix("[x] ")
        .or_else(|| text.strip_prefix("[X] "))
        .or_else(|| text.strip_prefix("[✓] "))
    {
        return Block::todo(rest, true);
    }
    if let Some(rest) = text.strip_prefix("- ").or_else(|| text.strip_prefix("• ")) {
        return Block::bullet(rest);
    }
    if let Some(rest) = strip_number_prefix(text) {
        return Block::number(rest);
    }
    if let Some(path) = text
        .strip_prefix("[image:")
        .and_then(|value| value.strip_suffix(']'))
        .filter(|path| !path.is_empty())
    {
        return Block::image(path);
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        return Block::link(text);
    }
    Block::text(text)
}

fn strip_number_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == 0 {
        return None;
    }
    line.get(index..)?.strip_prefix(". ")
}

fn body_to_text(body: &[Block]) -> String {
    let mut numbered = 0usize;
    body.iter()
        .map(|block| match block {
            Block::Text { text } => {
                numbered = 0;
                text.clone()
            }
            Block::Todo { text, done } => {
                numbered = 0;
                format!("[{}] {text}", if *done { "x" } else { " " })
            }
            Block::Bullet { text } => {
                numbered = 0;
                format!("- {text}")
            }
            Block::Number { text } => {
                numbered += 1;
                format!("{numbered}. {text}")
            }
            Block::Link { url } => {
                numbered = 0;
                url.clone()
            }
            Block::Image { attachment_id } => {
                numbered = 0;
                format!("[image:{attachment_id}]")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_subtasks(body: &[Block]) -> Vec<(usize, &str, bool)> {
    body.iter()
        .filter_map(|block| match block {
            Block::Todo { text, done } => Some((text.as_str(), *done)),
            _ => None,
        })
        .enumerate()
        .map(|(index, (text, done))| (index + 1, text, done))
        .collect()
}

fn subtask_body_index(body: &[Block], one_based: usize) -> Result<usize, CliError> {
    if one_based == 0 {
        return Err(CliError::validation(
            "subtask index is 1-based (use 1 for the first subtask)",
        ));
    }
    let mut count = 0usize;
    for (body_index, block) in body.iter().enumerate() {
        if matches!(block, Block::Todo { .. }) {
            count += 1;
            if count == one_based {
                return Ok(body_index);
            }
        }
    }
    Err(CliError::validation(format!(
        "no subtask at index {one_based} (task has {count} subtask(s))"
    )))
}

fn subtasks_json(body: &[Block]) -> Vec<Value> {
    subtasks_to_json(&collect_subtasks(body))
}

fn subtasks_to_json(subtasks: &[(usize, &str, bool)]) -> Vec<Value> {
    subtasks
        .iter()
        .map(|(index, text, done)| json!({ "index": index, "text": text, "done": done }))
        .collect()
}

fn category_name<'a>(categories: &'a [Category], task: &Task) -> Option<&'a str> {
    task.category_id
        .as_ref()
        .and_then(|id| categories.iter().find(|category| category.id == *id))
        .map(|category| category.name.as_str())
}

fn task_json(categories: &[Category], task: &Task) -> Value {
    let subtasks = collect_subtasks(&task.body);
    let subtasks_json = subtasks_to_json(&subtasks);
    json!({
        "id": task.id,
        "title": task.title,
        "body": body_to_text(&task.body),
        "subtasks": subtasks_json,
        "subtasks_done": subtasks.iter().filter(|(_, _, done)| *done).count(),
        "subtasks_total": subtasks.len(),
        "due": task.due,
        "done": task.done,
        "importance": task.importance,
        "category": {
            "id": task.category_id,
            "name": category_name(categories, task),
        },
        "created": task.created,
    })
}

fn category_json(category: &Category) -> Value {
    json!({
        "id": category.id,
        "name": category.name,
        "description": category.description,
    })
}

fn validate_time(raw: &str) -> Result<String, CliError> {
    let value = raw.trim();
    if value.len() == 5 && NaiveTime::parse_from_str(value, "%H:%M").is_ok() {
        Ok(value.to_string())
    } else {
        Err(CliError::validation(format!(
            "invalid time {raw:?}; use HH:MM (24h), e.g. 14:30"
        )))
    }
}

fn due_for_add(due: Option<&str>, time: Option<&str>) -> Result<String, CliError> {
    let due = due.map(str::trim).filter(|value| !value.is_empty());
    match (due, time) {
        (None, None) => Ok(String::new()),
        (None, Some(time)) => validate_time(time),
        (Some(due), None) => Ok(due.to_string()),
        (Some(due), Some(time)) => {
            if due.contains(':') {
                return Err(CliError::validation(format!(
                    "due already includes a time ({due}); omit --time or pass date-only --due"
                )));
            }
            Ok(format!("{due} {}", validate_time(time)?))
        }
    }
}

fn split_inline_title(raw: &str) -> Result<(String, String), CliError> {
    let (inline_due, title) = crate::due::parse(raw.trim());
    if !inline_due.is_empty() {
        crate::due::normalize_for_write(&inline_due)
            .map_err(|error| CliError::validation(error.to_string()))?;
    }
    Ok((title, inline_due))
}

/// Resolve edit due semantics inside the write transaction so shorthand is
/// anchored at the actual write time.
fn due_for_edit(
    current: &str,
    due: Option<&str>,
    time: Option<&str>,
) -> Result<Option<String>, CliError> {
    if due.is_none() && time.is_none() {
        return Ok(None);
    }
    let existing_time = current.split_once(' ').map(|(_, time)| time.to_string());
    let existing_date = current
        .split_once(' ')
        .map(|(date, _)| date.to_string())
        .or_else(|| (!current.is_empty() && !current.contains(':')).then(|| current.to_string()));

    if let Some(raw_due) = due {
        let raw_due = raw_due.trim();
        if raw_due.contains(':') {
            if time.is_some() {
                return Err(CliError::validation(
                    "pass either a full --due datetime or --due date + --time, not both",
                ));
            }
            return crate::due::normalize_for_write(raw_due)
                .map(Some)
                .map_err(|error| CliError::validation(error.to_string()));
        }
        let chosen_time = match time {
            Some(time) => Some(validate_time(time)?),
            None => existing_time,
        };
        let combined = match chosen_time {
            Some(time) => format!("{raw_due} {time}"),
            None => raw_due.to_string(),
        };
        return crate::due::normalize_for_write(&combined)
            .map(Some)
            .map_err(|error| CliError::validation(error.to_string()));
    }

    let Some(time) = time else {
        return Ok(None);
    };
    let time = validate_time(time)?;
    let combined = match existing_date {
        Some(date) => format!("{date} {time}"),
        None => time,
    };
    crate::due::normalize_for_write(&combined)
        .map(Some)
        .map_err(|error| CliError::validation(error.to_string()))
}

fn task_line(
    categories: &[Category],
    task: &Task,
    show_category: bool,
    date_format: &str,
) -> String {
    let check = if task.done { "[✓]" } else { "[ ]" };
    let title = terminal_text(&task.title);
    let due = crate::due::display(&task.due, date_format);
    let due = if due.is_empty() {
        String::new()
    } else {
        format!("  {}", terminal_text(&due))
    };
    let flag = if task.importance > 0 {
        format!("  {}", crate::model::importance_marks(task.importance))
    } else {
        String::new()
    };
    let category = if show_category {
        format!(
            "  [{}]",
            terminal_text(category_name(categories, task).unwrap_or("—"))
        )
    } else {
        String::new()
    };
    let progress = crate::model::todo_progress(task)
        .map(|(done, total)| format!("  ({done}/{total})"))
        .unwrap_or_default();
    format!(
        "{} {check} {title}{category}{due}{flag}{progress}\n",
        terminal_text(&short_id(&task.id))
    )
}

// ---------------------------------------------------------------- commands

fn cmd_list(
    store: &Store,
    category: Option<&str>,
    open_only: bool,
    done_only: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let category_id = category
        .map(|query| data.resolve_category_id(query))
        .transpose()?;
    let show_category = category_id.is_none();
    let tasks: Vec<_> = data
        .tasks
        .iter()
        .filter(|task| {
            category_id
                .as_ref()
                .is_none_or(|id| task.category_id.as_deref() == Some(id.as_str()))
        })
        .filter(|task| {
            if open_only {
                !task.done
            } else if done_only {
                task.done
            } else {
                true
            }
        })
        .collect();
    let value = Value::Array(
        tasks
            .iter()
            .map(|task| task_json(&data.categories, task))
            .collect(),
    );
    let mut plain = String::new();
    if tasks.is_empty() {
        plain.push_str("(no tasks)\n");
    } else {
        for task in &tasks {
            plain.push_str(&task_line(
                &data.categories,
                task,
                show_category,
                &data.settings.date_format,
            ));
        }
        let done = tasks.iter().filter(|task| task.done).count();
        plain.push_str(&format!("— {} task(s), {done} done\n", tasks.len()));
    }
    Ok(rendered(json_mode, value, plain))
}

fn cmd_categories_list(store: &Store, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let stats: Vec<_> = data
        .categories
        .iter()
        .map(|category| {
            let (done, total) = data
                .tasks
                .iter()
                .filter(|task| task.category_id.as_deref() == Some(category.id.as_str()))
                .fold((0, 0), |(done, total), task| {
                    (done + usize::from(task.done), total + 1)
                });
            (category, done, total)
        })
        .collect();
    let categories: Vec<_> = stats
        .iter()
        .map(|(category, done, total)| {
            json!({
                "id": category.id,
                "name": category.name,
                "description": category.description,
                "total": total,
                "done": done,
            })
        })
        .collect();
    let (uncategorized_done, uncategorized_total) = data
        .tasks
        .iter()
        .filter(|task| task.category_id.is_none())
        .fold((0, 0), |(done, total), task| {
            (done + usize::from(task.done), total + 1)
        });
    let value = json!({
        "categories": categories,
        "uncategorized": {
            "total": uncategorized_total,
            "done": uncategorized_done,
        },
    });
    let mut plain = String::new();
    if data.categories.is_empty() {
        plain.push_str("(no categories)\n");
    } else {
        for (category, done, total) in &stats {
            plain.push_str(&format!(
                "{}  {}/{}\n",
                terminal_text(&category.name),
                done,
                total
            ));
        }
    }
    if uncategorized_total > 0 {
        plain.push_str(&format!(
            "— uncategorized  {}/{}\n",
            uncategorized_done, uncategorized_total
        ));
    }
    Ok(rendered(json_mode, value, plain))
}

fn cmd_category_add(
    store: &mut Store,
    name: &str,
    description: Option<&str>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let name = name.trim().to_string();
    let description = description.unwrap_or_default().to_string();
    let category = store.update(|data| data.create_category(name, description))?;
    Ok(rendered(
        json_mode,
        category_json(&category),
        format!("created category {}\n", terminal_text(&category.name)),
    ))
}

fn cmd_category_edit(
    store: &mut Store,
    query: &str,
    new_name: Option<&str>,
    description: Option<&str>,
    clear_description: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    if new_name.is_none() && description.is_none() && !clear_description {
        return Err(CliError::validation(
            "nothing to edit; pass --name / --description / --clear-description",
        ));
    }
    if clear_description && description.is_some() {
        return Err(CliError::validation(
            "--clear-description cannot be combined with --description",
        ));
    }
    let patch = CategoryPatch {
        name: new_name.map(|name| name.trim().to_string()),
        description: if clear_description {
            Some(String::new())
        } else {
            description.map(str::to_string)
        },
    };
    let query = query.to_string();
    let category = store.update(|data| {
        let id = data.resolve_category_id(&query)?;
        data.edit_category(&id, patch)
    })?;
    Ok(rendered(
        json_mode,
        category_json(&category),
        format!("updated category {}\n", terminal_text(&category.name)),
    ))
}

fn cmd_category_delete(
    store: &mut Store,
    query: &str,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let query = query.to_string();
    let category = store.update(|data| {
        let id = data.resolve_category_id(&query)?;
        data.delete_category(&id)
    })?;
    Ok(rendered(
        json_mode,
        json!({ "deleted": category.name, "id": category.id }),
        format!(
            "deleted category {} (tasks uncategorized)\n",
            terminal_text(&category.name)
        ),
    ))
}

fn cmd_add(store: &mut Store, arguments: &AddArgs, json_mode: bool) -> Result<Rendered, CliError> {
    let raw_title = arguments
        .title
        .as_deref()
        .or(arguments.title_pos.as_deref())
        .unwrap_or_default()
        .trim();
    let (title, inline_due) = split_inline_title(raw_title)?;
    if title.is_empty() {
        return Err(CliError::validation(
            "title required (positional or --title)",
        ));
    }
    let mut body = arguments
        .body
        .as_deref()
        .map(body_from_text)
        .unwrap_or_default();
    for subtask in &arguments.subtasks {
        let text = subtask.trim();
        if text.is_empty() {
            return Err(CliError::validation("--subtask text cannot be empty"));
        }
        body.push(Block::todo(text, false));
    }
    let due = if arguments.due.is_none() && arguments.time.is_none() {
        inline_due
    } else {
        due_for_add(arguments.due.as_deref(), arguments.time.as_deref())?
    };
    let category_query = arguments.category.clone();
    let importance = arguments.importance;
    let (task_id, snapshot) = store.update_with_snapshot(|data| {
        let category_id = category_query
            .as_deref()
            .map(|query| data.resolve_category_id(query))
            .transpose()?;
        let task = data.create_task(title, body, due, importance, category_id)?;
        Ok(task.id)
    })?;
    let task = snapshot.task(&task_id)?.clone();
    let categories = snapshot.categories;
    let subtasks = collect_subtasks(&task.body).len();
    let plain = if subtasks == 0 {
        format!(
            "added {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&task.title)
        )
    } else {
        format!(
            "added {}  {}  ({} subtask{})\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&task.title),
            subtasks,
            if subtasks == 1 { "" } else { "s" }
        )
    };
    Ok(rendered(json_mode, task_json(&categories, &task), plain))
}

fn cmd_show(store: &Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let id = data.resolve_task_id(query)?;
    let task = data.task(&id)?;
    let mut plain = format!(
        "id:         {}\ntitle:      {}\ndone:       {}\ncategory:   {}\ndue:        {}\nimportance: {} ({})\ncreated:    {}\n",
        terminal_text(&task.id),
        terminal_text(&task.title),
        task.done,
        terminal_text(category_name(&data.categories, task).unwrap_or("—")),
        if task.due.is_empty() {
            "—".into()
        } else {
            terminal_text(&task.due)
        },
        task.importance,
        crate::model::importance_marks(task.importance),
        terminal_text(&task.created),
    );
    let subtasks = collect_subtasks(&task.body);
    if subtasks.is_empty() {
        plain.push_str("subtasks:   —\n");
    } else {
        plain.push_str(&format!(
            "subtasks:   {}/{}\n",
            subtasks.iter().filter(|(_, _, done)| *done).count(),
            subtasks.len()
        ));
        for (index, text, done) in &subtasks {
            plain.push_str(&format!(
                "  {index}. {} {}\n",
                if *done { "[✓]" } else { "[ ]" },
                terminal_text(text)
            ));
        }
    }
    let notes = body_note_lines(&task.body);
    if notes.is_empty() {
        plain.push_str("body:       —\n");
    } else {
        plain.push_str("body:\n");
        for note in notes {
            plain.push_str(&format!("  {}\n", terminal_text(&note)));
        }
    }
    Ok(rendered(
        json_mode,
        task_json(&data.categories, task),
        plain,
    ))
}

fn body_note_lines(body: &[Block]) -> Vec<String> {
    let mut numbered = 0usize;
    let mut notes = Vec::new();
    for block in body {
        match block {
            Block::Todo { .. } => numbered = 0,
            Block::Text { text } => {
                numbered = 0;
                if !text.trim().is_empty() {
                    notes.push(text.clone());
                }
            }
            Block::Bullet { text } => {
                numbered = 0;
                notes.push(format!("- {text}"));
            }
            Block::Number { text } => {
                numbered += 1;
                notes.push(format!("{numbered}. {text}"));
            }
            Block::Link { url } => {
                numbered = 0;
                notes.push(url.clone());
            }
            Block::Image { attachment_id } => {
                numbered = 0;
                notes.push(format!("[image:{attachment_id}]"));
            }
        }
    }
    notes
}

fn cmd_set_done(
    store: &mut Store,
    query: &str,
    done: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let query = query.to_string();
    let (task, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(&query)?;
        data.set_task_done(&id, done)
    })?;
    let categories = snapshot.categories;
    Ok(rendered(
        json_mode,
        task_json(&categories, &task),
        format!(
            "{} {}  {}\n",
            if done { "done" } else { "undone" },
            terminal_text(&short_id(&task.id)),
            terminal_text(&task.title)
        ),
    ))
}

fn cmd_delete(store: &mut Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let query = query.to_string();
    let (task, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(&query)?;
        data.delete_task(&id)
    })?;
    let categories = snapshot.categories;
    Ok(rendered(
        json_mode,
        task_json(&categories, &task),
        format!(
            "deleted {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&task.title)
        ),
    ))
}

fn cmd_move(
    store: &mut Store,
    query: &str,
    before: Option<&str>,
    after: Option<&str>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (relation, position, target_query) = match (before, after) {
        (Some(target), None) => ("before", RelativePosition::Before, target),
        (None, Some(target)) => ("after", RelativePosition::After, target),
        _ => {
            return Err(CliError::validation(
                "pass exactly one of --before or --after",
            ));
        }
    };
    let query = query.to_string();
    let target_query = target_query.to_string();
    let ((task, target), snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(&query)?;
        let target_id = data.resolve_task_id(&target_query)?;
        let target = data.task(&target_id)?.clone();
        let task = data.move_task_relative(&id, &target_id, position)?;
        Ok((task, target))
    })?;
    let categories = snapshot.categories;
    let value = json!({
        "moved": task_json(&categories, &task),
        "relation": relation,
        "target": { "id": target.id, "title": target.title },
    });
    let plain = format!(
        "moved {} {relation} {}\n",
        terminal_text(&short_id(&task.id)),
        terminal_text(&short_id(&target.id))
    );
    Ok(rendered(json_mode, value, plain))
}

fn cmd_purge(
    store: &mut Store,
    confirmed_done: bool,
    category: Option<&str>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    if !confirmed_done {
        return Err(CliError::validation(
            "refusing to purge without the explicit --done flag",
        ));
    }
    let category = category.map(str::to_string);
    let (removed, snapshot) = store.update_with_snapshot(|data| {
        let scope = match category.as_deref() {
            Some(query) => PurgeScope::Category(data.resolve_category_id(query)?),
            None => PurgeScope::All,
        };
        data.purge_completed(&scope)
    })?;
    let categories = snapshot.categories;
    let value = json!({
        "purged": removed
            .iter()
            .map(|task| task_json(&categories, task))
            .collect::<Vec<_>>(),
        "count": removed.len(),
    });
    let plain = format!("purged {} completed task(s)\n", removed.len());
    Ok(rendered(json_mode, value, plain))
}

fn cmd_edit(
    store: &mut Store,
    arguments: &EditArgs,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    if arguments.title.is_none()
        && arguments.body.is_none()
        && arguments.due.is_none()
        && arguments.time.is_none()
        && !arguments.clear_due
        && arguments.category.is_none()
        && !arguments.clear_cat
        && arguments.importance.is_none()
    {
        return Err(CliError::validation(
            "nothing to edit; pass --title / --body / --due / --time / --clear-due / --category / --clear-category / --importance",
        ));
    }
    if arguments.clear_due && (arguments.due.is_some() || arguments.time.is_some()) {
        return Err(CliError::validation(
            "--clear-due cannot be combined with --due / --time",
        ));
    }
    if arguments.clear_cat && arguments.category.is_some() {
        return Err(CliError::validation(
            "--clear-category cannot be combined with --category",
        ));
    }
    let query = arguments.id.clone();
    let (title, inline_due) = match arguments.title.as_deref() {
        Some(title) => {
            let (title, inline_due) = split_inline_title(title)?;
            (Some(title), inline_due)
        }
        None => (None, String::new()),
    };
    let body = arguments.body.as_deref().map(body_from_text);
    let due_argument = arguments.due.clone();
    let time_argument = arguments.time.clone();
    let clear_due = arguments.clear_due;
    let category_query = arguments.category.clone();
    let clear_category = arguments.clear_cat;
    let importance = arguments.importance;
    let (task_id, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(&query)?;
        let due = if clear_due {
            Some(String::new())
        } else if due_argument.is_none() && time_argument.is_none() && !inline_due.is_empty() {
            Some(inline_due)
        } else {
            due_for_edit(
                &data.task(&id)?.due,
                due_argument.as_deref(),
                time_argument.as_deref(),
            )
            .map_err(|error| StoreError::validation(error.message))?
        };
        let category_id = if clear_category {
            Some(None)
        } else {
            category_query
                .as_deref()
                .map(|query| data.resolve_category_id(query).map(Some))
                .transpose()?
        };
        let task = data.edit_task(
            &id,
            TaskPatch {
                title,
                body,
                due,
                importance,
                category_id,
                ..TaskPatch::default()
            },
        )?;
        Ok(task.id)
    })?;
    let task = snapshot.task(&task_id)?.clone();
    let categories = snapshot.categories;
    Ok(rendered(
        json_mode,
        task_json(&categories, &task),
        format!(
            "updated {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&task.title)
        ),
    ))
}

// --------------------------------------------------------------- subtasks

fn cmd_subtasks_list(store: &Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let id = data.resolve_task_id(query)?;
    let task = data.task(&id)?;
    let subtasks = collect_subtasks(&task.body);
    let value = json!({
        "task_id": task.id,
        "title": task.title,
        "subtasks": subtasks_json(&task.body),
        "done": subtasks.iter().filter(|(_, _, done)| *done).count(),
        "total": subtasks.len(),
    });
    let mut plain = format!(
        "{}  {}",
        terminal_text(&short_id(&task.id)),
        terminal_text(&task.title)
    );
    if subtasks.is_empty() {
        plain.push_str("  (no subtasks)\n");
    } else {
        plain.push('\n');
        for (index, text, done) in &subtasks {
            let check = if *done { "[✓]" } else { "[ ]" };
            plain.push_str(&format!("  {index}. {check} {}\n", terminal_text(text)));
        }
        plain.push_str(&format!(
            "— {}/{} done\n",
            subtasks.iter().filter(|(_, _, done)| *done).count(),
            subtasks.len()
        ));
    }
    Ok(rendered(json_mode, value, plain))
}

fn cmd_subtask_add(
    store: &mut Store,
    query: &str,
    text: &str,
    done: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(CliError::validation(
            "subtask text required (positional or --text)",
        ));
    }
    let query = query.to_string();
    let (task, index) = store.update(|data| {
        let id = data.resolve_task_id(&query)?;
        let mut body = data.task(&id)?.body.clone();
        body.push(Block::todo(&text, done));
        let task = data.edit_task(
            &id,
            TaskPatch {
                body: Some(body),
                ..TaskPatch::default()
            },
        )?;
        Ok((task, collect_subtasks(&data.task(&id)?.body).len()))
    })?;
    Ok(rendered(
        json_mode,
        json!({
            "task_id": task.id,
            "index": index,
            "text": text,
            "done": done,
            "subtasks": subtasks_json(&task.body),
        }),
        format!(
            "added subtask {index} on {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&text)
        ),
    ))
}

fn cmd_subtask_set_done(
    store: &mut Store,
    query: &str,
    index: usize,
    done: Option<bool>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let query = query.to_string();
    let (task, text, new_done) = store.update(|data| {
        let id = data.resolve_task_id(&query)?;
        let mut body = data.task(&id)?.body.clone();
        let body_index = subtask_body_index(&body, index)
            .map_err(|error| StoreError::validation(error.message))?;
        let Block::Todo { text, done: value } = &mut body[body_index] else {
            return Err(StoreError::Corrupt(
                "resolved subtask index does not point to a subtask".into(),
            ));
        };
        let new_done = done.unwrap_or(!*value);
        *value = new_done;
        let text = text.clone();
        let task = data.edit_task(
            &id,
            TaskPatch {
                body: Some(body),
                ..TaskPatch::default()
            },
        )?;
        Ok((task, text, new_done))
    })?;
    Ok(rendered(
        json_mode,
        json!({
            "task_id": task.id,
            "index": index,
            "text": text,
            "done": new_done,
            "subtasks": subtasks_json(&task.body),
        }),
        format!(
            "{} subtask {index} on {}  {}\n",
            if new_done { "done" } else { "undone" },
            terminal_text(&short_id(&task.id)),
            terminal_text(&text)
        ),
    ))
}

fn cmd_subtask_edit(
    store: &mut Store,
    query: &str,
    index: usize,
    text: &str,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(CliError::validation(
            "subtask text required (positional or --text)",
        ));
    }
    let query = query.to_string();
    let (task, done) = store.update(|data| {
        let id = data.resolve_task_id(&query)?;
        let mut body = data.task(&id)?.body.clone();
        let body_index = subtask_body_index(&body, index)
            .map_err(|error| StoreError::validation(error.message))?;
        let Block::Todo {
            text: current,
            done,
        } = &mut body[body_index]
        else {
            return Err(StoreError::Corrupt(
                "resolved subtask index does not point to a subtask".into(),
            ));
        };
        *current = text.clone();
        let done = *done;
        let task = data.edit_task(
            &id,
            TaskPatch {
                body: Some(body),
                ..TaskPatch::default()
            },
        )?;
        Ok((task, done))
    })?;
    Ok(rendered(
        json_mode,
        json!({
            "task_id": task.id,
            "index": index,
            "text": text,
            "done": done,
            "subtasks": subtasks_json(&task.body),
        }),
        format!(
            "updated subtask {index} on {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&text)
        ),
    ))
}

fn cmd_subtask_delete(
    store: &mut Store,
    query: &str,
    index: usize,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let query = query.to_string();
    let (task, text, done) = store.update(|data| {
        let id = data.resolve_task_id(&query)?;
        let mut body = data.task(&id)?.body.clone();
        let body_index = subtask_body_index(&body, index)
            .map_err(|error| StoreError::validation(error.message))?;
        let Block::Todo { text, done } = body.remove(body_index) else {
            return Err(StoreError::Corrupt(
                "resolved subtask index does not point to a subtask".into(),
            ));
        };
        let task = data.edit_task(
            &id,
            TaskPatch {
                body: Some(body),
                ..TaskPatch::default()
            },
        )?;
        Ok((task, text, done))
    })?;
    Ok(rendered(
        json_mode,
        json!({
            "task_id": task.id,
            "deleted": { "index": index, "text": text, "done": done },
            "subtasks": subtasks_json(&task.body),
        }),
        format!(
            "deleted subtask {index} on {}  {}\n",
            terminal_text(&short_id(&task.id)),
            terminal_text(&text)
        ),
    ))
}
