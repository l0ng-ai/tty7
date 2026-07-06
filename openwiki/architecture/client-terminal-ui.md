# Client, terminal, and UI architecture

## Layering

The GUI side is split into two major layers:

- `src/terminal/`: terminal emulation mirror, rendering, input, editor, completion, history, search, and terminal-specific behavior.
- `src/ui/`: application chrome: window shell, tab strip, split tree, settings panel, command palette, home page, keymap, theme.

`src/ui/mod.rs` documents the dependency direction: UI may depend on `core` and `terminal`; lower layers should not depend back on `ui`.

## Startup into the app shell

After daemon startup, `src/main.rs` opens a GPUI window and constructs `Tty7App` from `src/ui/app.rs`. `Tty7App` owns:

- `tabs: Vec<Tab>` and active tab index,
- global font size/line height/family cached from `Config`,
- command palette state,
- recently closed tab stack,
- optional inline tab rename state,
- optional maximized terminal leaf,
- home-page focus for zero-tab state.

`Tty7App::new` restores `Session` if present. A missing session creates one default terminal. A saved zero-tab session restores the home page instead of opening a terminal.

## Tabs, splits, and home

### Tabs

A `Tab` contains a `Pane` split tree, optional custom name, and optional settings state. Settings is represented as a dedicated tab with `Pane::Empty` and is not persisted.

Important tab workflows in `src/ui/app.rs` and `src/ui/tab_strip.rs`:

- New tab inherits cwd from the active focused pane when possible.
- New tab insertion follows `Config::new_tab_position`.
- Tab labels use custom names first, otherwise derive from terminal title.
- Reopen closed tab uses a serialized `SessionTab` snapshot and rebuilds shells/layout from saved cwd/pane ids.
- Drag/drop can reorder tabs.
- Settings tab toggles open/active with the settings action.

### Splits

`src/ui/pane.rs` represents split layout as a binary tree:

- `Leaf(Entity<TerminalView>)`,
- `Split { axis, a, b, ratio, dragging }`,
- `Empty` for transient/settings cases.

Split behavior:

- Split right/down creates a new terminal in the current pane's cwd.
- Divider ratio is clamped to avoid unusable panes.
- Closing a focused leaf collapses its parent to the sibling.
- Focus next/previous cycles leaves.
- Maximize renders only one leaf while preserving the tree.

### Zero-tab home page

`src/ui/home.rs` was added recently so zero tabs is a first-class state. Closing the last tab shows the home page; quitting from there saves/restores zero tabs. Enter or clicking the page opens a new terminal, while app-level shortcuts still work because `Tty7App` keeps a home focus handle.

## Terminal view and local emulator

`src/terminal/remote.rs` owns the daemon socket and local `alacritty_terminal::Term` mirror. `src/terminal/view.rs` wraps that backend in a GPUI view and coordinates input, search UI, local editor, cursor blinking, scroll, mouse reporting, and terminal events.

`TerminalView::new`:

- creates or attaches a `RemoteTerminal`,
- loads font/config state,
- starts async draining of backend events,
- tracks focus and cursor blink,
- polls foreground/prompt state for prompt-aware behavior and notifications,
- loads and ranks command history.

`TerminalView::handle_event` processes events emitted by the alacritty parser/event proxy, including title changes, child exit, clipboard operations, color requests, bell flash, terminal query replies, and PTY writes.

## Grid rendering

`src/terminal/element.rs` defines the custom GPUI element that paints terminal cells.

It resolves colors from:

- ANSI 16/256 palette (`src/terminal/palette.rs`),
- active theme/config colors,
- OSC color overrides from the terminal state.

It handles text attributes and terminal-specific drawing concerns such as bold/italic faces, underline styles, inverse/hidden cells, wide spacers, selection wash, search match highlights, hovered links, and disabled ligatures. It also measures cell size and drives terminal resize through `TerminalView::set_grid_size`.

## Input routing

`TerminalView::on_key_down` routes keys based on mode:

1. If scrollback search is focused, search handles text and Escape closes it.
2. Command-platform shortcuts go through app or terminal shortcut handlers.
3. If the local prompt editor is active, editor/readline behavior handles keys.
4. Otherwise keys are encoded to PTY bytes by `src/terminal/input.rs`.

`input_active()` is true only when the terminal is not exited, search is not focused, the terminal is not on the alternate screen, and shell integration says the shell is at a prompt.

`src/terminal/input.rs` supports legacy escape sequences and Kitty keyboard protocol (`CSI u`) based on active terminal modes. Comments note that `REPORT_EVENT_TYPES` and `REPORT_ALTERNATE_KEYS` are not implemented yet.

IME committed text flows through `TerminalView::commit_text`: at prompt it edits the local command buffer; in raw/TUI mode it writes to PTY.

Mouse handling supports local scrollback, SGR/X10 mouse reporting, alternate-scroll-to-arrows, link hover/click, and focus-follows-mouse when enabled.

Paste handling is security-sensitive:

- At prompt, paste inserts into the local editor and strips one trailing newline to avoid accidental submit.
- Outside prompt, paste writes to PTY.
- Bracketed paste strips ESC bytes to prevent embedded end-marker breakout.
- Non-bracketed paste normalizes newlines to carriage returns.

## Local command editor

`src/terminal/cmd_editor.rs` implements a custom single-line editor because GPUI input widgets consume keys tty7 needs for terminal-style editing.

Features include:

- UTF-8-safe char buffer and cursor,
- selection anchor/cursor,
- undo/redo snapshots,
- word movement/deletion,
- select word/all,
- byte offset mapping for rendering.

`TerminalView::apply_readline_ctrl` implements prompt keybindings such as Ctrl+A/E/B/F, Ctrl+W/U/K/H, Ctrl+C, Ctrl+D, Ctrl+R, and ghost-suggestion acceptance.

`src/terminal/preinit.rs` captures best-effort typeahead before shell integration first marks the prompt. When the local editor activates, tty7 can send Ctrl+U to clear shell-side stray text and seed the editor with reconstructed text. Unreconstructable inputs taint the capture.

## History, completion, and search

### History

`src/terminal/history.rs` stores tty7 history at `history` in the config dir. It also reads user shell histories (`~/.zsh_history`, `~/.bash_history`, and `$HISTFILE`) as read-only seeds so first launch has useful recall.

History behavior:

- tty7 writes `<cwd>\t<command>` lines when cwd is known.
- Legacy bare lines parse without cwd association.
- Load normalizes blanks/duplicates and caps entries.
- Frecency ranking combines recency, frequency, and current-directory bonus.
- Up/Down uses chronological history; ghost suggestion uses frecency-ranked history.

### Completion

`src/terminal/completion.rs` is tty7-owned completion, not shell completion. It offers:

- command-position completions from builtins and `$PATH`,
- path completions elsewhere.

It intentionally excludes history from the Tab menu. Whole-line recall belongs to ghost suggestions and Ctrl+R, matching the recent commit that made Tab completion a history-free picker.

Completion is intentionally shallow: whitespace-delimited word detection, simple path logic, hidden files only when prefix starts with `.`, `~` expansion, candidate cap, and sorting by closeness.

### Search

`src/terminal/search.rs` implements Cmd/Ctrl+F scrollback search and URL detection. Search highlights all matches and current match in the terminal element, caps total matches, and scrolls the current match into view.

URL handling prefers OSC 8 hyperlinks and falls back to bare URL detection. Cmd/Ctrl-click opens links when `Config::link_url` is enabled.

`src/terminal/reverse_search.rs` implements Ctrl+R history search with case-insensitive contains matching, repeated Ctrl+R to advance older, Enter to accept into editor, and Escape/Ctrl+G/Ctrl+C to cancel.

## Settings, keymap, and theme

`src/ui/settings.rs` renders settings sections for appearance, terminal behavior, shell, window/tabs, keybindings display, and about. Most settings mutate `Config`, call `save()`, and notify the app. Font and theme changes can apply live; shell, env, working directory, and scrollback limits primarily affect newly spawned/attached panes.

`src/ui/keymap.rs` installs defaults and reads `Config::keybindings` overrides. The settings UI displays effective/default bindings; keybinding editing is JSON-config-oriented in current source.

`src/ui/theme.rs` and `src/ui/presets.rs` apply color presets, config overrides, cursor hiding, and macOS native appearance synchronization.

## Change guidance

- UI changes should first check `gpui-component` patterns; the project convention is to reuse component widgets rather than hand-roll equivalents.
- Terminal input/editor changes should add unit tests near pure helpers (`input.rs`, `cmd_editor.rs`, `completion.rs`, `history.rs`, `search.rs`, `reverse_search.rs`, `preinit.rs`, and pure helpers in `view.rs`).
- Rendering changes should consider terminal invariants: fixed cells, no ligatures, wide spacers, alternate screen, mouse modes, selection, search highlights, link hover, and theme/OSC color precedence.
- Prompt-aware UX changes must account for shells without OSC 133 integration.
- Closing UI elements has runtime consequences: app/window close detaches; explicit pane/tab close kills daemon panes.
