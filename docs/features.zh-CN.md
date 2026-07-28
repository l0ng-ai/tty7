# 功能

<sub>[English](features.md) · 简体中文</sub>

## 输入

- **影子建议** —— 边打字边用你的历史补全整条命令，<kbd>→</kbd> 接受
- **带说明的 Tab 补全** —— 每个 flag、每个子命令都带说明，覆盖约 100 个常用命令；tty7 没有候选时 Tab 自动交给 shell 自己的补全，整个功能也可关闭（设置 → 终端 → 键盘，或 `config.json` 里的 `tab_completion`）
- **语法高亮** —— 边打边亮，什么都不用装
- **模糊历史搜索** —— <kbd>⌃ R</kbd> 看到每条命令在哪跑的、什么时候、有没有失败；关掉它（设置 → 终端 → 键盘，或 `config.json` 里的 `history_search`）后 <kbd>⌃ R</kbd> 直接交给 shell，你绑的 fzf / percol 照常可用
- **历史开箱即用** —— 你已有的 shell 历史直接生效，并跨会话延续
- **行编辑** —— 点击定位光标、鼠标选区、词级移动、撤销
- **多行编辑** —— 折行和多行命令原地编辑；网格自动上移，光标始终可见。<kbd>⇧ ⏎</kbd> · <kbd>⌥ ⏎</kbd> 插入换行而不提交（可改绑，动作名 `InsertNewline`），单独按 <kbd>⏎</kbd> 提交整个缓冲区

## 窗口

- **标签页与分屏** —— 永远开在当前目录
- **侧栏按仓库分组** —— 左侧标签栏按 git 仓库分组、每组一个标题行，不在仓库里的标签归入末尾的 *Scratch* 组；切分支、仓库内 `cd` 都不会挪动行（`config.json` 的 `sidebar_grouping`：默认 `repo`，`none` 恢复扁平列表）
- **命令面板** <kbd>⌘ P</kbd> · 回滚搜索 <kbd>⌘ F</kbd>
- **⌘ 点击打开链接** · 桌面通知 · 划选即复制（可选，设置 → 终端 → 剪贴板）
- **智能双击选中** —— 双击直接选中整条 URL、文件路径、括号/引号对，中文按词典分词出词；Shift 点击扩展选区（设置 → 终端 → 鼠标可开关；分隔符用 `config.json` 的 `word_separators` 配置）
- **9 套主题，也能自定义** — YAML 种子主题，背景支持纯色、渐变或图片；可导入 iTerm2 `.itermcolors`；应用内颜色编辑器带背景图选择
- **跟随系统外观** — 设置 → Appearance；分别选好浅色和深色主题，tty7 随系统深浅模式实时切换（`config.json` 中的 `theme_follow_system`、`theme_preset_light` / `theme_preset_dark`）
- **窗口透明与模糊** — 设置 → Appearance → Window；对所有主题生效，*Follow theme* 恢复主题自带的 `opacity` / `blur`
- **CJK / 输入法输入**

## 字体

- **内置 Hack** —— 打包进二进制，默认配置在各平台渲染完全一致，不依赖系统安装
- **主字体 + 有序 fallback** —— `config.json` 里的 `font_family` 和 `font_fallbacks`；可选 `font_family_bold` / `font_family_italic` 指定独立字面，`font_features` 透传 OpenType 特性（上下文连字默认关闭）
- **默认列表按平台分支** —— fallback 只写宿主系统真正自带的字体（macOS 用 PingFang SC / Apple Color Emoji，Windows 用 Microsoft YaHei / Segoe UI Emoji，Linux 用 Noto）。这些名字也会追加到你手写的列表后面，所以在别的平台写出来的 `config.json` 一样能落地

### 中文与两列网格

一个格子等于主字体的一个 advance，宽字符（CJK）被钉死在正好两格上。所以中文
fallback 只有在**汉字 advance 等于主字体西文 advance 的两倍**时，才能严丝合缝地
填满自己的槽。

内置 Hack 的 advance 是 0.60205em，两格就是 1.2041em —— 而系统自带的中文字体
（Microsoft YaHei、PingFang SC、Noto Sans CJK）全都是 1.0em。这些字形在槽里左
对齐，多出来的约 0.2em 就变成每个字右边的一道空隙。

[Maple Mono NF CN](https://github.com/subframe7536/maple-font) 在所有平台都排在
第一位正是因为这个 —— 西文 0.6em、中文 1.2em，对上 Hack 正好两格。它只按名字引
用，不打包（每字重约 20MB）：装上即生效，不用改配置。

想让中文排得**紧**而不只是均匀，要换的是主字体：选一个 advance 为 0.5em 的
（比如 Sarasa Mono SC 更纱黑体等宽），两格就正好 1.0em。

## Coding agent

tty7 能识别 pane 里跑着的第三方 coding agent（Claude Code、Codex、Gemini CLI、
Aider、Amp、OpenCode 等约 17 个）并在其外围加功能 —— 绝不包裹或替代 agent 本身。

- **品牌头像** —— 标签 chip / 侧栏行显示每个 pane 跑的是哪个 agent；自定义包装命令可通过 `config.json` 的 `agent_commands` 映射
- **状态点** —— 工作中（蓝）/ 等你输入（琥珀）/ 完成（绿），由 agent 自己上报的 OSC 事件驱动；在 设置 → Agents 一键装好对应 hooks（Claude Code、Codex、Copilot CLI、OpenCode、Pi、Grok Build）
- **通知** —— agent 卡在等你批准的那一刻弹 "needs your permission…"，每轮结束弹 "finished after Ns"，遵循你的通知策略
- **一眼看分支** —— 侧栏每行显示该 pane 的 git 分支和工作区改动（`+N −M`），`cd` 或命令跑完时自动刷新；点改动数字会打开 diff 浮层，关掉它（设置 → 窗口与标签，或 `config.json` 的 `sidebar_diff_preview: false`）分支和数字照常显示，只是不再可点
- **会话恢复** —— 重启后无法重连的 pane 会自动续上 agent 对话，并带上原始启动 flags（`claude --dangerously-skip-permissions --resume …`；`restore_agent_sessions`，默认开启）
- **Fork 会话** —— 直接调 agent 自己的 fork 命令（`codex fork <id>`、`claude --resume <id> --fork-session`，OpenCode 和 Grok Build 同样支持），把当前对话分叉成一个独立会话；原会话原封不动，两边各自往下走。在 pane 上右键可选择分屏位置，在标签 / 侧栏行上右键则直接开新标签。需要先装好该 agent 的 hooks（fork 认的是 hooks 上报的 session id）；远程 pane 不能 fork，因为命令会跑在本机的 agent 上；另外 fork 会整份复制对话历史，反复 fork 会在 agent 自己的会话目录里占掉不少磁盘
- **复制 Session ID** —— 把 agent 的原生 session id 复制到剪贴板，就在 *Copy Working Directory* 旁边，方便粘进 `codex resume`、bug 报告或别的工具
- **上下文回填** —— 面板命令把当前选区或仓库 `git diff` 打包成 prompt 直接喂给正在跑的 agent
- **托盘图标** —— 系统托盘 / 菜单栏常驻图标，任何 agent 等你输入时立即切换为提醒态；菜单列出所有 agent pane（品牌头像 + 状态点，点击直达）、可切换通知策略，并在保留会话的普通退出之外提供 *Quit and Stop Daemon*（`show_tray_icon`，默认开启）

## SSH

**唯一**路径就是原生 Rust SSH 栈（russh）—— profile、凭据、SFTP 全部内置，
不 shell 出 `ssh`，也没有系统 ssh 兼容模式。

- **QuickConnect** —— 面板里打 `user@host[:port]` 回车即连；支持 IPv6 `[::1]:port`
- **保存 profile** —— 完整连接配置，密码 / passphrase 进 OS keychain，不落盘
- **`~/.ssh/config` alias** —— 直接输入 alias 即连（原生解析常用字段，尽力而为，走 russh），也可在设置页一键导入为 profile
- **GUI 认证** —— pane 内 sheet 输入密码、私钥 passphrase、2FA，并确认主机密钥（新主机 vs 已变更）
- **内置 SFTP** —— 滑入式文件面板：浏览、上传 / 下载、重命名 / 删除 / chmod，可拖进 Finder
- **端口转发** —— Local / Remote / Dynamic，预配置或运行时增删，外加 ⌘ 点击 `localhost:PORT` 一键转发
- **跳板与代理** —— 经 profile 引用或 `ProxyJump` 多跳、ProxyCommand、SOCKS5 / HTTP

| 入口 | 连接方式 |
|---|---|
| 保存 profile · QuickConnect · 输入 `user@host[:port]` | 原生 russh —— SFTP · keychain · GUI 认证 · L/R/D 转发 |
| `~/.ssh/config` alias | 原生解析后走 russh（`Match`/canonicalize/GSSAPI 不支持，且无回退） |

## 快捷键

下表按 macOS 记法书写 —— 在 Windows 和 Linux 上，把 <kbd>⌘</kbd> 读作
<kbd>Ctrl</kbd>。最常用的几个：

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

**Settings → Keybindings**（<kbd>⌘ ,</kbd>）列出全部快捷键。点一行、按下新键即可
（<kbd>Esc</kbd> 取消，<kbd>Backspace</kbd> 恢复默认），改完立即生效。窗格缩放与
交换默认不绑定键 —— 在这里绑定，或从命令面板执行。

**tmux 预设** —— 把窗格/标签页操作映射到前缀键（默认 <kbd>⌃ B</kbd>）：
<kbd>⌃ B</kbd> <kbd>C</kbd> 新建标签页，<kbd>⌃ B</kbd> <kbd>%</kbd> 分屏，
<kbd>⌃ B</kbd> 接方向键切换焦点。单独按前缀键会在短暂延迟后送达 shell，
`前缀` + 未绑定的键原样透传给终端。

## 性能说明

- 以设备速度读取 PTY，在渲染路径之外成批解析
- 热路径全程无锁 —— 再大的 `cat` 也不会阻塞在渲染上
- 触发背压前，守护进程最多可领先窗口缓冲 16 MiB
