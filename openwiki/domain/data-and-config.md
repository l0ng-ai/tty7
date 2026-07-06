# Data and configuration

## Config directory resolution

`src/core/config.rs` centralizes all tty7 state paths. Resolution order is:

1. `--config-dir <path>` or `--config-dir=<path>` parsed in `src/main.rs`,
2. `TTY7_CONFIG_DIR`,
3. platform default config directory for `tty7`.

The config directory contains at least:

- `config.json` — user configuration,
- `session.json` — saved tab/split layout and pane ids,
- `history` — tty7 command history,
- daemon endpoint files such as Unix socket or Windows port file.

Use `cargo dev` to isolate this state under `.tty7-dev/` during development.

## `config.json`

`Config` in `src/core/config.rs` is the top-level config model. It is serde-defaulted so missing fields are filled from defaults. Bad or unreadable config falls back to defaults and logs warnings rather than preventing startup.

Major config domains:

- Fonts: primary/fallback/bold/italic family, font size, line height.
- Theme: light/dark mode string, theme preset, color overrides.
- Keybindings: action-name to keystroke override map.
- Shell: optional shell program and args.
- Behavior: URL opening, cursor blink, scrollback limit, new tab placement, command-finished notifications.
- Appearance/input/window: cursor style, mouse hide while typing, focus follows mouse, scroll multiplier, clipboard trim, startup mode.
- Shell environment: working directory policy and extra env map.

Important rules:

- Numeric values are sanitized/clamped on load.
- `Config::save()` writes pretty JSON using atomic sibling temp + rename.
- The config watcher in `src/main.rs` watches the directory, not only the file, because saves replace the file by rename.
- Theme/colors and fonts can live-apply. Shell/env/working-directory settings are used for new panes.
- Extra env injection does not allow overriding `TERM` or `COLORTERM` in daemon spawn logic.

## Working directory policy

`WorkingDirectory` combines a `strategy` with a `path`:

- `inherit`: inherit daemon cwd, with fallback behavior when unavailable or `/`.
- `home`: start in the user's home directory.
- `custom`: start in the configured custom path.

Explicit cwd passed by the client wins over config. New tabs and splits generally pass the active pane's foreground cwd when available. Session restore can also pass saved cwd.

## Shell configuration

`ShellConfig` contains:

- `program`,
- `args`.

When absent, the daemon chooses the platform default: login/default shell on Unix and PowerShell on Windows. Shell integration is applied only when `src/daemon/shell_integration.rs` recognizes and can safely wrap the shell invocation.

## Session model: `session.json`

`src/core/session.rs` persists layout, not process state itself.

Model:

- `Session`: active tab index and `Vec<SessionTab>`.
- `SessionTab`: optional custom name plus `SessionPane` tree.
- `SessionPane::Leaf`: saved cwd and optional daemon `pane_id`.
- `SessionPane::Split`: axis, ratio, and two child panes.

Load/save rules:

- Missing, unreadable, or corrupt session file means no session to restore.
- Writes are best-effort and atomic through `config::write_atomic`.
- A session with zero tabs is meaningful and restores the home page.
- On startup, UI lists daemon panes and reattaches leaves whose saved `pane_id` is still alive; otherwise it spawns fresh in saved cwd.

Do not store GPUI entities or terminal emulator objects in session data. Keep it serializable and tolerant of missing fields.

## Command history file

`src/terminal/history.rs` owns persistent command history.

Files and sources:

- tty7 writes its own config-dir `history` file.
- tty7 also reads user shell histories (`~/.zsh_history`, `~/.bash_history`, and `$HISTFILE`) as read-only seeds.

Format:

- New tty7 entries are `<cwd>\t<command>` when cwd is an absolute path.
- Legacy bare command lines are accepted.

Load behavior:

- Blanks are dropped.
- Duplicates are collapsed while preserving most recent occurrence semantics.
- Counts and cwd sets are accumulated for ranking.
- Entry count is capped.

Ranking:

- Recency contributes normalized score.
- Frequency contributes a logarithmic boost.
- Current cwd contributes a strong bonus.

Privacy note: history can contain sensitive commands or paths. Keep it local unless a future feature has explicit user consent and security review.

## Daemon protocol data

`src/daemon/protocol.rs` defines the client/daemon protocol. It is an internal data model, but changes must preserve both sides.

Core structs:

- `WinSize`: cols, rows, cell width, cell height.
- `PaneInfo`: pane id, optional cwd, title, alive flag.

Frame rules:

- Payload length is u32 little-endian and capped at `MAX_FRAME`.
- Raw PTY bytes are used for input/output/snapshot payloads.
- Control messages are JSON payloads keyed by one-byte kind.

Compatibility guidance:

- Because there is no protocol version or capability negotiation, update client and daemon together.
- Be careful when adding new daemon messages: snapshot replay suppression may need to know whether historical replay should trigger the new behavior.
- `PaneInfo` and session data are user-visible indirectly through restore behavior; preserve serde defaults for backward compatibility.

## OSC tokenizer and shell state

`src/core/osc.rs` is a streaming tokenizer used by daemon-side OSC sniffing. It handles BEL and ST terminators, split sequences, ignored IDs, malformed/unterminated recovery, and oversized payload abandonment.

The daemon uses it to parse:

- OSC 7 cwd updates,
- OSC 133 prompt lifecycle,
- notification-related OSC sequences on the client side through terminal event handling.

Changing OSC parsing can affect prompt-aware input, cwd inheritance, session labels, notifications, and security. Add regression tests for malformed, split, and oversized sequences.

## Atomic writes and corruption tolerance

Config and session writes use same-directory temp files, flush/sync, and rename. Load paths are deliberately tolerant:

- Config parse/read failures fall back to defaults.
- Session parse/read failures skip restore.
- History read failures return empty/seeded history.

This product choice favors always starting a usable terminal over strict failure. Future code should preserve that behavior for user-writable state files.
