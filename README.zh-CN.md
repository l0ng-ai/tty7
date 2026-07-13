<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**纯 Rust 编写的 GPU 渲染终端。**

<sub>GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=ff8a5c)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-%E5%8A%A0%E5%85%A5%E7%BE%A4%E7%BB%84-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

[**为什么是 tty7**](#为什么是-tty7) · [**安装**](#安装) · [**功能**](#功能) · [**基准测试**](#基准测试) · [**快捷键**](#快捷键)

<sub>[English](README.md) · 简体中文</sub>

</div>

## 为什么是 tty7

- **快** —— 同一台机器上，吞吐约为 Alacritty、Ghostty、Kitty 的
  2 倍（[基准测试](#基准测试)）
- **现代的提示符** —— 补全、语法高亮、历史搜索全部内置，不用东拼西凑插件
- **会话不死** —— 关窗口、退应用，shell 照样运行；无需 tmux
- **零配置** —— zsh、bash、fish、PowerShell 开箱即用

macOS、Windows、Linux 三平台原生构建，每个 release 一起打出。

## 安装

到 [**Releases**](https://github.com/l0ng-ai/tty7/releases) 下载对应平台的构建：

- **macOS** —— `tty7-<version>-macos-arm64.dmg`（Apple Silicon）或 `…-x86_64.dmg`
  （Intel）；打开后把 `tty7.app` 拖进「应用程序」即可。
- **Windows** —— `…-windows-x86_64-setup.exe`（安装包：带开始菜单快捷方式和
  卸载入口），或 `…-windows-x86_64.zip`（便携版：解压后运行 `tty7.exe`）。
- **Linux** —— `…-linux-x86_64.AppImage`(推荐:已打包 x11/wayland 依赖库,
  Fedora / Arch 等发行版免装依赖,`chmod +x` 后直接运行),或
  `…-linux-x86_64.tar.gz`(裸二进制,解压后运行 `./tty7`,需自行装齐常见的
  x11/wayland 运行时库)。

## 功能

### 提示符

- **影子建议** —— 边打字边用你的历史补全整条命令，<kbd>→</kbd> 接受
- **会解释的 Tab 补全** —— 每个 flag、每个子命令都带说明，覆盖约 100 个常用命令
- **语法高亮** —— 边打边亮，什么都不用装
- **模糊历史搜索** —— <kbd>⌃ R</kbd> 看到每条命令在哪跑的、什么时候、有没有失败
- **历史开箱即用** —— 你已有的 shell 历史直接生效，并跨会话延续
- **真正的行编辑** —— 选区、词级移动、撤销

### 窗口

- **标签页与分屏** —— 永远开在当前目录
- **命令面板** <kbd>⌘ P</kbd> · 回滚搜索 <kbd>⌘ F</kbd>
- **⌘ 点击打开链接** · 桌面通知
- **8 套主题** · CJK / 输入法输入

## 基准测试

四款终端在同一台机器上依次测完，网格统一为 155×40 —— Apple M1 Pro，
macOS 26.3.1，取五次运行的平均值（2026-07-04）：

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 IO —— 11 MB `cat` <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + 常驻守护进程 11 MB。</sub>

速度从哪来：

- 以设备速度读取 PTY，在渲染路径之外成批解析
- 热路径全程无锁 —— 再大的 `cat` 也不会阻塞在渲染上
- 触发背压前，守护进程最多可领先窗口缓冲 16 MiB

测试方法（每款终端怎么驱动、网格是否公平、有哪些坑）连同一键复现脚本，都放在
[`scripts/bench/`](scripts/bench/README.md)，欢迎自己跑一遍。

## 快捷键

下表按 macOS 记法书写 —— 在 Windows 和 Linux 上，把 <kbd>⌘</kbd> 读作
<kbd>Ctrl</kbd>。按 <kbd>⌘ ,</kbd> 打开设置，可查看或重新映射全部键位。最常用的几个：

| | |
|---|---|
| <kbd>⌘ T</kbd> · <kbd>⌘ W</kbd> · <kbd>⌘ ⇧ T</kbd> | 新建标签页 · 关闭标签页 · 恢复关闭的标签页 |
| <kbd>⌘ 1</kbd>…<kbd>⌘ 9</kbd> · <kbd>⌃ ⇥</kbd> · <kbd>⌃ ⇧ ⇥</kbd> | 跳到第 1–9 个标签页 · 下一个 · 上一个标签页 |
| <kbd>⌘ D</kbd> · <kbd>⌘ ⇧ D</kbd> | 向右分屏 · 向下分屏 |
| <kbd>⌘ ]</kbd> · <kbd>⌘ [</kbd> | 下一个窗格 · 上一个窗格 |
| <kbd>⌘ ⌥ ←→↑↓</kbd> | 按方向切换焦点窗格 |
| <kbd>⌘ ⏎</kbd> · <kbd>⌘ ⇧ ⏎</kbd> | 切换全屏 · 最大化 / 还原窗格 |
| <kbd>⌘ K</kbd> | 清屏并清空回滚缓冲区 |
| <kbd>⌘ P</kbd> | 命令面板 |
| <kbd>⌘ F</kbd> | 搜索回滚缓冲区 |
| <kbd>⌃ R</kbd> | 模糊搜索 shell 历史 |
| <kbd>⌘ +</kbd> · <kbd>⌘ −</kbd> · <kbd>⌘ 0</kbd> | 字号增大 · 减小 · 重置 |

**Settings → Keybindings** 列出全部快捷键。点一行、按下新键即可（<kbd>Esc</kbd>
取消，<kbd>Backspace</kbd> 恢复默认），改完立即生效。窗格缩放与交换默认不绑定键 ——
在这里绑定，或从命令面板执行。习惯 tmux？打开 **tmux** 预设，把窗格/标签页操作
映射到前缀键（默认 <kbd>⌃ B</kbd>）：<kbd>⌃ B</kbd> <kbd>C</kbd> 新建标签页，
<kbd>⌃ B</kbd> <kbd>%</kbd> 分屏，<kbd>⌃ B</kbd> 接方向键切换焦点。单独按前缀键会在
短暂延迟后送达 shell，`前缀` + 未绑定的键会原样透传给终端。

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 与 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
