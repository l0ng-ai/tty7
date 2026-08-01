<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**High-performance terminal: persistent sessions, remote work, agents.**

<sub>Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-join%20chat-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>English · [简体中文](README.zh-CN.md)</sub>

</div>

## Why

- **Performance** — ~2× the throughput of Alacritty, Ghostty, or Kitty ([benchmarks](#benchmarks))
- **Sessions persist** — quit or reboot; your shells and supported agent sessions keep running, no tmux
- **Remote workspaces** — files, repos, tabs, panes, diffs, and worktrees run on the remote machine, not beside an SSH shell
- **Agent-aware** — recognizes Claude Code, Codex & co. in a pane: status, notifications, branch + diff, session resume
- **CLI + Skills** — gives coding agents an interface to create panes, run real TTY commands, inspect output, and work across connected machines

## Install

Native builds for each platform on [**Releases**](https://github.com/l0ng-ai/tty7/releases):

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | drag into Applications |
| **Windows** | `…-setup.exe` · portable `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` and run — X11/Wayland libraries bundled |

## What's inside

| | |
|---|---|
| **Input** | ghost suggestions from history · explained tab completion · syntax highlighting · multi-line editing · click places the caret · <kbd>⌃ R</kbd> fuzzy history |
| **Window** | tabs & splits · <kbd>⌘ P</kbd> palette · <kbd>⌘ F</kbd> scrollback search · nine themes · IME |
| **Agent-aware** | per-pane detection (~17 CLIs): status dot · notifications · branch + diff · resume after reboot · tray icon when input is needed |
| **Remote workspaces** | remote files, repos, changes, diffs, worktrees, tabs, and panes · reconnect from any client and continue where you left off |
| **CLI + Skills** | bundled `tty7` CLI · [agent skill](skills/tty7/SKILL.md) · pane/workspace control · real PTY commands · output, process, port, and agent status |
| **SSH** | native russh stack: profiles with keychain secrets · SFTP panel · port forwarding · jump hosts · one-time, unprivileged `tty7-server` install |

Terminal and keybinding reference: [docs/features.md](docs/features.md). The agent-facing CLI
interface is documented in [skills/tty7/SKILL.md](skills/tty7/SKILL.md).

## Benchmarks

Same machine, same day, same 155×40 grid — Apple M1 Pro, macOS 26.3.1,
five-run averages (2026-07-04):

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| Plaintext I/O — 11 MB `cat` <sub>(lower = better)</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) frame rate <sub>(higher = better)</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| Cold-launch memory | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + the persistent server 11 MB.</sub>

Methodology and one-command reproduction: [`scripts/bench/`](scripts/bench/README.md).

---

<div align="center">
<sub>

Built on [gpui](https://github.com/zed-industries/zed) and [`alacritty_terminal`](https://github.com/zed-industries/alacritty) · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [Changelog](CHANGELOG.md)

</sub>
</div>
