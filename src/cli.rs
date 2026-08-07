//! Command line front end for humans and agents.
//!
//! No subcommand → open the TUI. Subcommands read/write the data dir
//! (`--dir` > `$MACH_DIR` > `~/.mach`).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::model::{
    Block, Category, MAX_BODY_LINES, MAX_CATEGORY_COUNT, MAX_CATEGORY_NAME_LEN, MAX_IMPORTANCE,
    MAX_TASK_COUNT, Task,
};
use crate::{VERSION, banner, store};

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
    --time HH:MM         with --due, or alone = today
    -c, --category NAME  category (name or unique prefix)
    -i, --importance N   0–3 (default 0)
    --subtask TEXT       add subtask (repeatable)

  show ID                ID = uuid or unique prefix

  done ID
  undone ID

  delete ID

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
    [image:PATH]         image path under data dir

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

  update                 check GitHub for a newer release
    --install            run install.sh (release binary) if newer (or always with --force)
    --force              install even when already up to date

  (no command)           open TUI
  --json                 JSON stdout (global)
  --dir PATH             data directory (global)

Data: --dir PATH  >  $MACH_DIR  >  ~/.mach
";

#[derive(Parser)]
#[command(
    name = "mach",
    about = concat!("mach v", env!("CARGO_PKG_VERSION")),
    disable_version_flag = true,
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
    /// List / add / delete categories
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
    /// Edit task fields
    Edit(EditArgs),
    /// Subtasks on a task
    Subtasks {
        /// Parent task id / prefix
        task: String,
        #[command(subcommand)]
        action: Option<SubAction>,
    },
    /// Check GitHub for a newer release (optional install)
    Update {
        /// Run install.sh (release binary → ~/.local/bin) when an update is available
        #[arg(long)]
        install: bool,
        /// Install even if this binary is already the latest
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Args)]
struct AddArgs {
    /// Title (or use --title)
    #[arg(value_name = "TITLE")]
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
    /// Due time HH:MM (with --due, or alone = today)
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
    /// Delete category (tasks uncategorized)
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
        #[arg(value_name = "TEXT")]
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
        #[arg(value_name = "TEXT")]
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

pub fn run() {
    let cli = Cli::parse();
    if let Some(dir) = cli.dir {
        store::set_data_dir(dir);
    }

    if cli.version {
        print_version();
        return;
    }

    let json = cli.json;
    match cli.command {
        Some(Command::List {
            category,
            open,
            done,
        }) => cmd_list(category.as_deref(), open, done, json),
        Some(Command::Categories { action }) => match action {
            None | Some(CatAction::List) => cmd_cats_list(json),
            Some(CatAction::Add { name, description }) => {
                cmd_cat_add(&name, description.as_deref(), json)
            }
            Some(CatAction::Edit {
                name,
                new_name,
                description,
                clear_description,
            }) => cmd_cat_edit(
                &name,
                new_name.as_deref(),
                description.as_deref(),
                clear_description,
                json,
            ),
            Some(CatAction::Delete { name }) => cmd_cat_delete(&name, json),
        },
        Some(Command::Add(args)) => cmd_add(&args, json),
        Some(Command::Show { id }) => cmd_show(&id, json),
        Some(Command::Done { id }) => cmd_set_done(&id, true, json),
        Some(Command::Undone { id }) => cmd_set_done(&id, false, json),
        Some(Command::Delete { id }) => cmd_delete(&id, json),
        Some(Command::Edit(args)) => cmd_edit(&args, json),
        Some(Command::Subtasks { task, action }) => match action {
            None | Some(SubAction::List) => cmd_subs_list(&task, json),
            Some(SubAction::Add {
                text_pos,
                text,
                done,
            }) => {
                let text = text.or(text_pos).unwrap_or_default();
                cmd_subs_add(&task, &text, done, json);
            }
            Some(SubAction::Done { index }) => cmd_subs_set_done(&task, index, Some(true), json),
            Some(SubAction::Undone { index }) => cmd_subs_set_done(&task, index, Some(false), json),
            Some(SubAction::Toggle { index }) => cmd_subs_set_done(&task, index, None, json),
            Some(SubAction::Edit {
                index,
                text_pos,
                text,
            }) => {
                let text = text.or(text_pos).unwrap_or_default();
                cmd_subs_edit(&task, index, &text, json);
            }
            Some(SubAction::Delete { index }) => cmd_subs_delete(&task, index, json),
        },
        Some(Command::Update { install, force }) => cmd_update(install, force, json),
        None => {
            if let Err(err) = crate::run_tui() {
                eprintln!("mach: {err}");
                std::process::exit(1);
            }
        }
    }
}

fn cmd_update(do_install: bool, force: bool, json: bool) {
    let info = match crate::update::check() {
        Ok(info) => info,
        Err(err) => {
            if json {
                println!("{}", serde_json::json!({ "ok": false, "error": err }));
            } else {
                die(err);
            }
            return;
        }
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "current": info.current,
                "latest": info.latest,
                "newer": info.newer,
                "prerelease": info.prerelease,
                "url": info.release_url,
            })
        );
    } else {
        println!("{}", info.summary());
        if info.newer {
            println!();
            println!("{}", info.install_hint());
        }
    }

    if do_install {
        if !info.newer && !force {
            if !json {
                println!("Already up to date — pass --force to reinstall.");
            }
            return;
        }
        if !json {
            println!();
            println!("Installing from {} …", crate::update::GIT_URL);
        }
        if let Err(err) = crate::update::install() {
            die(err);
        }
        if json {
            // Second line for agents that already consumed the check object.
            println!("{}", serde_json::json!({ "installed": true }));
        } else {
            println!("Installed. Restart mach to use the new build.");
        }
    }
}

fn print_version() {
    for line in banner::BANNER {
        println!("{line}");
    }
    println!("\nmach v{VERSION}");
}

// ---------------------------------------------------------------- helpers

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("mach: {msg}");
    std::process::exit(1);
}

fn short_id(id: &str) -> &str {
    if id.len() >= 8 { &id[..8] } else { id }
}

/// Parse CLI body text into blocks (see HELP BODY MARKUP).
fn body_from_text(text: &str) -> Vec<Block> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().map(line_to_block).collect()
}

fn line_to_block(line: &str) -> Block {
    let t = line.trim_end();
    if let Some(rest) = t.strip_prefix("[ ] ") {
        return Block::todo(rest, false);
    }
    if let Some(rest) = t
        .strip_prefix("[x] ")
        .or_else(|| t.strip_prefix("[X] "))
        .or_else(|| t.strip_prefix("[✓] "))
    {
        return Block::todo(rest, true);
    }
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("• ")) {
        return Block::bullet(rest);
    }
    if let Some(rest) = strip_number_prefix(t) {
        return Block::number(rest);
    }
    if let Some(path) = t
        .strip_prefix("[image:")
        .and_then(|s| s.strip_suffix(']'))
        .filter(|p| !p.is_empty())
    {
        return Block::image(path);
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        return Block::link(t);
    }
    Block::text(t)
}

/// `1. rest` / `12. rest` → `rest`.
fn strip_number_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    line.get(i..)?.strip_prefix(". ")
}

fn body_to_text(body: &[Block]) -> String {
    let mut run = 0usize;
    let mut lines = Vec::with_capacity(body.len());
    for b in body {
        let line = match b {
            Block::Text { text } => {
                run = 0;
                text.clone()
            }
            Block::Todo { text, done } => {
                run = 0;
                if *done {
                    format!("[x] {text}")
                } else {
                    format!("[ ] {text}")
                }
            }
            Block::Bullet { text } => {
                run = 0;
                format!("- {text}")
            }
            Block::Number { text } => {
                run += 1;
                format!("{run}. {text}")
            }
            Block::Link { url } => {
                run = 0;
                url.clone()
            }
            Block::Image { path } => {
                run = 0;
                format!("[image:{path}]")
            }
        };
        lines.push(line);
    }
    lines.join("\n")
}

fn find_category<'a>(cats: &'a [Category], name: &str) -> Option<&'a Category> {
    let q = name.trim();
    if q.is_empty() {
        return None;
    }
    let lower = q.to_lowercase();
    if let Some(c) = cats.iter().find(|c| c.name.eq_ignore_ascii_case(q)) {
        return Some(c);
    }
    let hits: Vec<_> = cats
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&lower))
        .collect();
    if hits.len() == 1 { Some(hits[0]) } else { None }
}

fn require_category<'a>(cats: &'a [Category], name: &str) -> &'a Category {
    if let Some(c) = find_category(cats, name) {
        return c;
    }
    let lower = name.trim().to_lowercase();
    let hits: Vec<_> = cats
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&lower))
        .map(|c| c.name.as_str())
        .collect();
    if hits.is_empty() {
        die(format!("no category matching {name:?}"));
    }
    die(format!(
        "ambiguous category {name:?}; matches: {}",
        hits.join(", ")
    ));
}

fn find_task_index(tasks: &[Task], key: &str) -> usize {
    let key = key.trim();
    if key.is_empty() {
        die("empty task id");
    }
    if let Some(i) = tasks.iter().position(|t| t.id == key) {
        return i;
    }
    let hits: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.id.starts_with(key))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [i] => *i,
        [] => die(format!("no task matching id {key:?}")),
        many => {
            let ids: Vec<_> = many.iter().map(|&i| short_id(&tasks[i].id)).collect();
            die(format!("ambiguous id {key:?}; matches: {}", ids.join(", ")));
        }
    }
}

fn cat_name_of(cats: &[Category], task: &Task) -> String {
    task.category_id
        .as_ref()
        .and_then(|id| cats.iter().find(|c| c.id == *id))
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "—".into())
}

fn print_task_line(cats: &[Category], task: &Task, show_cat: bool) {
    let check = if task.done {
        "\x1b[32m[✓]\x1b[0m"
    } else {
        "[ ]"
    };
    let title = if task.done {
        format!("\x1b[9m{}\x1b[0m", task.title)
    } else {
        task.title.clone()
    };
    let fmt = crate::settings::Settings::load().date_format;
    let due = crate::due::display(&task.due, &fmt);
    let due = if due.is_empty() {
        String::new()
    } else {
        format!("  {due}")
    };
    let flag = if task.importance > 0 {
        format!(
            "  \x1b[31m{}\x1b[0m",
            crate::model::importance_marks(task.importance)
        )
    } else {
        String::new()
    };
    let cat = if show_cat {
        format!("  [{}]", cat_name_of(cats, task))
    } else {
        String::new()
    };
    let progress = crate::model::todo_progress(task)
        .map(|(d, t)| format!("  ({d}/{t})"))
        .unwrap_or_default();
    println!(
        "{} {} {}{}{}{}{}",
        short_id(&task.id),
        check,
        title,
        cat,
        due,
        flag,
        progress
    );
}

/// 1-based index → body index of the Nth `Block::Todo`.
fn subtask_body_index(body: &[Block], one_based: usize) -> usize {
    if one_based == 0 {
        die("subtask index is 1-based (use 1 for the first subtask)");
    }
    let mut n = 0usize;
    for (i, b) in body.iter().enumerate() {
        if matches!(b, Block::Todo { .. }) {
            n += 1;
            if n == one_based {
                return i;
            }
        }
    }
    die(format!(
        "no subtask at index {one_based} (task has {n} subtask(s))"
    ));
}

fn collect_subtasks(body: &[Block]) -> Vec<(usize, &str, bool)> {
    let mut out = Vec::new();
    let mut n = 0usize;
    for b in body {
        if let Block::Todo { text, done } = b {
            n += 1;
            out.push((n, text.as_str(), *done));
        }
    }
    out
}

fn subtasks_json(body: &[Block]) -> Vec<serde_json::Value> {
    collect_subtasks(body)
        .into_iter()
        .map(|(index, text, done)| {
            serde_json::json!({
                "index": index,
                "text": text,
                "done": done,
            })
        })
        .collect()
}

fn task_json(cats: &[Category], task: &Task) -> serde_json::Value {
    let subs = collect_subtasks(&task.body);
    let done_n = subs.iter().filter(|(_, _, d)| *d).count();
    serde_json::json!({
        "id": task.id,
        "title": task.title,
        "body": body_to_text(&task.body),
        "subtasks": subtasks_json(&task.body),
        "subtasks_done": done_n,
        "subtasks_total": subs.len(),
        "due": task.due,
        "done": task.done,
        "importance": task.importance,
        "category": {
            "id": task.category_id,
            "name": cat_name_of(cats, task),
        },
        "created": task.created,
    })
}

fn normalize_due(raw: &str) -> String {
    // Accept ISO-ish "2026-08-10T14:30" as well as "2026-08-10 14:30".
    let t = raw.trim().replace('T', " ");
    if t.is_empty() {
        return String::new();
    }
    if !crate::due::is_valid(&t) {
        die(format!(
            "invalid due {raw:?}; try YYYY-MM-DD, YYYY-MM-DD HH:MM, MM-DD, or HH:MM"
        ));
    }
    let (due, _) = crate::due::parse(&format!("[{t}]"));
    if due.is_empty() {
        die(format!("invalid due {raw:?}"));
    }
    due
}

fn normalize_time(raw: &str) -> String {
    let t = raw.trim();
    // Require HH:MM (exactly), which is already a valid due form.
    if t.len() == 5 && t.as_bytes().get(2) == Some(&b':') && crate::due::is_valid(t) {
        return t.to_string();
    }
    die(format!("invalid time {raw:?}; use HH:MM (24h), e.g. 14:30"));
}

/// Combine optional date (`--due`) and time (`--time`) into a stored due string.
///
/// - neither → empty  
/// - time only → `HH:MM` (today)  
/// - date only → date as given  
/// - both → `DATE HH:MM` (date must not already include a time)
fn resolve_due(due: Option<&str>, time: Option<&str>) -> String {
    match (due.map(str::trim).filter(|s| !s.is_empty()), time) {
        (None, None) => String::new(),
        (None, Some(t)) => normalize_time(t),
        (Some(d), None) => normalize_due(d),
        (Some(d), Some(t)) => {
            let d = normalize_due(d);
            let t = normalize_time(t);
            if d.contains(':') {
                die(format!(
                    "due already includes a time ({d}); omit --time or pass date-only --due"
                ));
            }
            normalize_due(&format!("{d} {t}"))
        }
    }
}

/// For `edit`: apply --due / --time on top of the current value.
fn resolve_due_edit(current: &str, due: Option<&str>, time: Option<&str>) -> Option<String> {
    if due.is_none() && time.is_none() {
        return None;
    }
    let date_part = |s: &str| -> String {
        if s.is_empty() {
            return String::new();
        }
        match s.split_once(' ') {
            Some((d, _)) => d.to_string(),
            None if s.contains(':') => String::new(), // bare time
            None => s.to_string(),
        }
    };
    let time_part = |s: &str| -> Option<String> {
        if s.is_empty() {
            return None;
        }
        if let Some((_, t)) = s.split_once(' ') {
            return Some(t.to_string());
        }
        if s.contains(':') && !s.contains('-') {
            return Some(s.to_string());
        }
        // "YYYY-MM-DD" with no time, or "MM-DD"
        None
    };

    let new_date = match due {
        Some(d) => {
            let d = normalize_due(d);
            if d.contains(':') && d.contains('-') {
                // Full datetime in --due; --time must not also be set
                if time.is_some() {
                    die("pass either a full --due datetime or --due date + --time, not both");
                }
                return Some(d);
            }
            if d.contains(':') && !d.contains('-') {
                // bare time via --due
                if time.is_some() {
                    die("pass time via --time or --due, not both");
                }
                return Some(d);
            }
            d
        }
        None => date_part(current),
    };
    let new_time = match time {
        Some(t) => Some(normalize_time(t)),
        None => time_part(current),
    };
    Some(match (new_date.as_str(), new_time) {
        ("", None) => String::new(),
        ("", Some(t)) => t,
        (d, None) => d.to_string(),
        (d, Some(t)) => normalize_due(&format!("{d} {t}")),
    })
}

fn save_or_die(tasks: &[Task], cats: &[Category]) {
    if let Err(e) = store::save_tasks(tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if let Err(e) = store::save_categories(cats) {
        die(format!("failed to save categories: {e}"));
    }
}

// ---------------------------------------------------------------- commands

fn cmd_list(category: Option<&str>, open_only: bool, done_only: bool, json: bool) {
    let (cats, tasks) = store::load_all();
    let cat_filter: Option<String> = category.map(|n| require_category(&cats, n).id.clone());
    let show_cat = cat_filter.is_none();

    let filtered: Vec<&Task> = tasks
        .iter()
        .filter(|t| match &cat_filter {
            Some(cid) => t.category_id.as_deref() == Some(cid.as_str()),
            None => true,
        })
        .filter(|t| {
            if open_only {
                !t.done
            } else if done_only {
                t.done
            } else {
                true
            }
        })
        .collect();

    if json {
        let arr: Vec<_> = filtered.iter().map(|t| task_json(&cats, t)).collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return;
    }

    if filtered.is_empty() {
        println!("(no tasks)");
        return;
    }
    for t in &filtered {
        print_task_line(&cats, t, show_cat);
    }
    let done_n = filtered.iter().filter(|t| t.done).count();
    println!("— {} task(s), {} done", filtered.len(), done_n);
}

fn cmd_cats_list(json: bool) {
    let (cats, tasks) = store::load_all();
    if json {
        let arr: Vec<_> = cats
            .iter()
            .map(|c| {
                let total = tasks
                    .iter()
                    .filter(|t| t.category_id.as_deref() == Some(c.id.as_str()))
                    .count();
                let done = tasks
                    .iter()
                    .filter(|t| t.category_id.as_deref() == Some(c.id.as_str()) && t.done)
                    .count();
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "description": c.description,
                    "total": total,
                    "done": done,
                })
            })
            .collect();
        // uncategorized bucket
        let unc_total = tasks.iter().filter(|t| t.category_id.is_none()).count();
        let unc_done = tasks
            .iter()
            .filter(|t| t.category_id.is_none() && t.done)
            .count();
        let out = serde_json::json!({
            "categories": arr,
            "uncategorized": { "total": unc_total, "done": unc_done },
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }

    if cats.is_empty() {
        println!("(no categories)");
    } else {
        for c in &cats {
            let total = tasks
                .iter()
                .filter(|t| t.category_id.as_deref() == Some(c.id.as_str()))
                .count();
            let done = tasks
                .iter()
                .filter(|t| t.category_id.as_deref() == Some(c.id.as_str()) && t.done)
                .count();
            println!("{}  {}/{}", c.name, done, total);
        }
    }
    let unc_total = tasks.iter().filter(|t| t.category_id.is_none()).count();
    if unc_total > 0 {
        let unc_done = tasks
            .iter()
            .filter(|t| t.category_id.is_none() && t.done)
            .count();
        println!("— uncategorized  {}/{}", unc_done, unc_total);
    }
}

fn cmd_cat_add(name: &str, description: Option<&str>, json: bool) {
    let name = name.trim();
    if name.is_empty() {
        die("category name required");
    }
    if name.chars().count() > MAX_CATEGORY_NAME_LEN {
        die(format!(
            "category name too long (max {MAX_CATEGORY_NAME_LEN})"
        ));
    }
    let mut cats = store::load_categories();
    if cats.len() >= MAX_CATEGORY_COUNT {
        die(format!("category limit reached ({MAX_CATEGORY_COUNT})"));
    }
    if cats.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
        die(format!("category {name:?} already exists"));
    }
    let mut cat = Category::new(name);
    if let Some(d) = description {
        cat.description = d.to_string();
    }
    cats.push(cat);
    if let Err(e) = store::save_categories(&cats) {
        die(format!("failed to save categories: {e}"));
    }
    let cat = cats.last().expect("just pushed");
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": cat.id,
                "name": cat.name,
                "description": cat.description,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("created category {}", cat.name);
    }
}

fn cmd_cat_edit(
    name: &str,
    new_name: Option<&str>,
    description: Option<&str>,
    clear_description: bool,
    json: bool,
) {
    if new_name.is_none() && description.is_none() && !clear_description {
        die("nothing to edit; pass --name / --description / --clear-description");
    }
    if clear_description && description.is_some() {
        die("--clear-description cannot be combined with --description");
    }
    let mut cats = store::load_categories();
    let i = {
        let cat = require_category(&cats, name);
        cats.iter().position(|c| c.id == cat.id).expect("found")
    };
    if let Some(n) = new_name {
        let n = n.trim();
        if n.is_empty() {
            die("category name cannot be empty");
        }
        if n.chars().count() > MAX_CATEGORY_NAME_LEN {
            die(format!(
                "category name too long (max {MAX_CATEGORY_NAME_LEN})"
            ));
        }
        if cats
            .iter()
            .enumerate()
            .any(|(j, c)| j != i && c.name.eq_ignore_ascii_case(n))
        {
            die(format!("category {n:?} already exists"));
        }
        cats[i].name = n.to_string();
    }
    if clear_description {
        cats[i].description.clear();
    } else if let Some(d) = description {
        cats[i].description = d.to_string();
    }
    let cat = cats[i].clone();
    if let Err(e) = store::save_categories(&cats) {
        die(format!("failed to save categories: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": cat.id,
                "name": cat.name,
                "description": cat.description,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("updated category {}", cat.name);
    }
}

fn cmd_cat_delete(name: &str, json: bool) {
    let (mut cats, mut tasks) = store::load_all();
    let cat = require_category(&cats, name);
    let id = cat.id.clone();
    let cat_name = cat.name.clone();
    cats.retain(|c| c.id != id);
    for t in &mut tasks {
        if t.category_id.as_deref() == Some(id.as_str()) {
            t.category_id = None;
        }
    }
    save_or_die(&tasks, &cats);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "deleted": cat_name,
                "id": id,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("deleted category {cat_name} (tasks uncategorized)");
    }
}

fn cmd_add(args: &AddArgs, json: bool) {
    let title = args
        .title
        .as_deref()
        .or(args.title_pos.as_deref())
        .unwrap_or_default()
        .trim();
    if title.is_empty() {
        die("title required (positional or --title)");
    }
    let importance = args.importance;
    if importance > MAX_IMPORTANCE {
        die(format!("importance must be 0–{MAX_IMPORTANCE}"));
    }
    let (cats, mut tasks) = store::load_all();
    if tasks.len() >= MAX_TASK_COUNT {
        die(format!("task limit reached ({MAX_TASK_COUNT})"));
    }
    let cat_id = args
        .category
        .as_deref()
        .map(|n| require_category(&cats, n).id.clone());
    let due_s = resolve_due(args.due.as_deref(), args.time.as_deref());
    let mut task = Task::new(title, importance, cat_id, &due_s);
    if let Some(b) = args.body.as_deref() {
        task.body = body_from_text(b);
    }
    for s in &args.subtasks {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        task.body.push(Block::todo(t, false));
    }
    if task.body.len() > MAX_BODY_LINES {
        die(format!("body line limit reached ({MAX_BODY_LINES})"));
    }
    tasks.push(task);
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    let task = tasks.last().expect("just pushed");
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task_json(&cats, task)).unwrap_or_default()
        );
    } else {
        let n = collect_subtasks(&task.body).len();
        if n > 0 {
            println!(
                "added {}  {}  ({} subtask{})",
                short_id(&task.id),
                task.title,
                n,
                if n == 1 { "" } else { "s" }
            );
        } else {
            println!("added {}  {}", short_id(&task.id), task.title);
        }
    }
}

fn cmd_show(id: &str, json: bool) {
    let (cats, tasks) = store::load_all();
    let i = find_task_index(&tasks, id);
    let task = &tasks[i];
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task_json(&cats, task)).unwrap_or_default()
        );
        return;
    }
    println!("id:         {}", task.id);
    println!("title:      {}", task.title);
    println!("done:       {}", task.done);
    println!("category:   {}", cat_name_of(&cats, task));
    println!(
        "due:        {}",
        if task.due.is_empty() {
            "—"
        } else {
            &task.due
        }
    );
    println!(
        "importance: {} ({})",
        task.importance,
        crate::model::importance_marks(task.importance)
    );
    println!("created:    {}", task.created);
    let subs = collect_subtasks(&task.body);
    if subs.is_empty() {
        println!("subtasks:   —");
    } else {
        let done_n = subs.iter().filter(|(_, _, d)| *d).count();
        println!("subtasks:   {}/{}", done_n, subs.len());
        for (idx, text, done) in &subs {
            let check = if *done { "[✓]" } else { "[ ]" };
            println!("  {idx}. {check} {text}");
        }
    }
    // Non-todo body lines (notes / bullets / links / images), with markers.
    let mut run = 0usize;
    let mut notes = Vec::new();
    for b in &task.body {
        match b {
            Block::Todo { .. } => run = 0,
            Block::Text { text } => {
                run = 0;
                if !text.trim().is_empty() {
                    notes.push(text.clone());
                }
            }
            Block::Bullet { text } => {
                run = 0;
                notes.push(format!("- {text}"));
            }
            Block::Number { text } => {
                run += 1;
                notes.push(format!("{run}. {text}"));
            }
            Block::Link { url } => {
                run = 0;
                notes.push(url.clone());
            }
            Block::Image { path } => {
                run = 0;
                notes.push(format!("[image:{path}]"));
            }
        }
    }
    if notes.is_empty() {
        println!("body:       —");
    } else {
        println!("body:");
        for line in notes {
            println!("  {line}");
        }
    }
}

// ---------------------------------------------------------------- subtasks

fn cmd_subs_list(task_key: &str, json: bool) {
    let tasks = store::load_tasks();
    let i = find_task_index(&tasks, task_key);
    let task = &tasks[i];
    let subs = collect_subtasks(&task.body);
    if json {
        let out = serde_json::json!({
            "task_id": task.id,
            "title": task.title,
            "subtasks": subtasks_json(&task.body),
            "done": subs.iter().filter(|(_, _, d)| *d).count(),
            "total": subs.len(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }
    if subs.is_empty() {
        println!("{}  {}  (no subtasks)", short_id(&task.id), task.title);
        return;
    }
    println!("{}  {}", short_id(&task.id), task.title);
    for (idx, text, done) in &subs {
        let check = if *done { "\x1b[32m[✓]\x1b[0m" } else { "[ ]" };
        let text = if *done {
            format!("\x1b[9m{text}\x1b[0m")
        } else {
            (*text).to_string()
        };
        println!("  {idx}. {check} {text}");
    }
    let done_n = subs.iter().filter(|(_, _, d)| *d).count();
    println!("— {}/{} done", done_n, subs.len());
}

fn cmd_subs_add(task_key: &str, text: &str, done: bool, json: bool) {
    let text = text.trim();
    if text.is_empty() {
        die("subtask text required (positional or --text)");
    }
    let mut tasks = store::load_tasks();
    let i = find_task_index(&tasks, task_key);
    if tasks[i].body.len() >= MAX_BODY_LINES {
        die(format!("body line limit reached ({MAX_BODY_LINES})"));
    }
    tasks[i].body.push(Block::todo(text, done));
    let index = collect_subtasks(&tasks[i].body).len();
    let task = tasks[i].clone();
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": done,
                "subtasks": subtasks_json(&task.body),
            }))
            .unwrap_or_default()
        );
    } else {
        println!("added subtask {index} on {}  {text}", short_id(&task.id));
    }
}

/// `done = Some(true/false)` sets; `None` toggles.
fn cmd_subs_set_done(task_key: &str, index: usize, done: Option<bool>, json: bool) {
    let mut tasks = store::load_tasks();
    let i = find_task_index(&tasks, task_key);
    let bi = subtask_body_index(&tasks[i].body, index);
    let (new_done, text) = match &mut tasks[i].body[bi] {
        Block::Todo { text, done: d } => {
            let nd = done.unwrap_or(!*d);
            *d = nd;
            (nd, text.clone())
        }
        _ => die("internal: body index is not a subtask"),
    };
    let task = tasks[i].clone();
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": new_done,
                "subtasks": subtasks_json(&task.body),
            }))
            .unwrap_or_default()
        );
    } else {
        let verb = if new_done { "done" } else { "undone" };
        println!("{verb} subtask {index} on {}  {text}", short_id(&task.id));
    }
}

fn cmd_subs_edit(task_key: &str, index: usize, text: &str, json: bool) {
    let text = text.trim();
    if text.is_empty() {
        die("subtask text required (positional or --text)");
    }
    let mut tasks = store::load_tasks();
    let i = find_task_index(&tasks, task_key);
    let bi = subtask_body_index(&tasks[i].body, index);
    match &mut tasks[i].body[bi] {
        Block::Todo { text: t, .. } => *t = text.to_string(),
        _ => die("internal: body index is not a subtask"),
    }
    let task = tasks[i].clone();
    let done = matches!(&task.body[bi], Block::Todo { done: true, .. });
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task.id,
                "index": index,
                "text": text,
                "done": done,
                "subtasks": subtasks_json(&task.body),
            }))
            .unwrap_or_default()
        );
    } else {
        println!("updated subtask {index} on {}  {text}", short_id(&task.id));
    }
}

fn cmd_subs_delete(task_key: &str, index: usize, json: bool) {
    let mut tasks = store::load_tasks();
    let i = find_task_index(&tasks, task_key);
    let bi = subtask_body_index(&tasks[i].body, index);
    let removed = tasks[i].body.remove(bi);
    let (text, done) = match removed {
        Block::Todo { text, done } => (text, done),
        _ => die("internal: body index is not a subtask"),
    };
    let task = tasks[i].clone();
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task.id,
                "deleted": { "index": index, "text": text, "done": done },
                "subtasks": subtasks_json(&task.body),
            }))
            .unwrap_or_default()
        );
    } else {
        println!("deleted subtask {index} on {}  {text}", short_id(&task.id));
    }
}

fn cmd_set_done(id: &str, done: bool, json: bool) {
    let (cats, mut tasks) = store::load_all();
    let i = find_task_index(&tasks, id);
    tasks[i].done = done;
    let task = tasks[i].clone();
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task_json(&cats, &task)).unwrap_or_default()
        );
    } else {
        let verb = if done { "done" } else { "undone" };
        println!("{verb} {}  {}", short_id(&task.id), task.title);
    }
}

fn cmd_delete(id: &str, json: bool) {
    let (cats, mut tasks) = store::load_all();
    let i = find_task_index(&tasks, id);
    let removed = tasks.remove(i);
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task_json(&cats, &removed)).unwrap_or_default()
        );
    } else {
        println!("deleted {}  {}", short_id(&removed.id), removed.title);
    }
}

fn cmd_edit(args: &EditArgs, json: bool) {
    if args.title.is_none()
        && args.body.is_none()
        && args.due.is_none()
        && args.time.is_none()
        && !args.clear_due
        && args.category.is_none()
        && !args.clear_cat
        && args.importance.is_none()
    {
        die(
            "nothing to edit; pass --title / --body / --due / --time / --clear-due / --category / --clear-category / --importance",
        );
    }
    let (cats, mut tasks) = store::load_all();
    let i = find_task_index(&tasks, &args.id);
    if let Some(t) = args.title.as_deref() {
        let t = t.trim();
        if t.is_empty() {
            die("title cannot be empty");
        }
        tasks[i].title = t.to_string();
    }
    if let Some(b) = args.body.as_deref() {
        // Full replace — use BODY MARKUP (or `show` / `--json` export) to keep structure.
        let body = body_from_text(b);
        if body.len() > MAX_BODY_LINES {
            die(format!("body line limit reached ({MAX_BODY_LINES})"));
        }
        tasks[i].body = body;
    }
    if args.clear_due {
        if args.due.is_some() || args.time.is_some() {
            die("--clear-due cannot be combined with --due / --time");
        }
        tasks[i].due.clear();
    } else if let Some(d) =
        resolve_due_edit(&tasks[i].due, args.due.as_deref(), args.time.as_deref())
    {
        tasks[i].due = d;
    }
    if args.clear_cat {
        tasks[i].category_id = None;
    } else if let Some(n) = args.category.as_deref() {
        tasks[i].category_id = Some(require_category(&cats, n).id.clone());
    }
    if let Some(imp) = args.importance {
        if imp > MAX_IMPORTANCE {
            die(format!("importance must be 0–{MAX_IMPORTANCE}"));
        }
        tasks[i].importance = imp;
    }
    let task = tasks[i].clone();
    if let Err(e) = store::save_tasks(&tasks) {
        die(format!("failed to save tasks: {e}"));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&task_json(&cats, &task)).unwrap_or_default()
        );
    } else {
        println!("updated {}  {}", short_id(&task.id), task.title);
    }
}
