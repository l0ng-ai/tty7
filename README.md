<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**A GPU-rendered, daemon-backed terminal in pure Rust.**

<sub>built on Zed's gpui and Alacritty's VT core</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=ff8a5c)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

[**Install**](#-install) · [**Benchmarks**](#-benchmarks) · [**Shortcuts**](#️-shortcuts) · [**Contributing**](#-contributing)

<sub>English · [简体中文](README.zh-CN.md)</sub>

<br />

<img src="docs/screenshot.jpg" alt="tty7" width="820" />

</div>

<br />

tty7 is split into two Rust processes: a **persistent daemon** that owns the
shells, and a GPU-rendered client that talks to it over a local socket. Because
the shells live in the daemon, quitting and reopening the app leaves the session
intact — detach and reattach, no tmux required.

- 🔌 **Persistent sessions** — the daemon holds the PTYs and child processes,
  so closing a window, quitting the app, or swapping in a new build never takes
  a shell down.
- ⚡ **Performance** — an 11 MB `cat` completes in **95 ms**, versus 179–239 ms
  for Alacritty/Ghostty/Kitty; DOOM-fire renders at **888 fps** against their
  485–617. Measured on the same machine and grid; the harness is in the repo
  ([benchmarks](#-benchmarks)).
- 🧠 **Shell-aware** — new tabs and splits open in the current working
  directory, and path completion is always relative to it. No configuration is
  required; zsh, bash, fish, and PowerShell are set up automatically.
- ⌨️ **Enhanced prompt** — inline completion, syntax highlighting, history, and
  in-terminal search at the prompt. Typing `git commit --`, `kubectl`, or `npm`
  shows each flag and subcommand with its description — rich signatures for
  ~100 common commands, generated from Fig's spec corpus.

Also included: tabs (drag to reorder, inline rename, number keys to switch) and
resizable splits, a command palette, click-to-open links, desktop notifications,
and focus-follows-mouse. Eight built-in themes from light to dark, with the
native window chrome following the one you pick, plus CJK/IME input.

Native builds for macOS, Windows, and Linux — every release ships all three.

<br />

<div align="center">

**[Download the latest release&nbsp;&nbsp;▶](https://github.com/l0ng-ai/tty7/releases/latest)**

</div>

<br />

## 📊 Benchmarks

All four terminals measured back-to-back on the same machine, same day, same
155×40 grid — Apple M1 Pro, macOS 26.3.1, five-run averages (2026-07-04):

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| Plaintext IO — 11 MB `cat` <sub>(lower = better)</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) frame rate <sub>(higher = better)</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| Cold-launch memory | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + the persistent daemon 11 MB.</sub>

The daemon/client split also accounts for these numbers: the daemon drains the
PTY at device speed (it can run up to 16 MiB ahead of the client before
backpressure applies, so a large `cat` does not wait on rendering), the client
parses output in large batches off the render path, and the hot paths are
lock-free.

Methodology (how each terminal is driven, grid fairness, known pitfalls) and
one-command reproduction live in [`scripts/bench/`](scripts/bench/README.md) —
run it yourself.

## 🚀 Install

Grab the build for your platform from [**Releases**](https://github.com/l0ng-ai/tty7/releases):

- **macOS** — `tty7-<version>-macos-arm64.dmg` (Apple Silicon) or `…-x86_64.dmg`
  (Intel); open it and drag `tty7.app` into Applications.
- **Windows** — `…-windows-x86_64.zip`; unzip and run `tty7.exe`.
- **Linux** — `…-linux-x86_64.tar.gz`; extract and run `./tty7` (needs the usual
  x11/wayland runtime libraries).

## ⌨️ Shortcuts

Keys are shown in macOS notation — on Windows and Linux, read <kbd>⌘</kbd> as
<kbd>Ctrl</kbd>. Open Settings with <kbd>⌘ ,</kbd> to browse or remap them all.
The essentials:

| | |
|---|---|
| <kbd>⌘ T</kbd> · <kbd>⌘ W</kbd> · <kbd>⌘ ⇧ T</kbd> | new tab · close tab · reopen closed tab |
| <kbd>⌘ D</kbd> · <kbd>⌘ ⇧ D</kbd> | split right · split down |
| <kbd>⌘ ]</kbd> · <kbd>⌘ [</kbd> | next pane · previous pane |
| <kbd>⌘ ⏎</kbd> | maximize / restore the pane |
| <kbd>⌘ P</kbd> | command palette |
| <kbd>⌘ F</kbd> | search the scrollback |
| <kbd>⌃ R</kbd> | reverse-search shell history |
| <kbd>⌘ +</kbd> · <kbd>⌘ −</kbd> · <kbd>⌘ 0</kbd> | font size up · down · reset |

The full list — and any overrides — lives in **Settings → Keybindings**.

## 💭 Built with & inspired by

- [gpui](https://github.com/zed-industries/zed) — Zed's GPU-accelerated UI framework
- [`alacritty_terminal`](https://github.com/zed-industries/alacritty) (Zed's fork) — VT emulator, grid, and PTY
- [gpui-component](https://github.com/longbridge/gpui-component) — UI widgets, via a [pinned fork](https://github.com/l0ng-ai/gpui-component/tree/tty7)
- [tmux](https://github.com/tmux/tmux) — the inspiration for the persistent-daemon design

## 🤝 Contributing

Bug reports and PRs are welcome. Security issues go through
[SECURITY.md](SECURITY.md); notable changes land in the
[CHANGELOG](CHANGELOG.md).

## 📝 License

[Apache License 2.0](LICENSE) · © 2026 l0ng-ai

<br />

<div align="center">

<img src="assets/app-icon.svg" alt="" width="28" height="28" />

<sub><b>tty7</b> — a GPU-rendered, daemon-backed terminal in pure Rust.</sub>

</div>
