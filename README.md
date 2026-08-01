<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

# tty7

**A high-performance terminal with persistent sessions, remote workspaces, and coding-agent support.**

<sub>Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-join%20chat-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>English · [简体中文](README.zh-CN.md)</sub>

</div>

tty7 is a desktop terminal for local and remote development. A background server owns
workspaces and panes, so closing a window does not end the shell or coding agent running
inside it.

## Install

Download a native build from [**Releases**](https://github.com/l0ng-ai/tty7/releases).

| Platform | Download | Installation |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | Drag into Applications |
| **Windows** | `…-setup.exe` · portable `….zip` | Run the installer, or unzip |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` and run; X11/Wayland libraries are bundled |

Each installer includes the `tty7` CLI and adds it to PATH.

## Performance

On an Apple M1 Pro running macOS 26.3.1, using a 155×40 grid and five-run averages
(2026-07-04):

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| Plaintext I/O — 11 MB `cat` <sub>(lower is better)</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) frame rate <sub>(higher is better)</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| Cold-launch memory | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + persistent server 11 MB.</sub>

Methodology and a one-command reproduction script: [`scripts/bench/`](scripts/bench/README.md).

## Terminal features

- Input: history suggestions, explained tab completion, syntax highlighting, fuzzy search,
  multi-line editing, and mouse cursor placement.
- Window: tabs, splits, command palette, scrollback search, IME, nine themes, custom theme
  editing, system-appearance sync, and a tmux keybinding preset.
- Agents: session fork, context commands, and a tray indicator for agents waiting on input.

For details, see [Features](docs/features.md) · [中文](docs/features.zh-CN.md).

## Persistent sessions

The server keeps shells and panes alive when the application exits. They can be restored
after the machine restarts. This removes the need to run tmux solely to keep terminal work
alive.

## Remote workspaces

tty7 uses a native Rust SSH implementation (russh). It supports saved profiles with
credentials in the OS keychain, `~/.ssh/config` aliases, GUI authentication, SFTP, port
forwarding, and jump hosts.

When you open a remote workspace, tty7 installs `tty7-server` on the host over the existing
SSH connection. The installation is one-time and does not require `sudo`. The server holds
the remote workspace and its panes; they continue running while the local app is closed or
the connection is unavailable. Reconnect from this or another client to continue using them.

## Agent-aware

tty7 detects supported coding agents in each pane, including Claude Code, Codex, Gemini CLI,
Aider, and OpenCode. It shows their status, sends notifications when an agent needs input,
and displays the pane's git branch and working-tree changes. Supported agent conversations can
be resumed after a restart.

## CLI + Skills

The `tty7` CLI exposes workspaces and panes to coding agents. The companion
[agent skill](skills/tty7/SKILL.md) teaches an agent this interface: what each command does, how
to address a workspace or pane, and when a task needs a real terminal instead of an ordinary
shell command.

Together, they let an agent use tty7 as a shared terminal environment: create a pane for a task,
run a server or interactive program there, send more input later, read its output, inspect its
processes and ports, and work with the same local or connected remote workspaces that you see.

```sh
tty7 ls                                  # list workspaces and panes
tty7 pane split %42 --v                  # create a sibling pane; prints its id
tty7 send %83 'pnpm dev' --enter          # start work in that pane
tty7 capture %83 --plain                 # read its output
tty7 procs %83                           # inspect its processes and listening ports
tty7 run -- cargo test                   # run a one-off command in a real PTY
tty7 -m devbox ls                        # use the same interface on a connected remote
```

`--json` gives agents structured results when they need to make a decision from workspace, pane,
process, port, or agent status. The full interface is documented in
[`skills/tty7/SKILL.md`](skills/tty7/SKILL.md).

---

<div align="center">
<sub>

Built on [gpui](https://github.com/zed-industries/zed) and [`alacritty_terminal`](https://github.com/zed-industries/alacritty) · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [Changelog](CHANGELOG.md)

</sub>
</div>
