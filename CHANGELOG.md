# Changelog

All notable changes to tty7 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Context menus and the "+" dropdown now highlight the hovered row with the
  same soft fill the command palette uses for its selected row, instead of the
  stock saturated accent that snapped hard against the rest of the UI. The
  hover text stays at the normal foreground so it reads clearly on the quieter
  fill.

### Fixed

- Nerd Font prompt icons no longer render sliced off on the right. A non-Mono
  Nerd Font (and the proportional `➜`/`❯` the OS cascade hands back when nothing
  in your font list covers them) gives an icon a single-cell *advance* but draws
  ink up to ~1.9 cells wide, and tty7 clipped every lone glyph to exactly one
  cell — severing the overflow into the half-icons and cut-off arrow from the
  report. A single glyph now paints into a two-cell window, so it renders whole
  (bleeding into the trailing blank the way iTerm2 and Terminal.app do), bounded
  at two cells so a stray face can't smear across the row. Pairs with the native
  powerline separators from #19; Mono Nerd Fonts are unchanged. (#17)

- New tabs and splits no longer stall for seconds while a zsh plugin manager
  reinstalls itself. tty7 launches zsh through a throwaway `ZDOTDIR` (so it can
  layer its shell integration on top of your config), but it used to leave
  `ZDOTDIR` pointing at that empty temp dir the whole time — so tools that find
  their own state via `${ZDOTDIR:-$HOME}` (Zim, oh-my-zsh, `compinit`'s
  `.zcompdump`) looked in the wrong place and rebuilt from scratch on every
  pane, e.g. Zim reprinting `modules/…: Installed` and hanging for ~3s. Each
  redirector now points `ZDOTDIR` back at your real config dir while your
  startup files run, and restores it for the live session, so plugin managers
  and completion caches resolve correctly and load instantly. As a bonus this
  also fixes the classic relocated-config layout (a tiny `~/.zshenv` that sets
  `ZDOTDIR=~/.config/zsh`), which previously loaded your config but silently
  dropped tty7's integration. (#15)

## [0.6.1] - 2026-07-08

### Fixed

- Tab completion (and other line editing) now stays out of the way over `ssh`.
  A remote shell that emits its own prompt marks — fish 4.x on a Linux server,
  most visibly, which ships OSC 133 on by default — used to engage tty7's
  *local* line editor, so Tab ran completion against the local machine's
  filesystem instead of reaching the remote shell. tty7 now only drives the
  inline editor while the shell it launched is itself idle at its prompt;
  whenever a foreground command (ssh, a TUI, a nested shell) owns the terminal,
  keystrokes pass straight through to it. (#26, follow-up to #18)

## [0.6.0] - 2026-07-08

### Added

- The "+" button now opens a shell picker: tty7 detects the shells installed
  on this machine (on Unix the login shell, `/etc/shells`, plus well-known
  shells found on `PATH` — fish, nushell, pwsh and friends installed by
  Homebrew/nix are never registered in `/etc/shells`; on Windows PowerShell 7,
  Windows PowerShell, Command Prompt, Git Bash and WSL distributions)
  and lists them in a dropdown, so opening a tab in a different shell
  no longer requires editing `config.json`. The default entry leads the menu,
  ⌘T / Ctrl+T still opens a default tab in one keystroke, and splitting a pane
  inherits its shell — a fish tab splits into more fish, not back to the
  default. Shells picked this way aren't remembered across restarts (restored
  panes re-attach to their still-running shells anyway).

### Changed

- The Windows default shell now prefers PowerShell 7 (`pwsh.exe`) when
  installed — probed across Program Files (x64/x86/ARM), the Microsoft Store,
  scoop, dotnet tools and `PATH` — and falls back to Windows PowerShell as
  before. Set `shell` in `config.json` to override, as ever.

### Fixed

- Powerline prompt separators (powerlevel10k, oh-my-posh, oh-my-zsh) now render
  pixel-perfect at any font, size and line-height: the eight solid separators
  (sharp triangles, round caps, slants) are drawn natively as fill paths sized
  to the exact cell instead of relying on a Nerd Font, so segments meet their
  backgrounds cleanly with no gaps, narrow wedges or tofu. The bundled Hack font
  is also appended to every font-fallback chain, so common prompt glyphs (➜, ❯,
  box drawing) no longer render truncated or missing when no Nerd Font is
  installed. (#17)
- A URL glued directly to a full-width bracket with no space — e.g.
  `…/pull/343（fix/… → dev）` — no longer swallows the bracket text into the
  link. URL detection now stops at the first non-URL character (a CJK glyph,
  full-width bracket, arrow or emoji), while interior ASCII parens
  (Wikipedia-style URLs) are still preserved.
- Fish, nushell, pwsh and other shells installed by Homebrew or nix now appear
  in the "+" shell picker even when they aren't registered in `/etc/shells`
  (which those package managers leave to the user): a curated set of well-known
  shells is now probed on `PATH` as a catch-all, after the `/etc/shells`
  entries. (#18)
- Upgrading tty7 while an older daemon is still running in the background no
  longer breaks new tabs. A stale daemon that accepts the connection but can't
  serve the new client's request is now restarted once and retried
  automatically; on macOS the GUI also forwards the shell it was launched with
  to the detached daemon, so panes use the right shell instead of a stale
  `$SHELL` inherited from LaunchServices.

## [0.5.0] - 2026-07-07

### Added

- Windows releases now ship an Inno Setup installer
  (`tty7-<version>-windows-x86_64-setup.exe`) alongside the portable zip. It
  installs per-user by default (no admin prompt, with an all-users option),
  adds a Start Menu shortcut and an "Apps" uninstall entry, and offers an
  optional desktop icon. Still unsigned, so SmartScreen warns on first launch.
- Startup update check: tty7 asks GitHub once, in the background, whether a
  newer release has shipped. If so, it pops a one-time "Update available" dialog
  (once per version — remembered in `update.json`, so it never nags twice for
  the same release) and keeps a persistent "Download" prompt in Settings →
  About. Both open the Releases page; tty7 never downloads or updates itself —
  you still install by hand. Turn the check off with `check_for_updates` in
  `config.json` or the "Check for updates on launch" toggle in About. A failed
  or offline check is silent.
- ⌘K (Ctrl+K on Windows/Linux) clears the screen and scrollback — the same
  "Clear" the right-click menu already offered, now on the keyboard shortcut
  Terminal.app, iTerm2, and Ghostty users expect. Also available from the
  command palette, and remappable as `ClearScrollback` in `keybindings`.
- ⌘⏎ toggles window fullscreen (new `ToggleFullscreen` action, also in the
  View menu and command palette), matching the Ghostty/iTerm2 default. It
  previously toggled pane maximize — which silently did nothing in a
  single-pane tab, so the chord felt dead.
- The right-click menu now shows each item's keyboard shortcut. Copy, Paste,
  Select All, and Find previously showed nothing (they're dispatched inline,
  with no bound key for the menu to read a hint from) while the other items
  did, so the menu looked half-labelled. ⌘A / ⌘F stay hint-less on
  Windows/Linux, where those chords keep their readline meaning.

### Changed

- Maximize / restore pane moved from ⌘⏎ to ⌘⇧⏎ (Ghostty's `toggle_split_zoom`
  default), making room for fullscreen on the bare chord. An existing
  `ToggleMaximizePane` override in `keybindings` still wins.

### Fixed

- Windows: launching tty7 no longer opens a stray console window behind the
  app. Release builds are now linked with the `windows` subsystem; debug
  builds keep the console so `println!` output stays visible. (#10)
- The right-click "Select All" now matches the ⌘A shortcut: at the prompt it
  selects the edited command line, otherwise the whole terminal buffer. It
  previously always selected the whole buffer, so click and keystroke behaved
  differently at the prompt.
- Ctrl+R reverse-search now accepts plain ASCII keystrokes. The query only
  took text from the IME commit path, so a non-CJK input source on macOS — and
  all typing on Linux — was swallowed: the search box opened but ate every key.
  Reported on V2EX.

## [0.3.0] - 2026-07-07

### Added

- PowerShell shell integration: `powershell.exe` and `pwsh` now emit the OSC 133
  semantic-prompt marks and OSC 7 cwd that zsh/bash/fish already do, injected
  via `-EncodedCommand` after the user's profile loads (their config is never
  touched). This turns on the inline line editor at the PowerShell prompt — so
  clicking positions the caret and new tabs/splits inherit the working
  directory — which is what previously made mouse clicks a no-op at the prompt
  on Windows.

### Fixed

- Typing `exit` (or Ctrl-D) left a dead "process exited" pane behind instead
  of closing it. A pane whose shell genuinely ends now closes itself —
  collapsing its split, or closing the tab when it was the only pane (the
  last tab falls back to the home page), like every other terminal. A pane
  that merely *lost its daemon connection* still stays visible: auto-closing
  those would silently discard — and kill — sessions that may still be alive
  daemon-side. Panes that died while detached clean themselves up on the next
  attach the same way.

- A full-screen TUI dying without restoring the terminal — the canonical case
  being an ssh session dropping mid-`htop`/`vim` — left the pane stranded on
  the alt screen with a hidden cursor and live mouse reporting: a visible
  prompt with no cursor anywhere, mouse clicks echoing `0;19;42M`-style junk,
  and broken scrollback. The client now scrubs this residue the moment the
  shell reports its next prompt (OSC 133): it leaves the stranded alt screen,
  re-shows the DECTCEM-hidden cursor, and disables stale mouse/focus reporting
  and kitty keyboard flags — each reset only when its mode is actually set.
  Reattach self-heals the same way, since the daemon replays the prompt state
  after the ring.

- Windows shell integration never engaged even for the default shell: detection
  keyed off `portable-pty`'s `get_shell()`, which reports `%ComSpec%` (cmd.exe)
  regardless of what's actually spawned, so the PowerShell default was mistaken
  for an unsupported shell. It now resolves to `powershell.exe` directly.

## [0.2.0] - 2026-07-04

### Added

- Underline styles: undercurl, double, dotted, and dashed underlines render distinctly.
- `config.json` hot reload — edits apply to the running app without a restart.
- Desktop notifications driven by OSC 9 / OSC 777 escape sequences.
- Kitty keyboard protocol (CSI u progressive enhancement) for TUI apps like Neovim and Helix.
- Shell integration for bash and fish, alongside the existing zsh support.
- Windows support: cross-platform daemon, PowerShell as the default shell, embedded app icon.
- Linux support: builds against gpui's x11/wayland backends, `/proc`-based foreground cwd + pane-title tracking, Linux CI job, and documented build dependencies.
- Downloadable builds for every platform: the release workflow now packages and uploads all four targets — signed/notarized macOS DMGs (arm64 + x86_64) plus unsigned archives for Windows (`.zip`) and Linux (`.tar.gz`), each via its own `.github/scripts/bundle-<os>` script.
- Settings UI: terminal / appearance / behavior options are configurable from the GUI, with a searchable font-family dropdown and a wider theme gallery.
- Configurable default shell.

### Changed

- Project renamed to **tty7**.
- macOS releases ship as drag-to-Applications DMGs instead of zips, and the
  Intel build moved to the `macos-15-intel` runner (`macos-13` was retired,
  which had silently kept x86_64 assets from ever publishing).
- Pixel-smooth scrollback: scrolling carries a sub-line fraction and shifts the paint instead of jumping whole lines.
- Smoother scrolling on dense screens: glyph shaping is batched and wakeups are coalesced.
- CJK-dense screens paint ~2.4× faster: consecutive wide glyphs batch into single shaped runs (two columns per glyph) instead of painting cell-by-cell; the grid snapshot buffer is reused across frames and the selection/search overlay scans are skipped when nothing is highlighted. Release builds now use thin LTO.
- Type-ahead is integrated into the line editor instead of being stranded on zle's line.
- New tabs open next to the active tab instead of at the end.
- Terminal throughput ~12× faster (11 MB `cat`: ~2.0 s → ~0.16 s; DOOM-fire: ~47 fps → ~920 fps, both at 155×40 on an M1 Pro — now ahead of Alacritty/Ghostty on the same machine): the daemon's replay ring is a `VecDeque` so a full ring no longer memmoves 8 MiB per ~1 KiB PTY read, and the per-connection writer coalesces queued `Output` frames (≤256 KiB) so a flood reaches the client as a few large frames instead of thousands of tiny ones. A backpressure gate (4 MiB high-water) pauses the PTY reader while the client catches up, so a runaway `yes` can't grow daemon memory without bound. `TTY7_TRACE=1` prints per-second reader-loop accounting on both sides for future diagnosis.
- Second throughput pass, another ~1.4× on bulk output (11 MB `cat`: ~160 ms → ~100 ms; sustained plaintext drain 124 → 148 MB/s, vs ~170 MB/s for a raw do-nothing PTY reader on the same machine; DOOM-fire is unchanged — it is producer-bound at ~96 MB/s): the backpressure high-water grows to 16 MiB so a big burst drains at PTY speed while the client parses in its own time; daemon⇄GUI socket buffers grow from macOS's 8 KiB default to 256 KiB; the client applies consecutive `Output` frames as one batched parser pass (one term-lock + wakeup per burst, latency-free — the batch never waits for unarrived bytes); the shared OSC tokenizer skips Ground/Ignore runs with SIMD `memchr`; the gate's hot path is a lock-free atomic (previously a Mutex plus an unconditional `notify_all` per socket write); and the four threads on the interactive output path ask macOS for `USER_INTERACTIVE` QoS to stay off the efficiency cores (`TTY7_NO_QOS=1` opts out).

### Fixed

- A long `--config-dir` path crashed the GUI at startup ("path must be shorter than SUN_LEN"): when `<config>/daemon.sock` would exceed the OS socket-path limit (104 bytes on macOS), the endpoint now falls back to a short per-user path keyed by a stable hash of the config dir ($XDG_RUNTIME_DIR, else the OS temp dir). Short paths keep the original layout, so existing daemons stay reachable.
- Typing right after a command finished could leave a stray echoed character plus zsh's reverse-video `%` in the scrollback: the "command finished" mark (OSC 133;D) is now emitted the instant the command exits — prepended ahead of the user's precmd hooks (zsh/bash) — instead of after slow prompt frameworks (oh-my-zsh git status, conda), so the local input editor takes keystrokes back hundreds of milliseconds sooner.
- Typing while a command was still running stranded those keystrokes on zle's line at the next prompt — un-editable and double-drawn under the line editor's overlay. Type-ahead adoption (wipe the shell's line, seed the editor) now runs at every prompt, not just the shell's first, and the wipe waits until zle is actually reading (the live `133;B` mark) so it is consumed silently instead of being kernel-echoed into the scrollback as a literal `^U`.
- Typing ahead of a fast command left kernel-echoed debris in the scrollback (`ls` plus zsh's reverse-video `%`). Reconstructable gap input is now held client-side for up to 150 ms: a command that finishes inside the window hands the keystrokes straight to the line editor with the PTY untouched — zero echo; a longer command (or one reading stdin) gets the bytes released verbatim, so REPLs and password prompts still work.
- fish shell integration silently never installed, so fish users got no prompt marks or cwd tracking.
- **Security:** pasted clipboard content is stripped of ESC bytes, closing a bracketed-paste escape that could inject auto-executing commands.
- Crash when copying/cutting right after a forward word/line delete left a stale selection anchor.
- `Ctrl+Alt+<letter>` was indistinguishable from `Ctrl+<letter>` because the legacy key encoder dropped the Alt ESC prefix.
- Plain Enter/Tab/Backspace were wrongly CSI-u-encoded at the kitty-keyboard DISAMBIGUATE level, which could wedge the shell after a crashed TUI.
- No-op edits (e.g. Backspace at the start of the line) no longer swallow the first undo.
- OSC scanners (daemon-side and notification-side) dropped a well-formed sequence that directly followed an unterminated one.
- Daemon pane teardown is hardened: process-group kill, bounded join, dead panes are reclaimed.
- New shells default to `$HOME` when launched from the app bundle with cwd `/`.

## [0.1.0] - 2026-06-30

Initial release.

- Sessions live in a persistent daemon and survive window close / app restart.
- GPU-rendered terminal grid on [gpui], backed by Zed's `alacritty_terminal` fork.
- Tabs and pane splits (split right/down, maximize, focus movement).
- Command palette with fuzzy search over every action.
- Smart line editing: inline completion, syntax highlighting, history, in-terminal search.
- zsh shell integration (OSC 7 cwd + OSC 133 prompt marks) via a throwaway `ZDOTDIR`.
- Native macOS light/dark themes that follow the system appearance.

[Unreleased]: https://github.com/l0ng-ai/tty7/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/l0ng-ai/tty7/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/l0ng-ai/tty7/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/l0ng-ai/tty7/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/l0ng-ai/tty7/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/l0ng-ai/tty7/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/l0ng-ai/tty7/releases/tag/v0.1.0
[gpui]: https://github.com/zed-industries/zed
