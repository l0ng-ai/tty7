# tty7 OpenWiki quickstart

## What this repository is

`tty7` is a Rust desktop terminal emulator built on Zed's `gpui` and `alacritty_terminal` fork. Its core product promise is that **the window is only a view; shells live in a persistent daemon**. Closing or restarting the GUI detaches from running shells instead of killing them, and a later launch can reattach to daemon panes when they are still alive.

Key user-facing capabilities from the current source and README:

- Persistent PTY/session daemon behind a thin GUI client (`src/daemon/`, `src/terminal/remote.rs`).
- GPU-rendered terminal grid through GPUI and `alacritty_terminal` (`src/terminal/element.rs`).
- Shell-aware prompt/cwd tracking through injected OSC 7 and OSC 133 integration for zsh, bash, and fish (`src/daemon/shell_integration.rs`).
- Tabs, splits, command palette, settings UI, themes, zero-tab home page, and desktop notifications (`src/ui/`).
- Smart prompt features: local line editor, history/ghost suggestion, Tab completion, syntax highlighting, reverse search, and scrollback search (`src/terminal/`).

The package metadata lives in `Cargo.toml`; the binary entrypoint is `src/main.rs`.

## Start here by task

- **Understand the process/runtime model:** read [Runtime architecture](architecture/runtime.md).
- **Change terminal rendering, input, editor, tabs, settings, or UI:** read [Client, terminal, and UI architecture](architecture/client-terminal-ui.md).
- **Reason about product behavior and user workflows:** read [Product workflows and domain behavior](domain/product-workflows.md).
- **Change config, session persistence, history, or protocol-shaped data:** read [Data and configuration](domain/data-and-config.md).
- **Build, test, package, or prepare a PR/release:** read [Development, testing, and release operations](operations/development-testing-release.md).

Existing top-level docs remain useful:

- `README.md` is the user-facing overview, install notes, and shortcut summary.
- `CHANGELOG.md` summarizes current user-visible changes.

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/main.rs` | Parses `--config-dir`, branches into `--daemon` mode or GUI mode, ensures the daemon is running, registers fonts/assets, installs keymap, watches config, opens the GPUI window. |
| `src/core/` | Cross-cutting data and actions: config, session model, OSC tokenizer, action declarations. |
| `src/daemon/` | Persistent backend: local transport, framed IPC protocol, daemon launcher/server, PTY pane lifecycle, shell integration scripts. |
| `src/terminal/` | Client-side terminal mirror and interaction: remote socket client, alacritty grid rendering element, input encoding, local command editor, completion, history, search, highlighting. |
| `src/ui/` | GPUI window shell: tab strip, split tree, settings tab, command palette, home page, theme/keymap wiring. |
| `assets/` | App icons and bundled Hack fonts. |
| `.cargo/config.toml` | Defines `cargo dev` alias using isolated `.tty7-dev/` state. |
| `.github/workflows/ci.yml` | Rustfmt plus build/test matrix for macOS, Windows, and Linux. |
| `.github/workflows/release.yml`, `.github/scripts/bundle.sh` | macOS release packaging and optional Developer ID signing/notarization. |

## Local development quickstart

Prerequisite: Rust stable. Linux additionally needs the GPUI x11/wayland/font/SSL/zstd build dependencies listed in `README.md` and `.github/workflows/ci.yml`.

```bash
cargo dev          # cargo run -- --config-dir .tty7-dev
cargo test         # unit tests
cargo fmt          # format; CI runs cargo fmt --check
cargo clippy       # advisory, useful before PRs
```

Use `cargo dev`, not plain `cargo run`, when working interactively. It stores `config.json`, `session.json`, history, and the daemon endpoint under `.tty7-dev/` instead of the real user config directory. See `.cargo/config.toml`.

## Runtime in one paragraph

GUI startup calls `daemon::spawn::ensure_running()` and then creates `Tty7App`. Each `TerminalView` owns a `RemoteTerminal`, which opens one local socket/stream connection to the daemon for one pane. The daemon owns the actual PTY and child shell, records a bounded byte replay ring, sniffs OSC 7/133 for cwd and prompt state, and sends framed `DaemonMsg`s to the attached client. Dropping a `RemoteTerminal` sends `Detach`, while explicit pane/tab close sends `Kill`. On app restart, saved session leaves can reattach by daemon `pane_id` if the daemon still has that pane alive, otherwise tty7 spawns a fresh shell in the saved cwd.

## Important behavior to avoid breaking

- **Window close is not pane close.** Window/app teardown detaches; explicit close actions kill terminal panes. The close prompt in `src/ui/app.rs` explains this distinction.
- **Zero tabs is valid.** Recent history added `src/ui/home.rs`; closing the last tab shows the home page and quitting there restores zero tabs.
- **Attach replay order matters.** Daemon sends `Size` before `Snapshot` so the client grid replays scrollback at the right width (`src/daemon/protocol.rs`, `src/terminal/remote.rs`).
- **Snapshot replay must suppress side effects.** Historical escape sequences should not answer terminal queries, clobber the clipboard, ring the bell, or resend notifications.
- **Shell integration is best-effort but central to smart prompt behavior.** Unsupported shells or bash with custom args may not emit prompt marks, which affects the local editor/completion/history-at-prompt experience.
- **The protocol is internal.** It is versionless and optimized for this client/daemon pair, not a stable public API.
- **macOS is the primary platform; Windows and Linux are fully supported.** All three build/test in CI, and every release ships macOS DMGs (arm64 + x86_64) plus unsigned Windows (`.zip`) / Linux (`.tar.gz`) archives.

## Current git/context notes

Recent commits show active work around daemon lifecycle, prompt marks, Linux support, zero-tab home, history-free Tab completion, and correctness tests. `git status --short` at the start of this documentation run showed only untracked `todo.md`; it was not used as source evidence.
