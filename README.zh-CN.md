<div align="center">

<img src="assets/app-icon.svg" alt="" width="96" height="96" />

<h1>tty7</h1>

**关掉窗口，终端还在。**

<sub>纯 Rust · GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/release/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-3FDD8C)](LICENSE)

<sub>[English](README.md) · 简体中文</sub>

<br />

<a href="assets/tour.mp4"><img src="assets/tour.webp" alt="tty7 一分钟演示：多仓库 agent、一个 agent 通过 CLI 驱动另一个、提示符编辑器、diff、拖动 pane、退出应用后会话仍在运行" width="900" /></a>

</div>

<br />

大部分终端把 shell 挂在窗口上。窗口一关，shell 就跟着死了。为了不让它死，大家又在终端里再套一层
tmux，学一套新的快捷键，写一份配置文件。这是在窗口的限制上再加一层绕路。

tty7 的做法很简单：shell 归后台的 server 管，窗口只负责显示。窗口关了，shell 照样活着。
下面这些特性，几乎都是从这一点自然推出来的，并不需要额外发明什么。

## 退出应用，会话不断

退出 tty7，所有 shell 原地接着跑，编译不会断。

重启电脑，进程当然没了，这是操作系统层面的事。但布局和屏幕上最后的内容会回来，
已支持的 agent 也会接上原来的对话。

不需要 tmux，不需要配置。

→ [退出、重启后分别保留什么](docs/getting-started/concepts.mdx)

## agent 指挥 agent

现在很多人同时开好几个 agent，在好几个仓库里干活，然后就在窗口之间来回切，看哪个跑完了，
哪个在等你回话。这种事不应该由人来做。

tty7 能认出 26 个编程 CLI（Claude Code、Codex、Gemini、Cursor、OpenCode 等），把它们的状态、
通知、分支和 diff 放在同一个侧边栏里。哪个需要你，一眼就知道。

再往前走一步：既然状态能用命令查到，那调度 agent 的就不一定是人，也可以是另一个 agent。
整个流程就四行，不需要任何框架，GUI 不开也能跑：

```sh
PANE=$(tty7 split --v)                                       # 给干活的 agent 开个 pane
tty7 send "$PANE" 'claude "add tests for the parser"' --enter
tty7 wait "$PANE" --until waiting,done --changed --timeout 600  # 等它做完，或卡住需要人
tty7 capture "$PANE" --plain                                 # 读取输出
```

tty7 不给 agent 套壳，也不做代理。你启动的就是原版 agent，跑在普通的
PTY 里。简单的东西才可靠。

→ [编排 agent](docs/agents/orchestration.mdx) ·
[agent skill](skills/tty7/SKILL.md) · [完整支持矩阵](#支持的-agent)

## 远程机器也一样

远程工作区就是在远端跑同一个 server，本地只负责显示。标签页、pane、文件树、git、diff
都在远端，文件不用同步。换台电脑连上去，还是你离开时的样子。

这跟本地其实是同一件事，只不过 server 换了个地方。

SSH 是 tty7 自己用 Rust 实现的，不依赖系统的 ssh。profile、keychain、跳板机、SFTP、端口自动转发都有。
远端的 `tty7-server` 装一次就好，不需要 root。

→ [远程工作区](docs/remote/workspaces.mdx)

## 性能

性能不是 tty7 的卖点，但也不能是短板。`cat` 一个 11 MB 的文件，tty7 用 95 ms，第二名 179 ms。
DOOM-fire 跑到 888 fps，第二名 617 fps。测试方法和脚本都公开，一条命令就能复现。

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 I/O：`cat` 一个 11 MB 文件 <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>Apple M1 Pro，macOS 26.3.1，155×40 网格，五次取平均，2026-07-04。¹ GUI 占 105 MB，常驻 server 占 11 MB。
测试方法与一条命令复现：[`scripts/bench/`](scripts/bench/README.md)。</sub>

## 其他功能

剩下的都是细节，但细节做对了，用起来就是顺手。提示符有历史建议、带说明的 Tab 补全、语法高亮、
多行编辑，<kbd>⌃ R</kbd> 模糊搜索历史。<kbd>⌘ P</kbd> 随处搜索，<kbd>⌘ J</kbd> 打开侧边面板，
看进程、端口、文件、改动，还有 GitHub 的 issue 和 PR。shell 集成覆盖 zsh、bash、fish、PowerShell
和 WSL，什么都不用装。

→ [完整文档](docs/)（英文）· [快捷键](docs/reference/keyboard-shortcuts.mdx) ·
[config.json](docs/reference/configuration.mdx) · [CLI 参考](docs/cli/reference.mdx)

## 安装

到 [**Releases**](https://github.com/l0ng-ai/tty7/releases/latest) 下载对应平台的安装包：

| | |
|---|---|
| **macOS** | `.dmg`，Apple 芯片和 Intel 各一版，拖进「应用程序」即可 |
| **Windows** | `-setup.exe`，或免安装的 `.zip` |
| **Linux** | `.AppImage`，`chmod +x` 后直接运行，X11/Wayland 依赖已打包 |

给 agent 装上 tty7 的 skill：

```sh
npx skills add l0ng-ai/tty7    # 安装
npx skills update tty7         # 后续更新
```

## 支持的 agent

**识别**无需配置：品牌头像、分支与 diff、标签页标题。
**状态**要在「设置 → 集成」里为对应 agent 装一个 hook（点一下就行）。装好后才有状态点、通知、托盘提醒、`tty7 wait`，以及重启后恢复会话。
**Fork** 需要两个条件：agent 本身有 fork 命令，并且装了 hook，这样 tty7 才知道要 fork 哪个会话。
**历史会话**直接读 agent 自己的历史文件，列在随处搜索里，回车即可恢复。

<details>
<summary>完整支持矩阵</summary>

| Agent | 识别 | 状态 · 重启恢复 | Fork | 历史会话 |
|---|:-:|:-:|:-:|:-:|
| **Claude Code** | ✓ | ✓ | ✓ | ✓ |
| **Codex** | ✓ | ✓ | ✓ | ✓ |
| **TraeCode** | ✓ | ✓ | ✓ | |
| **Grok** | ✓ | ✓ | ✓ | |
| **OpenCode** | ✓ | ✓ | ✓ | ✓ |
| **Oh My Pi** | ✓ | ✓ | ✓ | ✓ |
| **Prime Agent** | ✓ | ✓ | ✓ | |
| **Droid** | ✓ | ✓ | ✓ | ✓ |
| **Qwen Code** | ✓ | ✓ | ✓ | ✓ |
| **Goose** | ✓ | ✓ | ✓ | |
| **Qoder CLI** | ✓ | ✓ | ✓ | ✓ |
| **Qoder CN CLI** | ✓ | ✓ | ✓ | ✓ |
| **CodeBuddy** | ✓ | ✓ | ✓ | ✓ |
| **Gemini** | ✓ | ✓ | | ✓ |
| **Copilot** | ✓ | ✓ | | ✓ |
| **Kimi Code** | ✓ | ✓ | | ✓ |
| **Pi** | ✓ | ✓ | | ✓ |
| **Crush** | ✓ | ✓ | | |
| **Antigravity** | ✓ | ✓ | | |
| **Cursor** | ✓ | ✓ | | ✓ |
| Aider | ✓ | | | |
| Amp | ✓ | | | |
| Auggie | ✓ | | | |
| Hermes | ✓ | | | |
| Vibe | ✓ | | | |
| Empryo | ✓ | | | |

</details>

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 和 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
