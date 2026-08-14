# Mach Repository Guide

This file applies to the entire repository. Keep it repository-specific: the
general working rules supplied by the user still apply and should not be
duplicated here.

## Product and toolchain

- `mach` is a terminal-first task manager with two public entry points: the
  interactive TUI and the shell/agent-friendly CLI.
- The Cargo package is `mach-tui`; the library and executable are both named
  `mach`.
- The minimum supported Rust version is 1.90. Keep the edition, MSRV, lockfile,
  CI, and release workflow compatible with that toolchain.
- SQLite under the selected data directory is the authoritative task store.
  The default directory is `~/.mach`. Application-wide update scheduling is
  separate in `~/.mach/update.db` and never follows `--dir` or `MACH_DIR`.

## Start with the real state

Before editing:

1. Inspect `git status --short --branch` and the relevant diff. Preserve user
   changes that are unrelated to the task.
2. Trace the affected behavior through its real entry points, storage path,
   tests, and documentation. A TUI fix may also require a CLI or Store change.
3. Read the closest existing abstraction before adding a new one. Prefer a
   focused cleanup of the touched area over another adapter or fallback.

Never run experiments against the user's default `~/.mach` data. Tests and
manual checks must use a temporary directory passed with `--dir`; application
code should receive an explicit `Store` where possible. Do not copy, delete, or
rewrite real user data as part of development.

## Architecture map

- `src/bin/mach.rs`: thin executable entry point.
- `src/lib.rs`: startup orchestration, terminal lifecycle, event loop,
  housekeeping, and background image/update polling.
- `src/app.rs`: interactive application state and user operations.
- `src/cli.rs`: CLI parsing and rendering. Data-bearing commands open their own
  Store; mutations operate against a fresh snapshot.
- `src/store.rs`: SQLite schema, validation, migrations, transactions,
  revisions, settings, and attachment metadata.
- `src/model.rs`: task, category, and label data model, limits, and legacy JSON
  schema.
- `src/input.rs`: keyboard and mouse control paths.
- `src/ui.rs`: layout, drawing, clipping, overlays, and mouse hit regions.
- `src/form.rs`, `src/description.rs`, `src/text_input.rs`, `src/undo.rs`:
  editing, structured description content, Unicode input, and undo/redo.
- `src/due.rs`, `src/duepicker.rs`: due-date parsing, display, and selection.
- `src/fuzzy.rs`, `src/open.rs`: type-to-jump matching and safe platform URL
  opening.
- `src/image.rs`: attachment decoding, cache limits, and terminal image
  protocols.
- `src/update.rs`: GitHub Release selection, checksum verification, and atomic
  self-update installation.
- `src/update_state.rs`: global update deadlines, retry state, ETag cache,
  latest release cache, and cross-process leases.
- `src/banner.rs`, `src/slash.rs`, `src/settings.rs`, `src/theme.rs`: help and
  release copy, commands, persisted preferences, and presentation policy.

## Contracts that must stay consistent

### Data and storage

- Data-directory precedence is `--dir PATH`, then `MACH_DIR`, then `~/.mach`.
  Keep help text, CLI behavior, TUI startup, and tests aligned with it.
- SQLite and legacy JSON schema versions are different contracts. Do not reuse
  one version number for the other. Legacy JSON is imported once and left
  untouched.
- Store mutations use a fresh read-modify-write transaction with
  `BEGIN IMMEDIATE`. Validate the fresh snapshot and commit tasks, categories,
  settings, attachment metadata, and exactly one revision increment atomically.
- A failed mutation must not partially update the database or attachment
  metadata. Attachments are immutable, content-addressed files; preserve that
  identity and cleanup model.
- `All tasks` is a view sentinel, never a persisted category.
- Deleting a category preserves its tasks as uncategorized. This semantic must
  match in Store, CLI, TUI confirmation text, and tests.

### CLI and TUI

- Public operations must have the same business semantics in CLI and TUI even
  though the CLI does not need to route through interactive `App` state.
- `--json` writes exactly one JSON document to stdout, including failures. Do
  not leak progress text, terminal control sequences, or a second document.
- Human-readable output belongs on the intended stream and must remain useful
  in a non-interactive shell.
- Unicode limits and truncation use grapheme clusters and terminal cell width,
  not byte count or Unicode scalar count. Preserve the separate byte limits
  used to bound persisted input.

### TUI presentation

- Every layout must survive narrow and short terminals without panics,
  underflow, or drawing outside its rectangle.
- Optional row metadata must consume width only on the row where it is drawn;
  one task's due date, progress, flags, or labels must not shorten every
  other title in the list.
- Completion and selection styling must remain coherent across a task title,
  due date, subtask progress, and flags. Selection must remain readable even
  when completed metadata is muted or struck through.
- Recompute mouse hit regions from the final clipped layout each frame. Hidden
  or overlaid controls must not retain stale click targets.
- Keep internal implementation terminology out of user-facing copy.

## Implementation and tests

Apply the global TDD rules to durable behavior, using the smallest test layer
that owns the contract. Do not add tests for trivial spacing, obvious wiring,
or visual facts that are cheaper and clearer to inspect manually.

Choose the test layer that owns the behavior:

- model, parsing, editor, and updater invariants: colocated unit tests;
- Store, migration, validation, concurrency, and attachment contracts:
  `tests/store_contract.rs`;
- interactive application semantics: `tests/app.rs`;
- CLI stdout, JSON, exit, and command contracts: `tests/cli_contract.rs`;
- meaningful keyboard, mouse, or confirmation behavior: `tests/keys.rs`;
- layout algorithms, overlays, clipping, and narrow-terminal regressions only:
  `tests/render.rs`;
- real TUI input/persistence/event-loop round trips: `scripts/e2e-tui.sh`.

When a change crosses entry points, validate every affected contract. A green
unit suite is not evidence that CLI, TUI, persistence, and documented behavior
still agree.

## Validation

Run focused tests while developing, then validate in proportion to risk. Common
targeted commands are:

```sh
cargo test --locked --test store_contract
cargo test --locked --test app
cargo test --locked --test cli_contract
cargo test --locked --test keys
cargo test --locked --test render
```

The normal full Rust gate is:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --locked
```

For event-loop or end-to-end TUI changes, build first and run:

```sh
cargo build --locked
scripts/e2e-tui.sh target/debug/mach
```

For packaging, installer, updater, dependency, or release changes, also match
the relevant CI gates:

```sh
cargo build --release --locked
shellcheck install.sh scripts/*.sh
scripts/test-install.sh
scripts/test-release.sh
cargo deny check --hide-inclusion-graph
```

Use `cargo package --allow-dirty --locked` only as a local preview while the
tree is dirty. It is not release evidence. Run `cargo package --locked` from
the clean release commit, and check release-sensitive work with Rust 1.90
locally or in CI.

After any edit, inspect the final diff and run `git diff --check`. For a purely
instructional or prose-only change, this review is normally sufficient unless
the text changes a command or public contract.

## Release boundary

A release requires explicit user authorization. Preparing one includes all of
the following:

- update the version in `Cargo.toml` and `Cargo.lock`;
- replace `src/banner.rs::WHATS_NEW` with real highlights for that version;
- add `.github/release-notes/v<version>.md` with user-visible highlights and
  the exact changelog range from the previous release;
- run the full source, TUI, shell, packaging, and MSRV validation relevant to
  `.github/workflows/release.yml`;
- ensure the release commit is on `main`, and tag that exact commit as
  `v<package-version>`.

The tag workflow is the publisher. It verifies ancestry and version, builds
and smoke-tests four platform binaries, packages each as a `.tar.gz`, generates
the versioned checksum manifest and attestations, publishes `mach-tui` through
the crates.io trusted publisher, and publishes the GitHub Release. Until the
archive-aware bridge release has been published and verified, it must also ship
the legacy raw binaries and `SHA256SUMS` required by older self-updaters. Do not
manually run `cargo publish` or substitute hand-built assets for that workflow.

Do not call a release complete until the exact tag and SHA, CI result, GitHub
archives and versioned checksum manifest, required bridge assets, attestations,
crates.io version, and final Git state have all been verified.

## Documentation and repository hygiene

- Treat the README as a long-lived project entry point, not rolling release
  notes. Update it only for enduring product positioning, installation entry
  points, or stable public command and data contracts. Do not append
  version-specific feature walkthroughs, option inventories, migration notes,
  color/value lists, or implementation details; those belong in the versioned
  GitHub release notes and `src/banner.rs::WHATS_NEW`.
- Never add agent attribution, telemetry, scratch notes, demo databases, or
  local test artifacts to the repository.
