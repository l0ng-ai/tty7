# Plan — issue #61「tmux like input model」落地方案

> Issue:<https://github.com/l0ng-ai/tty7/issues/61>
> 方向已在 issue 评论中定调:**不解析 `~/.tmux.conf`**、不改默认零配置体验;分三步 —— ① Settings 内直接改快捷键 → ② 补齐 pane/tab 操作 → ③ 一键 tmux preset。
> 本文把三步落成三个可独立发版的 PR,含文件级落点、验收标准与已核实的框架事实。

## 0. 已核实的技术事实(方案成立的前提)

| 事实 | 出处 | 对方案的意义 |
|---|---|---|
| gpui 原生支持多击序列绑定 + pending 回放:`ctrl-b n` 这类绑定会把 `ctrl-b` 置为 pending,1s 超时后 flush 并**回放**给下层(终端能收到);后继键未命中绑定时两键都回放 | zed rev `1d217ee` `gpui/src/window.rs:4744-4806` | tmux prefix 不需要自造输入模型;裸 `ctrl-b` 到 shell 有 1s 延迟(可接受,需文案说明) |
| `App::bind_keys` 后绑定优先;有 `NoAction`/`Unbind` 空绑定可压掉旧键 | `gpui/src/action.rs:4`、`gpui/src/keymap.rs:66,196` | 热重绑可以**只追加**:NoAction 压旧键 + 追加新表,不必 clear |
| `cx.clear_key_bindings()` 清的是**全局** keymap | `gpui/src/app.rs:2011` | 禁用——会连 gpui-component 的 input/list/menu 绑定一起清掉(`gpui-component …/src/{input/state.rs:133, list/list.rs:28, root.rs:25} …`) |
| `App::intercept_keystrokes` 在 keymap 派发**之前**执行,可吞键 | `gpui/src/app.rs:1982`、`gpui/src/window.rs:4742` | 录键控件的实现基础:录键时按 ⌘T 必须被录下而不是触发 NewTab |
| config 里的 keystroke 值**今天就接受序列**(按空白拆 chord 逐个 `Keystroke::parse`) | `src/ui/keymap.rs:168`(`keystroke_is_valid`) | preset 只是数据;但 `key_tokens`(`keymap.rs:105`)不会渲染序列,Settings 列表显示有缺口,需补 |
| ⌘/Ctrl+1-9 切标签是裸 `on_key_down` 硬编码,不是 action | `src/ui/app.rs:1213-1230` | 目前不可重绑、preset 也够不着;需迁移成正式 action |
| defaults→overrides 合并逻辑写了两份 | `src/ui/keymap.rs:19-29`(init)与 `src/ui/settings.rs:1302-1312`(渲染) | 先收敛成单一 `effective_bindings()`,PR 3 还要在中间插 preset 层 |
| pane 树是纯二叉 split 树,leaf 泛型可用 `Pane<u32>` 纯值测试;ratio 夹在 0.1..0.9 | `src/ui/pane.rs:25,57,404` | 方向聚焦/resize/swap 全部可做成纯树算法 + 纯值单测 |

## 1. PR 1 — Settings 内直接改快捷键(录键 + 即时生效)

**目标**:Keybindings 页从只读变可编辑;改完立即生效并 `Config::save()` 写回 `config.json`;删掉 "restart to apply" 文案。config 格式不变(`keybindings: action名 → keystroke`,`core/config.rs:49`)。

### 落点

- **`ui/keymap.rs` — 抽 `effective_bindings(cx) -> Vec<(String, String)>`**
  - defaults → user overrides 的合并收敛为单一来源;`init`、`effective_key`、Settings 渲染全部改走它。
- **`ui/keymap.rs` — 新增 `rebind(cx)`(热更新,不 clear)**
  - 记录上一份生效表(存 gpui global 或 keymap.rs 内部 static);对其中每个被改动/删除的 keystroke 追加一条 `KeyBinding::new(key, NoAction, ctx)` 压掉旧绑定,再整体追加新的生效表(后绑定优先,新表在 NoAction 之后追加即可复用同一个键)。
  - keymap 仅随「用户编辑次数」线性增长,可接受;`secondary-+` 别名与 `tab`/`shift-tab` 的 Terminal 特绑(`keymap.rs:44-51`)在 rebind 中原样保留。
- **`ui/settings.rs` — Keybindings 行内编辑**
  - 点击行 → 该行进入录键态(显示 "Press new shortcut… / Esc to cancel");录键期间挂 `cx.intercept_keystrokes` 吞键并记录;Esc 取消,Backspace/Delete = 清除 override 回默认。
  - 归一化:录到的平台主修饰键(mac ⌘ / 其它 Ctrl)写成 `secondary`,保持 config 跨平台;其余照实写。
  - **冲突检测**:新键与生效表其他 action 相同 → 行内警告 + 「覆盖并解绑原 action」二次确认(原 action 写入 NoAction 语义 = override 为空?→ 直接把原 action 的 override 设为被让出,UI 显示 "—")。
  - 每行 reset 按钮 + 顶部 "Restore all defaults"(清空 `config.keybindings`)。
  - 持久化路径:`cx.update_global::<Config>` → `Config::save()`(`core/config.rs:334`,原子写已就绪)→ `keymap::rebind(cx)` → `cx.notify()`。
- **顺手修显示缺口**:`key_tokens` 支持空白分隔序列,渲染成两组 keycap(`⌃B` `N`);Settings/palette 的 hint 同步受益(palette 走 `effective_key`,已动态读 config,无需改)。

### 验收 & 测试

- [ ] 改键后不重启:新键立即触发、旧键立即失效;写盘后重启行为一致。
- [ ] 录键时按 ⌘T/⌘W 等被录下而非触发原 action;Esc 退出录键态无副作用。
- [ ] 单测:override 合并优先级;NoAction 压键后同键重绑生效;录键 `Keystroke` → spec 字符串 → `Keystroke::parse` 往返一致;`key_tokens` 序列渲染。

## 2. PR 2 — 补齐 pane / tab 操作(tmux 用户依赖的缺口)

**目标**:方向聚焦 / resize / swap / 相邻及编号切 tab 全部成为正式 action(可绑键、进命令面板、被 preset 引用)。

### 新 action 与实现要点

| Action(`core/actions.rs`) | 实现(落点) | 默认键 |
|---|---|---|
| `FocusPaneLeft/Right/Up/Down` | `pane.rs` 纯树算法:根 = 单位矩形,按 split 轴 + ratio 递归算每个 leaf 的归一化 rect;从焦点 leaf 沿方向找「边缘最近、垂直轴重叠最大」的 leaf(tmux 语义) | `secondary-alt-left/right/up/down` |
| `ResizePaneLeft/Right/Up/Down` | 找包含焦点 leaf 且轴向匹配的**最近祖先 split**,按焦点在 `a`/`b` 侧调 ratio ±0.05(沿用 `MIN/MAX_RATIO` 夹紧,`pane.rs:17`);结构变更后 `save_session` | 不绑(palette + 自行绑) |
| `SwapPaneNext/Prev` | 交换两个 leaf 的 payload(`Entity<TerminalView>`),焦点跟随;`maximized` 置 None | 不绑 |
| `NextTab` / `PrevTab` | `app.rs` `activate(active±1 mod len)`;目前相邻切 tab 完全没有 action | `ctrl-tab` / `ctrl-shift-tab`(需验证与系统/终端冲突,冲突则不绑) |
| `ActivateTab1..9`(9 个 unit action) | 迁移 `app.rs:1213-1230` 硬编码逻辑 → keymap 正式绑定,行为不变;删掉裸 `on_key_down` 分支 | `secondary-1..9` |

- unit action 而非带参 action:`make_binding`(`keymap.rs:181`)是「字符串 action 名 → 绑定」的模型,config/Settings 面板全按名字索引,9 个 unit struct 最贴合。
- **palette**:`ui/palette.rs` 的 `CommandKind` 补全部新 action,未绑键的操作至少可从面板执行。
- **Settings 列表**:`default_bindings()` 表加新行;未绑默认键的 action 显示 "—",靠 PR 1 的编辑器绑定。
- **文档**:README Keybindings 表 + zh-CN 同步更新。

### 验收 & 测试

- [ ] `Pane<u32>` 纯值单测:rect 计算(嵌套 split + 非 0.5 ratio)、方向查找(含并列取重叠最大)、resize 祖先选择(焦点在 `a`/`b` 两侧、无匹配轴时 no-op)、swap 后 `leaves()` 顺序。
- [ ] ⌘1-9 迁移后行为与现状逐键一致,且可在 Settings 重绑。
- [ ] 结构操作(resize/swap)后 session 持久化正确,重启还原布局。

## 3. PR 3 — 一键 tmux preset

**目标**:Settings 一键切换 tmux 风格前缀键位;纯数据层(映射表)+ 一层合并,零新输入模型。

### 落点

- **`core/config.rs`**:新增 `keybinding_preset: "default" | "tmux"`(serde 宽松解析,未知值回 default)+ `prefix: String`(默认 `"ctrl-b"`,校验 `Keystroke::parse`;常见备选 `ctrl-a`)。
- **`ui/keymap.rs`**:生效表改为三层合并 **defaults → preset(前缀代入)→ user overrides**,全走 PR 1 的 `effective_bindings()`;切换 preset = 改 config + `save()` + `rebind()`。
- **preset 映射表**(tmux 肌肉记忆 → action;`P` = prefix):

| tmux | tty7 action | 备注 |
|---|---|---|
| `P c` | NewTab | |
| `P x` | CloseActiveTab | 语义即「关焦点 pane,最后一个则关 tab」(`app.rs:1913` → `close_pane`),与 kill-pane 吻合;`&` 不做 |
| `P %` / `P "` | SplitRight / SplitDown | |
| `P ←→↑↓` | FocusPane 方向聚焦 | PR 2 |
| `P ctrl-←→↑↓` | ResizePane 方向 | PR 2;不支持按住连发,每次完整序列(先接受) |
| `P {` / `P }` | SwapPanePrev / Next | PR 2 |
| `P z` | ToggleMaximizePane | |
| `P o` / `P ;` | FocusNextPane / FocusPrevPane | `;` 是 last-pane 的近似 |
| `P n` / `P p` | NextTab / PrevTab | PR 2 |
| `P 1..9` | ActivateTab1..9 | PR 2 |
| `P [`、send-keys、choose-tree 等 | **不做** | 无对应语义,维持 issue 回复立场 |

- **`ui/settings.rs`**:Keybindings 页顶部加 preset 选择(Default / tmux)+ prefix 选择;列表实时显示序列 keycap(PR 1 已支持渲染)。
- **文案(必须)**:开启 preset 后,裸 prefix 键到 shell 有 1s 延迟(gpui pending 超时回放);`prefix+未绑定键` 会原样回放进终端。
- **issue 收尾**:三个 PR 合入后在 #61 回复(英文)并关闭。

### 验收 & 测试

- [ ] 三层合并优先级单测:preset 覆盖 default、user override 覆盖 preset;prefix 代入后所有序列可 parse。
- [ ] 实测:开 preset 后 `ctrl-b %` 分屏、裸 `ctrl-b` 1s 后到达 readline(光标左移);关 preset 立即还原。
- [ ] 终端内 `ctrl-b` 序列 pending 期间,焦点在 settings/输入框时无异常吞键。

## 4. 风险与权衡

- **热重绑(PR 1)是全案风险最高点**:NoAction 层叠语义、intercept 吞键边界(录键中途失焦/关窗)需 dogfood;建议 PR 1 先行合入留观察期。
- **裸 prefix 1s 延迟**:tmux 用户可接受(tmux 本身就吞 prefix),但必须在 UI 与 README 写明;若反馈强烈,后续可探索缩短 gpui pending 超时(上游改动,本期不做)。
- **`ctrl-tab` 默认键**:部分平台/输入法占用,合入前逐平台验证,冲突平台不给默认键。
- **keymap 追加式增长**:仅随编辑次数增长、进程内有界;若未来做「频繁热切 preset」再考虑上游暴露分层 keymap。

## 5. 顺序与工作量

| PR | 依赖 | 估算 |
|---|---|---|
| PR 1 编辑器 + 热重绑 | — | ~400-600 行(含测试),2-3 天 |
| PR 2 pane/tab action | 与 PR 1 无硬依赖,可并行 | ~500-700 行(树算法 + 单测占大头),2-3 天 |
| PR 3 tmux preset | 依赖 PR 1(合并层/渲染)+ PR 2(action) | ~200-300 行,1 天 |
