//! On-disk storage: `~/.mach/{tasks,categories,settings}.json`
//!
//! Directory resolution (first wins): `--dir` / [`set_data_dir`], then
//! `$MACH_DIR`, then `~/.mach`.
//!
//! Files use a versioned envelope (`schema` = [`SCHEMA_VERSION`]) and UUID ids.
//! Missing files load as empty. Wrong schema or unreadable JSON exits with an
//! error (never silently wiped). Deleted tasks are not kept — delete removes
//! them from `tasks.json`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::model::{Category, SCHEMA_VERSION, Task};

/// Explicit data directory from CLI (`--dir`). Set before first [`paths`] call.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static PATHS: OnceLock<Paths> = OnceLock::new();

pub struct Paths {
    pub dir: PathBuf,
    pub tasks: PathBuf,
    pub categories: PathBuf,
    pub settings: PathBuf,
    pub images: PathBuf,
}

/// Override the data directory (e.g. from `--dir`). No-op if already set
/// or if [`paths`] has already been called with another source.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(expand_user(dir));
}

fn expand_user(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or(path);
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path
}

fn resolve_data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR.get() {
        return dir.clone();
    }
    if let Some(dir) = std::env::var_os("MACH_DIR") {
        return expand_user(PathBuf::from(dir));
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mach")
}

pub fn paths() -> &'static Paths {
    PATHS.get_or_init(|| {
        let dir = resolve_data_dir();
        let _ = fs::create_dir_all(&dir);
        Paths {
            tasks: dir.join("tasks.json"),
            categories: dir.join("categories.json"),
            settings: dir.join("settings.json"),
            images: dir.join("images"),
            dir,
        }
    })
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    serde_json::from_reader(reader).ok()
}

/// Atomic write via temp file + rename (no fsync).
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let data = serde_json::to_vec(value)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&data)?;
        file.write_all(b"\n")?;
    }
    fs::rename(&tmp, path)
}

fn die_load(path: &Path, msg: impl std::fmt::Display) -> ! {
    eprintln!("mach: {}: {msg}", path.display());
    std::process::exit(1);
}

// ---------------------------------------------------------------- files

#[derive(Debug, Serialize, Deserialize)]
struct TasksFile {
    schema: u32,
    tasks: Vec<Task>,
}

#[derive(Debug, Serialize)]
struct TasksFileRef<'a> {
    schema: u32,
    tasks: &'a [Task],
}

#[derive(Debug, Serialize, Deserialize)]
struct CategoriesFile {
    schema: u32,
    categories: Vec<Category>,
}

#[derive(Debug, Serialize)]
struct CategoriesFileRef<'a> {
    schema: u32,
    categories: Vec<&'a Category>,
}

/// Load categories (real ones only — no "All Tasks").
pub fn load_categories() -> Vec<Category> {
    let path = &paths().categories;
    if !path.exists() {
        return Vec::new();
    }
    let Some(file) = read_json::<CategoriesFile>(path) else {
        die_load(path, "could not read categories (invalid JSON?)");
    };
    if file.schema != SCHEMA_VERSION {
        die_load(
            path,
            format!(
                "unsupported schema {} (this mach expects {SCHEMA_VERSION})",
                file.schema
            ),
        );
    }
    file.categories
        .into_iter()
        .filter(|c| !c.is_all())
        .collect()
}

pub fn save_categories(categories: &[Category]) -> std::io::Result<()> {
    let real: Vec<&Category> = categories.iter().filter(|c| !c.is_all()).collect();
    let file = CategoriesFileRef {
        schema: SCHEMA_VERSION,
        categories: real,
    };
    write_json(&paths().categories, &file)
}

pub fn load_tasks() -> Vec<Task> {
    let path = &paths().tasks;
    if !path.exists() {
        return Vec::new();
    }
    let Some(file) = read_json::<TasksFile>(path) else {
        die_load(path, "could not read tasks (invalid JSON?)");
    };
    if file.schema != SCHEMA_VERSION {
        die_load(
            path,
            format!(
                "unsupported schema {} (this mach expects {SCHEMA_VERSION})",
                file.schema
            ),
        );
    }
    file.tasks
}

pub fn save_tasks(tasks: &[Task]) -> std::io::Result<()> {
    let file = TasksFileRef {
        schema: SCHEMA_VERSION,
        tasks,
    };
    write_json(&paths().tasks, &file)
}

pub fn load_all() -> (Vec<Category>, Vec<Task>) {
    (load_categories(), load_tasks())
}
