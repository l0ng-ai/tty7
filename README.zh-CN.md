<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**高性能终端：常驻会话、远程工作、agent。**

<sub>纯 Rust · GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-%E5%8A%A0%E5%85%A5%E7%BE%A4%E7%BB%84-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>[English](README.md) · 简体中文</sub>

</div>

## 为什么

- **性能** —— 吞吐约为 Alacritty、Ghostty、Kitty 的 2 倍（[基准测试](#基准测试)）
- **持久会话** —— 退出应用、重启机器后，shell 和已支持的 agent 会话照样运行；无需 tmux
- **编辑器级输入** —— 建议、补全、语法高亮、历史搜索
- **远程开发** —— 文件、仓库、pane 和 git 信息都留在远端机器上
- **原生 SSH** —— profile、SFTP、端口转发和跳板机
- **Agent-aware** —— Claude Code、Codex 等：状态、通知、git 上下文
- **CLI + Skills** —— agent 创建 pane、运行命令、读取输出

## 安装

三平台原生构建都在 [**Releases**](https://github.com/l0ng-ai/tty7/releases)：

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | 拖进「应用程序」 |
| **Windows** | `…-setup.exe` · 便携版 `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` 直接运行，X11/Wayland 依赖已打包 |

## 有什么

| | |
|---|---|
| **编辑器级输入** | 历史影子建议 · 带说明的 Tab 补全 · 语法高亮 · 多行编辑 · 点击定位光标 · <kbd>⌃ R</kbd> 模糊历史搜索 |
| **窗口** | 标签页与分屏 · <kbd>⌘ P</kbd> 命令面板 · <kbd>⌘ F</kbd> 回滚搜索 · 9 套主题 · 输入法 |
| **Agent-aware** | 按 pane 识别约 17 个 CLI agent：状态点 · 通知 · 分支 + diff · 重启后续上会话 · 托盘图标提醒需要输入 |
| **远程工作区** | 远端文件、仓库、Changes、diff、worktree、标签页和 pane · 任意客户端重连后原地继续 |
| **CLI + Skills** | 安装包自带 `tty7` CLI · [agent skill](skills/tty7/SKILL.md) · pane/工作区控制 · 真实 PTY 命令 · 输出、进程、端口和 agent 状态 |
| **SSH** | 原生 russh 栈：profile 凭据进 keychain · SFTP 面板 · 端口转发 · 跳板机 · 一次无 sudo 安装 `tty7-server` |

终端和快捷键参考：[docs/features.zh-CN.md](docs/features.zh-CN.md)。面向 agent 的 CLI 接口见
[skills/tty7/SKILL.md](skills/tty7/SKILL.md)。

## 基准测试

同一台机器、同一天、统一 155×40 网格 —— Apple M1 Pro，macOS 26.3.1，
取五次运行的平均值（2026-07-04）：

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 I/O —— 11 MB `cat` <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + 常驻 server 11 MB。</sub>

测试方法与一键复现脚本：[`scripts/bench/`](scripts/bench/README.md)。

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 与 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
