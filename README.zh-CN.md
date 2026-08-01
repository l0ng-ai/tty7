<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

# tty7

**高性能终端：常驻会话、远程工作区和 coding agent 支持。**

<sub>纯 Rust · GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-%E5%8A%A0%E5%85%A5%E7%BE%A4%E7%BB%84-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>[English](README.md) · 简体中文</sub>

</div>

tty7 是用于本地和远程开发的桌面终端。后台 server 持有工作区和 pane；关闭窗口不会结束
其中运行的 shell 或 coding agent。

## 安装

在 [**Releases**](https://github.com/l0ng-ai/tty7/releases) 下载对应平台的原生构建。

| 平台 | 下载 | 安装方式 |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | 拖进「应用程序」 |
| **Windows** | `…-setup.exe` · 便携版 `….zip` | 运行安装程序，或解压使用 |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` 后直接运行；X11/Wayland 依赖已打包 |

每个安装包都包含 `tty7` CLI，并会将其加入 PATH。

## 性能

Apple M1 Pro、macOS 26.3.1、155×40 网格、五次运行平均值（2026-07-04）：

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 I/O —— 11 MB `cat` <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + 常驻 server 11 MB。</sub>

测试方法和一键复现脚本：[`scripts/bench/`](scripts/bench/README.md)。

## 终端功能

- 输入：历史影子建议、带说明的 Tab 补全、语法高亮、模糊搜索、多行编辑和鼠标定位光标。
- 窗口：标签页、分屏、命令面板、回滚搜索、输入法、9 套主题、主题编辑、跟随系统外观和 tmux 快捷键预设。
- Agent：会话 fork、上下文命令，以及 agent 等待输入时的托盘提醒。

详细说明见 [功能列表](docs/features.zh-CN.md) · [English](docs/features.md)。

## 常驻会话

应用退出后，server 仍会保持 shell 和 pane；机器重启后可恢复。这样即使不使用 tmux，
终端中的工作也不会因为关闭窗口而中断。

## 远程工作区

tty7 使用原生 Rust SSH 实现（russh），支持保存的 profile、OS keychain 中的凭据、
`~/.ssh/config` alias、GUI 认证、SFTP、端口转发和跳板机。

打开远程工作区时，tty7 会通过现有 SSH 连接在主机上安装 `tty7-server`。这项安装只需一次，
不需要 `sudo`。远端 server 持有工作区及其 pane；本地应用关闭或连接暂时不可用时，它们仍会
继续运行。之后可从当前或另一台客户端重新连接。

## Agent 感知

tty7 会按 pane 识别已支持的 coding agent，包括 Claude Code、Codex、Gemini CLI、Aider 和
OpenCode。它会显示状态、在 agent 等待输入时发送通知，并显示该 pane 的 git 分支和工作区改动。
已支持的 agent 对话可在重启后恢复。

## CLI + Skills

`tty7` CLI 将工作区和 pane 暴露给 coding agent。配套的 [agent skill](skills/tty7/SKILL.md) 会把
这套接口教给 agent：每个命令的作用、工作区和 pane 的寻址方式，以及什么任务需要真实终端而不是普通
shell 命令。

两者结合后，agent 可以把 tty7 当成与用户共享的终端环境：为任务创建 pane，在其中运行服务器或交互式
程序，之后继续发送输入、读取输出、检查进程和端口；本地和已连接远端的工作区都使用同一套操作。

```sh
tty7 ls                                  # 列出工作区和 pane
tty7 pane split %42 --v                  # 新建同级 pane，并打印其 id
tty7 send %83 'pnpm dev' --enter          # 在该 pane 中启动任务
tty7 capture %83 --plain                 # 读取其输出
tty7 procs %83                           # 查看其进程和监听端口
tty7 run -- cargo test                   # 在真实 PTY 中运行一次性命令
tty7 -m devbox ls                        # 对已连接远端使用同一套接口
```

`--json` 让 agent 可以根据工作区、pane、进程、端口或 agent 状态的结构化结果做决定。完整接口见
[`skills/tty7/SKILL.md`](skills/tty7/SKILL.md)。

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 与 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
