//! Command-line front end for humans and agents.
//!
//! Every invocation owns one [`Store`]. Read commands use one snapshot and
//! mutations execute one fresh read-modify-write transaction. JSON mode emits
//! exactly one document on stdout, including usage and runtime errors.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum, builder::PossibleValue};
use serde_json::{Value, json};

use crate::VERSION;
use crate::model::{
    Block, Category, Label, LabelColor, Task, caseless_key, labels_for_task, task_matches_query,
};
use crate::store::{
    CategoryPatch, LabelPatch, PurgeScope, RelativePosition, Store, StoreData, StoreError,
    TaskPatch,
};

/// Full CLI reference under `mach --help`.
const HELP: &str = "\
  list
    --query QUERY        search task text and label names
    -c, --category NAME  only this category
    --label NAME         require label (repeatable; all must match)
    --open               only incomplete
    --done               only completed

  categories
    (no args)            list categories (done/total)
    add NAME
      -d, --description TEXT
    ensure NAME
      -d, --description TEXT  create if missing; conflict if different
    edit NAME            rename / set description
      -n, --name NEW
      -d, --description TEXT
      --clear-description
    delete NAME          delete category; tasks become uncategorized

  labels
    (no args)            list labels (done/total)
    add NAME [--color COLOR]
    ensure NAME [--color COLOR]
                         create if missing; conflict if different
    edit NAME [--name NEW] [--color COLOR]
                         edit label; assignments stay attached
    delete NAME          delete label; tasks stay in place

  Label colors: red, orange, yellow, lime, green, teal, cyan, blue, indigo, purple, pink, brown

  add [TITLE]
    -t, --title TITLE    title (required if no positional TITLE)
    -d, --description TEXT  description (newlines = lines; see DESCRIPTION MARKUP)
    --due DATE            YYYY-MM-DD | MM-DD | HH:MM | DATEThh:mm
    --time HH:MM         with --due, or alone = next occurrence
    -c, --category NAME  category (name or unique prefix)
    --label NAME         existing label (repeatable)
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
    -d, --description TEXT  replace entire description (DESCRIPTION MARKUP; wipes old description)
    --due DATE            date-only keeps existing time
    --time HH:MM         keeps existing date if no --due
    --clear-due          remove due date/time
    -c, --category NAME
    --clear-category     uncategorized
    --add-label NAME     assign existing label (repeatable)
    --remove-label NAME  unassign label (repeatable)
    --clear-labels       remove all labels; may combine with --add-label
    -i, --importance N   0–3

  DESCRIPTION MARKUP (add/edit --description, one block per line)
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

  export [FILE]          portable .mach archive (tasks, categories, labels, images)
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
        /// Search task text and label names
        #[arg(long = "query", value_name = "QUERY")]
        query: Option<String>,
        /// Category name / prefix
        #[arg(short = 'c', long = "category", value_name = "NAME")]
        category: Option<String>,
        /// Require label (repeatable; all must match)
        #[arg(long = "label", value_name = "NAME")]
        labels: Vec<String>,
        /// Incomplete only
        #[arg(long, conflicts_with = "done")]
        open: bool,
        /// Done only
        #[arg(long)]
        done: bool,
    },
    /// List / add / ensure / edit / delete categories
    Categories {
        #[command(subcommand)]
        action: Option<CatAction>,
    },
    /// List / add / ensure / edit / delete labels
    Labels {
        #[command(subcommand)]
        action: Option<LabelAction>,
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
    /// Export tasks, categories, labels, and images to a portable archive
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
    /// Description text (newlines → lines)
    #[arg(short = 'd', long = "description")]
    description: Option<String>,
    /// Due date
    #[arg(long = "due", value_name = "DATE")]
    due: Option<String>,
    /// Due time HH:MM (with --due, or alone = next occurrence)
    #[arg(long = "time", value_name = "HH:MM")]
    time: Option<String>,
    /// Category
    #[arg(short = 'c', long = "category", value_name = "NAME")]
    category: Option<String>,
    /// Existing label (repeatable)
    #[arg(long = "label", value_name = "NAME")]
    labels: Vec<String>,
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
    /// Replace description
    #[arg(short = 'd', long = "description")]
    description: Option<String>,
    /// Due date
    #[arg(long = "due", value_name = "DATE")]
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
    /// Assign existing label (repeatable)
    #[arg(long = "add-label", value_name = "NAME")]
    add_labels: Vec<String>,
    /// Unassign label (repeatable)
    #[arg(long = "remove-label", value_name = "NAME")]
    remove_labels: Vec<String>,
    /// Remove all labels; may be combined with --add-label
    #[arg(long = "clear-labels")]
    clear_labels: bool,
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
    /// Return an exact-name category, or create it
    Ensure {
        /// Exact name identity
        name: String,
        /// If the category exists, its description must match or the command conflicts
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
enum LabelAction {
    /// List labels (default)
    List,
    /// Create label
    Add {
        /// Name
        name: String,
        /// Logical color (automatically balanced when omitted)
        #[arg(long, value_enum)]
        color: Option<LabelColor>,
    },
    /// Return an exact-name label, or create it
    Ensure {
        /// Exact name identity
        name: String,
        /// If the label exists, its color must match or the command conflicts
        #[arg(long, value_enum)]
        color: Option<LabelColor>,
    },
    /// Edit label name or color
    Edit {
        /// Current name / prefix
        name: String,
        /// New name
        #[arg(
            short = 'n',
            long = "name",
            value_name = "NEW",
            required_unless_present = "color"
        )]
        new_name: Option<String>,
        /// Logical color
        #[arg(long, value_enum)]
        color: Option<LabelColor>,
    },
    /// Delete label (tasks remain in place)
    Delete {
        /// Name / prefix
        name: String,
    },
}

impl ValueEnum for LabelColor {
    fn value_variants<'a>() -> &'a [Self] {
        &Self::SWATCHES
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.as_str()))
    }
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
            StoreError::Conflict { .. }
            | StoreError::MetadataConflict { .. }
            | StoreError::StaleEntity { .. } => "conflict",
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

fn rendered(
    json_mode: bool,
    json: impl FnOnce() -> Value,
    plain: impl FnOnce() -> String,
) -> Rendered {
    if json_mode {
        Rendered::Json(json())
    } else {
        Rendered::Plain(plain())
    }
}

pub fn run() {
    let arguments = normalize_documented_description_values(std::env::args_os().collect());
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
/// Preserve that unambiguous behavior except for the documented `- ` description
/// bullet; explicit `--description=...` remains the escape hatch for all other text.
fn normalize_documented_description_values(arguments: Vec<OsString>) -> Vec<OsString> {
    let mut normalized = Vec::with_capacity(arguments.len());
    let mut arguments = arguments.into_iter().peekable();
    let mut options = true;
    while let Some(argument) = arguments.next() {
        if options && argument == "--" {
            options = false;
            normalized.push(argument);
            continue;
        }
        let description_option = options && (argument == "--description" || argument == "-d");
        let documented_bullet = description_option
            && arguments
                .peek()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with("- "));
        if documented_bullet {
            let value = arguments
                .next()
                .expect("peeked description value must exist");
            let mut combined = OsString::from("--description=");
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
            query,
            category,
            labels,
            open,
            done,
        } => cmd_list(
            store,
            query.as_deref(),
            category.as_deref(),
            &labels,
            open,
            done,
            json_mode,
        ),
        Command::Categories { action } => match action {
            None | Some(CatAction::List) => cmd_categories_list(store, json_mode),
            Some(CatAction::Add { name, description }) => {
                cmd_category_add(store, &name, description.as_deref(), json_mode)
            }
            Some(CatAction::Ensure { name, description }) => {
                cmd_category_ensure(store, &name, description.as_deref(), json_mode)
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
        Command::Labels { action } => match action {
            None | Some(LabelAction::List) => cmd_labels_list(store, json_mode),
            Some(LabelAction::Add { name, color }) => cmd_label_add(store, &name, color, json_mode),
            Some(LabelAction::Ensure { name, color }) => {
                cmd_label_ensure(store, &name, color, json_mode)
            }
            Some(LabelAction::Edit {
                name,
                new_name,
                color,
            }) => cmd_label_edit(store, &name, new_name.as_deref(), color, json_mode),
            Some(LabelAction::Delete { name }) => cmd_label_delete(store, &name, json_mode),
        },
        Command::Add(arguments) => cmd_add(store, &arguments, json_mode),
        Command::Show { id } => cmd_show(store, &id, json_mode),
        Command::Done { id } => cmd_set_done(store, &id, true, json_mode),
        Command::Undone { id } => cmd_set_done(store, &id, false, json_mode),
        Command::Delete { id } => cmd_delete(store, &id, json_mode),
        Command::Move { id, before, after } => {
            cmd_move(store, &id, before.as_deref(), after.as_deref(), json_mode)
        }
        Command::Purge { done: _, category } => cmd_purge(store, category.as_deref(), json_mode),
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
    Ok(rendered(
        json_mode,
        || {
            json!({
                "ok": true,
                "archive": summary.path.display().to_string(),
                "tasks": summary.tasks,
                "categories": summary.categories,
                "labels": summary.labels,
                "images": summary.images,
            })
        },
        || {
            let contents = crate::archive::content_count_text(
                summary.tasks,
                summary.categories,
                summary.labels,
                summary.images,
            );
            format!(
                "exported {contents} to {}\n",
                terminal_text(&summary.path.display().to_string())
            )
        },
    ))
}

fn cmd_import(
    store: &mut Store,
    path: &std::path::Path,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let summary = crate::archive::import(store, path)?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "ok": true,
                "archive": summary.path.display().to_string(),
                "tasks_added": summary.tasks_added,
                "tasks_unchanged": summary.tasks_unchanged,
                "categories_added": summary.categories_added,
                "categories_unchanged": summary.categories_unchanged,
                "labels_added": summary.labels_added,
                "labels_unchanged": summary.labels_unchanged,
                "images_added": summary.images_added,
                "images_unchanged": summary.images_unchanged,
            })
        },
        || {
            let added = crate::archive::content_count_text(
                summary.tasks_added,
                summary.categories_added,
                summary.labels_added,
                summary.images_added,
            );
            let unchanged = crate::archive::content_count_text(
                summary.tasks_unchanged,
                summary.categories_unchanged,
                summary.labels_unchanged,
                summary.images_unchanged,
            );
            if summary.changed() {
                format!("imported {added}; {unchanged} already present\n")
            } else {
                format!("nothing imported; {unchanged} already present\n")
            }
        },
    ))
}

fn cmd_update(do_install: bool, json_mode: bool) -> Result<Rendered, CliError> {
    let now = Utc::now().timestamp();
    let mut update_state = crate::update_state::UpdateStateStore::open_default().ok();
    let lease = update_state
        .as_mut()
        .and_then(|store| store.claim_manual(now).ok());
    let checked = match crate::update::check_with_etag(None) {
        Ok(crate::update::CheckResponse::Modified { value: info, etag }) => (info, etag),
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

    Ok(rendered(
        json_mode,
        || {
            json!({
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
            })
        },
        || {
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
            } else if let Some(result) = &install {
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
            plain
        },
    ))
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

fn description_from_text(text: &str) -> Vec<Block> {
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

fn description_to_text(description: &[Block]) -> String {
    let mut numbered = 0usize;
    description
        .iter()
        .map(|block| description_block_text(block, &mut numbered))
        .collect::<Vec<_>>()
        .join("\n")
}

fn description_block_text(block: &Block, numbered: &mut usize) -> String {
    match block {
        Block::Text { text } => {
            *numbered = 0;
            text.clone()
        }
        Block::Todo { text, done } => {
            *numbered = 0;
            format!("[{}] {text}", if *done { "x" } else { " " })
        }
        Block::Bullet { text } => {
            *numbered = 0;
            format!("- {text}")
        }
        Block::Number { text } => {
            *numbered += 1;
            format!("{numbered}. {text}")
        }
        Block::Link { url } => {
            *numbered = 0;
            url.clone()
        }
        Block::Image { attachment_id } => {
            *numbered = 0;
            format!("[image:{attachment_id}]")
        }
    }
}

fn collect_subtasks(description: &[Block]) -> Vec<(usize, &str, bool)> {
    description
        .iter()
        .filter_map(|block| match block {
            Block::Todo { text, done } => Some((text.as_str(), *done)),
            _ => None,
        })
        .enumerate()
        .map(|(index, (text, done))| (index + 1, text, done))
        .collect()
}

fn subtask_description_index(description: &[Block], one_based: usize) -> Result<usize, StoreError> {
    if one_based == 0 {
        return Err(StoreError::validation(
            "subtask index is 1-based (use 1 for the first subtask)",
        ));
    }
    let mut count = 0usize;
    for (description_index, block) in description.iter().enumerate() {
        if matches!(block, Block::Todo { .. }) {
            count += 1;
            if count == one_based {
                return Ok(description_index);
            }
        }
    }
    Err(StoreError::validation(format!(
        "no subtask at index {one_based} (task has {count} subtask(s))"
    )))
}

fn subtasks_json(description: &[Block]) -> Vec<Value> {
    subtasks_to_json(&collect_subtasks(description))
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

fn label_json(label: &Label) -> Value {
    json!({
        "id": label.id,
        "name": label.name,
        "color": label.color,
    })
}

fn task_label_text(labels: &[Label], task: &Task) -> String {
    labels_for_task(task, labels)
        .map(|label| terminal_text(&label.name))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_label_ids(data: &StoreData, queries: &[String]) -> Result<Vec<String>, StoreError> {
    let mut selected = HashSet::with_capacity(queries.len());
    for query in queries {
        selected.insert(data.resolve_label_id(query)?);
    }
    Ok(data
        .labels
        .iter()
        .filter(|label| selected.contains(&label.id))
        .map(|label| label.id.clone())
        .collect())
}

fn task_json(categories: &[Category], labels: &[Label], task: &Task) -> Value {
    task_json_with_category(labels, task, category_name(categories, task))
}

fn task_json_with_category(labels: &[Label], task: &Task, category_name: Option<&str>) -> Value {
    let subtasks = collect_subtasks(&task.description);
    let subtasks_json = subtasks_to_json(&subtasks);
    json!({
        "id": task.id,
        "title": task.title,
        "description": description_to_text(&task.description),
        "subtasks": subtasks_json,
        "subtasks_done": subtasks.iter().filter(|(_, _, done)| *done).count(),
        "subtasks_total": subtasks.len(),
        "due": task.due,
        "done": task.done,
        "importance": task.importance,
        "category": {
            "id": task.category_id,
            "name": category_name,
        },
        "labels": labels_for_task(task, labels)
            .map(label_json)
            .collect::<Vec<_>>(),
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
    if crate::due::parse_time(value).is_some() {
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
    task: &Task,
    category_name: Option<&str>,
    labels: &[Label],
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
        format!("  [{}]", terminal_text(category_name.unwrap_or("—")))
    } else {
        String::new()
    };
    let label_text = task_label_text(labels, task);
    let labels = if label_text.is_empty() {
        String::new()
    } else {
        format!("  {label_text}")
    };
    let progress = crate::model::todo_progress(task)
        .map(|(done, total)| format!("  ({done}/{total})"))
        .unwrap_or_default();
    format!(
        "{} {check} {title}{category}{labels}{due}{flag}{progress}\n",
        terminal_text(&short_id(&task.id))
    )
}

// ---------------------------------------------------------------- commands

fn cmd_list(
    store: &Store,
    query: Option<&str>,
    category: Option<&str>,
    label_queries: &[String],
    open_only: bool,
    done_only: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let query = query.map(str::trim);
    if query.is_some_and(str::is_empty) {
        return Err(CliError::validation("search query cannot be empty"));
    }
    let query_key = query.map(caseless_key);
    let data = store.snapshot()?;
    let category_id = category
        .map(|query| data.resolve_category_id(query))
        .transpose()?;
    let label_ids = resolve_label_ids(&data, label_queries)?;
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
            label_ids
                .iter()
                .all(|label_id| task.label_ids.contains(label_id))
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
        .filter(|task| {
            query_key
                .as_deref()
                .is_none_or(|query| task_matches_query(task, &data.labels, query))
        })
        .collect();
    let category_names: HashMap<_, _> = data
        .categories
        .iter()
        .map(|category| (category.id.as_str(), category.name.as_str()))
        .collect();
    let name_for = |task: &Task| {
        task.category_id
            .as_deref()
            .and_then(|id| category_names.get(id).copied())
    };
    Ok(rendered(
        json_mode,
        || {
            Value::Array(
                tasks
                    .iter()
                    .map(|task| task_json_with_category(&data.labels, task, name_for(task)))
                    .collect(),
            )
        },
        || {
            let mut plain = String::new();
            if tasks.is_empty() {
                plain.push_str("(no tasks)\n");
            } else {
                for task in &tasks {
                    plain.push_str(&task_line(
                        task,
                        name_for(task),
                        &data.labels,
                        show_category,
                        &data.settings.date_format,
                    ));
                }
                let done = tasks.iter().filter(|task| task.done).count();
                plain.push_str(&format!("— {} task(s), {done} done\n", tasks.len()));
            }
            plain
        },
    ))
}

fn cmd_categories_list(store: &Store, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let category_indices: HashMap<_, _> = data
        .categories
        .iter()
        .enumerate()
        .map(|(index, category)| (category.id.as_str(), index))
        .collect();
    let mut counts = vec![(0usize, 0usize); data.categories.len()];
    let mut uncategorized = (0usize, 0usize);
    for task in &data.tasks {
        let count = match task.category_id.as_deref() {
            Some(id) => category_indices.get(id).map(|index| &mut counts[*index]),
            None => Some(&mut uncategorized),
        };
        if let Some((done, total)) = count {
            *done += usize::from(task.done);
            *total += 1;
        }
    }
    Ok(rendered(
        json_mode,
        || {
            let categories: Vec<_> = data
                .categories
                .iter()
                .zip(&counts)
                .map(|(category, (done, total))| {
                    json!({
                        "id": category.id,
                        "name": category.name,
                        "description": category.description,
                        "total": total,
                        "done": done,
                    })
                })
                .collect();
            json!({
                "categories": categories,
                "uncategorized": {
                    "total": uncategorized.1,
                    "done": uncategorized.0,
                },
            })
        },
        || {
            let mut plain = String::new();
            if data.categories.is_empty() {
                plain.push_str("(no categories)\n");
            } else {
                for (category, (done, total)) in data.categories.iter().zip(&counts) {
                    plain.push_str(&format!(
                        "{}  {done}/{total}\n",
                        terminal_text(&category.name),
                    ));
                }
            }
            if uncategorized.1 > 0 {
                plain.push_str(&format!(
                    "— uncategorized  {}/{}\n",
                    uncategorized.0, uncategorized.1
                ));
            }
            plain
        },
    ))
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
        || category_json(&category),
        || format!("created category {}\n", terminal_text(&category.name)),
    ))
}

fn cmd_category_ensure(
    store: &mut Store,
    name: &str,
    description: Option<&str>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (category, created) = store.ensure_category(name, description.map(str::to_string))?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "created": created,
                "category": category_json(&category),
            })
        },
        || {
            if created {
                format!("created category {}\n", terminal_text(&category.name))
            } else {
                format!(
                    "category {} already exists\n",
                    terminal_text(&category.name)
                )
            }
        },
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
    let category = store.update(|data| {
        let id = data.resolve_category_id(query)?;
        data.edit_category(&id, patch)
    })?;
    Ok(rendered(
        json_mode,
        || category_json(&category),
        || format!("updated category {}\n", terminal_text(&category.name)),
    ))
}

fn cmd_category_delete(
    store: &mut Store,
    query: &str,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let category = store.update(|data| {
        let id = data.resolve_category_id(query)?;
        data.delete_category(&id)
    })?;
    Ok(rendered(
        json_mode,
        || json!({ "deleted": category.name, "id": category.id }),
        || {
            format!(
                "deleted category {} (tasks uncategorized)\n",
                terminal_text(&category.name)
            )
        },
    ))
}

fn cmd_labels_list(store: &Store, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let label_indices: HashMap<_, _> = data
        .labels
        .iter()
        .enumerate()
        .map(|(index, label)| (label.id.as_str(), index))
        .collect();
    let mut counts = vec![(0usize, 0usize); data.labels.len()];
    for task in &data.tasks {
        for label_id in &task.label_ids {
            if let Some(index) = label_indices.get(label_id.as_str()) {
                counts[*index].0 += usize::from(task.done);
                counts[*index].1 += 1;
            }
        }
    }
    Ok(rendered(
        json_mode,
        || {
            json!({
                "labels": data.labels.iter().zip(&counts).map(|(label, (done, total))| {
                    json!({
                        "id": label.id,
                        "name": label.name,
                        "color": label.color,
                        "total": total,
                        "done": done,
                    })
                }).collect::<Vec<_>>(),
            })
        },
        || {
            if data.labels.is_empty() {
                return "(no labels)\n".to_string();
            }
            data.labels
                .iter()
                .zip(&counts)
                .map(|(label, (done, total))| {
                    format!(
                        "{}  {}  {done}/{total}\n",
                        terminal_text(&label.name),
                        label.color
                    )
                })
                .collect()
        },
    ))
}

fn cmd_label_add(
    store: &mut Store,
    name: &str,
    color: Option<LabelColor>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let label = store.update(|data| match color {
        Some(color) => data.create_label_with_color(name, color),
        None => data.create_label(name),
    })?;
    Ok(rendered(
        json_mode,
        || label_json(&label),
        || {
            format!(
                "created label {} ({})\n",
                terminal_text(&label.name),
                label.color
            )
        },
    ))
}

fn cmd_label_ensure(
    store: &mut Store,
    name: &str,
    color: Option<LabelColor>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (label, created) = store.ensure_label(name, color)?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "created": created,
                "label": label_json(&label),
            })
        },
        || {
            if created {
                format!(
                    "created label {} ({})\n",
                    terminal_text(&label.name),
                    label.color
                )
            } else {
                format!(
                    "label {} ({}) already exists\n",
                    terminal_text(&label.name),
                    label.color
                )
            }
        },
    ))
}

fn cmd_label_edit(
    store: &mut Store,
    query: &str,
    new_name: Option<&str>,
    color: Option<LabelColor>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let label = store.update(|data| {
        let id = data.resolve_label_id(query)?;
        data.edit_label(
            &id,
            LabelPatch {
                name: new_name.map(str::to_string),
                color,
            },
        )
    })?;
    Ok(rendered(
        json_mode,
        || label_json(&label),
        || {
            format!(
                "updated label {} ({})\n",
                terminal_text(&label.name),
                label.color
            )
        },
    ))
}

fn cmd_label_delete(store: &mut Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let (label, tasks_unassigned) = store.update(|data| {
        let id = data.resolve_label_id(query)?;
        let tasks_unassigned = data
            .tasks
            .iter()
            .filter(|task| task.label_ids.contains(&id))
            .count();
        let label = data.delete_label(&id)?;
        Ok((label, tasks_unassigned))
    })?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "deleted": label.name,
                "id": label.id,
                "tasks_unassigned": tasks_unassigned,
            })
        },
        || {
            format!(
                "deleted label {} (unassigned from {tasks_unassigned} task{})\n",
                terminal_text(&label.name),
                if tasks_unassigned == 1 { "" } else { "s" }
            )
        },
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
    let mut description = arguments
        .description
        .as_deref()
        .map(description_from_text)
        .unwrap_or_default();
    for subtask in &arguments.subtasks {
        let text = subtask.trim();
        if text.is_empty() {
            return Err(CliError::validation("--subtask text cannot be empty"));
        }
        description.push(Block::todo(text, false));
    }
    let due = if arguments.due.is_none() && arguments.time.is_none() {
        inline_due
    } else {
        due_for_add(arguments.due.as_deref(), arguments.time.as_deref())?
    };
    let category_query = arguments.category.as_deref();
    let label_queries = &arguments.labels;
    let importance = arguments.importance;
    let (task_id, snapshot) = store.update_with_snapshot(move |data| {
        let category_id = category_query
            .map(|query| data.resolve_category_id(query))
            .transpose()?;
        let label_ids = resolve_label_ids(data, label_queries)?;
        let task = data.create_task(title, description, due, importance, category_id)?;
        let task_id = task.id;
        data.set_task_labels(&task_id, label_ids)?;
        Ok(task_id)
    })?;
    let task = snapshot.task(&task_id)?.clone();
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || task_json(&categories, &labels, &task),
        || {
            let subtasks = collect_subtasks(&task.description).len();
            if subtasks == 0 {
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
            }
        },
    ))
}

fn cmd_show(store: &Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let id = data.resolve_task_id(query)?;
    let task = data.task(&id)?;
    Ok(rendered(
        json_mode,
        || task_json(&data.categories, &data.labels, task),
        || {
            let labels = task_label_text(&data.labels, task);
            let mut plain = format!(
                "id:         {}\ntitle:      {}\ndone:       {}\ncategory:   {}\nlabels:     {}\ndue:        {}\nimportance: {} ({})\ncreated:    {}\n",
                terminal_text(&task.id),
                terminal_text(&task.title),
                task.done,
                terminal_text(category_name(&data.categories, task).unwrap_or("—")),
                if labels.is_empty() { "—" } else { &labels },
                if task.due.is_empty() {
                    "—".into()
                } else {
                    terminal_text(&task.due)
                },
                task.importance,
                crate::model::importance_marks(task.importance),
                terminal_text(&task.created),
            );
            let subtasks = collect_subtasks(&task.description);
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
            let notes = description_note_lines(&task.description);
            if notes.is_empty() {
                plain.push_str("description:       —\n");
            } else {
                plain.push_str("description:\n");
                for note in notes {
                    plain.push_str(&format!("  {}\n", terminal_text(&note)));
                }
            }
            plain
        },
    ))
}

fn description_note_lines(description: &[Block]) -> Vec<String> {
    let mut numbered = 0usize;
    let mut notes = Vec::new();
    for block in description {
        match block {
            Block::Todo { .. } => {
                numbered = 0;
                continue;
            }
            Block::Text { text } if text.trim().is_empty() => {
                numbered = 0;
                continue;
            }
            _ => {}
        }
        notes.push(description_block_text(block, &mut numbered));
    }
    notes
}

fn cmd_set_done(
    store: &mut Store,
    query: &str,
    done: bool,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (task, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(query)?;
        data.set_task_done(&id, done)
    })?;
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || task_json(&categories, &labels, &task),
        || {
            format!(
                "{} {}  {}\n",
                if done { "done" } else { "undone" },
                terminal_text(&short_id(&task.id)),
                terminal_text(&task.title)
            )
        },
    ))
}

fn cmd_delete(store: &mut Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let (task, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(query)?;
        data.delete_task(&id)
    })?;
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || task_json(&categories, &labels, &task),
        || {
            format!(
                "deleted {}  {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&task.title)
            )
        },
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
    let ((task, target), snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(query)?;
        let target_id = data.resolve_task_id(target_query)?;
        let target = data.task(&target_id)?.clone();
        let task = data.move_task_relative(&id, &target_id, position)?;
        Ok((task, target))
    })?;
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "moved": task_json(&categories, &labels, &task),
                "relation": relation,
                "target": { "id": target.id, "title": target.title },
            })
        },
        || {
            format!(
                "moved {} {relation} {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&short_id(&target.id))
            )
        },
    ))
}

fn cmd_purge(
    store: &mut Store,
    category: Option<&str>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (removed, snapshot) = store.update_with_snapshot(|data| {
        let scope = match category {
            Some(query) => PurgeScope::Category(data.resolve_category_id(query)?),
            None => PurgeScope::All,
        };
        data.purge_completed(&scope)
    })?;
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "purged": removed
                    .iter()
                    .map(|task| task_json(&categories, &labels, task))
                    .collect::<Vec<_>>(),
                "count": removed.len(),
            })
        },
        || format!("purged {} completed task(s)\n", removed.len()),
    ))
}

fn cmd_edit(
    store: &mut Store,
    arguments: &EditArgs,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    if arguments.title.is_none()
        && arguments.description.is_none()
        && arguments.due.is_none()
        && arguments.time.is_none()
        && !arguments.clear_due
        && arguments.category.is_none()
        && !arguments.clear_cat
        && arguments.add_labels.is_empty()
        && arguments.remove_labels.is_empty()
        && !arguments.clear_labels
        && arguments.importance.is_none()
    {
        return Err(CliError::validation(
            "nothing to edit; pass --title / --description / --due / --time / --clear-due / --category / --clear-category / --add-label / --remove-label / --clear-labels / --importance",
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
    if arguments.clear_labels && !arguments.remove_labels.is_empty() {
        return Err(CliError::validation(
            "--clear-labels cannot be combined with --remove-label",
        ));
    }
    let query = arguments.id.as_str();
    let (title, inline_due) = match arguments.title.as_deref() {
        Some(title) => {
            let (title, inline_due) = split_inline_title(title)?;
            (Some(title), inline_due)
        }
        None => (None, String::new()),
    };
    let description = arguments.description.as_deref().map(description_from_text);
    let due_argument = arguments.due.as_deref();
    let time_argument = arguments.time.as_deref();
    let clear_due = arguments.clear_due;
    let category_query = arguments.category.as_deref();
    let clear_category = arguments.clear_cat;
    let add_label_queries = &arguments.add_labels;
    let remove_label_queries = &arguments.remove_labels;
    let clear_labels = arguments.clear_labels;
    let importance = arguments.importance;
    let (task_id, snapshot) = store.update_with_snapshot(|data| {
        let id = data.resolve_task_id(query)?;
        let add_label_ids = resolve_label_ids(data, add_label_queries)?;
        let remove_label_ids = resolve_label_ids(data, remove_label_queries)?;
        if let Some(label_id) = add_label_ids
            .iter()
            .find(|label_id| remove_label_ids.contains(label_id))
        {
            let label = data.label(label_id)?;
            return Err(StoreError::validation(format!(
                "label {:?} cannot be both added and removed",
                label.name
            )));
        }
        let due = if clear_due {
            Some(String::new())
        } else if due_argument.is_none() && time_argument.is_none() && !inline_due.is_empty() {
            Some(inline_due)
        } else {
            due_for_edit(&data.task(&id)?.due, due_argument, time_argument)
                .map_err(|error| StoreError::validation(error.message))?
        };
        let category_id = if clear_category {
            Some(None)
        } else {
            category_query
                .map(|query| data.resolve_category_id(query).map(Some))
                .transpose()?
        };
        let label_ids = if clear_labels || !add_label_ids.is_empty() || !remove_label_ids.is_empty()
        {
            let mut selected: HashSet<_> = if clear_labels {
                HashSet::new()
            } else {
                data.task(&id)?.label_ids.iter().cloned().collect()
            };
            for label_id in remove_label_ids {
                selected.remove(&label_id);
            }
            selected.extend(add_label_ids);
            Some(
                data.labels
                    .iter()
                    .filter(|label| selected.contains(&label.id))
                    .map(|label| label.id.clone())
                    .collect(),
            )
        } else {
            None
        };
        let task = data.edit_task(
            &id,
            TaskPatch {
                title,
                description,
                due,
                importance,
                category_id,
                label_ids,
                ..TaskPatch::default()
            },
        )?;
        Ok(task.id)
    })?;
    let task = snapshot.task(&task_id)?.clone();
    let categories = snapshot.categories;
    let labels = snapshot.labels;
    Ok(rendered(
        json_mode,
        || task_json(&categories, &labels, &task),
        || {
            format!(
                "updated {}  {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&task.title)
            )
        },
    ))
}

// --------------------------------------------------------------- subtasks

fn cmd_subtasks_list(store: &Store, query: &str, json_mode: bool) -> Result<Rendered, CliError> {
    let data = store.snapshot()?;
    let id = data.resolve_task_id(query)?;
    let task = data.task(&id)?;
    let subtasks = collect_subtasks(&task.description);
    let done_count = subtasks.iter().filter(|(_, _, done)| *done).count();
    Ok(rendered(
        json_mode,
        || {
            json!({
                "task_id": task.id,
                "title": task.title,
                "subtasks": subtasks_to_json(&subtasks),
                "done": done_count,
                "total": subtasks.len(),
            })
        },
        || {
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
                plain.push_str(&format!("— {done_count}/{} done\n", subtasks.len()));
            }
            plain
        },
    ))
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
    let (task, index) = store.update(|data| {
        let id = data.resolve_task_id(query)?;
        let mut description = data.task(&id)?.description.clone();
        description.push(Block::todo(&text, done));
        let task = data.edit_task(
            &id,
            TaskPatch {
                description: Some(description),
                ..TaskPatch::default()
            },
        )?;
        let index = collect_subtasks(&task.description).len();
        Ok((task, index))
    })?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": done,
                "subtasks": subtasks_json(&task.description),
            })
        },
        || {
            format!(
                "added subtask {index} on {}  {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&text)
            )
        },
    ))
}

enum SubtaskMutation<'a> {
    SetDone(Option<bool>),
    Edit(&'a str),
    Delete,
}

fn mutate_subtask(
    store: &mut Store,
    query: &str,
    index: usize,
    mutation: SubtaskMutation<'_>,
) -> Result<(Task, String, bool), CliError> {
    store
        .update(|data| {
            let id = data.resolve_task_id(query)?;
            let mut description = data.task(&id)?.description.clone();
            let description_index = subtask_description_index(&description, index)?;
            let (text, done) = match mutation {
                SubtaskMutation::SetDone(requested) => {
                    let Block::Todo { text, done } = &mut description[description_index] else {
                        unreachable!("subtask index resolved to a non-subtask block")
                    };
                    *done = requested.unwrap_or(!*done);
                    (text.clone(), *done)
                }
                SubtaskMutation::Edit(replacement) => {
                    let Block::Todo { text, done } = &mut description[description_index] else {
                        unreachable!("subtask index resolved to a non-subtask block")
                    };
                    *text = replacement.to_string();
                    (text.clone(), *done)
                }
                SubtaskMutation::Delete => {
                    let Block::Todo { text, done } = description.remove(description_index) else {
                        unreachable!("subtask index resolved to a non-subtask block")
                    };
                    (text, done)
                }
            };
            let task = data.edit_task(
                &id,
                TaskPatch {
                    description: Some(description),
                    ..TaskPatch::default()
                },
            )?;
            Ok((task, text, done))
        })
        .map_err(Into::into)
}

fn cmd_subtask_set_done(
    store: &mut Store,
    query: &str,
    index: usize,
    done: Option<bool>,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (task, text, new_done) =
        mutate_subtask(store, query, index, SubtaskMutation::SetDone(done))?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": new_done,
                "subtasks": subtasks_json(&task.description),
            })
        },
        || {
            format!(
                "{} subtask {index} on {}  {}\n",
                if new_done { "done" } else { "undone" },
                terminal_text(&short_id(&task.id)),
                terminal_text(&text)
            )
        },
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
    let (task, _, done) = mutate_subtask(store, query, index, SubtaskMutation::Edit(&text))?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": done,
                "subtasks": subtasks_json(&task.description),
            })
        },
        || {
            format!(
                "updated subtask {index} on {}  {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&text)
            )
        },
    ))
}

fn cmd_subtask_delete(
    store: &mut Store,
    query: &str,
    index: usize,
    json_mode: bool,
) -> Result<Rendered, CliError> {
    let (task, text, done) = mutate_subtask(store, query, index, SubtaskMutation::Delete)?;
    Ok(rendered(
        json_mode,
        || {
            json!({
                "task_id": task.id,
                "deleted": { "index": index, "text": text, "done": done },
                "subtasks": subtasks_json(&task.description),
            })
        },
        || {
            format!(
                "deleted subtask {index} on {}  {}\n",
                terminal_text(&short_id(&task.id)),
                terminal_text(&text)
            )
        },
    ))
}
