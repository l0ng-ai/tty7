# Product workflows and domain behavior

## Product model

tty7's main product rule is: **a terminal window is a view over daemon-owned sessions**. This explains most behavior in the repository:

- The daemon owns PTYs and child shells (`src/daemon/pane.rs`).
- The GUI owns layout, rendering, and user interaction (`src/ui/`, `src/terminal/view.rs`).
- Session persistence stores enough layout/cwd/pane-id metadata to restore the user's workspace (`src/core/session.rs`).

## Session survival, close, and restore

There are three different closure concepts:

1. **Close window / quit app**
   - `Tty7App` saves the session on app quit.
   - `RemoteTerminal::drop` sends `Detach`.
   - Daemon panes keep running.
   - On next launch, saved leaves reattach to matching alive `pane_id`s if possible.

2. **Close pane or tab explicitly**
   - UI close paths kill the daemon pane(s) for removed terminal leaves.
   - Reopen closed tab reconstructs layout/name/cwd from a serialized snapshot, but it does not resurrect a killed process.

3. **Child process exits**
   - Attached clients receive `Exited` and can show final state.
   - Detached dead panes are reclaimed to avoid leaks, so final scrollback is not guaranteed to be available indefinitely after detached exit.

The close-window confirmation in `src/ui/app.rs` is intentionally informational: it reassures the user that sessions continue in the daemon.

## Zero-tab home

Recent work made zero tabs a valid app state. In `src/ui/home.rs` and `src/ui/app.rs`:

- Closing the last tab shows a home page.
- Quitting from the home page saves/restores zero tabs.
- Enter or click opens a new tab.
- App shortcuts such as new tab still work because the home page has a focus handle.
- A reopen-closed-tab hint appears when the closed-tab stack has entries.

Future changes should not assume `tabs` is non-empty. Clamp active indices and handle home focus explicitly.

## Tabs and splits

User workflows from `src/ui/app.rs`, `src/ui/pane.rs`, and `src/ui/tab_strip.rs`:

- New tab inherits cwd from active focused pane when available.
- New split inherits cwd from the pane being split.
- New tab placement is configurable: after current tab or at end.
- Tab labels can be custom names or derived from terminal title.
- Split ratios are user-draggable and clamped.
- Focus can cycle between panes; a pane can be maximized without destroying the split tree.
- Settings is a tab-like UI surface but is not persisted as a normal terminal tab.

## Smart prompt behavior

The local prompt/editor features depend on daemon shell integration reporting prompt state:

- Shell hooks emit OSC 133 to say when the prompt is active and when commands start/finish.
- `TerminalView::input_active()` only enables the local editor when the shell is at prompt and not in alternate-screen mode.
- At prompt, tty7 owns editing, completion, ghost suggestion, history navigation, and readline-style shortcuts.
- In TUI/raw modes, tty7 writes keys directly to the PTY.

This split is why shell integration timing matters. The changelog notes a fix that emits OSC 133 `D` before user `precmd` hooks so the local editor can regain input quickly after a command exits.

Known caveats:

- Unsupported shells launch without prompt integration.
- Bash with custom args currently skips integration.
- Shell integration is best-effort and should fail open to a working terminal, even if smart prompt features are reduced.

## Completion and history product rules

History and completion intentionally serve different workflows:

- `Tab` completion is for command/path candidates from builtins, `$PATH`, and filesystem paths.
- History is not mixed into the Tab menu.
- Whole-line recall uses ghost suggestions, Up/Down history, and Ctrl+R reverse search.
- Frecency ranking strongly boosts commands previously used in the current directory.

This behavior is documented directly in `src/terminal/completion.rs` and `src/terminal/history.rs`, and is reinforced by a recent commit making Tab completion a history-free picker.

Privacy/product note: tty7 reads user shell history files as read-only seeds (`~/.zsh_history`, `~/.bash_history`, `$HISTFILE`) and writes its own config-dir `history` file. Do not add network or external provider use of this data without explicit product/security review.

## Search and link workflows

- Cmd/Ctrl+F opens scrollback search.
- Enter and Shift+Enter step matches.
- Search state owns focus so typed search input does not leak to the PTY.
- URL hover/click prefers OSC 8 hyperlinks and falls back to bare URL detection.
- Link opening is gated by `Config::link_url`.

Mouse-mode TUI behavior has specific handling so link hover underline can still display while mouse reporting is active.

## Settings workflows

Settings are opened as a dedicated tab with Cmd/Ctrl+`,`.

Current settings domains include:

- Appearance: theme preset, font size, line height, primary/bold/italic font family, cursor shape/blink, color overrides.
- Terminal/input behavior: scrollback and mouse/clipboard options.
- Shell: shell program/args and working-directory strategy/path.
- Window/tabs: startup mode and tab placement.
- Keybindings: display effective/default bindings.
- About.

Most setting changes persist through `Config::save()` to `config.json`. Theme/colors and font settings apply live. Shell/env/working-directory settings affect newly spawned panes rather than changing already-running shells.

## Notifications

The terminal supports desktop notifications from OSC 9 / OSC 777 escape sequences and command-finished notifications. `Config::notify_on_command_finish` controls whether command-finished notifications are never shown, shown only when unfocused, or always.

Snapshot replay must not replay historical notifications; this is part of the terminal replay side-effect suppression contract.

## Cross-platform posture

From `README.md`, `Cargo.toml`, and CI:

- macOS is the primary platform, with arm64 and x86_64 release DMGs.
- Windows and Linux are fully supported: they compile/test in CI and ship packaged archives (`.zip` / `.tar.gz`) every release.
- Linux uses GPUI x11/wayland backends and `/proc`-based foreground cwd/title tracking.
- Windows uses PowerShell as default shell, ConPTY through `portable-pty`, and an embedded icon from `build.rs`.

When changing platform-specific code, use CI matrix expectations in `.github/workflows/ci.yml` as the compatibility baseline.