# Mach

A terminal todo TUI for people who live in the shell and work with agents.

Categories, due dates, subtasks, inline images, and a CLI that scripts or
agents can drive.

![Main screen](assets/screenshot.png)

![Help](assets/screenshot-help.png)

## Install

```sh
# 1. binary → ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh | sh

# 2. crates.io → ~/.cargo/bin  (needs rustup)
cargo install mach-tui

# 3. local clone
git clone https://github.com/Q1CHENL/mach.git && cd mach && cargo install --path .
```

Crate name **`mach-tui`**, binary **`mach`**.

## CLI

`mach` with no args opens the TUI. The same store is available through stable
CLI commands for humans and agents:

```sh
mach add "Review release" --due 8-12 --time 16:00 --category Work --importance 2
mach --json list --open
mach move TASK_ID --before OTHER_TASK_ID
mach purge --done --category Work
```

Task arguments accept a full ID or an unambiguous prefix. `move` preserves the
task's category, and permanent bulk deletion requires the explicit
`purge --done` interlock. In `--json` mode every invocation writes exactly one
JSON document to stdout; failures use a non-zero exit status and put no prose
around that document. Run `mach --help` for the full task, category, body-markup,
subtask, due-date, and update contract.

## Data

Tasks, categories, settings, and attachment metadata live in SQLite at
**`~/.mach/mach.db`**. When an image is added, mach validates it, copies it into
**`~/.mach/images`**, and stores an immutable SHA-256 attachment ID in the task;
identical images share one managed file, so the original source can move or be
deleted afterward. Writes use full-sync WAL transactions and a monotonic
revision, so concurrent CLI and TUI processes serialize mutations against fresh
state and the TUI reloads committed changes.

The first time a data directory is opened after upgrading, mach transactionally
imports any legacy `tasks.json`, `categories.json`, and `settings.json` once,
including copying legacy image references into managed attachments. Those JSON
files are left untouched after a successful import.

Another folder: `--dir PATH` or `MACH_DIR`; its database is `PATH/mach.db`.
For a filesystem-level backup, copy the whole data directory while mach is not
writing, or use a SQLite-aware backup tool so WAL data is included.

## License

[MIT](LICENSE)
