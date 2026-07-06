<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**纯 Rust 编写：GPU 渲染，守护进程托管。**

<sub>基于 Zed 的 gpui 与 Alacritty 的 VT 内核</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=ff8a5c)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

[**安装**](#-安装) · [**基准测试**](#-基准测试) · [**快捷键**](#️-快捷键) · [**参与贡献**](#-参与贡献)

<sub>[English](README.md) · 简体中文</sub>

<br />

<img src="docs/screenshot.jpg" alt="tty7" width="820" />

</div>

<br />

tty7 拆成两个进程：守护进程常驻后台，持有你所有的 shell；客户端负责 GPU
渲染，通过一条本地 socket 连上去，本身很薄。shell 都跑在守护进程那一侧，
所以关掉窗口、退出应用、再重新打开，会话原样都在 —— 随时断开、随时接回，
用不着 tmux。

- 🔌 **会话持久化** —— PTY 和子进程都归守护进程管，关窗口、退应用、乃至
  换上新版程序，都不会让任何一个 shell 中断。
- ⚡ **性能** —— 11 MB 的 `cat` 只花 **95 ms**，Alacritty/Ghostty/Kitty 要
  179–239 ms；DOOM-fire 跑到 **888 fps**，它们在 485–617 之间。同一台机器、
  同一网格测出来的，脚本就在仓库里（见[基准测试](#-基准测试)）。
- 🧠 **懂你的 shell** —— 新标签页和分屏都开在你当前所在的目录，路径补全也
  始终认得你在哪。零配置，zsh、bash、fish、PowerShell 开箱即用。
- ⌨️ **更好用的提示符** —— 内联补全、语法高亮、历史记录、终端内搜索，全在
  你敲命令的地方。敲下 `git commit --`、`kubectl`、`npm`，每个 flag、每个
  子命令都连着说明一起列出来 —— 覆盖约 100 个常用命令，数据来自 Fig 的语料。

其余该有的也没落下：标签页（拖拽重排、双击重命名、数字键切换）、可拖动分隔线
调节比例的分屏、命令面板、点击打开链接、桌面通知、焦点随鼠标移动。内置 8 套
主题（由浅及深），系统标题栏的明暗跟随所选主题；CJK 与输入法组合输入也一并
支持。

macOS、Windows、Linux 三个平台都有原生构建，每个 release 一起打出。

<br />

<div align="center">

**[下载最新版本&nbsp;&nbsp;▶](https://github.com/l0ng-ai/tty7/releases/latest)**

</div>

<br />

## 📊 基准测试

四款终端在同一台机器上一口气测完，网格统一为 155×40 —— Apple M1 Pro，
macOS 26.3.1，取五次运行的平均值（2026-07-04）：

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 IO —— 11 MB `cat` <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + 常驻守护进程 11 MB。</sub>

这些数字同样来自守护进程与客户端的拆分：守护进程按设备速度把 PTY 排空
（最多可领先客户端 16 MiB 才触发背压，再大的 `cat` 也不会卡在渲染上），
客户端则在渲染路径之外成批解析输出，而且热路径全程无锁。

测试方法（每款终端怎么驱动、网格是否公平、有哪些坑）连同一键复现脚本，都放在
[`scripts/bench/`](scripts/bench/README.md)，欢迎自己跑一遍。

## 🚀 安装

到 [**Releases**](https://github.com/l0ng-ai/tty7/releases) 下载对应平台的构建：

- **macOS** —— `tty7-<version>-macos-arm64.dmg`（Apple Silicon）或 `…-x86_64.dmg`
  （Intel）；打开后把 `tty7.app` 拖进「应用程序」即可。
- **Windows** —— `…-windows-x86_64.zip`；解压后运行 `tty7.exe`。
- **Linux** —— `…-linux-x86_64.tar.gz`；解压后运行 `./tty7`（需要常见的
  x11/wayland 运行时库）。

## ⌨️ 快捷键

下表按 macOS 记法书写 —— 在 Windows 和 Linux 上，把 <kbd>⌘</kbd> 读作
<kbd>Ctrl</kbd>。按 <kbd>⌘ ,</kbd> 打开设置，可查看或重新映射全部键位。最常用的几个：

| | |
|---|---|
| <kbd>⌘ T</kbd> · <kbd>⌘ W</kbd> · <kbd>⌘ ⇧ T</kbd> | 新建标签页 · 关闭标签页 · 恢复关闭的标签页 |
| <kbd>⌘ D</kbd> · <kbd>⌘ ⇧ D</kbd> | 向右分屏 · 向下分屏 |
| <kbd>⌘ ]</kbd> · <kbd>⌘ [</kbd> | 下一个窗格 · 上一个窗格 |
| <kbd>⌘ ⏎</kbd> | 最大化 / 还原窗格 |
| <kbd>⌘ P</kbd> | 命令面板 |
| <kbd>⌘ F</kbd> | 搜索回滚缓冲区 |
| <kbd>⌃ R</kbd> | 反向搜索 shell 历史 |
| <kbd>⌘ +</kbd> · <kbd>⌘ −</kbd> · <kbd>⌘ 0</kbd> | 字号增大 · 减小 · 重置 |

完整列表（以及你改过的自定义键位）在 **Settings → Keybindings**。

## 💭 站在这些之上

- [gpui](https://github.com/zed-industries/zed) —— Zed 的 GPU 加速 UI 框架
- [`alacritty_terminal`](https://github.com/zed-industries/alacritty)（Zed 的 fork）—— VT 模拟器、网格与 PTY
- [gpui-component](https://github.com/longbridge/gpui-component) —— UI 组件，经由一个[固定版本的 fork](https://github.com/l0ng-ai/gpui-component/tree/tty7)
- [tmux](https://github.com/tmux/tmux) —— 常驻守护进程设计的灵感来源

## 🤝 参与贡献

欢迎提 bug 和 PR。安全问题请走 [SECURITY.md](SECURITY.md)；重要改动都记在
[CHANGELOG](CHANGELOG.md)。

## 📝 许可证

[Apache License 2.0](LICENSE) · © 2026 l0ng-ai

<br />

<div align="center">

<img src="assets/app-icon.svg" alt="" width="28" height="28" />

<sub><b>tty7</b> —— 纯 Rust 编写：GPU 渲染，守护进程托管。</sub>

</div>
