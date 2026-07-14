# PRD · tty7 SSH 连接管理器

| 项 | 内容 |
|---|---|
| **文档状态** | Draft v2(评审后修订) |
| **对标** | Tabby (`tabby-ssh`, 1.0.216 起基于 russh) |
| **产品定位** | 从"能 SSH 的终端"升级为"内置一流连接管理器的终端" |
| **架构结论** | 默认路径全面切换到原生 SSH 库(**russh** + **russh-sftp**);现有 shell-out `ssh` 保留为 per-profile **"系统 ssh 兼容模式"** 逃生门(冻结,不再演进) |
| **一句话** | 功能对标 Tabby,交互沿用 tty7 自己的语汇(palette 优先、上下文面板、零 modal、渐进展开) |

---

## 1. 背景与目标

### 1.1 现状
tty7 当前把系统 `ssh` 二进制丢进 daemon 拥有的 PTY 运行(`daemon/pane.rs` 的 `build_managed_ssh_command`,每 pane 一个 ControlMaster socket),只在其上加了一层 **loopback 端口自动转发**(`daemon/forward.rs` 走 `ssh -O forward`)。设计哲学是 "OpenSSH is the source of truth"。

**已有可复用的地基:**
- daemon 拥有全部 PTY,GUI 经 socket 镜像字节流(`terminal/remote.rs` 的 `RemoteTerminal`)——字节源已抽象,是 russh shell channel 的天然接缝
- `core/ssh_config.rs` 已能解析 `~/.ssh/config`(含 Include),供导入与按名 resolve 使用
- pane 级滑入面板(`ui/forwards.rs`)、全窗口设置页、竖向 tab 侧栏均已存在
- **完全没有的**:profile 存储、keychain/凭据、GUI 认证、SFTP、Remote/Dynamic 转发

**这套架构做不了连接管理器的核心功能:**
- 无带凭据的连接 profile、无凭据保险库
- 无 SFTP / 文件浏览器
- 认证 / host-key 全靠终端里 `ssh` 自己打印,无法做 GUI 托管
- 端口转发仅 loopback,无 Remote / Dynamic / 预配置

### 1.2 目标(In Scope)
| 目标 | 说明 |
|---|---|
| **G1 连接 Profile + 凭据保险库** | 完整可编辑的连接配置,密码/passphrase 进 OS keychain |
| **G2 原生认证与 host-key 托管** | GUI 提示密码/passphrase/2FA,指纹确认与 known_hosts 管理 |
| **G3 内置 SFTP 文件面板** | 浏览 / 上传 / 下载 / 增删改 / chmod |
| **G4 端口转发补全** | Local / Remote / Dynamic + profile 预配置 + 运行时增删 |
| **G5 高级连接能力** | Jump host / 代理 / 算法 / keepalive / X11 / agent forwarding |
| **G6 保持 tty7 的克制 UX** | palette 优先、上下文面板、零常驻工具栏、渐进展开 |

### 1.3 非目标(Out of Scope,v1)
- 跨设备云同步 / 团队共享(未来可加)
- 会话录制 / 审计日志
- Telnet / Serial / 其它协议(Tabby 有,本期不做)
- Windows 为一等公民(v1 以 macOS 为主,架构保留跨平台可能)

### 1.4 成功指标
| 指标 | 目标 |
|---|---|
| Tabby SSH 功能覆盖率 | ≥ 90%(见 §4 对照表) |
| 新建并连接一个 profile 的操作步数 | ≤ 4 步(打开 palette → 输 host → 认证 → 连上) |
| 常用连接"零表单"直连 | palette 打字即连,无需进编辑页 |

---

## 2. 用户与场景

### 2.1 目标用户
- **开发/运维**:每天连十几到上百台机器,重度依赖 `~/.ssh/config`、agent、跳板。
- **从 Termius / Tabby 迁移者**:要 profile 管理 + SFTP,不想回退到裸终端。

### 2.2 关键场景
| # | 场景 | 期望体验 |
|---|---|---|
| S1 | 临时连一台机器 | palette 打 `user@host` 回车即连 |
| S2 | 连常用机器 | palette 模糊搜 profile 名,回车秒连(复用会话) |
| S3 | 首连需确认指纹 | pane 内滑出 sheet 显示指纹,回车信任 |
| S4 | 输密码并记住 | pane 内 sheet 输入,勾"记住"存 keychain |
| S5 | 传文件 | 热键呼出 SFTP 分栏,拖到 Finder |
| S6 | 建隧道访问远端服务 | 转发面板加一条,或点终端里的 `localhost:3000` |
| S7 | 走跳板连内网 | profile 选跳板,自动多跳 |
| S8 | 导入现有 ssh_config | 一键把 alias 导成 profile |

---

## 3. 架构决策

### 3.1 默认路径采用 russh(原生 Rust SSH 库)+ 系统 ssh 兼容模式逃生门
| 决策 | 理由 |
|---|---|
| **库 = russh + russh-sftp** | 纯 Rust + async,契合 tty7/gpui;Tabby 1.0.216 起用它跑通全部目标功能,是现成存在证明。SFTP client 不在 russh 本体,由 `russh-sftp` crate 提供 |
| **russh 为默认且唯一的管理器路径** | 连接管理器的 SFTP / GUI 认证 / 凭据保险库要求拥有协议栈;profile 连接默认全部走 russh |
| **保留"系统 ssh 兼容模式"逃生门** | per-profile 开关:勾选后该 profile 退回今天的 shell-out 行为(含 ControlMaster loopback 转发),**不提供** SFTP / GUI 认证 / 凭据保险库。前车之鉴:Tabby 切 russh 后爆出私钥加载失败(#10207)、ProxyCommand 缺 `%h`(#11058)、复杂 ssh_config 不兼容(#10188),社区强烈要求保留 OpenSSH 选项(#10162)而 Tabby 未提供——tty7 不重蹈覆辙 |
| **兼容模式 = 冻结** | 旧路径代码保留但不再演进,不接任何新功能;它只服务 russh 覆盖不了的场景(GSSAPI、PKCS#11 直连、复杂 Match/canonicalize config) |
| **russh 连接归 daemon 层** | daemon 已负责 pane / 远程会话生命周期;shell channel 字节流替换本地 PTY 数据源,**必须完整复用 daemon pane 语义**:`DaemonMsg::Output` 帧、8 MiB replay ring、reattach、`OutputGate` 背压(详见 FR-C4) |
| **一条已认证 client 复用** | shell + SFTP + 转发共用同一连接(内存级复用,替代 ControlMaster) |

> russh 路径下,loopback 一键转发改由 russh `direct-tcpip` channel 实现;兼容模式沿用现有 `ssh -O forward` 机制不变。

### 3.2 russh 能力边界
| OpenSSH 特性 | 方案 |
|---|---|
| 证书认证(OpenSSH cert) | russh 原生支持 ✅ |
| FIDO `sk-*` 硬件密钥 / PKCS#11 | 经 **ssh-agent** 走 agent 认证(签名由 agent 完成)✅ |
| GSSAPI / Kerberos | russh 不支持 → 不进 v1 管理器;需要的用户走**兼容模式** |
| 复杂 ssh_config(Match / canonicalize / 深度嵌套 ProxyCommand) | russh 路径只解析常见字段;超出部分走**兼容模式** |

### 3.3 ssh_config 处理
| 决策 | v1 做法 |
|---|---|
| **alias 按名 resolve** | palette 只列保存的 profile(唯一来源,避免同一主机两套行为);config alias 不再实时列出,但在输入连接目标时按名现场 resolve(等价 `ssh <alias>`),或经导入转成 profile |
| **导入为 profile(可选)** | 解析常见字段(Host/HostName/User/Port/IdentityFile/ProxyJump)导成 profile,给想要凭据/SFTP/转发管理的条目用;导入是显式动作,可重复执行(按 alias 去重更新) |
| **运行时完整解析(Match/canonicalize)** | v1 不做;复杂 config 用户对该 profile 勾"系统 ssh 兼容模式"(§3.1) |

### 3.4 known_hosts
- 读写 OpenSSH 格式;**解析并原样保留**所有行类型(含 hashed host、`@cert-authority`、`@revoked`),绝不破坏文件。
- 信任判定 v1 覆盖:明文 host、hashed host、`@revoked`(命中即硬拒绝)。`@cert-authority`(host 证书校验)尽力而为,russh 不支持时按"未知主机"走确认流程,不误报"已变更"。
- russh 回调交出 host key,存储与信任决策由 tty7 管理。

---

## 4. 竞品对标(Tabby 功能覆盖)

图例:✅ 覆盖 · 🟢 覆盖并强化 · ⚪ v1 暂缓

| 领域 | 功能 | v1 | 备注 |
|---|---|---|---|
| **连接** | 直连 | ✅ | |
| | Jump host / 跳板 | ✅ | profile 选跳板,自动多跳 |
| | ProxyCommand | ✅ | |
| | SOCKS5 / HTTP 代理 | ✅ | |
| | 会话复用 | 🟢 | 一条 russh client 复用 shell+SFTP+转发 |
| | 系统 ssh 兼容模式 | 🟢 | per-profile 逃生门;Tabby 社区强烈要求(#10162)而未提供 |
| **认证** | 密码(可记住) | ✅ | GUI sheet + keychain |
| | 公钥(占位符 `%h/%r`) | ✅ | |
| | 加密私钥 passphrase(可记住) | ✅ | |
| | ssh-agent | ✅ | 复用 `SSH_AUTH_SOCK` |
| | keyboard-interactive / 2FA | ✅ | |
| | Auto 全试 | ✅ | |
| | Agent forwarding | ✅ | |
| | 证书认证 | ✅ | russh 原生 |
| | FIDO `sk-*` / PKCS#11 | ✅ | 经 ssh-agent |
| | GSSAPI / Kerberos | ⚪ | russh 不支持;兼容模式可覆盖 |
| **安全** | 指纹展示 | ✅ | |
| | 未知/变更 host key 确认 | 🟢 | 区分"新主机"与"已变更"大警告 |
| | known_hosts 管理界面 | ✅ | 应用内查看/删除 |
| | 可选关闭校验 | ✅ | |
| **Profile** | 保存的连接 profile | ✅ | 完整字段 |
| | 凭据保险库 | ✅ | OS keychain |
| | Profile 编辑界面 | 🟢 | 渐进展开,4 字段起步 |
| | QuickConnect(含 IPv6) | 🟢 | palette 统一入口 |
| | profile ↔ 连接串互转 | ✅ | |
| | ssh_config 导入 | ✅ | |
| | 分组 / 文件夹 | ✅ | 默认扁平 |
| **转发** | Local | ✅ | |
| | Remote | ✅ | |
| | Dynamic / SOCKS | ✅ | |
| | profile 预配置 | ✅ | |
| | 运行时增删面板 | ✅ | |
| | localhost 链接一键转发 | 🟢 | **tty7 独有招牌,保留** |
| **SFTP** | 文件面板(浏览/面包屑/过滤) | ✅ | |
| | 上传 / 下载(含目录递归) | ✅ | |
| | 新建/删除/重命名/chmod | ✅ | |
| | 跟随 cwd | ✅ | |
| | WinSCP 集成 | ⚪ | Windows 专属,v1 暂缓 |
| **会话** | 连接状态行内提示 | ✅ | ` SSH ` 彩条 |
| | Banner 显示 / 跳过 | ✅ | |
| | 关闭确认(warnOnClose) | ✅ | |
| | 断线重连(热键) | ✅ | |
| | 登录脚本 | ✅ | |
| | X11 转发 | ✅ | |
| **传输** | 算法配置(KEX/Cipher/MAC/HostKey/压缩) | ✅ | 高级折叠 |
| | Keepalive / 超时 | ✅ | |
| | 会话恢复 | ✅ | 复用现有 session-restore |

---

## 5. 功能需求(详细)

### 5.1 连接(P0)
- **FR-C1** 支持直连、Jump host(指向另一 profile,可多级)、ProxyCommand、SOCKS5、HTTP 代理五种传输。ProxyCommand 必须支持 `%h`/`%p` token(Tabby 缺失的已知痛点,#11058)。
- **FR-C2** 会话复用:相同(host/port/user/proxy/jump)在内存复用同一已认证 client;新 tab 秒开、不重认证。**爆炸半径明确**:底层连接断开时,共享它的所有 pane 同时进入"已断开"态并提示重连;任一 pane 触发重连即重建连接,其余 pane 随之恢复。
- **FR-C3** Keepalive 间隔、count max、连接超时可配置。
- **FR-C4** shell channel 终端管道对齐(russh 路径的隐性 P0):
  - `pty-req` 携带 TERM 与 terminal modes;pane resize → `window-change` 请求
  - channel `exit-status`/`exit-signal` → 映射为现有 `DaemonMsg::Exited` 语义
  - 字节流接入 daemon 现有管线:`DaemonMsg::Output` 帧、8 MiB replay ring(GUI 重启后 reattach 保留 scrollback)、`OutputGate` 背压(反压落到 channel window,不无限缓冲)
  - OSC 7 / OSC 133 sniffer(`core/osc.rs`)在 russh 字节流上原样工作——字节流端到端透明是硬约束
- **FR-C5** 系统 ssh 兼容模式:profile 高级区勾选后,该 profile 以今天的 shell-out 方式连接(含 ControlMaster loopback 转发),SFTP / GUI 认证 / 保险库功能置灰并注明原因。

### 5.2 认证(P0)
- **FR-A1** 认证方式:`自动 | 密码 | 公钥 | agent | keyboard-interactive`;默认"自动"按顺序全试。
- **FR-A2** 公钥支持多把私钥、`%h`/`%r` 占位符;`.pub` 误配自动识别并跳过。
- **FR-A3** 加密私钥弹 passphrase sheet,可"记住"(按 key 内容 hash 存 keychain)。
- **FR-A4** ssh-agent:复用 `SSH_AUTH_SOCK`;支持 agent identity 与 agent forwarding。
- **FR-A5** keyboard-interactive:面板逐项输入;password 类提示位可用已存密码自动填。
- **FR-A6** 认证成功且勾"记住"→ 存 keychain。**仅当服务端明确拒绝已存密码**(password 方法用存储值尝试且被拒)→ 重新弹 sheet 预告"已存密码被拒绝",用户提交新密码后覆盖;网络错误、超时、其它方法失败等**不得**触发清除。

### 5.3 Host key / 安全(P0)
- **FR-S1** 连接时展示 key 算法 + SHA256 指纹。
- **FR-S2** 未知主机 → 确认 sheet;已存但**指纹变更** → 红色大警告 sheet,明确"可能中间人",绝不自动接受。
- **FR-S3** known_hosts 读写 OpenSSH 格式;设置页可查看/删除已信任 key。
- **FR-S4** 可选关闭校验(per-profile + 全局)。

### 5.4 Profile 管理 + 凭据(P0)
- **FR-P1** Profile 存储完整字段(见 §7 数据模型),支持分组。
- **FR-P2** 密码 / passphrase 存 OS keychain(macOS Keychain;Windows Credential Manager;Linux libsecret),配置文件只存引用不存明文。
- **FR-P3** palette 为统一入口:SSH 列表只来自保存的 profile(按 frecency 排序)+ "现连"项;ssh_config 主机经导入成为 profile 后出现,或直接输入 alias 现连。
- **FR-P4** QuickConnect 解析 `[ssh] user@host[:port] [flags]`,支持 IPv6 `[::1]:port`。
- **FR-P5** profile → `user@host:port` 一键复制;`~/.ssh/config` 一键导入。

### 5.5 端口转发(P0/P1)
- **FR-F1**(P0)Local / Remote / Dynamic 三种类型,走 russh channel。
- **FR-F2**(P0)profile 预配置一组转发,连上自动建立。
- **FR-F3**(P0)运行时上下文面板增删转发,每条带 description。
- **FR-F4**(P0)保留 loopback 链接一键转发(Cmd-click `localhost:PORT`)。

### 5.6 SFTP(P0)
- **FR-T1** pane 内滑出可调宽分栏;远端文件树 + 面包屑 + 过滤。
- **FR-T2** 上传 / 下载支持单文件与目录递归;上传走 `.tty7-upload-*` 临时文件 + rename 收尾(优先 `posix-rename@openssh.com` 扩展,不可用则退回 SFTP rename)。
- **FR-T3** 新建目录 / 删除 / 重命名 / chmod / 跟随符号链接。
- **FR-T4** 可选定位到 shell 当前 cwd;机制 = 现有 OSC 7 cwd 跟踪(依赖远端 shell 有集成脚本,与今天一致);无 OSC 7 信号时该按钮置灰,不猜测。
- **FR-T5** 与 Finder 双向拖拽;传输进度走底部小托盘,不阻塞。

### 5.7 会话体验(P1)
- **FR-E1** 连接 / 转发 / 错误行内 ` SSH ` 彩条提示。
- **FR-E2** 竖向 tab 侧栏每 tab 显示状态点(连接中/已连/断开)。
- **FR-E3** 关闭 SSH tab 前可选二次确认(per-profile 覆盖全局)。
- **FR-E4** 断线提示 + `restart-ssh-session` 热键重连;重连成功后自动重建 profile 预配置转发与断线前的运行时转发。
- **FR-E5** 登录脚本:连上后自动发送命令序列。
- **FR-E6** Banner 显示,可 skip。

### 5.8 高级 / 传输(P1)
- **FR-X1** 算法配置(KEX/Cipher/MAC/HostKey/压缩),从 russh 枚举可选项,高级折叠区。
- **FR-X2** X11 转发开关。

---

## 6. UX 设计规范

### 6.1 三原则
1. **一个入口框,三种来源** —— 不分裂"快连栏 vs profile 列表"。
2. **上下文面板 > modal** —— 认证 / SFTP / 转发都在 pane 内滑出,零独立 OS 窗口、零常驻工具栏。
3. **渐进式展开** —— profile 编辑器 4 字段起步,其余折叠。

### 6.2 五个界面

**① 连接入口(palette)**
```
⌘K ┃ prod                                          │
   ┃ ⭐ prod-web       deploy@10.0.0.5      ↵ 连接  │
   ┃ 🔧 prod-bastion   (~/.ssh/config)      灰       │
   ┃ ↵  连接到 "prod.example.com"                    │
```
`↵` 连接 · `⌘↵`/`→` 进编辑 · 按 frecency 置顶。

**② Profile 编辑(全窗口页,复用设置页范式)**
```
名称  [ prod-web ]
Host  [ 10.0.0.5 ]        端口 [ 22 ]
用户  [ deploy ]
认证  ( 自动 ▾ )
▸ 跳板          (无)
▸ 端口转发      (2 条)
▸ 高级          算法 / keepalive / 代理 / X11 / 登录脚本 / 系统 ssh 兼容模式
```

**③ 认证 & host-key(pane 内 sheet,键盘优先,Esc 取消)**
```
┌ prod-web ──────────────────────────────┐
│ 🔑 deploy@10.0.0.5 的密码               │
│    [ •••••••••• ]   ☐ 记住(keychain)   │
│    ⏎ 连接    esc 取消                    │
└──────────────────────────────────────────┘

┌ ⚠ 主机密钥已变更 —— 可能存在中间人 ─────┐
│ 10.0.0.5  ED25519                        │
│ SHA256:aX3f…   (旧 SHA256:9Kp…)          │
│ ⏎ 仍然信任    esc 中断                   │
└──────────────────────────────────────────┘
```

**④ SFTP(pane 右侧滑入分栏)**
```
 shell 区域          │ 📁 /home/deploy        ⤴ ⤵ ⟳
                     │ ▸ src/
                     │   deploy.sh   4.2K
```

**⑤ 端口转发(上下文面板)** —— 列出活动转发 + 加/删 + 类型选 L/R/D。

### 6.3 禁止照搬 Tabby 的四点
- ❌ 庞大配置树 → 用渐进展开
- ❌ app-modal 弹窗 → 用 pane 内 sheet
- ❌ 常驻多按钮工具栏 → 用热键 + pane 右键菜单 + palette
- ❌ 快连栏与 profile 列表分裂 → 统一进 palette

---

## 7. 数据模型

### 7.1 SSH Profile(serde,持久化到配置)
```rust
struct SshProfile {
    id: Uuid,
    name: String,
    group: Option<String>,

    // 连接
    host: String,
    port: u16,              // 默认 22
    user: String,
    jump_host: Option<Uuid>,        // 指向另一 profile
    proxy_command: Option<String>,
    socks_proxy: Option<HostPort>,
    http_proxy: Option<HostPort>,

    // 认证
    auth: AuthMode,                 // Auto | Password | PublicKey | Agent | KeyboardInteractive
    identity_files: Vec<String>,    // 支持 %h/%r
    agent_forward: bool,
    // 凭据只存引用,明文在 keychain
    credential_ref: Option<CredentialRef>,

    // 转发
    forwards: Vec<ForwardRule>,     // {type: L|R|D, bind, target, description}

    // 会话
    keepalive_interval_s: Option<u32>,
    keepalive_count_max: Option<u32>,
    connect_timeout_s: Option<u32>,
    warn_on_close: Option<bool>,
    skip_banner: bool,
    login_scripts: Vec<String>,
    x11: bool,

    // 高级
    algorithms: Algorithms,         // kex/cipher/mac/hostkey/compression 列表,空=默认
    verify_host_keys: Option<bool>,
    use_system_ssh: bool,           // 兼容模式:走 shell-out `ssh`,管理器功能置灰
}
```

### 7.2 凭据存储
- keychain 条目按**端点**而非 profile 键控:密码用 `tty7-ssh:<user>@<host>:<port>`,私钥 passphrase 用 `tty7-ssh-key:<key-sha512>`。
  - 理由:QuickConnect(无 profile)也能"记住";多个 profile 指向同一端点时共享凭据、改密码只改一处。
- 配置文件永不落明文密码;`credential_ref` 只是指向 keychain 条目的引用。

---

## 8. 非功能需求

| 类别 | 要求 |
|---|---|
| **安全** | 凭据仅存 OS keychain;host key 变更强警告;known_hosts 遵循 OpenSSH 语义;不弱化默认算法 |
| **性能** | 复用连接开新 shell < 200ms;SFTP 目录列举流畅;转发/文件传输不阻塞 UI 线程 |
| **可靠性** | 断线自动清理转发监听与子会话;认证失败清除坏凭据 |
| **跨平台** | 架构不锁死 macOS;keychain 抽象层预留 Windows/Linux 后端 |
| **可迁移** | ssh_config 导入让存量用户平滑迁入 |

---

## 9. 交付拆分(一步到位 = 一个大版本,内部并行工作流)

> 目标是**一个完整的连接管理器版本**,而非分期上线半成品。下列为内部并行 workstream,非对外分期。

| Workstream | 内容 | 依赖 |
|---|---|---|
| **WS1 数据层** | Profile 模型 + keychain 抽象 + ssh_config 导入 | 无(可先行) |
| **WS2 russh 会话** | daemon 内 russh 连接 + shell channel → pane 字节流;含 FR-C4 全部管道对齐(pty-req/resize/exit-status/replay ring/背压/OSC 透传) | 无 |
| **WS3 认证/安全** | GUI sheet(密码/passphrase/2FA)+ host-key 确认 + known_hosts 管理 | WS2 |
| **WS4 转发** | L/R/D + 预配置 + 上下文面板 + 保留 loopback 魔法(russh 路径改走 direct-tcpip) | WS2 |
| **WS5 SFTP** | russh-sftp channel + 文件面板 + 传输 | WS2 |
| **WS6 UX 集成** | palette 统一入口 + profile 编辑页 + tab 状态点 | WS1/2/3 |
| **WS7 路径收口** | 默认路径切到 russh;shell-out 降级为兼容模式并**冻结**(去掉入口层对它的直接依赖,不删除);ControlMaster 相关代码仅由兼容模式引用 | WS2/4 |

**发布门槛(全绿才 GA):**
1. §4 对照表 P0 全部 ✅ + S1–S8 场景走通
2. 安全项(FR-S1~S4)通过评审
3. FR-C4 管道对齐验收:russh pane 的 reattach / session-restore / OSC 133 命令标记 / 背压行为与本地 PTY pane 无差异
4. **内部 dogfood ≥ 2 周**:日常连接全走 russh 路径(Tabby 切 russh 后的事故均为上线后才暴露,必须用真实机器群淌过一遍)
5. 兼容模式可用且冻结,默认路径不再触碰 shell-out 代码

---

## 10. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| **russh 兼容性长尾**(Tabby 前车之鉴:#10188/#10207/#11058) | 部分用户切换后连不上 | 系统 ssh 兼容模式逃生门 + GA 前 ≥2 周 dogfood + 认证失败信息可诊断(展示服务端拒绝原因) |
| russh 特性缺口(FIDO/PKCS#11) | 部分密钥类型连不上 | 引导用户经 ssh-agent 认证(签名由 agent 完成);仍不行走兼容模式 |
| GSSAPI/Kerberos 用户 | 管理器路径无法连接 | 兼容模式覆盖,文档说明 |
| ssh_config 复杂语义(Match/多跳) | 导入不完整 | 输入 alias 按名 resolve 保底;v1 只导入常见字段;复杂场景引导兼容模式 |
| known_hosts 格式细节 | 误报/漏报 host key | 只做明文/hashed/@revoked 的信任判定,@cert-authority 尽力而为;解析层绝不改写无关行;充分测试 |
| 连接复用爆炸半径 | 一条连接挂掉拖垮多个 pane | FR-C2 明确断开语义:所有共享 pane 同步提示,一键重连全部恢复 |
| X11 转发依赖 XQuartz(macOS) | 开了不生效,用户困惑 | 检测不到 X server 时提示安装 XQuartz,而非静默失败 |
| 凭据安全事故 | 严重 | 只走 keychain,代码评审 + 不落盘明文 + 不写日志 |
| 自己维护加密栈的 CVE 面 | 安全责任 | 跟随 russh 上游,建立依赖告警 |
| UX 变复杂,失去 tty7 克制感 | 产品走味 | 严守 §6 三原则,拒绝 Tabby 式堆砌 |

---

## 11. 未来(Out of Scope,后续版本)
- 跨设备云同步 / 团队凭据共享
- 会话录制与审计
- Telnet / Serial 协议
- Windows 一等公民 + WinSCP 集成
- ssh_config 运行时完整解析(Match / canonicalization)
- GSSAPI / Kerberos 认证(取决于 russh 上游支持)
