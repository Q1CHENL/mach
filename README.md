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

# 2. cargo → ~/.cargo/bin  (needs rustup)
cargo install --git https://github.com/Q1CHENL/mach

# 3. local clone
git clone https://github.com/Q1CHENL/mach.git && cd mach && cargo install --path .
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

## Data

Lives in **`~/.mach`** as plain JSON. Back it up however you back up
anything else.

Another folder: `--dir PATH` or `MACH_DIR`.

## License

[MIT](LICENSE)
