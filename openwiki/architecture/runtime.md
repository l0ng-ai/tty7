# Runtime architecture

## Design goal

The runtime is built around a tmux-like separation: the GUI may come and go, but shells and PTYs live in a background daemon. The source states this boundary in `src/daemon/mod.rs`: the daemon owns PTYs and child processes, while the client terminal lives in `terminal::remote::RemoteTerminal`.

## Entrypoint and process modes

`src/main.rs` has two modes:

1. **Daemon mode**: if `--daemon` is present, `daemon::server::run()` starts the headless server and blocks in its accept loop.
2. **GUI mode**: otherwise startup:
   - applies `--config-dir` before any config/session/history/endpoint path is read,
   - calls `daemon::spawn::ensure_running()`,
   - initializes GPUI/gpui-component assets,
   - registers bundled Hack fonts,
   - loads `Config` into a GPUI global,
   - starts the config watcher,
   - installs the keymap,
   - opens a window containing `Tty7App`.

`--config-dir` and `TTY7_CONFIG_DIR` are important because the daemon endpoint also lives under the config directory. A dev run must not accidentally talk to the user's real daemon; `cargo dev` passes `--config-dir .tty7-dev`.

## Daemon launch and transport

`src/daemon/spawn.rs` is the GUI-side daemon bootstrapper:

- `ensure_running()` first probes `transport::connect()`.
- If the endpoint exists but cannot connect, it removes the stale endpoint.
- It re-execs the current binary with `--daemon` and the resolved config dir.
- It waits up to a short timeout for the daemon endpoint to accept connections.

`src/daemon/transport.rs` abstracts local IPC:

- Unix uses a Unix-domain socket under the config directory.
- Windows currently uses loopback TCP with a config-dir port file.

The Windows transport is useful for cross-platform support but should not be treated as a hardened public network API. Security-sensitive changes should review local endpoint access and lifecycle behavior.

## Wire protocol

`src/daemon/protocol.rs` defines an internal framed protocol:

```text
[u32 LE payload_len][u8 kind][payload]
```

Important properties:

- `MAX_FRAME` is 64 MiB.
- Hot path variants carry raw PTY bytes with no JSON: `ClientMsg::Input`, `DaemonMsg::Output`, and `DaemonMsg::Snapshot`.
- Control messages serialize small structs as JSON.
- One pane stream connection carries one pane. Listing panes uses a short-lived control connection.
- The protocol is versionless and internal to this repository; document and change it as client/daemon code together.

Primary message shapes:

- Client to daemon: `Spawn`, `Attach`, `Input`, `Resize`, `Detach`, `Kill`, `List`.
- Daemon to client: `Spawned`, `Size`, `Snapshot`, `Output`, `Cwd`, `Prompt`, `Exited`, `PaneList`, `Error`.

`Size` immediately before `Snapshot` is not cosmetic. Replaying a byte ring at the wrong width corrupts wrapping and cursor positioning, so attach-time geometry handling is a correctness boundary.

## Server and pane lifecycle

`src/daemon/server.rs` owns the daemon listener and pane registry. `Registry` maps `pane_id` to `Arc<DaemonPane>` and allocates monotonically increasing IDs.

The first message on a connection determines its role:

- `Spawn { cwd, size }`: allocate a pane id, spawn a `DaemonPane`, reply `Spawned`, then stream the pane on this connection.
- `Attach { pane_id, size }`: look up an existing pane and attach this connection. The attach size is deliberately not used as the PTY's final size; the client sends a real `Resize` after layout.
- `List`: return `PaneList` and close.
- `Kill`: remove and kill the pane and close.

`src/daemon/pane.rs` owns one PTY, child process, replay ring, subscriber, cwd, prompt state, and liveness flag. The daemon uses `portable-pty` so Unix PTYs and Windows ConPTY share the same high-level code path.

Pane behavior to preserve:

- **Bounded replay ring:** raw output bytes are retained up to an implementation cap, then replayed on attach.
- **Single subscriber:** a pane currently has one attached client. A new attach replaces the old subscriber. `subscriber_epoch` prevents an old connection from detaching a newer subscriber.
- **Detach vs kill:** `Detach` removes the subscriber without killing the child. `Kill` terminates and removes the pane.
- **Dead detached reclamation:** if a child exits while detached, the server reclaims it to avoid leaking memory/PTY state.
- **Process-group teardown:** Unix kill paths signal process groups and use bounded reader-thread joins to avoid wedging connection handling.

## Client-side remote terminal

`src/terminal/remote.rs` is the GUI-side protocol consumer. `RemoteTerminal::spawn` sends `ClientMsg::Spawn`; `RemoteTerminal::attach` sends `ClientMsg::Attach`. Both build a local `alacritty_terminal::Term` mirror that is fed by daemon frames.

The reader thread handles:

- `Size`: hold/apply geometry for upcoming replay.
- `Snapshot`: replay raw bytes into the local emulator while suppressing side effects.
- `Output`: feed live bytes into the emulator.
- `Cwd`: update cached foreground cwd.
- `Prompt`: update shell-active / at-prompt state.
- `Exited` or EOF: mark terminal exited.

Snapshot replay suppression is subtle and important. Historical escape sequences must not trigger fresh PTY query replies, clipboard store/load, bell flashes, or desktop notifications.

Dropping a `RemoteTerminal` sends `Detach` and shuts down the stream. Explicit UI close paths call daemon kill behavior separately.

## Shell integration and prompt awareness

`src/daemon/shell_integration.rs` injects shell startup hooks when a pane is spawned. Supported shells are zsh, bash, and fish.

Purpose:

- OSC 7 reports foreground/current working directory.
- OSC 133 reports prompt/command lifecycle:
  - `A`: prompt start,
  - `B`: prompt end / command input begins,
  - `C`: command output begins,
  - `D;<exit>`: command finished.

Injection strategy:

- zsh uses a throwaway `ZDOTDIR` with redirector files that source the user's real dotfiles and append tty7 hooks.
- bash uses a throwaway `--rcfile` and a trimmed preexec-style integration. Bash with custom args currently does not get integration.
- fish uses startup `-C` / init-command behavior and wraps prompt functions.

The daemon sniffs output with `OscSniffer` in `pane.rs`, using the shared tokenizer in `src/core/osc.rs`. Recent changelog entries highlight why prompt mark timing matters: emitting OSC 133 `D` before slow user prompt hooks lets tty7 take input back quickly after a command finishes.

## Config and session relationship to runtime

`src/core/config.rs` determines config directory resolution and daemon spawn settings:

1. `--config-dir`,
2. `TTY7_CONFIG_DIR`,
3. platform default config dir.

New daemon panes read current config for shell command, working-directory policy, and extra environment. Existing panes keep their already-spawned shell/env state.

`src/core/session.rs` persists tab/split layout and each leaf's `pane_id` plus cwd. On GUI startup, `Tty7App` asks the daemon for live panes and reattaches to matching alive pane IDs; otherwise it spawns fresh shells in saved cwd.

## Change guidance

When changing daemon/runtime code:

- Start with `src/daemon/protocol.rs`, `src/daemon/server.rs`, `src/daemon/pane.rs`, and `src/terminal/remote.rs` together; most correctness properties cross this boundary.
- Preserve attach ordering: `Size` before `Snapshot`, then cwd/prompt/exited state.
- Keep replay side-effect suppression in mind for any new terminal event emitted by `alacritty_terminal` during historical replay.
- Add tests near the pure logic. Existing tests cover frame encoding/decoding, attach replay ordering, OSC sniffing, stale endpoints, and reclaim behavior.
- Watch for historically recurring bug classes when touching this boundary: attach races, duplicate replay responses, lifecycle leaks, OSC corruption, non-atomic writes, off-by-one boundaries, and geometry mismatch.
