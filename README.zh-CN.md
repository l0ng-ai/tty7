<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**终端工作台：会话常驻、远程开发、原生支持 agent。**

<sub>纯 Rust · GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/release/l0ng-ai/tty7?label=version&color=3FDD8C)](https://github.com/l0ng-ai/tty7/releases)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-blue)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-%E5%8A%A0%E5%85%A5%E8%AE%A8%E8%AE%BA-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>[English](README.md) · 简体中文</sub>

<br />

<img src="assets/hero.webp" alt="tty7 侧边栏列出多个仓库的 agent 会话，右侧运行 Claude Code" width="900" />

</div>

## 为什么

真正持有 shell 和 pane 的是后台常驻的 server，不是窗口。下面这些几乎都是这一个决定的结果。

- **性能**：吞吐是 Alacritty、Ghostty、Kitty 的两倍左右（[基准测试](#基准测试)）
- **会话常驻**：退出应用或重启机器后，shell 和已支持的 agent 会话继续运行，不需要 tmux
- **Agent 感知**：Claude Code、Codex 等 agent 的状态、通知和 git 上下文，多个仓库一屏看完
- **可被 agent 驱动**：一个 agent 能给另一个开 pane、派活、等它跑完、读走结果，GUI 开不开都行
- **编辑器级输入**：建议、补全、高亮、历史搜索，不用装任何插件
- **远程开发**：文件、仓库、pane 和 git 信息都留在远端机器上，走自带的 SSH 栈
- **Git 就在终端旁边**：源代码管理、diff、worktree，不用切出窗口

## 安装

macOS、Windows、Linux 的原生构建都在 [**Releases**](https://github.com/l0ng-ai/tty7/releases)：

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | 拖进「应用程序」 |
| **Windows** | `…-setup.exe` · 免安装 `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` 后直接运行，X11/Wayland 的库已打包在内 |

## 有什么

| | |
|---|---|
| **Agent 感知** | 逐 pane 识别 19 个 CLI agent · 状态点 · 通知 · 分支 + diff · 需要输入时托盘图标提醒 · 重启后续上会话 · 侧边栏按仓库分组 |
| **CLI + Skills** | 安装包自带 `tty7` CLI · [agent skill](skills/tty7/SKILL.md) · `run` 转发命令输出并原样返回退出码 · `split` · `send` · `wait --until free` · `capture` |
| **编辑器级输入** | 从历史推出影子建议 · Tab 补全附带说明 · 语法高亮 · 多行编辑 · 点击定位光标 · <kbd>⌃ R</kbd> 模糊搜索历史 |
| **窗口** | 标签页与分屏 · <kbd>⌘ P</kbd> 命令面板 · <kbd>⌘ F</kbd> 回滚搜索 · <kbd>⌘ J</kbd> 侧栏列出进程树和监听端口 · 13 套主题，也能写自己的 YAML 或导入 iTerm2 配色 · 输入法 |
| **Shell 集成** | pane 启动时自动注入，不用你装什么 · 提示符边界 · 工作目录 · 退出码 · 命令跑完发通知 · 覆盖 zsh、bash、fish、PowerShell、WSL 和远程 pane |
| **远程工作区** | 远端的文件、仓库、改动、diff、worktree、标签页和 pane · 从任意客户端重连，接着离开时的位置继续 |
| **SSH** | 自带 russh 实现，不依赖外部 ssh：profile 凭据存入 keychain · SFTP 面板 · 端口转发 · 跳板机 · `tty7-server` 只需安装一次，无需 root |
| **Git** | 源代码管理面板跟着焦点 pane 走 · 暂存、提交、amend、切分支、push、stash · 双栏或统一 diff · 提交图谱支持 cherry-pick、revert、reset · 新建 worktree 连同它的标签页 |

## 支持的 agent

**识别**无需配置：品牌头像、分支与 diff、标签页标题。
**状态**需要在设置 → Agents 中为该 agent 安装 hook，一次点击，之后才有状态点、通知、托盘提醒、`tty7 wait` 和重启后恢复会话。
**Fork** 两个条件都要：agent 自己提供 fork 命令，且 hook 已装——tty7 得知道 fork 的是哪个会话。

<details>
<summary>19 个 agent 的完整支持矩阵</summary>

| Agent | 识别 | 状态 · 重启恢复 | Fork |
|---|:-:|:-:|:-:|
| **Claude Code** | ✓ | ✓ | ✓ |
| **Codex** | ✓ | ✓ | ✓ |
| **Grok** | ✓ | ✓ | ✓ |
| **OpenCode** | ✓ | ✓ | ✓ |
| **Oh My Pi** | ✓ | ✓ | ✓ |
| **Droid** | ✓ | ✓ | ✓ |
| **Qwen Code** | ✓ | ✓ | ✓ |
| **Goose** | ✓ | ✓ | ✓ |
| **Gemini** | ✓ | ✓ | |
| **Copilot** | ✓ | ✓ | |
| **Kimi Code** | ✓ | ✓ | |
| **Pi** | ✓ | ✓ | |
| Aider | ✓ | | |
| Amp | ✓ | | |
| Cursor | ✓ | | |
| Auggie | ✓ | | |
| Hermes | ✓ | | |
| Vibe | ✓ | | |
| Antigravity | ✓ | | |

</details>

tty7 不包装、不代理其中任何一个 —— 你启动的就是那个 agent 本身，运行在普通 PTY 中，界面仍然是它自己的。
如果你通过 wrapper 脚本启动 agent，在 `config.json` 的 `agent_commands` 里把脚本名映射到对应 agent 即可。

## 文档

完整文档在 [**`docs/`**](docs/)，英文：
[快捷键](docs/reference/keyboard-shortcuts.mdx) ·
[config.json](docs/reference/configuration.mdx) ·
[CLI 参考](docs/cli/reference.mdx)。
agent 如何调用这套 CLI，另见 [skills/tty7/SKILL.md](skills/tty7/SKILL.md)。

安装 skill：

```sh
npx skills add l0ng-ai/tty7    # 安装
npx skills update tty7         # 后续更新
```

## 基准测试

同一台机器、同一天、同样的 155×40 网格：Apple M1 Pro，macOS 26.3.1，每项运行五次取平均（2026-07-04）。

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 I/O：`cat` 一个 11 MB 文件 <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 占 105 MB，常驻 server 占 11 MB。</sub>

测试方法与一条命令复现：[`scripts/bench/`](scripts/bench/README.md)。

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 和 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
