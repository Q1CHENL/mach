# Mach

A terminal-first task manager for people who live in the shell and work with agents.

Categories, due dates, subtasks, inline images, and a CLI that scripts or
agents can drive.

![Main screen](assets/screenshot.png)

![Help](assets/screenshot-help.png)

## Install

```sh
# 1. verified release binary → ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh | sh

# 2. crates.io → ~/.cargo/bin  (Rust 1.90+)
cargo install --locked mach-tui

# 3. local clone
git clone https://github.com/Q1CHENL/mach.git && cd mach && cargo install --locked --path .
```

Crate name **`mach-tui`**, binary **`mach`**.

## CLI

`mach` with no args opens the TUI. Subcommands cover add / list / edit /
done / delete / categories / subtasks and more, with flags for due dates,
importance, category, body markup, and `--json` for scripts.

```sh
mach --help
```

Agents work well against the same CLI — stable flags, unique ID prefixes,
and JSON output when you need it.

The TUI checks for updates daily. Run `/update` to download, verify, and install
the latest release, or use `mach update --install` from the shell.

## Data

Lives in **`~/.mach`**: tasks and settings in SQLite (`mach.db`), with managed
images under `images/`. The CLI and TUI safely share the same store, and changes
appear in the running app.

Existing JSON data is imported automatically on first launch and left
untouched. Back up the whole directory while mach is closed.

Another folder: `--dir PATH` or `MACH_DIR`.

## License

[MIT](LICENSE)
