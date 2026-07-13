# Plan — 垂直标签侧边栏(vertical tab sidebar)落地方案

> 目标:在保留现有顶部水平标签条的前提下,新增一个**左侧垂直标签侧边栏**,作为标签列表的另一种呈现。
> 设计立场(借鉴 Warp `warp-src` 的做法):**模型 / 操作 / 标签文本 / 颜色 token 全部复用,只有「壳」(方向、行形状、布局位置)是新写的**。tty7 的一致性成本天然很低——`Tab` 模型、`tab_label()`、`activate/close_tab/move_tab`、`DragTab`、以及全部颜色都已走 `cx.theme()` 单一来源,不需要「抽取共享」,只需换一个方向复用。
> 本文分三个可独立发版的阶段(P0 骨架 / P1 打磨 / P2 增强),含文件级落点、验收标准与已核实的框架事实。

## 0. 已核实的技术事实(方案成立的前提)

| 事实 | 出处 | 对方案的意义 |
|---|---|---|
| `Tab { pane, name: Option<String>, settings }`——**无 id/color/pinned/group**,标签身份靠 `Vec<Tab>` 里的位置索引;active 是独立 `usize` | `src/ui/app.rs:66`、`:114-115` | 侧边栏直接读同一个 `self.tabs`/`self.active`,**零新状态**,任一处改动天然同步 |
| 标签文本逻辑 `tab_label(tab, i, cx)`(name → `short_title(leaf_title)` → `"Session N"`)与布局方向无关 | `src/ui/tab_strip.rs:151`、`short_title`:60 | 侧边栏**原样调用**,文本与顶部条一致 |
| 全部标签操作已是 `pub(crate)` 方法:`activate`(`app.rs:1125`)、`close_tab`(`:1161`)、`move_tab`(`:1206`)、`start_rename`(`:1237`)、`new_tab`(`:886`)、`new_tab_with_shell`(`:892`) | `src/ui/app.rs` | 侧边栏只是新增一批**点击目标**,不新增业务逻辑 |
| 顶部条颜色**全部**读命名 token,无一处 hex:active=`secondary`+`foreground`、inactive=`muted_foreground`+hover `muted`、拖放高亮=`drag_border.opacity(0.2)` | `src/ui/tab_strip.rs:316-324` | 侧边栏复用同批 token,视觉一致是**构造上保证** |
| 侧边栏专属配色 token **已存在且已接线**:`sidebar/sidebar_sel/sidebar_fg`(`presets.rs:133-135`)已喂给 gpui-component 的 `tokens.sidebar` 等(`theme.rs:156-164`) | `src/ui/presets.rs:133`、`src/ui/theme.rs:156` | 若想让侧边栏比正文沉一档,现成可用;⚠️ gpui-component `Sidebar` 读 `tokens.sidebar` 而非普通字段,手写 `v_flex` 则无此坑 |
| 拖拽重排今天就有:`DragTab { index, label }`(mini `Render` 预览)+ `drag_over` + `on_drop → move_tab` | `src/ui/tab_strip.rs:126-145`、`:323-327` | `DragTab` 提为 `pub(crate)` 即可在侧边栏复用,甚至条↔栏互拖 |
| 布局根是 `flex_col`:`TitleBar(h40).child(strip)` + `div.flex_1(body)`;body 是单一全宽子节点 | `src/ui/app.rs:2088`、TitleBar:2226、body:2238 | 侧边栏 = 把 `body` 那行包成 `h_flex`,首子放 sidebar、`body` 作 `flex_1` 第二子 |
| 顶部条**刻意不放 per-chip 图标**,label 承载整个 chip("No leading context glyph") | `src/ui/tab_strip.rs:339-341` | 侧边栏延续同样克制,行也以 label 为主,视觉才对得上 |
| config 用 `#[serde(default, deserialize_with = "de_lenient")]` 收窄枚举(如 `NewTabPosition`),未知值回默认 | `src/core/config.rs:79-84`、`:18` | 新增 `tab_bar_position` 枚举照抄此模式,前向兼容 |
| 持久化模型 `Session/SessionTab`(`session.rs:55-68`)同样**无 id/color/pin/group** | `src/core/session.rs:55` | P0/P1 不碰;将来若给标签加颜色/分组,须**同时**扩展 `Tab` 与 `SessionTab` |
| tmux 键位工作正在 `feat/keybindings-input-model` 分支进行(见 `plan-issue61-keybindings.md`) | 本仓库 | 新增 `ToggleTabSidebar` action 正好并入当前键位体系 |

## 决策点(需拍板,已在本方案取推荐默认)

- **❓ 模式切换 vs 并存**:本方案按**模式切换**设计(配置 `tab_bar_position: top | left`,对应 Warp 的 `uses_vertical_tabs`)——`left` 时顶部条收起 chips 只留 `+`/`⋯`,侧边栏显示列表。对终端更干净、不冗余。若要「顶部窄条 + 左侧富列表」并存,P0 的布局分叉与顶部条收起逻辑需改。**推荐:模式切换。**
- **取色策略**:P0 用**顶部条同批 token**(`secondary`/`muted`/`muted_foreground`),像素级同色最一致;是否改用专属 `sidebar*` token(IDE 活动栏观感)留到 P1 再定。**推荐:先同批 token。**

## 1. P0 — 骨架(功能完整、视觉即一致,可发版)

**目标**:新增 `tab_bar_position` 配置 + 左侧 `v_flex` 侧边栏(纯 label 行)+ 布局分叉 + 切换 action。此阶段结束,垂直模式已可用且与顶部条同源。

### 落点

- **`src/core/config.rs` — 新增 `tab_bar_position`**
  - `pub enum TabBarPosition { Top, Left }`(`#[serde(rename_all="kebab-case")]` + `de_lenient`,默认 `Top`),照抄 `NewTabPosition` 模式(`config.rs:79`)。字段挂到 `Config`(`config.rs:18`)。
- **`src/core/actions.rs` — 新增 `ToggleTabSidebar`**
  - 加进 `actions!` 列表(`actions.rs:8`);语义 = 在 `Top`/`Left` 间切换并 `Config::save()`。
- **`src/ui/tab_sidebar.rs` — 新文件,`impl Tty7App` 块**(与 `tab_strip.rs` 完全同构的拆分)
  - `fn tab_sidebar(&self, window, cx) -> impl IntoElement`:`v_flex()` 固定宽(`w(px(220.))`)、`overflow_y` 竖直滚动;遍历 `self.tabs` 渲染整行。
  - 每行 `h_flex().id(("tab-row", i)).w_full().h(px(34.)).pl_3().pr_2().rounded_lg()`:
    - `child(tab_label(...))` 左对齐 `.truncate()`,active 加 `FontWeight::MEDIUM`;
    - active → `.bg(secondary).text_color(foreground)`;inactive → `.text_color(muted_foreground).hover(bg(muted))`(**照抄 `tab_strip.rs:316-322`**);
    - 单击 `activate(i)` / 双击 `start_rename(i)`(复用 `self.renaming` 的 `Input` 分支,照抄 `tab_strip.rs:227-264`);
    - 尾部 close `Button`(hover 显隐,active 常显,照抄 `tab_strip.rs:349-389`);
    - `on_drag(DragTab{...})` + `drag_over` + `on_drop(move_tab)`(照抄 `tab_strip.rs:323-327`)。
  - 底部或顶部放一枚 `+` 按钮,复用 `tab_strip.rs:402-475` 的 shell dropdown(可先只 `new_tab`,P1 再补 shell 菜单)。
  - 空态:`self.tabs` 空时不进垂直模式(与首页逻辑一致),无需空态文案。
- **`src/ui/mod.rs`** — 挂 `mod tab_sidebar;`。
- **`src/ui/app.rs::render` — 布局分叉**(`app.rs:2088`、`:2238`)
  - 读 `cx.global::<Config>().tab_bar_position`。
  - `Top`(现状):不变。
  - `Left`:① `strip` 收起 chips(顶部条只渲染 `+`/`⋯`,或整体隐藏——见决策点);② 把 `.child(div().flex_1()...child(body))` 换成 `.child(h_flex().flex_1().child(self.tab_sidebar(..)).child(div().flex_1()...child(body)))`。
  - 根元素加 `.on_action(cx.listener(|this,_:&ToggleTabSidebar,w,cx| this.toggle_tab_sidebar(w,cx)))`(照抄 `app.rs:2128` 一众 `on_action`)。
- **`src/ui/keymap.rs`** — 给 `ToggleTabSidebar` 一个默认键(建议无默认或 `secondary-shift-e`,避免冲突;进 Settings/palette 可绑),并加进 palette 的命令目录(`ui/palette.rs`)。

### 验收 & 测试

- [ ] 配置 `tab_bar_position: left` 后启动:左侧出现标签列表,顶部条 chips 按决策收起;`top` 恢复现状。
- [ ] 侧边栏行的 active/hover/拖放高亮与顶部条**同色**(同 token,肉眼比对无差)。
- [ ] 侧边栏内:单击切换、双击改名、close 关闭、拖拽重排,行为与顶部条逐项一致(共用同一批方法)。
- [ ] `ToggleTabSidebar`(键位 + palette)即时切换并写盘;重启保持。
- [ ] 顶部/侧边两模式下 `⌘1-9`、`⌘T`、`⌘W`、`ctrl-tab` 等既有 action 行为不变(action 派发不受布局影响)。

## 2. P1 — 打磨(交互质感)

**目标**:侧边栏达到「可长期使用」的完成度——可改宽、宽度持久化、副标题、滚动细节。

### 落点

- **可拖拽改宽**:侧边栏右缘加拖拽条,`w` 夹在 `[180, 视窗宽*0.5]`(参考 Warp `MIN_PANEL_WIDTH`/`MAX_PANEL_WIDTH_RATIO`)。优先用 gpui-component 现成 resizable;否则手写 drag 更新一个 `sidebar_width: f32` 状态。
- **宽度持久化**:`sidebar_width` 存入 `Config`(或 session),`Config::save()`;重启还原。
- **行内副标题(可选)**:竖排空间富余,行改两行——主行 `tab_label`,副行 dimmed `muted_foreground` 的 cwd(取 `leaf_title`/cwd)。默认可先关,配置项 `sidebar_show_subtitle: bool`。若要更克制则跳过。
- **取色策略定稿**:决定沿用 `secondary`/`muted`,还是切 `sidebar*` token(`theme.rs:156-164` 已接线;注意 `tokens.sidebar` 坑)。
- **滚动 & 溢出**:标签很多时竖直滚动顺滑;active 行自动滚入可视区。

### 验收 & 测试

- [ ] 拖拽改宽夹紧上下限;宽度重启保持。
- [ ] 副标题(若启用)不撑高行、超长 `.truncate()`。
- [ ] 数十个标签时滚动顺滑,切到不可见标签自动滚入视区。

## 3. P2 — 增强(可选,视需求做)

- **`DragTab` 提共享**:移到公共位置并 `pub(crate)`,支持顶部条 ↔ 侧边栏互拖(同一 payload,`move_tab` 复用)。
- **行内副标题富化**:pane 数、运行中命令等(仍保持克制,别做成 icon-per-row)。
- **右键菜单**:rename / close / duplicate / (未来)move,复用现有方法。
- **小重构(非必需)**:把顶部 chip body 的点击/改名/拖拽绑定抽成 `tab_row(tab,i,is_active)` inline helper,`h_flex` 与 `v_flex` 共调——但因这些本就是 `Tty7App` 方法,各自调也无重复逻辑,收益有限。
- **标签着色/分组(大改)**:需**同时**扩展 `Tab` 与 `SessionTab`(`session.rs:55`)加字段并处理迁移;超出本方案范围,单独立项。

## 4. 风险与权衡

- **布局分叉是主要改动面**:`app.rs::render`(`:2238`)从「TitleBar + 单 body」变「TitleBar(收起) + [sidebar | body] 行」,需确保 `maximized`、focus ring、命令面板 overlay 在两模式下都正确;建议 P0 先 dogfood 一版再上打磨。
- **顶部条收起的取舍**:`left` 模式下顶部条留 `+`/`⋯` 还是整条隐藏,影响标题栏空白与窗口拖拽区(gpui-component `TitleBar` 的 `WindowControlArea::Drag`)。倾向保留 `+`/`⋯` 一行,避免标题栏空荡。
- **无 per-tab 图标是刻意的**:侧边栏若擅自加图标会与顶部条风格背离;保持 label 优先。
- **持久化字段扩展**:P2 的着色/分组会动到 `Session` 结构,牵涉存量 `config`/session 迁移,单独发版。

## 5. 顺序与工作量

| 阶段 | 依赖 | 估算 |
|---|---|---|
| P0 骨架(配置 + `tab_sidebar.rs` + 布局分叉 + action) | — | ~300-400 行(含测试),1-2 天 |
| P1 打磨(改宽 + 持久化 + 副标题 + 取色定稿) | P0 | ~200-300 行,1-2 天 |
| P2 增强(互拖 / 右键菜单 / 着色分组) | P0(着色分组另依赖 Session 扩展) | 按选做项计,0.5-N 天 |
