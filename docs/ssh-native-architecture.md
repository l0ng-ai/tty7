# Native SSH (russh) engine — architecture & integration surface

Workstream 2 (WS2) reference for the other SSH workstreams. Covers the daemon
protocol additions, the interactive prompt-broker flow, the connection-registry
API WS4/WS5 build on, and the seams intentionally left open.

Code lives under `src/daemon/ssh/` plus a backend seam in `src/daemon/pane.rs`
and wire types in `src/daemon/protocol.rs`.

---

## 1. Where the bytes flow

| Layer | Local PTY pane (unchanged) | Native-SSH pane (new) |
|---|---|---|
| Byte source | `portable_pty` master | russh shell channel |
| Reader thread | blocking `Read` → ring + `Output` + OSC sniff | **same thread, unchanged** |
| Backpressure | `OutputGate` parks the PTY reader | `OutputGate` parks a bounded async→blocking channel |
| Writer | `MasterPty` writer | channel-driver command sender |
| Resize | `MasterPty::resize` (SIGWINCH) | `window-change` request |
| Kill/hangup | SIGHUP→SIGKILL process group | channel close + connection unref |
| Foreground pgid / cwd fallback | proc queries | **`None`** (OSC 133 gate is a no-op — correct for remote) |
| Exit | `read()` EOF → `Exited{None}` | driver drops data sender → `read()` EOF → `Exited{None}` |

**Key invariant:** the reader thread, replay ring, `OutputGate`, and OSC 7/133
sniffer are byte-for-byte identical for both backends. Only the handle-owning
methods differ, dispatching on `PaneBackend` (`Pty` vs `NativeSsh`).

**Bridge:** `daemon::ssh::session::make_bridge()` returns a blocking
`Read`/`Write` pair plus the async ends the channel driver takes. Output rides a
**bounded** tokio channel (depth 16) so a slow client backpressures the SSH
window (russh manages the window; we never spool unbounded). Input rides an
unbounded command channel (keystrokes are low-volume).

**Runtime:** one `tokio` multi-thread runtime owned by `SshManager` (lazy
`OnceLock`). The rest of the daemon stays std-threads and only crosses in through
the bridge channels and the broker. `russh::client::Handle` is `Send` but not
`Sync`, so `SshConnection` wraps it in a `tokio::Mutex` to stay `Send + Sync`
(required by the static manager + registry).

---

## 2. Protocol additions (`daemon::protocol`)

New kind bytes (a new spawn kind so a pre-WS2 daemon **rejects** rather than
mis-spawns):

| Direction | Constant | Byte | Message |
|---|---|---|---|
| client→daemon | `SPAWN_NATIVE_SSH` | 14 | `ClientMsg::SpawnNativeSsh { cwd, size, spec }` |
| client→daemon | `AUTH_RESPONSE` | 15 | `ClientMsg::AuthResponse { request_id, response }` |
| daemon→client | `AUTH_PROMPT` | 13 | `DaemonMsg::AuthPrompt { request_id, prompt }` |
| daemon→client | `SSH_STATUS` | 14 | `DaemonMsg::SshStatus { phase }` |

Existing daemon→client kinds `Output`/`Cwd`/`Prompt`/`Exited`/`Snapshot`/`Size`/
`RemoteContext` are reused **unchanged** — a native-SSH pane looks like any other
pane to the GUI reader.

### `NativeSshSpec` (the self-contained connect recipe)

The GUI (WS1/WS6) resolves a stored profile — including keychain secrets and any
jump-host **profile references** (into a nested `jump` chain) — into this before
sending. The daemon never reads the keychain or profile store.

- **Transport:** `host`, `port`, `user`, `proxy` (`None | Command(String) |
  Socks{host,port} | Http{host,port}`), `jump: Option<Box<NativeSshSpec>>`.
  ProxyCommand `%h`/`%p`/`%r` are substituted daemon-side.
- **Auth:** `auth_mode` (`Auto|Password|PublicKey|Agent|KeyboardInteractive`),
  `identity_files` (`%h`/`%r` + `~` expansion), `agent_forward`, `password`
  (secret), `key_passphrases: Map<path,passphrase>` (secret).
- **Session:** `keepalive_interval_s`, `keepalive_count_max`, `connect_timeout_s`,
  `term`, `login_script` (lines sent verbatim + `\n` after shell start),
  `skip_banner`, `verify_host_keys`, `algorithms` (kex/cipher/mac/host_key/
  compression; empty = russh defaults).
- **Carried-only seams (WS4/WS5):** `forwards: Vec<SshForwardRule>`, `x11: bool`.
- **UI labels:** `display_name`, `profile_id` (never affect connection behavior).

**Secrets discipline:** hand-written `Debug` redacts `password`/`key_passphrases`
(recursively through `jump`); `AuthResponse::Secret/Secrets` redact too.
`NativeSshSpec::without_secrets()` returns a persist-safe clone (used by WS6 for
session restore).

---

## 3. Prompt-broker flow (auth / host-key ⇄ GUI)

The connect task runs async and needs decisions only the user can make. It uses
`daemon::ssh::PromptBroker`, owned by the pane and given an `emit` closure that
sends `DaemonMsg` to the pane's *current* subscriber.

```
daemon (connect task)                         GUI (WS3 sheets)
  broker.status(Connecting) ───SshStatus────────►  status line
  broker.prompt(HostKeyUnknown{..}) ─AuthPrompt{id}►  host-key sheet
        (awaits oneshot, 120s timeout)           ◄─AuthResponse{id, HostKeyDecision}
  broker.prompt(Password{user,host}) ─AuthPrompt{id}► password sheet
                                                  ◄─AuthResponse{id, Secret(pw)}
  broker.banner(text) ───AuthPrompt{Banner}──────►  banner (no reply awaited)
  broker.status(Connected) ──SshStatus───────────►
  … Output frames begin …
```

- **`request_id`** matches a `DaemonMsg::AuthPrompt` to its
  `ClientMsg::AuthResponse`. `run_stream` routes the response via
  `DaemonPane::deliver_auth_response` → `PromptBroker::deliver`.
- **Prompt kinds:** `Password{user,host}`, `KeyPassphrase{key_path,comment}`,
  `KeyboardInteractive{name,instructions,prompts:[{text,echo}]}`,
  `HostKeyUnknown{host,port,algorithm,fingerprint_sha256}`,
  `HostKeyChanged{..,old_fingerprint_sha256}`, `Banner{text}` (fire-and-forget).
- **Responses:** `Secret(String)`, `Secrets(Vec<String>)`,
  `HostKeyDecision{accept,remember}`, `Cancelled`.
- **Timeouts / cancel / no-GUI** all fail the auth step cleanly (→ `Cancelled`),
  never hang the connection. Prompt delivery retries only while no subscriber has
  attached yet (so it never duplicates a sheet).
- **`SshStatus` phases:** `Connecting | Authenticating | Connected | Failed{reason}`.
  A failed connect also writes a red diagnostic line into the output stream and
  EOFs the pane, so it surfaces as a normal `Exited` even before WS3 renders the
  status.

### Auth ordering (Tabby-derived, `daemon::ssh::auth`)

`none` probe (learns server's remaining methods) → for `Auto`: publickey (each
identity file, then agent) → password → keyboard-interactive. Non-`Auto` modes
restrict to one family. The server's advertised remaining-methods set gates and
is refreshed after each failure (only when non-empty). `.pub` misconfig is
detected (parse-as-public-key first) and skipped; encrypted keys prompt for a
passphrase; RSA keys are offered with SHA-256; keyboard-interactive auto-fills
password-type prompts from the provided password and handles the zero-prompt
quirk.

---

## 4. Connection registry & reuse (FR-C2) — the WS4/WS5 API

`SshManager::global()` holds the runtime and a registry keyed by
`ConnectionKey` (host/port/user/proxy + recursive jump chain). A spawn for a key
with a **live** connection reuses its authenticated `Handle` — a new tab opens a
new *channel*, never re-authenticates. Per-key establishment is serialized by an
async mutex, so concurrent spawns for a new key don't double-connect.

**Blast radius:** all channels for a key share one `SshConnection`. If the
transport drops, every channel EOFs → every pane sharing it hits the existing
`Exited` path together. The registry holds a `Weak`; when the last shell/forward/
SFTP holding an `Arc<SshConnection>` drops, the connection disconnects. A shell's
`Arc` is held by its channel-driver task for the shell's lifetime.

### API WS4 (forwards) and WS5 (SFTP) consume

On `SshConnection` (obtain via a pane's connection — see the seam below):

| Method | Use |
|---|---|
| `open_session_channel() -> Channel<Msg>` | WS5 SFTP subsystem channel; also shells |
| `open_direct_tcpip(host, port) -> Channel<Msg>` | WS4 Local/Dynamic forwards; also the jump-host transport |
| `is_alive() -> bool` | reuse / reconnect decision |
| `key() -> &ConnectionKey` | identity |

**Seam to reach a pane's connection:**
`DaemonPane::ssh_connection() -> Option<Arc<SshConnection>>` returns the pane's
live connection (the connect task publishes it as a `Weak` the accessor upgrades;
`None` for a PTY pane, a still-authenticating pane, or a dropped connection).
Local/Dynamic forwards then open `direct-tcpip` channels on it and bridge
socket↔channel (replicate the bidirectional EOF/close propagation from the Tabby
brief §5); SFTP opens a session channel and drives the subsystem.

---

## 5. Seams left open (do NOT assume implemented)

| Seam | State in WS2 | Owner |
|---|---|---|
| Port forwards (L/R/D) | `NativeSshSpec.forwards` carried only; `open_direct_tcpip` provided | WS4 |
| `RemoteContext.control_path` | always `None` for native — `forward.rs` (ssh `-O`) correctly rejects native panes | WS4 |
| X11 forwarding | `NativeSshSpec.x11` carried only; no X11 channels | WS4/WS5 |
| SFTP | none; `open_session_channel` provided for the subsystem | WS5 |
| Agent forwarding channels | `agent_forward` requests `auth-agent-req` on the shell channel; incoming agent-channel bridging to `SSH_AUTH_SOCK` not wired | WS4/WS5 |
| Session restore respawn | `SessionPane::Leaf.ssh_spec` (secret-free) persisted; reconnection UX not built | WS6 |
| GUI auth/host-key sheets | protocol + broker ready; sheets not built | WS3 |
| known_hosts hardening | plaintext / hashed / `@revoked` / `@cert-authority`-skip + safe append implemented; wildcard/negation matching and a management UI pending | WS3 |

**Session restore note:** a *live* native-SSH pane reattaches for free (the
daemon + russh connection stay up across GUI restarts). Only a *dead* pane needs
respawn, for which WS6 persists `Leaf.ssh_spec` via `without_secrets()`. (Kept a
leaf field per the WS2 brief; the four `SessionPane::Leaf` literals in `ui/` got
a mechanical `ssh_spec: None`, no UI behavior changed.)
