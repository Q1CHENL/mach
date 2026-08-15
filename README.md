# Mach

A terminal-first task manager for people who live in the shell and work with agents.

Categories, reusable labels, due dates, subtasks, inline images, and a CLI that
scripts or agents can drive.

![Main screen](assets/screenshot.png)

![Help](assets/screenshot-help.png)

## Install

```sh
# 1. checksum-verified release archive → ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh | sh

# 2. crates.io → ~/.cargo/bin  (Rust 1.90+)
cargo install --locked mach-tui

# 3. local clone
git clone https://github.com/Q1CHENL/mach.git && cd mach && cargo install --locked --path .
```

Crate name **`mach-tui`**, binary **`mach`**.

## CLI

`mach` with no args opens the TUI. Subcommands cover add / list / edit / done /
delete / categories / labels / subtasks and more, with flags for due dates,
importance, categories, labels, description markup, and `--json` for scripts.

```sh
mach --help
```

Agents work well against the same CLI — stable flags, unique ID prefixes,
and JSON output when you need it.

Use `mach list --query TEXT` to search task titles, descriptions, and assigned
label names alongside the existing category, label, and open/done filters.
Automation can establish shared names idempotently with
`mach categories ensure NAME` and `mach labels ensure NAME`; explicitly
supplied descriptions or colors must match an existing record or the command
reports a conflict.

Export the task store to a portable archive and restore it into another data
directory:

```sh
mach export tasks.mach
mach --dir ~/restored-mach import tasks.mach
```

The TUI provides `/export` and `/import <FILE>` for the same workflow. Release
binary installations can update with `/update` or `mach update --install`;
Cargo installations update with `cargo install --locked mach-tui`.

## Data

Lives in **`~/.mach`**: tasks, categories, labels and settings in SQLite
(`mach.db`), with managed images under `images/`. The CLI and TUI safely share
the same store, and changes appear in the running app.

Portable `.mach` archives contain versioned JSON data plus the exact managed
image bytes referenced by those tasks. Back up the whole data directory while
Mach is closed when you need a complete local backup.

Another folder: `--dir PATH` or `MACH_DIR`.

## License

[MIT](LICENSE)
