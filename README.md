# Mach

A terminal todo TUI for people who live in the shell and work with agents.

Categories, due dates, subtasks, inline images, and a CLI that scripts or
agents can drive.

![Main screen](assets/screenshot.png)

![Help](assets/screenshot-help.png)

## Install

```sh
# release binary → ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh | sh

# or crates.io → ~/.cargo/bin (Rust 1.90+)
cargo install --locked mach-tui
```

Crate name **`mach-tui`**, binary **`mach`**.

## CLI

`mach` opens the TUI. CLI commands use the same data and support JSON output for
scripts and agents:

```sh
mach add "Review release" --due 8-12 --time 16:00 --category Work --importance 2
mach --json list --open
mach --help
```

Task commands accept a full ID or an unambiguous prefix.

## Data

Data lives in **`~/.mach`**: tasks and settings in `mach.db`, managed images in
`images/`. Existing JSON data is imported automatically on first launch and
left untouched.

Use another folder with `--dir PATH` or `MACH_DIR`. For backups, copy the whole
directory while mach is closed.

## License

[MIT](LICENSE)
