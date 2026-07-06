# Changelog

All notable changes to tty7 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- PowerShell shell integration: `powershell.exe` and `pwsh` now emit the OSC 133
  semantic-prompt marks and OSC 7 cwd that zsh/bash/fish already do, injected
  via `-EncodedCommand` after the user's profile loads (their config is never
  touched). This turns on the inline line editor at the PowerShell prompt — so
  clicking positions the caret and new tabs/splits inherit the working
  directory — which is what previously made mouse clicks a no-op at the prompt
  on Windows.

### Fixed

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

[Unreleased]: https://github.com/l0ng-ai/tty7/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/l0ng-ai/tty7/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/l0ng-ai/tty7/releases/tag/v0.1.0
[gpui]: https://github.com/zed-industries/zed
