# 远程 Workspace：实施契约

> 状态：契约定稿，供并行实现
> 日期：2026-07-27
> 配套：[`2026-07-27-remote-workspace-design.md`](./2026-07-27-remote-workspace-design.md)（下称"设计文档"）

## 0. 这份文档是什么

设计文档定方向，**这份定接口**。签名、字节布局、kind 数值、文件归属、测试清单 —— 都是可以直接抄进代码的形态。

**读法**：

| 你是 | 先读 |
|---|---|
| 任何 agent | §1 岔路口裁决、§9 模块归属、§11 设计文档勘误 |
| A1（crate 拆分） | §9、§11 的 11/12/14/15 条 |
| A2（Host + LocalHost） | §2 §3 §4 §10 |
| A3（调用点改造） | §1 §2 §5 §11 的 2/3/9/10/13 条 |
| A4（control wire + RemoteHost） | §6 §7 §10 |
| A5（control server + tty7-server） | §6 §7 §8 |

**冲突仲裁**：本文档与设计文档冲突时，**以本文档为准**；§11 逐条记录了偏差与理由。

**契约落地之后的变更**：这份文档定的是 M1–M3 那一波的接口，落地后不再逐条同步 —— 签名、wire 结构和字节布局一律**以代码里的 doc comment 为准**（`crates/tty7-core/src/host/mod.rs`、`crates/tty7-core/src/daemon/control.rs`）。目前已知的一处是 #239 的流式 git 读：`Host::git_lines`（默认实现就是缓冲读 `git` 再切行，所以 §4.4 的调用不变量原样成立，也没有绕开 `Host` 的第二条 git 出口），wire 上是 `ControlRequest::GitStream` + `ControlEvent::GitChunk`/`GitEnd`（见 §6.3）。

**行号约定**：正文里 `src/daemon/…` / `src/core/…` 形式的行号引用取自 **M1 拆分前**的树。A1 已经把它们搬到 `crates/tty7-core/src/{daemon,core}/…`（模块路径刻意保持不变，见 §9.1），行号大体不动。`src/ui/…` / `src/terminal/…` 的引用仍然有效。

---

## 1. 岔路口裁决：阻塞 vs 异步

### 裁决

**`Host` trait 保持同步阻塞签名。所有调用点一律搬到 background executor，不做例外。**

### 为什么不做 async trait

| 理由 | 展开 |
|---|---|
| **object safety** | 全树需要 `Arc<dyn Host>`（设计文档 §9"一个 workspace 一个 `Arc<dyn Host>`"）。`async fn` in trait 目前不 object-safe，只能 `#[async_trait]` 装箱，`LocalHost` 每次 `stat` 都要一次堆分配 —— 而 `LocalHost` 是 99% 的路径 |
| **服务端复用** | `tty7-server` 侧的 RPC handler 跑在线程池上，本来就是阻塞的。同一个 `LocalHost` 实例既服务本地 GUI 又服务远程客户端，只有阻塞签名能一份代码两处用 |
| **传染性** | GPUI 的 `&mut Context<Self>` 不能跨 `.await` 持有。把 Host 变 async 不会减少调用点改造量 —— 该拆的还是要拆，只是多背一个 async runtime |
| **既有先例** | `terminal/view.rs:3963-3980` 的远程路径补全已经是这个形状：`cx.spawn` → `cx.background_spawn(阻塞调用)` → `this.update` 落地。它工作良好，照抄即可 |

### 但设计文档 §9 那句话是错的，必须正面处理

> 设计文档 §9："这些调用点现在**全部已经**在 background executor 上跑……保持阻塞语义意味着调用点的结构一行不用动。"

**实测不成立。** 已经在 background 上的只有 `file_tree` 的 `read_dir` + gitignore 编译（`request_load` :448-479）。以下全部在 **UI 线程同步跑**：

| 文件 | 行 | 调用 | 所在函数 |
|---|---|---|---|
| `ui/code_editor.rs` | 328 | `path.canonicalize()` | `open_file_in_editor` |
| `ui/code_editor.rs` | 343 | `std::fs::metadata` | `open_file_in_editor` |
| `ui/code_editor.rs` | 361 | `std::fs::read` | `open_file_in_editor` |
| `ui/code_editor.rs` | 382 | `std::fs::metadata` | `open_file_in_editor` |
| `ui/code_editor.rs` | 531 | `std::fs::write` | `editor_save_active` |
| `ui/code_editor.rs` | 535 | `std::fs::metadata` | `editor_save_active` |
| `ui/code_editor.rs` | 642 | `std::fs::metadata` | `editor_handle_external_change` |
| `ui/code_editor.rs` | 691 | `std::fs::read_to_string` | `editor_reload_from_disk` |
| `ui/code_editor.rs` | 697 | `std::fs::metadata` | `editor_reload_from_disk` |
| `ui/file_tree.rs` | 953 | `File::create_new` | `file_tree_commit_edit` |
| `ui/file_tree.rs` | 957 | `fs::create_dir` | `file_tree_commit_edit` |
| `ui/file_tree.rs` | 963 | `to.exists()` | `file_tree_commit_edit` |
| `ui/file_tree.rs` | 967 | `fs::rename` | `file_tree_commit_edit` |
| `ui/file_tree.rs` | 998 | `path.is_dir()` | `file_tree_delete` |
| `ui/file_tree.rs` | 1013/1015 | `remove_dir_all` / `remove_file` | `file_tree_delete` |
| `ui/app.rs` | 3997-4005 | `Command::new("git")` ×2 | `send_git_diff_to_agent` |
| `core/worktree.rs` | 62-73 | `Command::new("git")` | `git` helper（同步调用方待查） |

**后果**：M2 **不是**"只换实现来源"。M2 包含一次真实的异步化改造，且它**不是零行为变化**（见下表）。里程碑表 §19 说"M1 / M2 是纯重构、零行为变化"—— M1 是，M2 不是。这个预期要在 M2 开工前对齐。

### 允许的行为变化（M2 唯一豁免清单）

| 调用点 | 改造前 | 改造后 | 用户可见差异 |
|---|---|---|---|
| `open_file_in_editor` | 同步打开 | tab 立刻出现，内容异步填 | 大文件/远程时先看到空 tab + loading |
| `editor_save_active` | 同步落盘 | 异步落盘，落地前 buffer 标 `saving` | ⌘S 后 dirty 标记延迟一帧清除 |
| `file_tree_commit_edit` | 同步创建/改名 | 异步，乐观插入行 | 失败时行会消失 + 通知 |
| `file_tree_delete` | 同步删除 | 异步，乐观移除行 | 同上 |
| `send_git_diff_to_agent` | 同步 `git diff` | 异步 | 大 repo 上不再卡 UI（改善） |

**除此之外任何行为变化都是 bug。**

### 强制模式：`HostOps`

GUI 侧禁止直接 `host.stat(...)`。一律走 `ui/host_ops.rs` 的门面（§5）。理由：
- 单一出口便于加 in-flight 去重、staleness 校验、错误通知
- `debug_assert` 守卫能集中在一处，把"谁又在 UI 线程上阻塞了"变成一个 panic 而不是一次卡顿

---

## 2. `Host` trait 最终签名

**位置**：`crates/tty7-core/src/host/mod.rs`
**负责人**：A2

```rust
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 标识
// ---------------------------------------------------------------------------

/// 客户端进程内对一个 `Arc<dyn Host>` 的稳定标识。**不持久化**，只做
/// 进程内 key（`GitStatusCache`、pane 标识、in-flight 表）。
///
/// - `HostId::LOCAL` 是常量 0，本地 host 永远是它。
/// - 远程由 `RemoteRef` 的**连接部分**（不含 `WorkspaceId`）派生：同一台机器的
///   多个 workspace 共享一个 `HostId`，与 §7.1 的 `SshConnection` 复用同粒度。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct HostId(pub u64);

impl HostId {
    pub const LOCAL: HostId = HostId(0);

    /// 从连接标识派生。`key` 必须是规范化后的连接串（见 §4.2），
    /// 相同机器必须产出相同串。碰撞到 0 时强制搬到 1 —— 0 是本地的保留值。
    pub fn from_connection_key(key: &str) -> HostId {
        let h = fnv1a64(key.as_bytes());
        HostId(if h == 0 { 1 } else { h })
    }

    pub fn is_local(self) -> bool {
        self == HostId::LOCAL
    }
}

/// 与 `transport::socket_path_for` 同款 FNV-1a：跨编译器/std 版本稳定。
/// A1 把它从 `daemon/transport.rs` 提到 `tty7-core` 的公共位置，两处共用一份。
pub fn fnv1a64(bytes: &[u8]) -> u64 { /* 照搬 transport.rs:59-66 */ }

// ---------------------------------------------------------------------------
// 辅助类型
// ---------------------------------------------------------------------------

/// 一个目录项。**没有 `path` 字段** —— 路径由调用方 `Host::join(dir, &e.name)`
/// 重建。原因：远程路径的分隔符是远端的，`PathBuf::join` 在 Windows 客户端上
/// 会吐出 `/home/me\src`（见 §4.3）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// 符号链接本身是否是 link。`is_dir` 已经**跟随**链接解析过
    /// （与 `SftpEntry.target_is_dir` 的语义一致）。
    pub is_symlink: bool,
    /// 服务端算好的 gitignore 判定（含 `.git` 本身恒为 true）。
    /// 目录不在任何 repo 里，或远程没装 git 时，恒为 `false`。
    pub ignored: bool,
}

/// 文件元信息。刻意不是 `std::fs::Metadata`（不可构造、不可序列化）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    /// `None` = 平台/文件系统没有 mtime。
    pub mtime: Option<MTime>,
    pub readonly: bool,
}

/// 精确到纳秒的 mtime。**不用毫秒**：`code_editor` 的外部变更检测靠
/// mtime 相等判断"这是我们自己刚写的回声"，毫秒粒度会把同毫秒内的真实外部
/// 修改吞掉。**不用 `u128` 纳秒**：JSON 数字放不下 u128 而不失精度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct MTime {
    /// Unix epoch 起的整秒，可为负（1970 之前）。
    pub secs: i64,
    /// 0..1_000_000_000。
    pub nanos: u32,
}

/// 一次子进程执行的结果。刻意不是 `std::process::Output`
/// （`ExitStatus` 不可跨平台构造）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    /// `None` = 被信号杀死 / 无法取得退出码。
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool { self.status == Some(0) }
    /// stdout 按 UTF-8 lossy 解读并 trim —— 三个既有 git helper 的公共形状。
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// 一个**长命**的文件监听订阅。dir 集合可变 —— file tree 每次展开都会改动
/// 集合，重建整个订阅代价太大（远程等于一次往返 + 服务端重建 notify）。
///
/// Drop 即退订（远程侧发 `watch.close`）。
pub struct WatchSub {
    rx: smol::channel::Receiver<Vec<PathBuf>>,
    inner: Box<dyn WatchHandle>,
}

impl WatchSub {
    /// 批量事件流。服务端已按 100ms 窗口合并去重（§6.6）；本地实现同样合并，
    /// 两边行为一致。
    pub fn events(&self) -> &smol::channel::Receiver<Vec<PathBuf>> { &self.rx }

    /// 替换监听的目录集合。差量由实现自己算，调用方只给全集。
    /// 一律**非递归**。
    pub fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> { self.inner.set_dirs(dirs) }
}

pub trait WatchHandle: Send + Sync {
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// 一台机器的文件系统 + git 出口。
///
/// **所有方法都是阻塞的，且禁止在 UI 线程上调用**（§1）。GUI 侧一律经
/// `ui::host_ops::HostOps`，它负责 `background_spawn` 与落地。
///
/// object-safe 是硬要求：全树持 `Arc<dyn Host>`。
pub trait Host: Send + Sync + 'static {
    // ----- 标识 ------------------------------------------------------------

    fn id(&self) -> HostId;

    /// 该 host 的路径分隔符。本地 = `std::path::MAIN_SEPARATOR`，
    /// 远程 Linux/WSL 恒为 `'/'`。
    fn separator(&self) -> char;

    // ----- 路径算术（§4.3 强制走这里，禁止 `Path::join`） -------------------

    /// `dir` + `name`。用 `self.separator()`，不用 `PathBuf::push`。
    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        default_join(dir, name, self.separator())
    }

    /// 该 host 语义下 `p` 是否是绝对路径。**必须用它，不用 `Path::is_absolute`**
    /// —— Windows 客户端上 `/home/me` 的 `is_absolute()` 是 `false`
    /// （drive-relative），这正是 `local_cwd()` 挡板文档里那个陷阱。
    fn is_absolute(&self, p: &Path) -> bool;

    // ----- 读 --------------------------------------------------------------

    /// 一个目录的**已排序**列表（目录在前，然后按 lowercase 名字），
    /// 排序规则与 `file_tree::sort_entries` 逐字一致，服务端排好。
    ///
    /// `root` 是 gitignore 链的上界（`Entry::ignored` 从 root 走到 dir 逐级
    /// 求值，深者胜，`!` 白名单反转）。`root` 为 `None` 时 `ignored`
    /// 除 `.git` 外全 false。
    ///
    /// 隐藏文件**不过滤** —— `show_hidden` 是 UI 状态，留在客户端。
    fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>>;

    fn stat(&self, p: &Path) -> io::Result<Meta>;

    /// `p` 存在与否。`stat().is_ok()` 的省流版；实现可以合并成一次往返。
    fn exists(&self, p: &Path) -> bool {
        self.stat(p).is_ok()
    }

    /// 读整个文件。`max_bytes` 是**服务端**上限：超过就返回
    /// `ErrorKind::FileTooLarge`（映射见 §6.5）而不是传一遍再丢掉。
    /// 传 `MAX_FILE_BYTES`（`code_editor` 的既有常量）。
    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>>;

    /// 规范化。远程实现走服务端 `canonicalize`。失败时调用方沿用既有习惯
    /// （`unwrap_or_else(|_| p.to_path_buf())`）。
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;

    /// 广度优先的名字包含匹配，**服务端执行**。
    ///
    /// 这是设计文档 §9 漏掉的方法。`file_tree.rs::TreeLoader::search` 最多访问
    /// 2000 个目录才停 —— 逐目录 RPC 等于 2000 次往返，跨洲 = 400 秒。必须整个
    /// 搬到服务端。语义与既有实现逐字一致：BFS、跳过 ignored 目录、`limit` 命中
    /// 即停、`max_dirs` 访问上限即停。
    fn search(&self, roots: &[PathBuf], query: &str, limit: usize, max_dirs: usize)
        -> io::Result<Vec<SearchHit>>;

    // ----- 写 --------------------------------------------------------------

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;

    /// 排他创建，已存在即 `AlreadyExists`（对应 `File::create_new`）。
    fn create_file_new(&self, p: &Path) -> io::Result<()>;

    /// `recursive = false` → `fs::create_dir`；`true` → `create_dir_all`。
    /// （设计文档只给了 `create_dir`；`worktree` 写 `.tty7/.gitignore` 要 all。）
    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()>;

    /// 目标已存在时必须返回 `AlreadyExists`，**由实现保证**，不靠调用方先
    /// `exists()` 探一次（那是一次多余往返 + TOCTOU）。
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// `recursive` 只对目录有意义。文件传 `false`。
    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()>;

    // ----- git -------------------------------------------------------------

    /// `p` 所在仓库的工作树根：最近的、含 `.git`（目录或 worktree 文件）的祖先。
    /// 服务端一次走完，不逐级往返。
    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>>;

    /// `git -C <cwd> <args>`。
    ///
    /// 两个实现**必须**统一到同一份不变量（§4.4）：
    /// `GIT_OPTIONAL_LOCKS=0` + `stdin(null)` + Windows 上 `hide_console`。
    ///
    /// 返回 `Ok(Output)` 表示"git 跑起来了"，退出码在 `Output::status` 里。
    /// `Err` 只表示"没跑起来"（git 不存在、cwd 不存在、RPC 失败/超时）。
    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output>;

    // ----- 监听 ------------------------------------------------------------

    /// 建立一个长命订阅，初始集合为 `dirs`（可空）。**非递归**。
    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub>;

    // ----- 生命周期 --------------------------------------------------------

    /// 该 host 当前是否可用。`LocalHost` 恒 `true`；`RemoteHost` 在
    /// `Reconnecting` / `Preempted` 期间为 `false`，调用点据此显示上一份缓存
    /// 而不是错误。
    fn is_connected(&self) -> bool { true }
}

/// `search` 的一条命中。`path` 是**绝对**路径（服务端用自己的分隔符拼好）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

/// 便利别名，全树用它。
pub type SharedHost = Arc<dyn Host>;
```

### 方法清单与设计文档的差异

| 设计文档 §9 | 本契约 | 变化 |
|---|---|---|
| `read_dir(&self, p)` | `read_dir(&self, dir, root)` | 加 `root`：gitignore 链需要上界 |
| — | `search(...)` | **新增**，见上（2000 次往返问题） |
| — | `exists`, `canonicalize` | **新增**，既有调用点用得到（`file_tree:963/998`、`code_editor:328`） |
| — | `create_file_new` | **新增**，`file_tree:953` 用 `create_new` 语义 |
| `create_dir(&self, p)` | `create_dir(&self, p, recursive)` | `worktree` 要 `create_dir_all` |
| `read_file(&self, p)` | `read_file(&self, p, max_bytes)` | 服务端截断，省一次全量传输 |
| — | `id`, `separator`, `join`, `is_absolute` | **新增**，跨 OS 路径算术（§4.3） |
| — | `is_connected` | **新增**，降级态需要它 |
| `watch(&self, dirs) -> WatchSub` | 同名，`WatchSub` 可 `set_dirs` | 订阅长命，集合可变 |

---

## 3. `LocalHost`

**位置**：`crates/tty7-core/src/host/local.rs`
**负责人**：A2

```rust
pub struct LocalHost { /* 无状态，watcher 除外 */ }

impl LocalHost {
    pub fn new() -> Arc<dyn Host> { Arc::new(Self { .. }) }
}
```

| 约束 | 说明 |
|---|---|
| **零往返、零分配开销** | 每个方法直调 `std::fs` / `Command`。除了 `Vec`/`String` 的必要分配，不许有额外拷贝 |
| **`id()` 恒为 `HostId::LOCAL`** | |
| **`separator()`** | `std::path::MAIN_SEPARATOR` |
| **gitignore** | 用 A1 已提取的 `core::gitignore::GitignoreChain`（`is_ignored(path, is_dir, root)`）。`LocalHost` 内部持 `Mutex<GitignoreChain>` 做 matcher 缓存，不再由调用方 seed/handback（`TreeLoader` 那套来回搬是为了跨线程，`Arc<dyn Host>` 之后不需要了） |
| **`watch`** | `notify::recommended_watcher`，`RecursiveMode::NonRecursive`，内部 100ms 合并去重后批量投递 —— **与远程行为逐字一致**，不许本地"顺便"更实时 |
| **`git`** | 三套既有 helper 的合并版，见 §4.4 |

---

## 4. 四个必须钉死的约定

### 4.1 `Host` 从哪来

- 一个 workspace 一个 `SharedHost`。本地 workspace 拿进程级单例 `LocalHost`。
- `TerminalView` / `FileTreeState` / `CodeEditor` 都从所属 workspace 取，**不许自己 `LocalHost::new()`**。
- 进程内有一张 `HostRegistry: HashMap<HostId, SharedHost>`（`gpui::Global`，住 GUI crate），`HostId` → `SharedHost` 反查用于 `GitStatusCache` 这类只存 id 的地方。

### 4.2 `HostId` 的派生串

`HostId::from_connection_key` 的输入必须是规范化后的**连接**串，格式固定：

| host 种类 | key 串 |
|---|---|
| 本地 | 不派生，直接 `HostId::LOCAL` |
| SSH profile | `ssh-profile:<profile_uuid>` |
| `~/.ssh/config` alias | `ssh-alias:<alias>` |
| 裸 `user@host:port` | `ssh-direct:<user>@<host>:<port>`（port 省略时补默认 22） |
| WSL | `wsl:<distro>` |

**不含 `WorkspaceId`** —— 同机多 workspace 共享 `HostId`（设计文档 §10 已定）。

### 4.3 跨 OS 路径规则（设计文档完全没提，但是硬伤）

Windows 客户端连 Linux 远程时，远程路径是 `/home/me/proj`。

| `std::path` API | 在 Windows 上对远程 POSIX 路径 | 裁决 |
|---|---|---|
| `join` / `push` | ❌ 产出 `/home/me\proj` | **禁用**。用 `Host::join` |
| `is_absolute` | ❌ 返回 `false`（drive-relative） | **禁用**。用 `Host::is_absolute` |
| `canonicalize` | ❌ 走本地文件系统 | **禁用**。用 `Host::canonicalize` |
| `parent` / `file_name` / `with_file_name` | ✅ Windows 的 `std::path` 把 `/` 也当分隔符 | 可用 |
| `starts_with` / `strip_prefix` / `ancestors` / `components` | ✅ 同上 | 可用 |
| `exists` / `is_dir` / `metadata` / `read_dir` | ❌ 走本地文件系统 | **禁用**。用 `Host::*` |

**执行方式**：A3 改造完后，`ui/` 与 `terminal/` 里除 `host_ops.rs` 外不得出现 `std::fs::`、`Command::new("git")`、`.canonicalize()`、`.is_absolute()`。加一个 CI grep 守卫（§10.6）。

### 4.4 `git` 调用的统一不变量

三套既有出口的实测差异：

| 出口 | 位置 | `GIT_OPTIONAL_LOCKS=0` | `stdin(null)` | cwd 传递 | 错误表示 | 线程 |
|---|---|---|---|---|---|---|
| `git_status::git` | 拆分前 `terminal/git_status.rs:332-345`；A1 已提到 `core::git::git` | ✅ | ✅ | `-C` | `Option<String>`，stderr 丢弃 | background |
| `worktree::git` | `core/worktree.rs:63-75` | ❌ | ❌ | `-C` | `Result<String, String>`，stderr 是错误文本 | 调用方决定 |
| `app.rs` 内联闭包 | `ui/app.rs:3995-4005` | ❌ | ❌ | `current_dir` | 静默空串 | **UI 线程** |

**统一后的 `Host::git` 不变量（两个实现都必须满足）**：

1. `git -C <cwd> <args>` —— 一律 `-C`，不用 `current_dir`
2. `GIT_OPTIONAL_LOCKS=0`
3. `stdin` = null
4. stdout **和** stderr 都捕获进 `Output`（不丢 stderr —— `worktree` 需要它做错误文本）
5. Windows 上 `hide_console`（本地实现；远程无此概念）
6. 不继承 GUI 进程的 `GIT_*` 环境变量以外的 git 相关污染 —— 显式 `env_remove("GIT_DIR")`、`env_remove("GIT_WORK_TREE")`

**调用方适配**（A3 负责）：

| 原调用方 | 适配 |
|---|---|
| `git_status` / `git_diff` | `host.git(...).ok().filter(Output::success).map(\|o\| o.stdout_trimmed())` —— 保留 `Option<String>` 语义 |
| `worktree` | `match host.git(...) { Ok(o) if o.success() => Ok(o.stdout_trimmed()), Ok(o) => Err(o.stderr_trimmed()), Err(e) => Err(format!("failed to run git: {e}")) }` —— 逐字保持既有 `Result<String, String>` 形状 |
| `app.rs::send_git_diff_to_agent` | 直接 `stdout_trimmed()`，失败当空串（保持既有行为），但整段搬到 background |

**行为变化警告**：`worktree` 和 `app.rs` 拿到 `GIT_OPTIONAL_LOCKS=0` 是一个真实变化（`git worktree add` 是写操作，`optional_locks=0` 只影响可选锁如 `index.lock` 的刷新，**不影响写操作的必要锁**）。已确认安全，但 M2 的回归测试必须覆盖 `worktree add` / `list` 的完整路径。

---

## 5. `HostOps`：GUI 侧唯一出口

**位置**：`src/ui/host_ops.rs`（留在 GUI crate，因为它认识 `gpui`）
**负责人**：A2 定义，A3 消费

```rust
/// GPUI 侧的 Host 门面。把每个阻塞方法包成
/// `spawn → background_spawn → update` 的三段式，并统一处理
/// in-flight 去重、staleness、错误通知。
///
/// **这是 GUI 里唯一允许触碰 `Arc<dyn Host>` 的地方。**
pub struct HostOps;

impl HostOps {
    /// 通用逃生舱：在 background 上跑 `f`，结果回到 UI 线程交给 `land`。
    /// 用于契约里没有专门包装的方法。
    pub fn run<T, E, F, L>(
        host: SharedHost,
        cx: &mut Context<E>,
        f: F,
        land: L,
    ) where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut E, T, &mut Context<E>) + 'static,
    { /* cx.spawn + cx.background_spawn + this.update */ }

    /// 带 window 的变体（需要 `push_notification` / `focus` 的落地）。
    pub fn run_in<T, E, F, L>(
        host: SharedHost, window: &mut Window, cx: &mut Context<E>, f: F, land: L,
    ) { /* cx.spawn_in */ }
}
```

**UI 线程守卫**（debug 构建）：

```rust
// tty7-core::host 里
#[cfg(debug_assertions)]
pub fn assert_off_ui_thread() {
    debug_assert!(
        !crate::host::is_ui_thread(),
        "Host call on the UI thread — route it through ui::host_ops::HostOps"
    );
}
```

`is_ui_thread()` 比对启动时 `main.rs` 注册的 `ThreadId`。每个 `Host` 方法的默认实现入口调一次（release 下被优化掉）。

**必备模式（照抄 `terminal/view.rs:3963-3980`）**：

| 关注点 | 做法 |
|---|---|
| **in-flight 去重** | 每种请求一张 key → bool 表（`file_tree::Loads` 已经是这个形状，复用它的 `begin`/`finish`/`invalidate` 三态） |
| **staleness** | 落地前必须重查前置条件（`remote_path_results` 检查 `self.cmd.text() != line`）。目录列表检查 `Loads::finish` 是否被 `invalidate` 过 |
| **错误** | `Err` 一律 `window.push_notification`，**不许静默**，除非既有行为就是静默（`git` 探针） |
| **断连** | `!host.is_connected()` 时不发请求，直接显示上一份缓存 |

---

## 6. control 连接协议

**位置**：`crates/tty7-core/src/daemon/control.rs`（wire 定义 + 编解码 + 测试）
**负责人**：A4 定义并落地 wire，A5 消费

### 6.1 kind 数值：**60-63**（两个方向各一套）

先把号段现状钉死（`src/daemon/protocol.rs:974-1061`）：

| 空间 | 已用 | 保留（注释显式声明） | 退役 |
|---|---|---|---|
| Client → daemon | 1-12, 14-17, 20-22, 30-34, 40, 50 | 15-19（WS3 auth）、20-24（WS4 forward）、30-36（SFTP） | **13**（曾是 `SPAWN_MANAGED_SSH`） |
| Daemon → client | 1-15, 20-22, 30-33, 40, 50 | 同上 | — |

**分配**：

| 方向 | 名字 | 值 | payload 形态 |
|---|---|---|---|
| C→S | `CONTROL_HELLO` | **60** | `[JSON]`（无 req_id） |
| C→S | `CONTROL_REQUEST` | **61** | `[u64 req_id][u32 json_len][JSON]` |
| C→S | `CONTROL_REQUEST_BLOB` | **62** | `[u64 req_id][u32 json_len][JSON][raw bytes]` |
| C→S | `CONTROL_CANCEL` | **63** | `[u64 req_id][u32 json_len = 0]` |
| S→C | `CONTROL_HELLO_OK` | **60** | `[JSON]` |
| S→C | `CONTROL_RESPONSE` | **61** | `[u64 req_id][u32 json_len][JSON]` |
| S→C | `CONTROL_RESPONSE_BLOB` | **62** | `[u64 req_id][u32 json_len][JSON][raw bytes]` |
| S→C | `CONTROL_EVENT` | **63** | `[u64 req_id = 0][u32 json_len][JSON]` |

**为什么是 60-63**：

| 理由 | 展开 |
|---|---|
| **避开全部保留段** | 15-19 / 20-24 / 30-36 都被注释显式预留给 WS3/WS4/SFTP 的后续扩展。占用它们等于把那三块的扩展空间吃掉 |
| **不复用 13** | 13 是**退役**号，不是空闲号。一个 pre-WS2 的 daemon 会把 kind 13 解成 `SpawnManagedSsh` 并**静默 mis-spawn 一个 pane**，而不是报未知 kind。退役号一律永不复用 |
| **跟随既有分组习惯** | 现有 kind 按 10 分组（10 / 20 / 30 / 40 / 50）。60 是下一个整十位，把整个 control 方言收进一块 |
| **留出 64-69** | 同一块里给后续 control 帧留 6 个号（如未来的流式响应、背压信号），不必再挑新段 |

**实现前提**：`mod kind` 目前是**私有的**（`protocol.rs:974`）。A4 必须先把它改成 `pub(crate) mod kind`，或把 control kind 定义在 `control.rs` 里自己的 `pub mod kind`。**选后者** —— control 是独立方言，独立号段，独立模块，只在 `control.rs` 的头部注释里交叉引用 `protocol::kind` 的已用表。

### 6.2 三种 payload 的精确字节布局

所有多字节整数 **little-endian**。外层帧沿用 `[u32 LE payload_len][u8 kind][payload]`，`MAX_FRAME = 64 MiB` 不变。

```
CONTROL_HELLO / CONTROL_HELLO_OK (60)
┌──────────────────────────┐
│ JSON (payload_len bytes) │
└──────────────────────────┘

CONTROL_REQUEST / CONTROL_RESPONSE (61)
CONTROL_EVENT (63, S→C)
CONTROL_CANCEL (63, C→S)
┌───────────────┬───────────────┬──────────────────┐
│ u64 LE req_id │ u32 LE json_n │ JSON (json_n B)  │
└───────────────┴───────────────┴──────────────────┘
  8 bytes         4 bytes
必须满足：payload_len == 12 + json_n

CONTROL_REQUEST_BLOB / CONTROL_RESPONSE_BLOB (62)
┌───────────────┬───────────────┬─────────────────┬──────────────────────┐
│ u64 LE req_id │ u32 LE json_n │ JSON (json_n B) │ raw blob (剩余全部)   │
└───────────────┴───────────────┴─────────────────┴──────────────────────┘
blob_len == payload_len - 12 - json_n
```

**为什么大 payload 也带 JSON 头**（设计文档 §8 写的是 `[u64 req_id][raw bytes]`，无 JSON）：
`write_file` 必须携带目标路径，`read_file` 的响应必须携带 `Meta`（`code_editor` 需要写入后的 mtime，否则要再来一次 `stat` 往返）。裸 blob 形态承载不了参数 —— 这是设计文档 §8 的一处实打实的漏洞（§11 第 7 条）。

**为什么事件也带 req_id 且恒为 0**：
冗余 8 字节换来"所有非 HELLO 的 control 帧共享同一个头解析器"。reader 一律 `read u64 → read u32 → 取 JSON`，然后按 kind 分派。`req_id != 0` 的 `CONTROL_EVENT` 是协议错误（`InvalidData`），reader 必须校验。

**解码校验（缺一不可）**：

| 检查 | 违反时 |
|---|---|
| `payload_len >= 12`（61/62/63） | `InvalidData` |
| `12 + json_n <= payload_len` | `InvalidData`（防 `json_n` 溢出导致越界切片） |
| `61`/`63` 上 `12 + json_n == payload_len` | `InvalidData` |
| `CONTROL_EVENT` 的 `req_id == 0` | `InvalidData` |
| `CONTROL_REQUEST` 的 `req_id != 0` | `InvalidData` |
| 未知 kind | `InvalidData`（与既有协议一致，是错误不是跳过） |

### 6.3 `req_id` 语义

| 项 | 规则 |
|---|---|
| **分配方** | 客户端。服务端从不分配 |
| **起点 / 步进** | 从 1 开始，`fetch_add(1)` 单调递增。**0 永久保留给推送** |
| **回绕** | `u64` 不考虑回绕 |
| **匹配** | 乱序。客户端维护 `HashMap<u64, oneshot::Sender<Reply>>`；响应到达时 `remove` 并投递 |
| **未知 req_id 的响应** | **静默丢弃**，不当错误 —— 超时后取消的请求可能仍会收到迟到的响应 |
| **一请求一响应** | 严格：一个 req_id 恰好一个响应，没有多段响应。大文件靠 `MAX_FRAME` 兜底，超过就 `FileTooLarge`。#239 加的流式 git 读不破这一条 —— `GitStream` 本身照常一问一答（立刻回 `Unit`），数据走 req_id = 0 的事件推送（`ControlEvent::GitChunk`/`GitEnd`，id 由客户端选），细节以代码为准，见 §0 |
| **连接重建** | in-flight 全部以 `ErrorKind::ConnectionReset` 失败；req_id 计数器**不重置**（无所谓，但重置也不会错） |

### 6.4 每个 RPC 的请求/响应结构

**请求 JSON** 是一个 externally-tagged enum：

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    // --- 探活 ---
    Ping,

    // --- fs 读 ---
    ReadDir      { dir: String, root: Option<String> },
    Stat         { path: String },
    Exists       { path: String },
    Canonicalize { path: String },
    ReadFile     { path: String, max_bytes: u64 },
    Search       { roots: Vec<String>, query: String, limit: u64, max_dirs: u64 },

    // --- fs 写 ---
    /// blob = 文件内容（走 CONTROL_REQUEST_BLOB）
    WriteFile      { path: String },
    CreateFileNew  { path: String },
    CreateDir      { path: String, recursive: bool },
    Rename         { from: String, to: String },
    Remove         { path: String, recursive: bool },

    // --- git ---
    RepoRoot { path: String },
    Git      { cwd: String, args: Vec<String> },

    // --- watch ---
    /// 建立订阅，服务端回 `WatchId`
    WatchOpen  { dirs: Vec<String> },
    WatchSet   { id: u64, dirs: Vec<String> },
    WatchClose { id: u64 },

    // --- workspace store（M5，A4 只留位，不实现） ---
    WorkspaceList,
    WorkspaceGet    { id: String },
    WorkspacePut    { id: String, json: serde_json::Value },
    WorkspaceDelete { id: String },
}
```

**路径一律 `String`，不是 `PathBuf`。** 原因：`PathBuf` 的 serde 在非 UTF-8 路径上的表示是平台相关的，而两端可能是不同 OS。远程侧路径恒为 UTF-8 POSIX；非 UTF-8 路径由服务端在 `read_dir` 时以 lossy 形式返回并标记（与既有 `file_tree` 的 `to_string_lossy` 行为一致）。

**响应 JSON**：

```rust
#[derive(Serialize, Deserialize)]
pub enum ControlReply {
    #[serde(rename = "ok")]
    Ok(ReplyOk),
    #[serde(rename = "err")]
    Err(WireError),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOk {
    Unit,
    Pong,
    Entries(Vec<Entry>),
    Meta(Meta),
    Bool(bool),
    Path(String),
    OptPath(Option<String>),
    /// `ReadFile` 的响应：内容在 blob 里，这里只有元信息
    FileMeta { meta: Meta },
    Hits(Vec<SearchHit>),
    Output(Output),
    WatchId(u64),
    /// workspace store（M5）
    Json(serde_json::Value),
}
```

**逐方法对照表**：

| 请求 | 帧 kind | blob | 响应 `ReplyOk` | 响应帧 kind |
|---|---|---|---|---|
| `Ping` | 61 | — | `Pong` | 61 |
| `ReadDir` | 61 | — | `Entries` | 61 |
| `Stat` | 61 | — | `Meta` | 61 |
| `Exists` | 61 | — | `Bool` | 61 |
| `Canonicalize` | 61 | — | `Path` | 61 |
| `ReadFile` | 61 | — | `FileMeta` | **62**（blob = 内容） |
| `Search` | 61 | — | `Hits` | 61 |
| `WriteFile` | **62** | 内容 | `Meta`（写后的 mtime） | 61 |
| `CreateFileNew` | 61 | — | `Unit` | 61 |
| `CreateDir` | 61 | — | `Unit` | 61 |
| `Rename` | 61 | — | `Unit` | 61 |
| `Remove` | 61 | — | `Unit` | 61 |
| `RepoRoot` | 61 | — | `OptPath` | 61 |
| `Git` | 61 | — | `Output` | 61 |
| `WatchOpen` | 61 | — | `WatchId` | 61 |
| `WatchSet` / `WatchClose` | 61 | — | `Unit` | 61 |
| `Workspace*` | 61 | — | `Json` / `Unit` | 61 |

`Output.stdout`/`stderr` 是 `Vec<u8>`，走 JSON 会被 serde 序列化成数字数组 —— 一个 1MB 的 diff 变成约 4MB JSON。**优化（必做）**：`Output` 的两个字段用 `#[serde(with = "serde_bytes")]`（`Vec<u8>` → base64 字符串）。若 A4 觉得 base64 的 33% 仍然贵，可把 `Git` 的响应也走 kind 62（blob = stdout），JSON 只带 `status` 和 stderr。**裁决：v1 用 base64，简单；`git diff HEAD` 的典型量级在 100KB 内，不值得为它做第二条 blob 路径。** 若实测大 repo 上成为瓶颈，再迁到 62 —— 那是纯加法（`ReplyOk::Output` 保留，新增 `ReplyOk::OutputMeta` + blob）。

### 6.5 错误编码

`io::ErrorKind` 既不是 serde，其变体集也不稳定（`#[non_exhaustive]`）。所以过线用一个**受限的字符串枚举**：

```rust
#[derive(Serialize, Deserialize)]
pub struct WireError {
    pub kind: WireErrorKind,
    /// 人类可读，直接进通知。**不含路径以外的服务端内部细节。**
    pub msg: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    /// 超过 `max_bytes`
    FileTooLarge,
    /// 服务端上没有 git / git 起不来
    GitUnavailable,
    TimedOut,
    /// control 连接断了（客户端本地合成，不过线）
    ConnectionReset,
    /// 兜底。`msg` 带原始描述
    Other,
}
```

**映射表（两个方向都必须实现，且必须是彼此的逆）**：

| `io::ErrorKind` | `WireErrorKind` |
|---|---|
| `NotFound` | `NotFound` |
| `PermissionDenied` | `PermissionDenied` |
| `AlreadyExists` | `AlreadyExists` |
| `InvalidInput` / `InvalidFilename` | `InvalidInput` |
| `NotADirectory` | `NotADirectory` |
| `IsADirectory` | `IsADirectory` |
| `DirectoryNotEmpty` | `DirectoryNotEmpty` |
| `FileTooLarge` | `FileTooLarge` |
| `TimedOut` | `TimedOut` |
| `ConnectionReset` / `BrokenPipe` / `UnexpectedEof` | `ConnectionReset` |
| 其它 | `Other` |

反向（`WireErrorKind` → `io::Error`）：同表反查，`GitUnavailable` → `io::ErrorKind::NotFound`，`Other` → `io::ErrorKind::Other`。`msg` 一律作为 `io::Error` 的 payload。

**关键约束**：`Err` 只表示"操作没能执行"。**`git` 的非零退出码不是 `Err`** —— 它是 `Ok(Output { status: Some(1), .. })`。这一条决定了 `git_status` 的 `Option` 语义能否原样保留。

### 6.6 事件推送（`CONTROL_EVENT`, kind 63, req_id = 0）

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEvent {
    /// 文件变更。服务端按 100ms 滚动窗口合并、去重后批量推。
    /// 一个窗口内的路径去重；窗口内路径数超过 `WATCH_BURST_CAP`(1024) 时
    /// 退化成 `WatchOverflow`，客户端整树 invalidate。
    Watch { id: u64, paths: Vec<String> },
    WatchOverflow { id: u64 },

    /// M5/M6 保留位（A4 只定义，不实现）
    PaneExited { pane_id: u64, code: Option<i32> },
    AgentStatus { pane_id: u64, json: serde_json::Value },
    Preempted { by: String },
    WorkspaceChanged { id: String },
}
```

### 6.7 握手：`CONTROL_HELLO` / `CONTROL_HELLO_OK`

```rust
#[derive(Serialize, Deserialize)]
pub struct ControlHello {
    /// 客户端说的 control 方言版本。当前 1。
    pub control_version: u32,
    /// 这条 control 连接绑定的 workspace。`None` = 只用 Host RPC，
    /// 不绑 workspace（M3 的 stdio 端到端测试走这条）。
    pub workspace: Option<String>,
    /// 客户端会话 token + 主机名，用于 §10 的接管。
    pub client_token: String,
    pub client_hostname: String,
}

#[derive(Serialize, Deserialize)]
pub struct ControlHelloOk {
    pub control_version: u32,
    /// 服务端的 `PROTOCOL_VERSION`（冗余，方便诊断）
    pub protocol_version: u32,
    /// 服务端二进制版本，display only
    pub build: String,
    /// 服务端的路径分隔符（客户端据此实现 `Host::separator`）
    pub separator: char,
    /// 服务端 `$HOME`（首页"新建 workspace 落在 `~`"要用）
    pub home: String,
    /// 能力位，见 §6.9
    #[serde(default)]
    pub features: Vec<String>,
}
```

握手失败（`control_version` 不匹配）时服务端回 `CONTROL_HELLO_OK` 里带自己的版本然后**关连接**，客户端据此报明确错误 —— 不发 `CONTROL_RESPONSE`，因为 HELLO 没有 req_id。

### 6.8 超时

| 类别 | 方法 | 默认 deadline |
|---|---|---|
| 探活 | `Ping` | 5s |
| 元数据 | `ReadDir` `Stat` `Exists` `Canonicalize` `RepoRoot` `Watch*` | **5s** |
| 内容 | `ReadFile` `WriteFile` | **30s** |
| 变更 | `CreateFileNew` `CreateDir` `Rename` `Remove` | **10s** |
| git | `Git` | **20s** |
| 搜索 | `Search` | **20s** |
| workspace | `Workspace*` | 10s |

**超时行为**：

1. 客户端本地放弃，向服务端发 `CONTROL_CANCEL { req_id }`
2. 调用方拿到 `io::Error(TimedOut)`
3. 服务端收到 cancel **尽力**中止（`Git` 杀子进程；fs 操作不可中断，跑完丢弃结果）—— **不保证**
4. 迟到的响应到达时 req_id 已不在表里，静默丢弃（§6.3）
5. **超时不断连**。设计文档 §17"单个 `Host` RPC 超时 → 该请求返回 `TimedOut`，不影响其它请求"

**连接层 keepalive**（与 RPC 超时正交）：

| 项 | 值 |
|---|---|
| 客户端 `Ping` 间隔 | 15s（仅在 30s 内无任何入站帧时才发） |
| 判死阈值 | **45s** 无任何入站帧 |
| 判死后 | 整个 workspace 转 `Reconnecting`（设计文档 §17：control 断 = workspace 断） |

### 6.9 `PROTOCOL_VERSION` bump 到 3

**先说清楚：按 `protocol.rs:42-48` 自己写的规则，新增 kind 是 additive，本来不需要 bump。** 设计文档 §8 直接说"bump 到 3"，没给理由。真实理由是另一个：

> **v3 = "我会说 control 方言"这一能力位可以被正向查询。**

不 bump 的话，客户端只能靠"开 control 连接 → 收到 unknown kind 错误 → 连接被对端关掉"来探测。而未知 kind 在既有实现里**会直接杀掉那条连接**（`InvalidData`），且与真正的 desync 无法区分。代价是一次无谓往返 + 一条误导性错误日志。

同时**新增 `DaemonVersion.features`**（additive，`#[serde(default)]`）：

```rust
pub struct DaemonVersion {
    pub protocol: u32,
    #[serde(default)]
    pub build: String,
    /// 细粒度能力位。v3 之后新增能力不再 bump 版本号。
    /// 已定义：`"control"`、`"host-rpc"`、`"workspace-store"`、`"stdio-bridge"`
    #[serde(default)]
    pub features: Vec<String>,
}
```

**兼容性影响**：

| 组合 | 行为 |
|---|---|
| v3 GUI ↔ v3 daemon/server | 正常 |
| v2 GUI ↔ v3 daemon | ✅ **无降级**。v2 GUI 从不发 60-63；v3 daemon 对既有 kind 的解码逐字不变 |
| v3 GUI ↔ v2 **本地** daemon | ❌ 本地 daemon 不认 60-63。走既有 `spawn::ensure_running` 的不兼容询问路径。**本地 daemon 必须先升到 v3，远程 workspace 才能用** |
| v3 GUI ↔ v2 **远程** server | ❌ 同上，但这是"远程需要升级"的独立提示，不能与本地 daemon 的提示混为一谈 |

**两次独立握手**（设计文档没说清）：远程 workspace 涉及**两个** `PROTOCOL_VERSION` 检查 ——
1. GUI ↔ 本地 daemon（既有 `spawn::ensure_running`，不变）
2. GUI ↔ 远程 tty7-server（**端到端**，穿过本地 daemon 的纯转发层）

第 2 次必须是端到端的，否则本地 daemon 就得解析 control 流，违背 §6 架构图的"RemoteRouter 纯字节转发，不解析"（§11 第 16 条）。

---

## 7. `Stream` 抽象（M4）

### 7.1 复核结论：设计文档 §4 说错了，但结论意外正确

| 设计文档 §4 说 | 实际（`daemon/transport.rs:44`, `:397`） |
|---|---|
| "传输抽象：`Stream = Read + Write + try_clone`" | `Stream` 是 **type alias**：Unix `= UnixStream`，Windows `= TcpStream`。不是 trait |
| "多一种传输形态不破坏上层" | **结论成立，理由完全不同** |

真实理由：**远程流根本不经过 `transport::Stream`。**

设计文档 §6 自己写了"GUI 侧传输代码一行不改…本地 daemon 对远程流只做字节转发"。所以：

```
GUI ──transport::Stream (UnixStream/TcpStream，不动)──> 本地 daemon
                                                          │
                                       RemoteLink (新, async) ──> SSH channel / WSL stdio
```

**新抽象住在本地 daemon 里，不在 GUI 里。** 而 daemon 的 SSH 侧已经是 tokio async（russh 就是），所以新抽象应该是 **async 的，且应复用已经存在的形状** —— `daemon/ssh/connect.rs:29-33` 的 `Transport` 枚举（`Tcp` / `Process(ProcessStream)` / `Channel(russh::ChannelStream<Msg>)`，各自 delegate `AsyncRead`/`AsyncWrite`）。

### 7.2 `RemoteLink`（daemon 侧，M4 真正要引入的东西）

**位置**：`crates/tty7-core/src/daemon/remote_link.rs`
**负责人**：A4（M4 阶段；M3 只需要 `Stdio` 变体）

```rust
/// 本地 daemon 与远程 tty7-server 之间的一条逻辑流。
///
/// 枚举而非 trait object —— 与既有 `ssh::connect::Transport` 同款理由：
/// 每个变体的 `AsyncRead`/`AsyncWrite` 是直接 delegate，没有 vtable。
pub enum RemoteLink {
    /// 首选：`direct-streamlocal@openssh.com` 直连远程 daemon.sock。
    /// russh: `client::Handle::channel_open_direct_streamlocal(socket_path)`
    /// （已确认存在于 `russh/src/client/mod.rs:854`，签名
    ///  `async fn(&self, S: Into<String>) -> Result<Channel<Msg>, Error>`）
    StreamLocal(russh::ChannelStream<russh::client::Msg>),

    /// 回退：session channel + `exec tty7-server --stdio`。
    /// 与 StreamLocal 同类型，语义不同 —— 分开是为了让日志/诊断能区分。
    SessionExec(russh::ChannelStream<russh::client::Msg>),

    /// WSL：`wsl.exe -d <distro> -- tty7-server --stdio` 的 stdin/stdout。
    /// 直接复用既有 `ssh::connect::ProcessStream`（:82-113）。
    Wsl(crate::daemon::ssh::connect::ProcessStream),

    /// CI / 端到端测试：本机 `tty7-server --stdio` 子进程。
    /// 与 Wsl 同类型，分开是为了让测试路径显式。
    LocalStdio(crate::daemon::ssh::connect::ProcessStream),
}

impl tokio::io::AsyncRead  for RemoteLink { /* 四路 delegate */ }
impl tokio::io::AsyncWrite for RemoteLink { /* 四路 delegate，poll_shutdown 全实现 */ }

impl RemoteLink {
    /// 这条流的诊断标签，进日志和状态条。
    pub fn kind_label(&self) -> &'static str { .. }
}
```

**为什么四个变体只有两种底层类型**：诊断价值。`StreamLocal` 失败要触发一次性回退探测（设计文档 §7.1"结果缓存在 `SshConnection` 上"），日志里必须能区分"streamlocal 断了"和"stdio bridge 断了"。

### 7.3 现有调用方的用法能否满足

实测调用点（`transport::Stream` 上的具体方法，全树只有这几处）：

| 位置 | 用法 | 在新架构下 |
|---|---|---|
| `terminal/remote.rs:333` | `stream.try_clone()` | **仍是本地 `UnixStream`**，不受影响 |
| `daemon/server.rs:235` | `read_stream.try_clone()` | 本地 accept 侧，不受影响 |
| `terminal/remote.rs:1059` | `shutdown(Shutdown::Write)`（`kill_pane`） | 本地流，不受影响 |
| `terminal/remote.rs:1488` | `writer.shutdown(Shutdown::Both)` | 本地流，不受影响 |
| `terminal/remote.rs:828` | `set_read_timeout` | 本地流，不受影响 |
| `terminal/view.rs:7123` | `set_read_timeout(250ms)`（DEC 2026 同步更新 deadline） | 本地流，不受影响 |
| `daemon/spawn.rs:179` | `set_read_timeout(HANDSHAKE_TIMEOUT)` | 本地流，不受影响 |

**结论：`transport::Stream` 一个字都不用改，`try_clone` / `shutdown` / `set_read_timeout` 全部保留。** 这是 M4 最大的好消息，也是设计文档"多一种传输形态不破坏上层"的真实成因。

### 7.4 服务端侧确实需要一个阻塞 trait

`tty7-server --stdio` 模式下，服务端要把自己的 `stdin`/`stdout` 当成一条流用，而 `server.rs` 的 accept 循环是阻塞 + `try_clone` 的形状。

**位置**：`crates/tty7-core/src/daemon/duplex.rs`
**负责人**：A5

```rust
/// 服务端一条双工流。抽掉 `try_clone` —— stdin/stdout 天然是两个 handle，
/// 无法 clone 出一个双向流。改成构造期 split。
pub trait Duplex: Send + 'static {
    type Read: io::Read + Send + 'static;
    type Write: io::Write + Send + 'static;
    fn split(self) -> io::Result<(Self::Read, Self::Write)>;
}

impl Duplex for std::os::unix::net::UnixStream { /* try_clone */ }
impl Duplex for std::net::TcpStream           { /* try_clone */ }

/// `--stdio` 模式：进程的 stdin/stdout。
pub struct StdioDuplex;
impl Duplex for StdioDuplex {
    type Read = std::io::Stdin;
    type Write = std::io::Stdout;
    fn split(self) -> io::Result<(Stdin, Stdout)> { Ok((io::stdin(), io::stdout())) }
}
```

**改动量**：`daemon/server.rs:235` 的 `read_stream.try_clone()?` → `stream.split()?`，一行。这是 M4 **唯一**需要改的既有传输调用点。

---

## 8. 远程 socket 路径与 `--stdio` 桥

**负责人**：A5

| 项 | 值 |
|---|---|
| 首选 socket | `$XDG_RUNTIME_DIR/tty7/daemon.sock` |
| 无 `XDG_RUNTIME_DIR` | `~/.local/share/tty7/daemon.sock` |
| 超长路径回退 | 沿用 `transport::socket_path_for` 的短路径 + FNV-1a64 哈希（`transport.rs:59-90`），**原样搬，不重写** |
| 权限 | socket 0600，目录 0700 |
| 粒度 | 一台机器一个 `tty7-server`（per user） |

**`tty7-server --stdio`** 是一个纯字节转发的小进程：把自己的 stdin/stdout 接到上面那个 socket，不解析任何内容。它**不是** server 本体 —— server 本体是 `tty7-server --daemon`。

**`--stdio` 的三个用途**：
1. `AllowStreamLocalForwarding no` 时的回退（设计文档 §7.1）
2. WSL（无 SSH）
3. **CI 端到端测试**（设计文档 §18 的核心杠杆）

---

## 9. 模块归属与 agent 分工

### 9.1 crate 布局（A1 产出）

**A1 已落地**（本契约成文时 working tree 里的实际形状）：

```
Cargo.toml                       # workspace root，[patch] 留在这里
src/                             # tty7（GUI bin），依赖 gpui + tty7-core
crates/tty7-core/src/lib.rs      # pub mod core; pub mod daemon;  —— 无 gpui
crates/tty7-core/src/core/       # agent_hooks cli_agent config crash git gitignore
                                 # osc proc session shells threads window_state worktree
crates/tty7-core/src/daemon/     # mod pane pidfile procinfo protocol remote server
                                 # shell_integration spawn transport winproc + ssh/
crates/tty7-server/src/main.rs   # headless bin，只依赖 tty7-core
```

**模块路径刻意保持不变**（`crate::core::config`、`crate::daemon::protocol`），GUI crate 以同名 re-export，所以两侧调用点读起来一样。新增的 `host` 模块**必须遵循同一约定**：`crates/tty7-core/src/host/`，`lib.rs` 加 `pub mod host;`。

**已确认的 A1 产出**（不要重做）：

| 模块 | 内容 |
|---|---|
| `core::git` | `probe` / `branch_name` / `git(cwd, args) -> Option<String>`（带 `GIT_OPTIONAL_LOCKS=0` + `stdin(null)`） |
| `core::gitignore` | `GitignoreChain { is_ignored(path, is_dir, root) -> bool, absorb, clear, len }` |
| `core::window_state` | A1 把它搬进了 core（设计文档 §11 说留 GUI）。**不是错误**，接受现状 |

### 9.2 文件归属表（新增文件）

| 路径 | crate | 内容 | 负责人 |
|---|---|---|---|
| `crates/tty7-core/src/host/mod.rs` | core | `Host` trait + 辅助类型 + `HostId` + `fnv1a64` | **A2** |
| `crates/tty7-core/src/host/local.rs` | core | `LocalHost`（内部用 `core::git` + `core::gitignore::GitignoreChain`） | **A2** |
| `crates/tty7-core/src/host/conformance.rs` | core | §10 的共用测试套 | **A2** 定义骨架 + 全部 case |
| `src/ui/host_ops.rs` | GUI | §5 门面 | **A2** |
| `src/ui/host_registry.rs` | GUI | `HostId → SharedHost` 的 `gpui::Global` | **A2** |
| `crates/tty7-core/src/daemon/control.rs` | core | control wire（kind / 帧 / enum / 编解码 / round-trip 测试） | **A4**（第一个 commit） |
| `crates/tty7-core/src/host/remote.rs` | core | `RemoteHost`（control 客户端 + req_id 表 + 超时） | **A4** |
| `crates/tty7-core/src/daemon/remote_link.rs` | core | §7.2（M4） | **A4** |
| `crates/tty7-core/src/host/server.rs` | core | control 服务端 handler（`ControlRequest` → `LocalHost`） | **A5** |
| `crates/tty7-core/src/daemon/duplex.rs` | core | §7.4 `Duplex` trait | **A5** |
| `crates/tty7-server/src/main.rs` | server | `--daemon` / `--stdio` / `agent-hook` 三个子命令（A1 已建空壳） | **A5** |
| `src/ui/{file_tree,code_editor,app}.rs`, `src/terminal/{git_status,git_diff,view}.rs` | GUI | 调用点改造 | **A3** |

**A2 的额外任务**：`core::git::git` 现在返回 `Option<String>`，丢掉了 stderr 和退出码。`Host::git` 需要 `Output`。做法：在 `core::git` 里**新增** `pub fn git_output(cwd, args) -> io::Result<Output>` 作为底层，把既有 `git()` 改写成它的薄封装（`git_output(..).ok().filter(Output::success).map(|o| o.stdout_trimmed())`）—— 既有调用方零改动。
| `src/ui/host_ops.rs` | GUI | **新** —— §5 | **A2** 定义，A3 消费 |
| `src/ui/host_registry.rs` | GUI | **新** —— `HostId → SharedHost` 的 `gpui::Global` | A2 |
| `src/ui/{file_tree,code_editor,app}.rs`, `src/terminal/{git_status,git_diff,view}.rs` | GUI | 改造调用点 | **A3** |

### 9.3 五个 agent 的分工与依赖

| Agent | 范围 | 依赖 | 完成标志 |
|---|---|---|---|
| **A1** | crate 拆分（M1）+ 三处提取（gitignore / git helper / session 数据） | — | `cargo test --workspace` 全绿；`tty7-server` 空壳能在无头 Linux 上 `--version` |
| **A2** | `Host` trait + `LocalHost` + `HostOps` + `HostRegistry` + conformance 骨架 | A1 完成 | conformance 套在 `LocalHost` 上全绿 |
| **A3** | 调用点改造（M2） | A2 的**签名**冻结（不必等实现） | 现有全部测试逐条绿 + §1 豁免清单外零行为变化 |
| **A4** | `control.rs` wire + `RemoteHost` + `remote_link.rs` | A1 完成；**wire 先落地**（见下） | wire round-trip 测试全绿；`RemoteHost` 在 conformance 套上全绿 |
| **A5** | control 服务端 handler + `tty7-server` + `duplex.rs` + stdio 桥 | A4 的 **wire 模块**（不必等 `RemoteHost`） | stdio 端到端：本机起子进程跑通全套 conformance |

**同步点**：

```
A1 ────┬──> A2 ──签名冻结──> A3
       │
       └──> A4 第一个 commit = control.rs wire（纯类型 + 编解码 + 测试）
                    │
                    ├──> A4 继续做 RemoteHost
                    └──> A5 消费 wire，做服务端
```

**A4 的第一个 commit 必须只含 wire**，不含 `RemoteHost`。这是 A4/A5 并行的唯一前提。

**互不重叠保证**：

| 文件 | 唯一写者 |
|---|---|
| `crates/tty7-core/src/host/{mod,local,conformance}.rs` | A2 |
| `src/ui/{host_ops,host_registry}.rs` | A2 |
| `crates/tty7-core/src/host/remote.rs`, `crates/tty7-core/src/daemon/{control,remote_link}.rs` | A4 |
| `crates/tty7-core/src/host/server.rs`, `crates/tty7-core/src/daemon/duplex.rs`, `crates/tty7-server/**` | A5 |
| `src/ui/{file_tree,code_editor,app}.rs`, `src/terminal/*` | A3 |
| 搬迁、`Cargo.toml`、`.github/**` | A1（A1 完成后，各 agent 只加自己的依赖行） |

**三处共享文件的追加规则**（避免撞车）：

| 文件 | 规则 |
|---|---|
| `crates/tty7-core/src/lib.rs` | 只有 A2 加 `pub mod host;`，一行 |
| `crates/tty7-core/src/daemon/mod.rs` | A4 加 `pub mod control;` + `pub mod remote_link;`；A5 加 `pub mod duplex;`。各加各的行，不重排 |
| `crates/tty7-core/src/core/git.rs` | 只有 A2 动（加 `git_output`），A3 只读 |

---

## 10. conformance 测试规格

**位置**：`crates/tty7-core/src/host/conformance.rs`
**负责人**：A2 定义 + 落地全部 case，A4/A5 只接线

### 10.1 组织方式：`&dyn Host` + 宏展开成独立 `#[test]`

**不用泛型函数** —— `Host` 已经 object-safe（这是 §1 保持阻塞签名的直接收益），`&dyn Host` 更简单且能验证 object safety 本身。

**不用单个大函数** —— 失败必须定位到具体 case，一个 `Vec<Failure>` 汇总会让 CI 输出难读。

```rust
// conformance.rs

/// 每个 case 的签名。`h` 是被测 host，`sandbox` 是一个该 host 上的空目录
/// （本地是 tempdir，远程是服务端上的 tempdir）。
pub type Case = fn(h: &dyn Host, sandbox: &Path);

/// 全部 case 的注册表：`(名字, 函数)`。
pub const CASES: &[(&str, Case)] = &[
    ("read_dir_lists_and_sorts",            read_dir_lists_and_sorts),
    ("read_dir_missing_is_not_found",       read_dir_missing_is_not_found),
    // ...
];

/// 为一个 host 工厂展开出全部 `#[test]`。
#[macro_export]
macro_rules! host_conformance_suite {
    ($modname:ident, $factory:expr) => {
        mod $modname {
            $crate::host::conformance::declare_cases!($factory);
        }
    };
}
```

`declare_cases!` 用一个 `const` 列表配合 `seq`-风格展开做不到 —— Rust 宏不能遍历 `const` 数组。**实际做法**：把 case 名字写进宏本身。

```rust
// conformance.rs 里
#[macro_export]
macro_rules! for_each_host_case {
    ($cb:ident) => {
        $cb!(read_dir_lists_and_sorts);
        $cb!(read_dir_missing_is_not_found);
        $cb!(stat_reports_len_and_mtime);
        // ... 一行一个
    };
}

#[macro_export]
macro_rules! host_conformance_suite {
    ($modname:ident, $factory:expr) => {
        mod $modname {
            macro_rules! __case {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        let (h, sandbox) = ($factory)();
                        $crate::host::conformance::$name(&*h, sandbox.path());
                    }
                };
            }
            $crate::for_each_host_case!(__case);
        }
    };
}
```

**用法**：

```rust
// host/local.rs 的 tests
host_conformance_suite!(local, || (LocalHost::new(), TempDir::new().unwrap()));

// host/remote.rs 的 tests（A4）
host_conformance_suite!(remote_stdio, || spawn_stdio_server_and_host());
```

**新增一个 case = 改两处**：`for_each_host_case!` 加一行，写一个同名 `pub fn`。两个 host 自动都跑上。忘记加进宏 = case 不跑 —— 加一个元测试 `every_pub_case_is_registered` 用 `include_str!` + 计数比对来兜底。

### 10.2 Case 清单

**fs 读**

| case | 断言 |
|---|---|
| `read_dir_lists_and_sorts` | 目录在前、然后 lowercase 名字序；与 `file_tree::sort_entries` 逐字一致 |
| `read_dir_includes_hidden` | `.hidden` 出现在结果里（过滤是 UI 的事） |
| `read_dir_missing_is_not_found` | `ErrorKind::NotFound` |
| `read_dir_on_a_file_errors` | `NotADirectory`（Windows 上可能是 `Other`，断言"是 Err"即可） |
| `read_dir_marks_dotgit_ignored` | `.git` 的 `ignored == true`，即使没有 `.gitignore` |
| `read_dir_applies_gitignore_chain` | 根 `.gitignore` 有 `*.log`，`src/.gitignore` 有 `!keep.log` → `drop.log` ignored、`src/keep.log` 不 ignored（照抄 `file_tree.rs:1580-1590` 的既有 fixture） |
| `read_dir_without_root_ignores_nothing` | `root = None` 时除 `.git` 外全 `ignored == false` |
| `read_dir_symlink_to_dir_is_dir` | `is_dir == true` 且 `is_symlink == true`（Windows 跳过） |
| `stat_reports_len_and_mtime` | `len` 精确；`mtime` 有值 |
| `stat_missing_is_not_found` | `NotFound` |
| `exists_matches_stat` | `exists(p) == stat(p).is_ok()`，覆盖存在/不存在/无权限三态 |
| `canonicalize_resolves_dotdot` | `a/../b` → `b` |
| `read_file_roundtrips_bytes` | 含 NUL、非 UTF-8 字节、10MB 内容各一 |
| `read_file_over_max_bytes_errors` | `FileTooLarge`，且**不传输内容**（远程侧断言帧大小） |
| `read_file_on_a_dir_errors` | Err |

**fs 写**

| case | 断言 |
|---|---|
| `write_file_creates_and_overwrites` | 新建 + 覆盖，返回的 `Meta.mtime` 与随后 `stat` 一致 |
| `write_file_to_missing_parent_errors` | `NotFound`，且**不创建父目录** |
| `create_file_new_rejects_existing` | `AlreadyExists` |
| `create_dir_non_recursive_needs_parent` | 无父目录时 `NotFound` |
| `create_dir_recursive_makes_chain` | 多级一次建成；已存在时 `Ok` |
| `rename_moves_and_rejects_existing_target` | 目标存在 → `AlreadyExists`（**实现保证**，不靠调用方先探） |
| `rename_across_dirs_works` | 同一 host 内跨目录 |
| `remove_file_then_missing` | 删后 `exists == false`；再删 `NotFound` |
| `remove_dir_non_recursive_needs_empty` | 非空且 `recursive = false` → `DirectoryNotEmpty` |
| `remove_dir_recursive_clears_tree` | 深树一次清掉 |

**git**

| case | 断言 |
|---|---|
| `repo_root_finds_nearest_git` | 深层子目录 → repo 根；repo 外 → `Ok(None)`（**不是 Err**） |
| `repo_root_handles_worktree_file` | `.git` 是文件（linked worktree）时也认 |
| `git_status_porcelain_reflects_changes` | 在 sandbox 里 `git init` + 造一个改动，`git(["status","--porcelain"])` 的 stdout 含该文件 |
| `git_nonzero_exit_is_ok_not_err` | `git(["rev-parse","--show-toplevel"])` 在非 repo 目录下 → `Ok(Output { status: Some(128), .. })`，**不是 `Err`**。这一条守住 §6.5 的核心约定 |
| `git_missing_binary_is_err` | `PATH` 剥掉 git 后 → `Err`（本地可测；远程侧用一个注入点） |
| `git_optional_locks_env_is_set` | 用 `git(["config","--get","--type=bool","x"])` 测不到 —— 改用 `git(["var","-l"])` 的输出或一个 stub `git` shim 断言 `GIT_OPTIONAL_LOCKS=0` 在 env 里 |
| `git_stdin_is_null` | 造一个会 prompt 的场景（如无 credential helper 的 `git fetch` 到需要密码的 URL），断言**立即失败**而非挂起（带 10s 硬超时） |

**路径算术**

| case | 断言 |
|---|---|
| `join_uses_host_separator` | `host.join("/a", "b")` 在远程 host 上恒为 `/a/b`，即使客户端是 Windows |
| `is_absolute_matches_host_semantics` | 远程 host 上 `/home` 是绝对；Windows 客户端的 `Path::is_absolute` 会说 false，`Host::is_absolute` 必须说 true |

**search**

| case | 断言 |
|---|---|
| `search_is_breadth_first` | 浅层命中排在深层前 |
| `search_skips_ignored_dirs` | `node_modules`（在 `.gitignore` 里）内的命中不出现 |
| `search_respects_limit` | `limit = 3` 时恰好 3 条 |
| `search_respects_max_dirs` | 大树 + `max_dirs = 2` 时提前停，不挂起 |

**watch**

| case | 断言 |
|---|---|
| `watch_reports_create_and_delete` | 建文件 → 事件里含它的路径（2s 内） |
| `watch_is_non_recursive` | 子目录内的变更**不**上报（除非该子目录也在集合里） |
| `watch_set_dirs_adds_and_drops` | `set_dirs` 后新目录有事件、旧目录无事件 |
| `watch_coalesces_within_window` | 100ms 内 50 次写 → 事件批数 ≤ 2 且路径去重 |
| `watch_drop_unsubscribes` | drop `WatchSub` 后再改文件，无事件（远程侧断言服务端 watcher 已释放） |

**连接语义（只有 `RemoteHost` 有意义，`LocalHost` 上是平凡真）**

| case | 断言 |
|---|---|
| `is_connected_is_true_when_healthy` | |
| `id_is_stable_across_calls` | |
| `separator_matches_hello` | |

### 10.3 sandbox 工厂的契约

```rust
/// 两个 host 的工厂都必须满足：
/// - 返回一个**空**目录，测试结束时清理
/// - 该目录在 `h` 的命名空间里（远程时是服务端上的路径）
/// - `git` 可用（工厂负责在里面 `git init` 或提供一个能 init 的环境）
pub trait Sandbox {
    fn path(&self) -> &Path;
}
```

### 10.4 conformance 之外的测试（设计文档 §18 的其它行）

| 层 | 位置 | 负责人 |
|---|---|---|
| control wire round-trip | `daemon/control.rs` 的 `mod tests`，照抄 `protocol.rs:1438` 的真 TcpListener 双线程模式 + `:1503` 的逐 variant Cursor 模式 | A4 |
| 帧解码的**恶意输入** | `json_n` 溢出、`payload_len < 12`、`CONTROL_EVENT` 带非零 req_id、未知 kind —— 每条一个 `#[test]`，断言 `InvalidData` 而非 panic | A4 |
| 版本 skew 握手 | `control_version` 不匹配的分支 | A4 |
| 传输：stdio 回退 | 起真的 `tty7-server --stdio` 子进程 | A5 |
| 端到端 | 开 workspace → 开 pane → 断开 → 重连补屏 → 接管 | A5（M3 范围内只做前两步） |
| 回归护栏 | **M1 的现有全部测试逐条绿，不许改测试来适配** | A1 |

### 10.5 M2 的回归护栏（M2 不是零行为变化，所以要额外的）

| 守卫 | 内容 |
|---|---|
| `worktree add` / `list` 全路径 | §4.4 给 `worktree` 加了 `GIT_OPTIONAL_LOCKS=0`，必须实测 |
| `code_editor` mtime 冲突检测 | 保存→外部改→重载三态，异步化后仍然正确（`MTime` 纳秒精度是为它设计的） |
| `file_tree` 乐观更新回滚 | 新建/改名/删除失败时行必须恢复 |

### 10.6 CI grep 守卫（§4.3 的执行手段）

✅ **已落地**：`.github/scripts/check-host-boundary.sh`，接在 `ci.yml` 的 `host boundary (§10.6)` job 上(**故意不设 required** —— required 只有 `rustfmt` + 三个 `build & test (<target>)`，加进去会当场卡死所有在开的 PR)。

本节原来写的裸 grep **不能直接用**,干净树上 42 个命中、一个都不是 git。脚本对它做了两处修正:

| 问题 | 裸 grep | 脚本 |
|---|---|---|
| 测试体 | `grep -v '#\[cfg(test)\]'` 只滤掉属性行本身,后面整个 test mod 还在扫描范围里(42 中的 23 行) | 扫到收尾的 `#[cfg(test)] mod …` 就截断。**只认后面紧跟 `mod ` 的那一个** —— `presets.rs:540` / `search.rs:513` 是挂在函数上的 `#[cfg(test)]`,截在那儿会把真实代码整段漏掉 |
| 本地路径 | 无豁免,19 处必然本地的路径全报 | (文件, 模式) 二元组允许清单,每条写明"为什么这个路径不可能是远程的";按模式而不是按文件豁免,所以 `app.rs` 豁免了 `create_dir_all` 之后再出现 `std::fs::read_to_string` 照样红 |

清单目前 19 行,分五类:主题/预设(`presets.rs`、`app.rs` 的打开主题目录)、shell 历史(`history.rs`)、补全(`completion.rs` 远程走 `cwd: None` 分支、`signature.rs` 的 bundled specs)、SSH 私钥读取(`ssh_prompt.rs`、`ssh_connect.rs`)、剪贴板截图暂存(`view.rs` 的 `temp_dir()`)。加一条是有意行为:**路径有可能是 workspace 路径,答案就是 `Host`,不是清单。**

脚本自身还守两件事:扫描根不存在 → exit 2;扫到的文件数 < 10 → exit 2。"扫了个空然后报绿"比噪音守卫更糟。

---

## 11. 设计文档勘误清单

复核过的每一条，标明设计文档哪一节、实际是什么、本契约怎么处理。

| # | 设计文档 | 说的 | 实际 | 本契约 |
|---|---|---|---|---|
| 1 | §4 表格 "传输抽象" | "`Stream = Read + Write + try_clone`"，暗示是 trait | **type alias**：`daemon/transport.rs:44` Unix `= UnixStream`，`:397` Windows `= TcpStream` | §7.1 |
| 2 | §4 同上 | "多一种传输形态不破坏上层" | **结论成立，理由完全不同**：远程流根本不经过 `transport::Stream`，它止步于本地 daemon | §7.1-7.3 |
| 3 | §9 | "这些调用点现在**全部已经**在 background executor 上跑…调用点的结构一行不用动" | **只有** `file_tree` 的 `read_dir` + gitignore（`request_load` :448-479）。`code_editor` 9 处、`file_tree` 变更操作 5 处、`app.rs` 的 git 2 处**全在 UI 线程同步跑** | §1，含完整清单 + 豁免的行为变化表 |
| 4 | §19 | "M1 / M2 是纯重构、零行为变化" | **M2 不是**。异步化必然带来 §1 表里那 5 类可见变化 | §1，把变化显式列成豁免清单 |
| 5 | §9 watch 那行 | 把"只 watch 已展开目录、非递归"写成现状 | `file_tree.rs:413` 是 `RecursiveMode::**Recursive**` watch 所有 root。（`code_editor.rs:520` 确实是 NonRecursive —— 两个 watcher 行为不同） | §2 `WatchSub`，明确这是**新行为** |
| 6 | §9 `git` 约定 | "`GIT_OPTIONAL_LOCKS=0` 的只读约定（现在在 `git_status::git` helper 里）" | **三套出口**：`git_status.rs:332`（有）、`worktree.rs:63`（**无**，且返回 `Result<String,String>`）、`app.rs:3995`（**无**，用 `current_dir`，**UI 线程**） | §4.4 统一表 + 三个调用方的逐一适配 |
| 7 | §8 帧表 | 大 payload 形态 = `[u64 req_id][raw bytes]` | **承载不了参数**。`write_file` 要路径，`read_file` 响应要 `Meta` | §6.2 改为 `[u64 req_id][u32 json_len][JSON][raw]` |
| 8 | §8 | "`PROTOCOL_VERSION` bump 到 **3**"，未给理由 | 按 `protocol.rs:42-48` 自述规则，**新增 kind 是 additive，不需要 bump** | §6.9 给出真实理由（能力探测位）+ 新增 `features` 字段让后续能力不再 bump |
| 9 | §9 方法表 | 没有 `search` | `file_tree.rs::TreeLoader::search`(:177+) BFS 最多 2000 目录。逐目录 RPC = 2000 次往返 | §2 新增 `Host::search`，服务端执行 |
| 10 | §9 方法表 | 缺 `exists` / `canonicalize` / `create_file_new` / `create_dir(recursive)` | 既有调用点用得到：`file_tree:963`(`exists`) `:998`(`is_dir`) `:953`(`create_new`)、`code_editor:328`(`canonicalize`)、`worktree` 的 `.tty7/` 要 `create_dir_all` | §2 全部补上 |
| 11 | §11 | "`git_status` 的 shell-out helper **搬**进 tty7-core" 与 "留在 GUI crate：`terminal/*`" | 自相矛盾 —— `git_status.rs` 就在 `src/terminal/` | ✅ A1 已按"提取"解决：`core::git`（`probe`/`branch_name`/`git`），`git_status.rs` 本体留 GUI |
| 12 | §11 | "gitignore 解析…搬" | **不是独立模块**，是 `ui/file_tree.rs::TreeLoader::is_gitignored`(:142-175) 的内联实现 | ✅ A1 已提取为 `core::gitignore::GitignoreChain` |
| 12b | §11 | "留在 GUI crate：`core/{actions, window_state, update}`" | A1 把 `window_state` 搬进了 core | 接受现状，不回滚 |
| 13 | §9 表格 | 引 `ui/app.rs:3895` 的 agent diff | 实际 **3978-4015**（`send_git_diff_to_agent`） | §1 表已用真实行号 |
| 14 | §4 | 引 `ui/file_tree.rs:628` 的 repo root | 实际 **626-630**（`repo_root_for`）。另有第二份：`core/worktree.rs:58-60` `is_inside_repo`（返回 `bool`） | §2 `Host::repo_root` 统一两份 |
| 15 | §6 图 | "RemoteRouter ◄── 新：**纯字节转发，不解析**" | 与 §12 的远程 `ensure_running` 潜在冲突：本地 daemon 若要做远程版本握手就必须解析 | §6.9：**GUI 端到端握手**，router 保持纯转发。远程 workspace 有**两次独立**版本检查 |
| 16 | — | 未提及 | `mod kind`（`protocol.rs:974`）是**私有**的，跨模块加 kind 需要先改可见性 | §6.1：control kind 定义在 `control.rs` 自己的 `pub mod kind` 里，不动 `protocol::kind` |
| 17 | — | 未提及 | kind **13** 是**退役**号（曾是 `SPAWN_MANAGED_SSH`），不是空闲号。复用它会让 pre-WS2 daemon 静默 mis-spawn 一个 pane | §6.1：退役号永不复用；选 60-63 |
| 18 | — | 未提及 | **跨 OS 路径算术**：Windows 客户端上 `PathBuf::join("/home/me", "src")` → `/home/me\src`；`"/home/me".is_absolute()` → `false` | §4.3 完整禁用/可用表 + §2 的 `Host::join` / `is_absolute` |
| 19 | — | 未提及 | `Output.stdout` 走 JSON 会膨胀 ~4× | §6.4：`serde_bytes` base64 |
| 20 | 任务简报 | `local_cwd()` 挡板 8 处 | **9 处**：`app.rs:2827/2932/3297/3458/3988/5796`、`view.rs:3798/4590/4750`（漏了 `app.rs:2932`） | A3 按 9 处处理 |

**复核通过、无偏差的**（不必再查）：

| 项 | 结论 |
|---|---|
| §11"`src/daemon/` 依赖的 core 模块只有 7 个" | ✅ 实测正好 `agent_hooks` `cli_agent` `config` `osc` `proc` `shells` `threads` |
| 帧格式 / `MAX_FRAME = 64 MiB`(:33) / `write_frame`(:1064) / `read_frame`(:1081) / `take_frame`(:1105) | ✅ |
| 热路径零序列化、冷路径 JSON tuple（`Spawn` → `(cwd, size)`, :1147） | ✅ |
| `PROTOCOL_VERSION = 2`(:59)，bump 规则 :42-57 | ✅ |
| 未知 kind → `InvalidData`（:1287-1292） | ✅ |
| 一条连接一个 pane、控制类短连接、无 req_id、无多路复用 | ✅ |
| round-trip 测试模式 :1438 / :1503 | ✅ |
| russh `channel_open_direct_streamlocal` 存在于 fork `0d1d073` 的 `client/mod.rs:854`，签名如简报所述 | ✅ |
| `file_tree` 的 `read_dir` + gitignore 在 background（:456-466） | ✅ |
| `GitStatusCache` 5 张 `PathBuf` 表 + `impl gpui::Global`（:128-149） | ✅ |
| 远程路径补全的分流写法（`view.rs:3953-4020`）可作模板 | ✅ 已定为 §5 的强制模式 |
| `daemon/ssh/connect.rs:29-33` 已有 `Transport` 枚举（Tcp/Process/Channel），是 §7.2 的现成形状 | ✅ 新发现，直接复用 |

---

## 12. 明确不在本契约内

留给后续里程碑，本波 agent **不要**动：

| 项 | 归属 |
|---|---|
| 安装 / 下载 / sha256 / `uname` 解析 | M4 |
| SSH 侧的 `RemoteRouter` 接线、连接复用 | M4 |
| workspace store 的服务端实现（本契约只定 RPC 位） | M5 |
| `Workspace.host` 字段、`RemoteRef`、首页「连接主机」 | M5 |
| 重连退避 / 接管 / 启动排队认证 | M6 |
| 端口转发的 workspace 维度、SFTP 接线 | M7 |
| WSL | M8 |
