<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**A GPU-rendered terminal in pure Rust.**

<sub>GPU rendering on Zed's gpui · VT core from Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=ff8a5c)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-join%20chat-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

[**Why tty7**](#why-tty7) · [**Install**](#install) · [**Features**](#features) · [**Benchmarks**](#benchmarks) · [**Keybindings**](#keybindings)

<sub>English · [简体中文](README.zh-CN.md)</sub>

</div>

## Why tty7

- **Fast** — about 2× the throughput of Alacritty, Ghostty, or Kitty on the
  same hardware ([benchmarks](#benchmarks))
- **A modern prompt** — completion, syntax highlighting, and history search
  built in; no plugins to assemble
- **Sessions that survive** — close the window or quit the app, your shells
  keep running; no tmux
- **Zero config** — zsh, bash, fish, and PowerShell work out of the box

Native builds for macOS, Windows, and Linux ship with every release.

## Install

Download the build for your platform from
[**Releases**](https://github.com/l0ng-ai/tty7/releases):

- **macOS** — `tty7-<version>-macos-arm64.dmg` (Apple Silicon) or `…-x86_64.dmg`
  (Intel); open it and drag `tty7.app` into Applications.
- **Windows** — `…-windows-x86_64-setup.exe` (installer: Start Menu shortcut +
  uninstall entry), or `…-windows-x86_64.zip` (portable: unzip and run
  `tty7.exe`).
- **Linux** — `…-linux-x86_64.AppImage` (recommended: bundles the x11/wayland
  libraries, so it runs on Fedora / Arch / etc. with no extra packages —
  `chmod +x` and run), or `…-linux-x86_64.tar.gz` (bare binary; extract and run
  `./tty7`, needs the usual x11/wayland runtime libraries installed).

## Features

### At the prompt

- **Ghost suggestions** — your history completes the whole line as you type; <kbd>→</kbd> to accept
- **Tab completion that explains** — every flag and subcommand with its description, for ~100 common commands
- **Syntax highlighting** — as you type, nothing to install
- **Fuzzy history search** — <kbd>⌃ R</kbd> shows what you ran, where, and whether it failed
- **History from day one** — your existing shell history just works, and carries across sessions
- **Real line editing** — selection, word motion, undo

### In the window

- **Tabs & splits** — always open in the current directory
- **Command palette** <kbd>⌘ P</kbd> · scrollback search <kbd>⌘ F</kbd>
- **⌘-click links** · desktop notifications
- **Eight themes** · CJK / IME input

### CLI coding agents

tty7 recognizes third-party coding agents running in a pane (Claude Code,
Codex, Gemini CLI, Aider, Amp, OpenCode, and ~10 more) and enriches them — it
never wraps or replaces the agent.

- **Brand avatars** — the tab chip / sidebar row shows which agent runs where; custom wrappers map in via `agent_commands` in `config.json`
- **Live status dot** — working (blue) / needs your input (amber) / done (green), driven by agent-reported events over an OSC channel; run *Agent: Install Claude Code Hooks* from the palette to wire Claude Code up
- **Notifications that matter** — "needs your permission…" the moment an agent blocks on you, and "finished after Ns" per turn, honoring your notification policy
- **Branch at a glance** — each sidebar row shows its pane's git branch and working-tree diff (`+N −M`), refreshed on `cd` and when a command finishes
- **Session resume** — panes lost to a reboot re-launch their agent conversation (`claude --resume …`) on restore (`restore_agent_sessions`, on by default)
- **Context feed** — palette commands send the current selection or the repo's `git diff` to the running agent as a ready-made prompt

### SSH connection manager

A native Rust SSH stack (russh) is the **only** path — profiles, credentials,
and SFTP without ever shelling out to `ssh`. There is no system-ssh compat mode.

- **QuickConnect** — type `user@host[:port]` in the palette and connect; IPv6 `[::1]:port` supported
- **Saved profiles** — full connection config with passwords / passphrases in the OS keychain, never on disk
- **`~/.ssh/config` aliases** — type one to connect (resolved natively — common fields, best-effort — over russh), or import them as profiles in Settings
- **GUI auth** — in-pane sheets for password, key passphrase, 2FA, and host-key confirmation (new vs. changed)
- **Built-in SFTP** — a slide-in file panel: browse, upload / download, rename / delete / chmod, drag to Finder
- **Port forwarding** — Local / Remote / Dynamic, preconfigured or added live, plus ⌘-click `localhost:PORT` to auto-forward
- **Jump hosts & proxies** — multi-hop via profile references or `ProxyJump`, ProxyCommand, SOCKS5 / HTTP

| Entry point | Connects via |
|---|---|
| Saved profiles · QuickConnect · typed `user@host[:port]` | Native russh — SFTP · keychain · GUI auth · L/R/D forwards |
| `~/.ssh/config` aliases | Resolved natively, then russh (`Match`/canonicalize/GSSAPI unsupported — no fallback) |

## Benchmarks

All four terminals measured back-to-back on the same machine, same day, same
155×40 grid — Apple M1 Pro, macOS 26.3.1, five-run averages (2026-07-04):

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| Plaintext IO — 11 MB `cat` <sub>(lower = better)</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) frame rate <sub>(higher = better)</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| Cold-launch memory | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + the persistent daemon 11 MB.</sub>

Where the speed comes from:

- The PTY is read at device speed and parsed in large batches, off the render path
- Hot paths are lock-free — a big `cat` never waits on drawing
- The daemon buffers up to 16 MiB ahead of the window before backpressure applies

Methodology (how each terminal is driven, grid fairness, known pitfalls) and
one-command reproduction live in [`scripts/bench/`](scripts/bench/README.md) —
run it yourself.

## Keybindings

Keys are shown in macOS notation — on Windows and Linux, read <kbd>⌘</kbd> as
<kbd>Ctrl</kbd>. Open Settings with <kbd>⌘ ,</kbd> to browse or remap them all.
The essentials:

| | |
|---|---|
| <kbd>⌘ T</kbd> · <kbd>⌘ W</kbd> · <kbd>⌘ ⇧ T</kbd> | new tab · close tab · reopen closed tab |
| <kbd>⌘ 1</kbd>…<kbd>⌘ 9</kbd> · <kbd>⌃ ⇥</kbd> · <kbd>⌃ ⇧ ⇥</kbd> | jump to tab 1–9 · next tab · previous tab |
| <kbd>⌘ D</kbd> · <kbd>⌘ ⇧ D</kbd> | split right · split down |
| <kbd>⌘ ]</kbd> · <kbd>⌘ [</kbd> | next pane · previous pane |
| <kbd>⌘ ⌥ ←→↑↓</kbd> | focus the pane in that direction |
| <kbd>⌘ ⏎</kbd> · <kbd>⌘ ⇧ ⏎</kbd> | toggle fullscreen · maximize / restore the pane |
| <kbd>⌘ K</kbd> | clear the screen and scrollback |
| <kbd>⌘ P</kbd> | command palette |
| <kbd>⌘ F</kbd> | search the scrollback |
| <kbd>⌃ R</kbd> | fuzzy-search shell history |
| <kbd>⌘ +</kbd> · <kbd>⌘ −</kbd> · <kbd>⌘ 0</kbd> | font size up · down · reset |

**Settings → Keybindings** lists every shortcut. Click one, press the new keys
(<kbd>Esc</kbd> cancels, <kbd>Backspace</kbd> resets to default), and it takes
effect immediately. Pane resize and swap have no default keys — bind them here or
run them from the command palette. Prefer tmux muscle memory? Flip the **tmux**
preset to remap pane/tab actions onto a prefix (default <kbd>⌃ B</kbd>): <kbd>⌃ B</kbd>
<kbd>C</kbd> opens a tab, <kbd>⌃ B</kbd> <kbd>%</kbd> splits, <kbd>⌃ B</kbd> then an
arrow moves focus. A bare prefix reaches the shell after a brief pause, and
`prefix` + an unbound key is passed straight through to the terminal.

---

<div align="center">
<sub>

Built on [gpui](https://github.com/zed-industries/zed) and [`alacritty_terminal`](https://github.com/zed-industries/alacritty) · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [Changelog](CHANGELOG.md)

</sub>
</div>
