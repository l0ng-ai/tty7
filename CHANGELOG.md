# Changelog

All notable changes to tty7 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Update tty7 without leaving the app** — the launch check and
  **Settings → About → Check Now** now offer **Update and Relaunch** for
  packaged macOS installs instead of sending the user to GitHub Releases. A
  dedicated `tty7-updater` helper verifies the release checksum, bundle
  version, and code-signing requirement before replacing the app, and restores
  the previous bundle if relaunch fails. The new GUI reuses a running local
  server when its wire protocol is compatible, preserving shells; an
  incompatible server keeps its shells too and raises the existing explicit
  keep-or-restart prompt. Other platforms and unsupported layouts retain the
  release-page fallback, and Nightly remains unchanged for the first version.

- **`tty7 wait` — the primitive an agent team was missing** — block until a
  pane's agent needs input, finishes its turn, or dies:
  `tty7 wait %3 --until waiting,done --changed --timeout 600`. The daemon
  already tracked, per pane, which coding agent runs there and its status
  (idle / working / **waiting on the user** / done, plus the agent's native
  session id), but the only consumer was the GUI's status dots. This hands
  the same view to programs: one agent can start a worker in another pane,
  sleep until its peer blocks on a permission prompt, answer it, and read
  the result — which a tmux-based agent team has to fake by screen-scraping
  `capture-pane` on a timer.

  The status the server keeps is a *level*, not an event: `done` stands
  until the next turn begins, `waiting` until the agent moves again. So
  `--changed` is not optional dressing — it ignores the state the pane was
  already in when the wait began, which is what every round after the first
  needs. Without it, a wait issued right after a `send` answers with the
  *previous* turn's state before the worker has even read the input; the
  JSON's `stale` flag says when that happened. Exit codes are made for
  scripts: `0` matched, `124` timed out (the `timeout(1)` convention), `1`
  with `"status": "exit"` when the worker died before it could get there.
  The reply carries the agent's message and native session id, so a wake-up
  is directly actionable.

  Polling `AgentStates` rather than subscribing to events, on purpose: a
  stateless question composes into scripts (`tty7 wait %3 && tty7 capture %3
  --plain`), survives a server restart mid-wait, and needs no cursor. The
  cost is one aggregate request per tick — the same one `tty7 agents` makes
  once. (#248)

- **An orchestration skill for Claude Code** — a switch under
  **Settings → Agents** installs `~/.claude/skills/tty7-orchestration`, a
  skill teaching a *primary* agent the whole delegation loop: open a worker
  pane, send it one bounded task, `wait` on it, answer what it asks, collect
  the result, close the pane. A skill rather than a global instruction on
  purpose — an earlier cut appended this to `~/.claude/CLAUDE.md`, which
  taxed every session's context window and, worse, encouraged *every* agent
  to go orchestrate its neighbours. As a skill, only its one-line
  description rides in context until something explicitly reaches for it,
  and worker agents never inherit orchestration authority. The file carries
  an ownership marker: uninstall removes a file tty7 wrote and refuses to
  touch one it didn't, so a hand-written skill that happens to share the
  directory name survives. (#248)

- **Smooth scrolling for wheel mice** — a notch now eases into place over a
  handful of frames instead of jumping the whole distance at once.
  **Settings → Terminal → Scrolling → Smooth scrolling**, on by default.

  Sub-line scroll positions have been in place for a while: the view keeps a
  fractional remainder and paints the grid shifted by it, so the scrollback
  need not land on a line boundary. But the position was a function of the
  *event*, not of *time* — a notch arrived and the whole thing was applied at
  once. Whether that looked smooth came down entirely to how fine-grained the
  platform's deltas were. A macOS trackpad reports pixels, so it did. A wheel
  on Windows reports whole lines (gpui multiplies the notch by the system's
  scroll-lines setting, three by default), the fraction came out zero every
  time, and the view jumped three lines per notch — the sub-line machinery was
  present and never engaged. That is the "no smooth scroll on Windows" report.

  macOS was no better, just differently broken: it reports a wheel mouse as
  *pixels* too — `hasPreciseScrollingDeltas` is set — so the fraction was never
  zero, but one detent arrives as a single ~103px event, which at a 21px line
  height is a five-line jump applied in one go. Worse than Windows, and
  invisible to any check based on the delta's type.

  A wheel detent is one discrete pulse; no amount of arithmetic on the delta
  recovers a continuous gesture from it. So the fix is to make position a
  function of time: a detent adds to a remaining distance, and each frame
  consumes a share of what is left. Roughly 120ms to land, exponential, so most
  of the travel happens immediately and the tail is invisible — under a pixel
  of remainder is snapped rather than approached, since every frame of it costs
  a repaint.

  Trackpads keep the direct path; putting an animation between the fingers and
  the grid would only add lag. What separates the two is **phase**, not delta
  type and not delta size — measured on this machine, a trackpad flick moves up
  to ~3 lines in one event while an inched wheel moves ~0.6, so size overlaps
  badly, but only a device that can gesture ever reports `Started`/`Ended`, and
  a wheel is `Moved` forever on every platform. The gesture is then held open
  on a 150ms idle timer rather than closed on `Ended`, because lifting the
  fingers is not the end of the stream: the momentum tail keeps delivering
  `Moved` events *larger* than the gesture itself, and animating those would
  smooth what the system is already smoothing.

  Two more things stay direct. A jump under a line reads as continuous already
  — inching a wheel one detent at a time lands there — and mouse reporting and
  alternate-scroll forward whole lines to the application, which cannot be
  spread over frames at all.

  The distance in flight is relative, not an absolute target, so output
  arriving mid-scroll shifts the grid without dragging the animation somewhere
  else. Everything that moves the view on its own — jumping to a prompt,
  dragging a selection past the edge, clearing the scrollback, the keyboard and
  mouse-reporting paths — cancels what is in flight first.

- **Oh My Pi (`omp`) is a first-class agent** — the eighteenth CLI tty7
  recognizes in a pane, with the full set: brand avatar, status dot, session
  resume (`omp --resume <id>`), and **Fork Session** (`omp --fork <id>`).
  **Settings → Agents** installs its hooks like the others.

  Oh My Pi is a fork of Pi, but it is its own binary with its own
  `~/.omp` config directory, so it gets its own entry rather than sharing
  Pi's — a pane running `omp` was previously not detected at all, and
  aliasing it onto Pi would have written the bridge to `~/.pi` and offered
  `pi --session <id>` to a binary that spells it `--resume`. The status
  bridge is the Pi extension the fork inherited (same default-exported
  factory, same four lifecycle events), so one template now serves both,
  landing at `~/.omp/agent/extensions/tty7/index.ts`.

### Fixed

- **Maximizing a Windows pane no longer shreds it** — two separate resize
  disagreements stacked up here, and the visible one was the second.

  ConPTY emits no repaint after a resize; conhost silently re-anchors its
  own layout and keeps painting with absolute cursor addresses computed
  against it. Measured against a live ConPTY: growing the window keeps
  rows and cursor pinned and opens blank rows at the bottom — nothing is
  restored from scrollback — and shrinking scrolls the last *written* row
  (PSReadLine parks a continuation hint below the prompt) to the new
  bottom. The grid resized the alacritty way instead: growing pulled
  scrollback back into view and pushed the cursor down by that many rows.
  After a maximize the two layouts disagreed by exactly the pulled row
  count, so every later absolute-CUP paint — PSReadLine redraws the prompt
  that way per keystroke — landed mid-screen inside the old output. The
  vendored `alacritty_terminal` now has a `conpty_resize` mode that
  mirrors conhost's model for the primary screen (the alternate screen
  keeps stock behavior; its application repaints itself), and every pane
  on Windows opts in — a shell, `wsl.exe`, and `ssh.exe` are all presented
  through conhost alike. The cost is Windows-Terminal-standard behavior:
  maximizing no longer reveals extra scrollback below the fold.

  Separately, a resize during a burst of output reflowed the grid ahead of
  the backlog: the daemon can hold up to 16 MiB of queued old-width bytes,
  and the client parsed them into the already-resized grid. The daemon now
  echoes a `Size` frame to the controller at the exact stream position
  where the PTY changed geometry — the same geometry-tagging the reattach
  replay has always used — and the client defers its reflow to that
  marker, so every queued byte parses into the grid it was rendered for.
  Observers, which already received live `Size` frames but only applied
  them after a replay snapshot, follow the controller's resizes at the
  right moment too. The client keeps the reflow-at-request-time path
  against an older daemon that never echoes (new `resize-echo` feature
  probe), and on remote routes, where only the local daemon's features are
  known — a current remote daemon's echo lands there as a harmless
  same-size no-op, and probing per connection is the follow-up that
  extends deferral to remote panes.

- **Pasting a screenshot into a remote pane works on macOS too** — the
  clipboard-image paste that stages a file and hands the agent its path was
  built off macOS only, because a local macOS agent reads the system
  clipboard itself when it sees Ctrl+V and that carries the image at full
  fidelity. The reasoning stops at the pane boundary: an agent in an SSH
  pane or a remote workspace reads the clipboard of the host *it* runs on,
  which never holds this machine's screenshot, so SYN was a no-op and the
  paste did nothing at all. Remote panes now stage and upload on every
  platform, while a local macOS pane keeps SYN untouched. macOS screenshots
  arrive as TIFF, which agent vision rejects the same way it rejects a
  Windows BMP, so those transcode to PNG on the way out. (#338)

- **A WSL pane is handed a path it can actually open** — the same paste
  staged its file on the Windows side and pasted the Windows name for it, so
  an agent in WSL got `C:\Users\…\paste-1.png` and found nothing there. A
  WSL pane shares this machine's disk but not its path syntax, so there is
  nothing to upload and only a name to rewrite: the paste now carries the
  automount view, `/mnt/c/Users/…`. A path with no mapping — a UNC temp
  directory — keeps the Windows name, which at least says where the file
  went. (#338)

- **Scoop shims work again when tty7 is launched from a hardened Windows
  shell broker** — some brokers enforce `ProcessRedirectionTrustPolicy` on
  what they start. The daemon inherited it, every ConPTY shell under the
  daemon inherited it in turn, and PowerShell could then no longer traverse
  a user-created junction — which is exactly what Scoop's `current` links
  are. `oh-my-posh` and `fzf` died with `Shim: Could not determine if target
  is a GUI app`. Windows Terminal was unaffected because its process tree
  never picked the policy up in the first place.

  The policy cannot be relaxed once it is on, so the fix is to not inherit
  it: when tty7 detects the enforcing bit, it creates the daemon with
  `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` naming the interactive desktop
  shell, which supplies the ordinary desktop token, device map, and
  mitigation policy. Everything else about the spawn is unchanged, and the
  ordinary path still runs whenever the policy is absent — or whenever the
  desktop shell cannot be borrowed, in which case tty7 logs a warning and
  starts degraded rather than not starting at all. (#292)

- **A pane's last line of output no longer loses the race with its exit on
  Windows** — the process-exit monitor could observe a short-lived shell
  exiting before the ConPTY reader had delivered its final frame, so
  `Exited` reached clients ahead of the output that preceded it. The
  monitor now releases the pseudoconsole and lets the reader, which reports
  only after it has forwarded everything up to EOF, announce the death. A
  bounded window behind it still covers the case where EOF never arrives —
  a grandchild holding the ConPTY output pipe open keeps the shell's own
  exit from ever closing it. (#292)

## [26.8.1] - 2026-08-01

### Added

- **A remote workspace is a window that is one machine, not a pane that
  happens to be SSH'd somewhere** — pick a host from **Home → Connect to
  Host** (or the workspace switcher) and tty7 opens a window bound to that
  machine: its own tab and pane tree, file browser and editor, repo grouping,
  branch line, Changes panel, diff overlay and worktree list, all answered by
  *that* machine rather than read off the client's disk. Before this, reaching
  a remote box meant an SSH pane from the command palette — a shell and
  nothing else. Repo grouping, the diff overlay, the right-panel Changes list,
  worktrees: all of it read the client's local disk, so none of it existed
  over SSH. The file tree was SFTP browsing, a separate, lesser thing from the
  local editor. And the moment you closed the lid or lost the network, the
  agent running in that pane died with the connection — there was nothing on
  the far side to keep it alive.

  A remote workspace fixes all four at once by putting the same code on both
  ends. `tty7-server` — a new headless binary built on `tty7-core`, the crate
  the GUI's own daemon logic was extracted into — runs on the far machine and
  serves the identical `LocalHost` implementation the GUI uses for your own
  disk, over one control connection. Because both sides run the *same* code
  rather than a client-only reimplementation, a remote workspace has the exact
  feature set a local one does, not a cut-down version of it. Sessions live in
  that server, not in the window: closing the window detaches rather than
  kills, panes and agent conversations keep running on the far side, and
  reconnecting — from this laptop or another one — lands back where you left
  it.

  Local and remote workspaces are two different features, not one with a flag:
  a window is never a mix of an SSH pane and a real workspace, and that split
  is enforced rather than left to convention. The install itself is one-time
  and unprivileged — the server is a static binary pushed over the same
  connection, no `sudo`, confirmed once per machine. This is the foundation
  the WSL-machines, incremental git-streaming and per-machine `⌃R` history
  entries below build on: none of them had anywhere to attach to before this
  landed. (#235)

- **`tty7` is now a CLI, not just the app you launch by clicking an icon** —
  a new scriptable command-line client, built for coding agents to drive
  panes without a window in the way. The GUI binary is renamed `tty7-app`
  (bundle id, icons and display name unchanged); `tty7`, the name you type,
  is now the CLI. Both are clients of the same `tty7-server` a remote
  workspace runs, talking the same control and pane protocols the window
  does — an agent reaching through the CLI can do exactly what the CLI
  exposes, no more, no less.

  | | |
  |---|---|
  | Hot path | `ls`, `run -- <cmd>` (real exit code, `--keep` keeps the pane around), `new`, `send`, `capture`, `split`, `procs` |
  | Addressing | `ws` / `tab` (`@N`) / `pane` (`%N`) / `machine` / `server` |
  | Introspection | `agents`, `events` (NDJSON), `status`, `doctor` |
  | Global | `-m <machine>` routes over the server's existing SSH/WSL links; `--json` everywhere; implicit context from `TTY7_PANE` / `TTY7_WS` / `TTY7_SOCKET`, injected into every spawned shell |

  The server's control dialect grew several additive verbs to carry this: a
  read-only `Observe` for watching a pane without owning it, budgeted per
  observer; pane exit codes, which previously were always reported `None`
  and now actually carry the code; and aggregate `AgentStates` / `Routes` /
  `Status` queries. Writing into a pane now hands off cleanly instead of
  fighting the previous controller for it — a displaced client gets a clean
  EOF and its stale input is dropped, closing a long-standing hazard where
  two writers on one pane could leave it looking frozen. `tty7_core::client`
  exposes the `ControlClient`/`PaneClient` pair the CLI is built on as a
  public library, with zero behavior change to the GUI's own connection code.

  There is deliberately no interactive `attach` verb — agents drive panes
  with `run` / `send` / `capture` / `events` instead of taking one over
  interactively. An `attach` verb was designed and partway built during this
  work and then dropped before it ever reached a release, so this is design
  history, not a behavior change for anyone — the CLI itself is new in this
  release, so there was no previous `tty7 attach` to remove out from under
  existing scripts. The protocol-level `Attach` call the GUI's own panes use
  is untouched.

  Every unimplemented verb (`ws stop`, `machine connect`/`disconnect`, bare
  `tty7` launching the GUI) says so in `--help` rather than only failing at
  runtime. Along the way, the GUI's own copy was reworded for the same two
  concepts the CLI now names — "server" for the background process, "shell"
  for what runs in a pane — replacing inconsistent uses of "daemon" and
  "session" in the tray, palette and confirmation dialogs (no behavior
  change, wording only). (#274)

- **The CLI ships in every install and lands on PATH automatically** —
  previously it was built by every release and then discarded: the bundle
  scripts copied only `tty7-app`, and the upload glob never reached the CLI
  binary. It now ships inside the macOS `.app`, the Linux tarball and
  AppImage, and the Windows installer and zip, and a fresh launch puts it
  where a shell can find it without you doing anything.

  The install is split into two halves on purpose. The **environment half**
  prepends the CLI's directory to the GUI process's own PATH before the
  server is spawned, so every pane — which inherits its environment from the
  server, which inherits it from the GUI — can run `tty7` immediately. It
  writes nothing to disk and cannot fail. The **on-disk half** covers typing
  `tty7` in some other terminal, and is allowed to fail safely: on Unix it
  symlinks into a fixed, ordered list of candidate directories
  (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, `~/bin`,
  `~/.cargo/bin`) rather than scanning PATH for the first writable entry —
  version-manager shim directories (pyenv, rbenv, asdf, mise) sit at the
  front of many people's PATH and are writable, and a binary dropped there
  survives only until that tool next rehashes and silently deletes it. On
  Windows it appends the app's directory to `HKCU\Environment` through the
  registry API rather than `setx`, which truncates and corrupts anything
  over 1024 characters, and reads/writes the value as UTF-16 end to end so a
  PATH entry Rust can't represent as a `String` survives untouched.

  It is deliberately conservative about what it touches: only a symlink (or,
  on the AppImage, a copy) this installer itself created is ever replaced —
  marked as ours with a small marker file so a user who moves from the
  AppImage to the tarball still gets recognized and re-linked rather than
  mistaken for someone else's binary — and a real file or a symlink aimed
  elsewhere is left alone. An occupied candidate directory doesn't end the
  scan, and every platform now reports whether the install actually wins the
  lookup (`InstalledShadowed` names whichever `tty7` beats it on PATH).
  Debug builds and cargo build trees only get the environment half, so a
  `cargo run` can't repoint your real `tty7` at a binary the next `cargo
  clean` deletes. The Windows uninstaller removes the PATH entry it added;
  on Unix there is no uninstall hook, so the symlink is left dangling and
  that limitation is stated in the module docs rather than left to be
  discovered. The whole thing turns off from Settings → About, or
  `install_cli_on_path: false` in `config.json`. (#277)

- **Pi is a first-class agent, not a fallback one** — Pi panes drew the generic
  robot glyph every unbranded agent shares, so a Pi tab was indistinguishable
  from an Aider or Qwen one in the sidebar, the tab chip and the tray menu. They
  now carry their own avatar on the existing sky accent, status dot unchanged.
  The mark is Pi's own, from pi.dev, rescaled to tty7's 24x24 icon grid — its
  `prefers-color-scheme` stylesheet dropped, since these avatars are tinted by
  the app. Restoring a Pi pane also resumes its conversation now: the tty7
  extension reports Pi's session id, and the resume command is
  `pi --session <id>` (Pi's `--resume` is a boolean that only opens the
  interactive picker), with `--session` / `--session-id` / `--fork` /
  `--resume` / `-r` / `--continue` / `-c` stripped off the replayed launch
  flags so the restored id wins. A pane launched with `--no-session` is not
  resumed at all — that pane never wrote a session to disk, and reopening one
  would override the choice to keep it ephemeral. (#240)

- **Fork an agent session** — branch a live agent conversation into a second,
  independent one, so a risky direction can be tried without losing the thread
  that got you there. tty7 shells the agent's *own* fork command rather than
  touching its transcript files: `codex fork <id>`, `claude --resume <id>
  --fork-session`, `opencode --session <id> --fork`, `grok --resume <id>
  --fork-session` — every one checked against the installed CLI's own help.
  Agents with no fork tty7 could verify simply don't offer the action, rather
  than getting a row that can only produce a usage error. The command carries
  the pane's original launch flags exactly as session restore does, and sheds
  the stale session-targeting ones so a fork of a fork can't branch twice or
  replay an old id as a prompt.

  Where the fork lands follows where you asked from: right-click a **pane** and
  it asks for a split placement (Right / Left / Down / Up), since a pane-level
  ask is a spatial one; right-click a **tab** or a sidebar row and it opens in a
  new tab, with no placement question. The command palette and the File menu
  carry the new-tab form only — a placement question means nothing when the ask
  wasn't a spatial one — while all five, the four directions included, are
  bindable in Settings → Keybindings.

  A fork needs the session id the agent's hooks report, so the row disables
  itself — rather than disappearing — until one arrives, and a remote pane can't
  fork at all (the command would run against the *local* agent). Forking while a
  turn is in flight is allowed but says so: agents fork from the persisted
  transcript, so the turn you're watching won't be in the copy. The parent is
  never modified either way. (#241)

- **Your WSL distributions are machines you can open a workspace on** — the
  transport for them has been there since remote workspaces landed
  (`wsl.exe -d <distro> -- tty7-server --stdio`: no SSH, no address, no
  credential, no host key), but nothing offered one, so nothing could reach it.
  Every installed distribution now appears in the workspace switcher beside your
  saved SSH hosts, and opening one is the whole setup — there is nothing to
  configure, because there is nothing that *could* be configured. The row is
  named exactly what `wsl -d` calls the distro, since that string is also how
  the machine is keyed (`wsl:<distro>`), and searching the switcher for `wsl`
  finds all of them.

  A distribution is served the Linux `tty7-server` this client shipped with
  rather than one downloaded from a release, so the first connect writes it into
  `~/.local/share/tty7/bin` inside the distro — with the same one-time
  confirmation any other machine gets, and no `sudo` anywhere. A build with no
  bundled server (any `cargo build`, and any platform that isn't Windows) says
  so and names the directories it looked in.

  Two things had to be fixed for this to work at all, both invisible until
  something could actually select a distribution. WSL kills what an interop
  session started the moment its `wsl.exe` exits, and `setsid` does not make the
  new daemon safe instantly — so the launch now holds its invocation open until
  the daemon answers, instead of reporting "started but nothing was answering on
  the control socket" over a correctly installed binary. And a pane's connection now asks
  for the remote's *pane* socket: the flag that says so was added by the SSH
  path and by the local `--stdio` one, but never by WSL, so the workspace would
  connect and the window would open with a pane that could not reach the machine.

  Ports need no forwarding (WSL shares `localhost`, so ⌘-clicking a dev server's
  URL just works), and files move over the same `Host` calls every remote
  workspace uses, or through `\\wsl$` directly. (#253)

- **Copy Session ID** — the agent's native session id on the clipboard, beside
  *Copy Working Directory* in the tab / sidebar context menu, the palette and
  the File menu. Codex has no copy-or-duplicate subcommand — forking *is* how
  you duplicate a conversation there — so copying the id is what "copy the
  session" means: paste it into `codex resume`, a bug report, or another tool.
  (#241)

- **Remote panes read big git output incrementally** — `Host` grew a streaming
  companion to its buffered `git`, implemented on both the local host and the
  remote wire protocol, so a read whose size scales with the work tree no
  longer has to exist in memory all at once on either side. The buffered call
  stays for the many reads that answer in bytes. A stream that goes silent for
  two minutes while the link stays up ends with a timeout rather than parking
  its reader forever — the wait is between chunks, not on the whole read, so a
  slow-but-alive `git diff` still runs to completion. And the queue *between*
  the two ends is bounded as well, not just the reads at either end: a peer that
  pushes faster than this side can parse is cut off at 32 MiB of arrears with an
  error, rather than quietly reassembling the whole diff in a channel. (#247)

- **Sidebar diff preview is optional** — clicking a sidebar row's `+N −N`
  working-tree counts opens the diff overlay, which is the point of them for
  most people but not for everyone. Settings → Window & Tabs → *Open diff
  preview from sidebar counts* turns the click off (`sidebar_diff_preview` in
  `config.json`, on by default). Off, the branch and the counts stay exactly
  where they are and read exactly the same; they simply stop being their own
  click target, so the press falls through to ordinary tab activation like any
  other part of the row. (#247)

- **Kitty graphics protocol, with a shared-memory fast path** — TUIs that
  draw images now render inline in a pane. The daemon intercepts kitty
  graphics escape sequences (`ESC _G…`) in the pane reader before they reach
  scrollback or the client's VT parser — a zero-copy sniff on ordinary
  output, allocating only once a chunk actually carries a graphics command —
  and answers `a=q` capability queries directly on the PTY so probing
  senders see support. Images and deletes travel to the client as compact
  binary frames interleaved with normal output in stream order, so an image
  lands at the cell the sender drew it at; decoding (inflate, BGRA swap,
  atlas placement) runs off the render thread with newest-frame-per-id
  coalescing, so a full frame from a graphics-heavy TUI can't stall PTY
  output or scrolling. For a local pane, file and POSIX shared-memory
  transfers are honored directly — the daemon reads the object and hands the
  client raw pixels, skipping the zlib inflate the compressed-inline path
  would otherwise force. A remote (SSH) pane keeps refusing shm/file and
  rides the compressed-inline path over the tunnel instead. (by @ayamir in
  #272)

### Changed

- **Windows and Linux stop taking keys the shell needs** — `secondary` means
  Cmd on macOS and Ctrl everywhere else, and the default keymap was carried over
  from macOS unchanged. That put window actions straight on top of terminal
  control codes: Ctrl+D could not send EOF (it split the pane), Ctrl+[ could not
  send ESC (it cycled panes), and Ctrl+W, Ctrl+K, Ctrl+P, Ctrl+J, Ctrl+T,
  Ctrl+Q and Ctrl+S were all spoken for. These are window-level bindings with no
  context, so they matched before the terminal ever saw the key — the code that
  sends EOF was there, just unreachable.

  Off macOS the rule is now that `ctrl-<letter>`, `ctrl-[`, `ctrl-]`, `ctrl-\`
  and `ctrl-space` belong to the terminal, and window actions live on
  `ctrl-shift-*` — the convention GNOME Terminal, Konsole, Windows Terminal and
  WezTerm already share. A test enforces it, so the next binding added cannot
  quietly reintroduce the problem.

  | action | was | now |
  |---|---|---|
  | Focus previous / next pane | Ctrl+[ / Ctrl+] | Ctrl+Shift+[ / Ctrl+Shift+] |
  | Split right / down | Ctrl+D / Ctrl+Shift+D | Ctrl+Shift+D / Ctrl+Alt+Shift+D |
  | Close tab | Ctrl+W | Ctrl+Shift+W |
  | New tab | Ctrl+T | Ctrl+Shift+T |
  | Reopen closed tab | Ctrl+Shift+T | Alt+Shift+T |
  | Clear scrollback | Ctrl+K | Ctrl+Shift+K |
  | Command palette | Ctrl+P | Ctrl+Shift+P |
  | Toggle right panel | Ctrl+J | Ctrl+Shift+J |
  | Toggle left panel | unbound | Ctrl+Shift+B |
  | Quit | Ctrl+Q | Ctrl+Shift+Q |
  | Fullscreen | Ctrl+Enter | F11 |
  | Activate tab 1-9 | Ctrl+1-9 | Alt+1-9 |
  | Focus pane by direction | Ctrl+Alt+arrow | Alt+arrow |

  Ctrl+S keeps saving in the code panel but now falls through to the terminal
  when the editor does not have focus, so a shell still receives XOFF.
  Ctrl+C, Ctrl+V and Ctrl+X are unchanged — Ctrl+C still copies only when there
  is a selection and sends SIGINT otherwise — and **Shift+Insert** now pastes.
  With Alt+←/→ focusing panes (Windows Terminal's default), word-by-word
  movement in the prompt editor lives on Ctrl+←/→ — the Windows/Linux text
  convention, which already worked — with Ctrl+Shift+←/→ and Alt+Shift+←/→
  both selecting by word. macOS bindings are untouched. Anything you rebound
  yourself still wins; only the defaults moved. (#270)

- **Ctrl+Shift+C and Ctrl+Shift+V copy and paste, like every GUI terminal** —
  the chord GNOME Terminal, Konsole, Windows Terminal and WezTerm all teach.
  Ctrl+C/Ctrl+V still work as before; Shift+Insert stays a second paste chord,
  and moving Paste to a key of your own retires both defaults together. Copy
  and Paste now sit in the default keymap table rather than being installed
  behind the scenes, so Settings → Keybindings lists them, records new chords
  for them, and warns when another binding would collide — previously
  Shift+Insert was invisible there and could not be reassigned. (#271)

- **The machine that runs your panes now owns their layout** — the workspace,
  tab and pane tree has moved out of the app and into the background service, so
  one machine has one tree that every client of it reads: the window on it, a
  laptop connected to it across the world, and (next) the session CLI. Clients
  send named edits ("split this pane", "rename that tab") and receive the
  incremental changes other clients make, which is what lets two windows on one
  machine both land their work instead of the last one to save winning. A pane's
  working directory, its coding agent and whether it is still running are now
  observed by the service that owns the PTY rather than remembered by whichever
  client last wrote a file — so after a service restart every pane is *known*
  dead and revives into its recorded directory with its agent conversation
  resumed, with no guessing about which saved ids survived.

  Two consequences worth knowing before you upgrade:

  - **Saved layouts do not carry over.** The tree is a new file
    (`~/.local/share/tty7/machine.json`) and the old `session.json` is not read;
    the upgrade also replaces the background service, which ends the panes it was
    holding. The first launch after upgrading comes up on a fresh workspace, and
    tabs from before it are not recoverable. `views.json` (window geometry and
    which workspaces you had open) replaces `session.json` for the client's own
    half; the old file is left on disk, unread.
  - **Windows keeps its panes but not its layout, for now.** The tree is served
    over the same control channel remote machines use, and that channel is
    Unix-socket-only today, so on Windows tabs do not come back across a restart.
    Panes, splits, agents and shell integration are unaffected within a session.
    (#260)

- **The prompt editor's soft newline is now a rebindable action** — `Shift+Enter`
  and `Alt+Enter` have inserted a literal newline into the command editor since
  the multi-line prompt editor landed, but the chords were hardcoded in the key
  handler: there was no `InsertNewline` to name in `keybindings`, and no way to
  move the gesture to a chord of your own. The behaviour is unchanged out of the
  box — both chords still insert, plain `Enter` still submits the whole buffer —
  but it now runs through an `InsertNewline` action, so it appears in Settings →
  Keybindings and can be rebound like anything else, and rebinding it retires
  both defaults. Only the prompt editor answers it; with a full-screen program on
  the pane the chord reaches the application exactly as before. `⌘⏎` fullscreen
  and `⌘⇧⏎` pane zoom are untouched.

  Two smaller behaviour changes come with it, both aligning on what other
  terminals do. With a completion menu open, the newline chords now insert a
  newline and close the menu instead of accepting the highlighted candidate —
  plain `Enter` remains the key that accepts it. And `Shift+Alt+Enter`, which
  the old modifier test caught by accident, now submits like any other `Enter`:
  keybindings match modifiers exactly, and no terminal treats that three-key
  chord as a newline. (#246)

- **Every header in the window moves it now** — grabbing the window by a header
  is a property of the whole app rather than a per-surface feature, so you never
  have to learn which rows happen to be draggable. The detail panel's section
  title joins the caption rows that already were, wherever the panel draws one —
  every tab off macOS, where that row is also the panel's tab switcher, and the
  remote Files header on macOS. In horizontal-tab mode the strip also keeps a
  bare 80px slice of caption for grabbing: its spacer was a flexible one with no
  minimum, so it collapsed to exactly 0px once the tab chips saturated the row —
  around 7-8 tabs on a 1440px window — leaving nothing to grab but three 6px
  gaps and a hairline above and below the chips. The chip row's fixed-chrome
  reserve is corrected to match: it was a flat 100px, sized when the corner held
  a 30px "+" and a 30px "⋯", and was never raised when the workspace chip
  absorbed the "⋯" menu in #169/#188 — so the row's width budget was ~20px of a
  lie. It now measures the group it is reserving for, ~121px of fixed chrome plus
  the 80px grab handle, so chips reach their minimum width and truncate a tab or
  two sooner. (#252)

- **Large working-tree diffs no longer stall the window** — the diff overlay
  had five costs that all scaled with the size of the tree rather than with
  what it could show. Four were named by issue #239's source-level analysis;
  the fifth, the untracked list, turned up while fixing those. Measured on a
  300-file, 90 000-line, 4.5 MB working-tree diff:

  - The full `git diff HEAD` was read into one `String` before parsing began.
    It is now read incrementally through a new streaming call on `Host`, so
    what is held at once is one 64 KiB read buffer plus the line being
    reassembled rather than the whole diff — and that line is itself capped at
    1 MiB, because a line is only complete at its newline and a minified bundle
    is one line of many megabytes. Measured on this repository's own `git log -p
    -n 400` (8 269 409 bytes of real git output, release build): 8.3 MB resident
    → 64 KiB, and *faster* end to end — 829 ms buffered (817 ms read + 12 ms
    parse) → 557 ms streamed, because parsing now overlaps with git producing
    output instead of waiting for all of it.
  - The parsed snapshot was deep-cloned onto every tab's overlay *on the UI
    update path*. It is now shared behind an `Arc`: 1.99 ms of main-thread
    copying per holder → 10 ns (426 µs → 10 ns even at the new retention
    budget).
  - The per-file 2000-line cap bounded one pathological file but not the sum,
    so two hundred ordinary files could retain 90 000 `DiffLine`s. A repo-wide
    budget now caps retained lines and files-with-hunks — 90 000 lines / 6.2 MiB
    of line text → 20 000 lines / 1.2 MiB — while `+N −N` keeps counting the
    whole diff, so the numbers stay exact and the overlay doesn't re-probe in a
    loop chasing a total it can no longer reach.
  - The untracked list escaped all of the above: `git ls-files --others` reports
    every path not yet ignored, and one un-ignored `node_modules` reached the
    overlay as tens of thousands of rows without touching the diff at all. It is
    now streamed, capped where it is retained and again where it is rendered —
    while the count shown stays the true total, so a capped list never reads as
    files having vanished. It deliberately does *not* drive the collapse-
    everything rule below: folding file bodies shut removes no untracked rows,
    so a tree with an un-ignored dependency directory and three edited files
    would have hidden the three cheap things and kept the expensive one.
  - The 400-line auto-collapse rule was per-file, so sixty medium files all
    opened at once (thousands of side-by-side rows, none of them individually
    large). Past a repo-wide total the overlay now opens with every file
    collapsed — zero rows built — leads with a summary saying the diff is too
    large to render efficiently, and points at expanding individual files or
    `git diff` in the terminal. That total counts the context lines git prints
    around every hunk, which is what is actually rendered, so it sits several
    times above the `+N −N` figure at which it would otherwise fire on an
    ordinary afternoon's work.

- **One `git diff` per repository, not two** — the Changes panel and the diff
  overlay each ran their own full-diff probe and kept their own snapshot of the
  same repository. They now share one probe and one snapshot, so opening both
  costs one shell-out and one parse, and opening the overlay while the panel is
  already showing that repo paints immediately instead of re-probing. (#247)

### Fixed

- **A remote workspace no longer loses or crosses its own layout on reopen
  or restart** — two related failure modes in the same area, closed
  together. Reopening a remote workspace, or coming back to one whose
  `tty7-server` had been replaced, could land on permanently `disconnected`
  panes with the coding-agent conversations gone: there was no way to tell a
  genuinely restarted server (which needs a rebuild from the saved layout
  and fresh shells) from a link that had merely blinked (which just needs
  re-attaching). A new `ControlHelloOk.instance` identifies the server
  *process* so a reconnect can tell the two apart — an absent instance reads
  as unknown, never as a restart — attach failures now fall back to
  spawning instead of stranding the pane in a link-down state, and "End
  Sessions" clears the recorded pane ids so reopening spawns fresh shells
  with `--resume` rather than trying to re-attach to panes that are already
  dead.

  Separately, a restart could materialize one *local* workspace's tabs
  inside a different workspace's window and auto-resume every one of their
  agents a second time — real, duplicate `claude --resume` calls against
  conversations already running elsewhere. The root corruption that seeded
  a workspace's records with another one's panes is still unattributed, but
  every way it could propagate or amplify is now closed: `Spawn` carries
  its owning workspace so restore refuses to attach a pane owned by another
  one; pane ids are bound to the daemon instance that issued them, so an id
  recorded against a dead process can't attach to an unrelated shell after a
  reboot (agent resume still happens on this path, since the pane really is
  gone); deduping two claims on one pane id now strips the loser's agent
  session id and launch flags along with the id, so a corrupted record can't
  double-resume an agent the winner already runs; and saving a session now
  logs an error naming both workspace ids if a window is ever caught holding
  a pane it doesn't own, so a recurrence is caught rather than silently
  re-amplified. All wire and schema changes are backward and forward
  compatible. (#257)

- **A captured pane reads like text, not a stream of raw escape codes, and a
  broken pipe ends quietly** — `tty7 capture --plain` replays a pane's bytes
  through the same `alacritty_terminal` grid the window renders panes with,
  instead of leaving a script to regex-strip escape sequences itself.
  Stripping alone gets the easy 90% and invents the rest: whether 200
  characters with no trailing newline in a 249-column pane is one logical
  line or a soft wrap is a fact about the terminal grid, not about the
  bytes, and the same is true of a `\r`-overwritten progress bar, a
  redrawing shell prompt, and cursor-addressed text. `--plain` now replays
  each streamed segment at the width the daemon actually recorded it with —
  panes on different machines are different widths, and a hardcoded
  assumption would rewrap both wrong — so a captured pane matches what the
  window would have shown. Raw `capture` (no `--plain`) is unchanged: still
  the exact bytes the daemon stored.

  Separately, every CLI verb that outputs to a pipe now ends the way `cat`
  does when the reader hangs up — silently, with the shell's own SIGPIPE
  exit code — instead of a Rust panic and a backtrace on stderr, which most
  verbs produced on something as ordinary as `tty7 ls | head -1`. Rust
  disables `SIGPIPE` by default, so a doomed write surfaces as an
  `io::Error` and `println!` turns that into a panic; the fix restores the
  default disposition on Unix so the kernel handles it directly, and
  recognizes the Windows equivalent (`ERROR_NO_DATA`) at every stdout write
  site since Windows has no signal to hand back to. (#283)

- **Restarting the server no longer strands the window on the home page** —
  after confirming "Restart Server?", the window's post-restart resync could
  go out on the control link to the server that had just been killed: the
  connected flag is an `AtomicBool` the reader thread only flips on EOF, so
  right after `restart()` returns the link often still read as up, the
  layout pull failed against a dead socket, and the failure was logged and
  discarded rather than retried — leaving the window sitting on an empty
  home page with no hint anything was owed to it. A failed hydration is now
  recorded as debt and retried on the next sync, and a window that hasn't
  yet recovered its layout no longer pushes its own (empty) state back to
  the server in the meantime — previously that could diff into "close every
  tab" and erase the layout on disk for a window that was only ever waiting
  its turn. (#282)

- **Tab no longer clears your command line when a remote pane's link is
  down** — `Tab` reaches the prompt editor through a `SendTab` action that
  bypassed the same-input guard ordinary typing goes through, so on a
  reconnecting remote workspace a completion with no candidates handed the
  line off to a dead link anyway: the command vanished, the editor stood
  down for the rest of the prompt cycle, and every keystroke after it —
  Enter included — was swallowed by the very guard the Tab had skipped. The
  guard now covers the Tab path too, so the line stays put and typing
  resumes once the link is back; a late-arriving SFTP completion result is
  also dropped, with the reason logged, rather than acting on a line the
  editor no longer owns. A local pane, which has no notion of a down link,
  is unaffected. (#273)

- **The IME candidate window follows the actual caret, not a parked
  cursor** — typing Chinese in a Kimi CLI pane on Windows stranded the
  candidate list at the input box's bottom-right corner instead of
  following it. Two independent causes stacked: gpui never answered
  `WM_IME_REQUEST` / `IMR_QUERYCHARPOSITION`, which is how Windows 11's
  default Microsoft Pinyin IME asks for caret position — it ignores the
  older `CANDIDATEFORM` call tty7 was already making once per composition —
  so an unanswered query fell back to the IME's own default spot; and Kimi
  CLI itself hides the hardware cursor and draws its own caret as a
  reverse-video cell that never gets parked at the logical input point,
  landing the only real anchor tty7 had at the right edge of the input box's
  border. The fix answers the position query from the input handler and
  re-anchors on every composition update instead of sampling once, and when
  the hardware cursor is hidden and its row holds exactly one caret-sized
  (≤2 cell) inverse run, that run is now read as the IME anchor. Apps that
  show the real cursor (pwsh, vim, current Claude Code) are unaffected; the
  heuristic only applies with the cursor hidden, where the previous anchor
  was arbitrary anyway. (#284)

- **A link that wraps across rows opens the whole URL, not just the clicked
  line** — ⌘-click and ⌘-hover resolved a URL only from the physical grid
  row under the pointer, so a soft-wrapped URL opened its first line only
  and 404'd. OSC 8 hyperlinks now resolve across a soft wrap as one
  contiguous run, using the link's own declared URI rather than
  reconstructing it from the clicked row; bare URLs and file paths resolve
  through the existing soft-wrap stitching, plus a new opt-in mode that also
  bridges a program's own *hard* newline when the row is full to the right
  edge and the last character continues the link into the next row's first
  column — so an ordinary short line is never glued to the next paragraph.
  Double-click word/smart-select is unaffected; only link click and hover
  bridge hard wraps. (by @ayamir in #258)

- **A workspace title stays tied to the workspace** — foreground process and
  agent names no longer replace the workspace title in the sidebar or its
  switcher button. An explicit workspace name wins; otherwise the title is
  derived from the workspace's repo/cwd, with `Untitled` as the final fallback.

- **Fullwidth CJK punctuation no longer overlaps, and prompt-mark scanning
  is faster** — wide glyphs are now shaped independently instead of being
  batched together, which was producing the overlap; OSC prompt-mark
  scanning moves from a byte-by-byte scan to SIMD-backed `memchr`; and Thai
  and Lao SARA AM combining-mark clustering is reconciled with upstream.
  (by @ChihGodlee in #250)

- **tty7 gets a taskbar icon and identity on Linux** — the window now sets
  `app_id: "tty7"` on every platform, which X11 uses as `WM_CLASS` and
  Wayland matches against the packaged `tty7.desktop` entry, so taskbars and
  window switchers can resolve the app's identity and icon where they
  previously couldn't. X11 additionally needs the icon on the window itself
  (`_NET_WM_ICON` ships raw pixels per window rather than reading the
  desktop entry), so `assets/app-icon.png` is decoded once and downscaled to
  256px — the most a taskbar uses — and attached via `WindowOptions::icon`.
  macOS and Windows already got their icons through the bundle and the
  embedded `.ico` respectively and are unaffected. (by @zerolover in #254)

- **Installing a remote server keys off the wire dialect it actually
  speaks, not its version string** — two builds between releases can share a
  version string but not a dialect, which previously let a client adopt an
  incompatible remote server and fail permanently in the handshake instead
  of reinstalling. (#264)

- **Listing WSL distributions no longer hangs the shell menu** — a
  starting-up, updating, or wedged `wsl.exe` is now given a bounded wait
  before the probe reports no distributions, matching what an unreachable
  WSL already produced instead of holding the whole menu open.

- **⌃R on a remote machine searches that machine's history** — tty7 owns ⌃R at
  the prompt and shows its own fuzzy menu, but the store behind it had no notion
  of *where* a command had run. Every pane read one file, so ssh'ing to a server
  and reaching for ⌃R offered the commands you had typed on your laptop —
  worse than offering nothing, since the answers look plausible until you run
  one. History is now kept per machine: the local store stays where it was, and
  each remote gets its own, keyed by the target you connected to. On a remote
  workspace tty7 also reads the far end's own `~/.zsh_history` and
  `~/.bash_history` through the same channel it already uses for git and file
  listings, so the first ⌃R on a freshly connected box has something in it
  rather than starting empty. Switching a pane between machines swaps the store
  under it, and the local history file is untouched by the upgrade. (#270)

- **A Windows clipboard pastes like every other clipboard** — text copied on
  Windows carries `\r\n`, and a bracketed paste forwarded it byte for byte. vim
  counts CR and LF as two line breaks, so pasting a block of code into it left a
  blank line under every line — bad enough to make tty7 unusable for editing.
  Bracketed pastes now fold `\r\n` down to a single `\n`, which is exactly what
  the same paste already produced on Linux and macOS. The non-bracketed path is
  untouched: with no paste mode to distinguish text from typing, a line break
  still has to arrive as the CR the Return key sends. (#270)

- **Closing every window before quitting no longer loses your place** — launch
  only ever restored a workspace that still had a window at quit, so closing them
  one by one and relaunching came up on the empty home page, with no hint that
  four workspaces were sitting there. Closing a window here is a *detach*: its
  panes keep running in the daemon, which makes that workspace every bit as much
  "where you left off" as one that still had a window on screen. Launch now falls
  back to the workspace closed last, and the only launch that comes up on a fresh
  workspace is a genuine first run. Deleting a workspace still means deleting it —
  that is the one gesture that drops it from the file. (#267)

- **An unsubscribed directory watch stops delivering immediately** — dropping a
  local watch handle now closes its delivery channel rather than only asking the
  OS backend to stand down. Tearing that backend down is not instantaneous, and
  on Windows a `ReadDirectoryChangesW` completion can fire *during* teardown and
  reach a consumer that has already unsubscribed. Batches already queued stay
  readable, which is the one thing a consumer racing its own drop may
  legitimately still see. (#247)

- **Rounded UI controls no longer square off their corners**
  ([#236](https://github.com/l0ng-ai/tty7/issues/236)) — the cursor-shape
  toggles (Block / Bar / Underline) are the clearest case: the selected
  segment's fill filled the whole corner of the track it caps, with the track's
  own anti-aliased border arc floating *inside* that square. The controls were
  relying on `overflow_hidden` to shape those fills to the track's rounding, and
  it cannot: gpui's overflow mask is an axis-aligned rectangle with no corner
  radii, tested per fragment as a hard discard, so it can only ever cut a square
  and never anti-aliases the cut. A container's own corners come from the quad
  shader's distance field instead, which is why a plain card looked smooth while
  anything with a filled child in the corner did not. Every such fill now
  carries its own radius, inset one border-width so it nests inside the border
  rather than bulging past it: the segmented controls, the −/value/+ steppers'
  hover fills, the theme picker's flush previews, and the diff overlay's card
  headers and closing rows. Most visible at a device pixel ratio of 1, where the
  hard clip edge is a whole physical pixel wide. (#244)

- **Dragging the window by a title bar worked only rarely with a trackpad** —
  five rows that stand in for the caption (the tab rail's top zone, the settings
  page's top strip, the detail panel's top zone, and the code and diff overlays'
  headers) armed their window-drag with a flag that was rebuilt on every render.
  Any repaint between the press and the first drag event threw the arm away, and
  the press *itself* schedules one — these rows carry a double-click, which makes
  gpui refresh the window on mouse-down — so the drag only survived if the first
  move beat the next vsync: 16ms at 60Hz, 8ms on ProMotion. A mouse press nudges
  the pointer and often won that race; a trackpad press is a finger pushing down
  without translating, and almost never did. The terminal's cursor blink disarmed
  it on its own even without a press. The arm now lives in element state, which
  survives frames — where gpui-component's own `TitleBar` has always kept it,
  which is why the ordinary caption strip was never affected. (#252)

- **An idle window stays idle while a file in the Files panel is being written**
  — the tree watches its roots plus every directory you have expanded, so what
  it hears about is a change in a directory it is *displaying*, and a file's
  contents being rewritten reports exactly as loudly as a file appearing. A
  formatter rewriting in place, an editor saving on every keystroke, a build
  dropping its log next to the sources: each of those repainted the whole
  window, twice over, for as long as it went on. A window with nothing new to
  draw never reached render idle, which is what issue #243 reports in its title.
  A batch now repaints only when the re-read comes back different from what is
  already on screen, and that re-read is issued from the watcher callback rather
  than by asking for a paint in order to get one. A closed Files panel does no
  work at all: the change is recorded and picked up on reopening. A panel showing
  search hits is the same case — the hits are their own walk, so no listing is on
  screen to refresh — and the change is picked up when you clear the search box.
  Real changes still arrive exactly as fast.

  Measured headlessly by counting window draws, before and after: five rewrites
  of a file in a displayed directory cost 10 frames and now cost 0.

  `.gitignore` edits keep their whole-cache refresh, now taken only when the file
  can actually govern a directory the tree holds. That one is a correctness guard
  on the most expensive branch here rather than a measurable saving — the watch
  is non-recursive, so a `.gitignore` has to sit directly in a displayed
  directory to arrive in the first place.

  The other half of that report — the Files panel flickering — was fixed
  independently by "keep file-tree listings on screen while they refresh", which
  landed first and is the mechanism the panel now uses. This is only the frames.
  Note the redraw behaviour is platform-independent and is what that number is
  about; the reporter's ~15% CPU figure, and whether their flicker also
  involved something in the Wayland presentation path, were not reproducible on
  macOS and are not claimed to be confirmed. (#249)

- **Return no longer confirms the file tree's delete prompt** — it was the only
  destructive prompt in tty7 with the destructive action first, and on macOS
  (NSAlert) and Windows (TaskDialog) the first button is the Return-key
  default, so pressing Return deleted — recursive folder deletion included. The
  buttons now put the safe option first, matching every other destructive
  prompt, with Escape still cancelling. Linux uses gpui's click-only fallback
  dialog, so the swap only reorders the buttons there. (#255)

- **"Finder" is no longer named on Linux and Windows** — the file-tree context
  menu and the SFTP job list's reveal tooltip hardcoded Finder-flavoured
  labels on every platform; only the Info row's button was conditional. All
  three sites now share that one conditional, so the action reads "Reveal in
  Finder" on macOS and "Open Folder" everywhere else. On macOS the SFTP
  tooltip's "Show in Finder" becomes "Reveal in Finder" too, retiring a third
  name for the same action. (#255)

- **Grok Build turns up in settings search** — the agent renders a Settings →
  Agents row but had no search-index entry, so searching could never surface
  it. The other five agent entries had drifted from what their rows actually
  say ("Claude Code hooks" for a row titled "Claude Code"), as had "Option
  acts as Meta" from the rendered "Option (⌥) acts as Meta"; the index titles
  now match the rows, the mechanism words (hooks / plugin / extension) stay
  behind as search keywords, and a test derives the Agents index from the
  agent list so a future agent can't ship unsearchable. (#255)

## [26.7.6] - 2026-07-28

### Added

- **Readline parity at the prompt** — keys that worked in a raw terminal stopped
  working the moment shell integration engaged, because the local command editor
  owned the keyboard at the prompt and swallowed every chord it didn't
  recognise. `Ctrl-P` / `Ctrl-N` now walk history exactly like ↑ / ↓ (completion
  picker and reverse search included); `Ctrl-Y` yanks back whatever `Ctrl-W` /
  `Ctrl-U` / `Ctrl-K` / `Alt-D` last cut, from a one-slot kill ring of the
  editor's own — zle's ring holds text this editor never cut, so borrowing it
  would paste the wrong thing; and `Alt-.` inserts the previous command's last
  word, repeating to walk further back. Any chord the editor has no answer for
  — `Ctrl-T`, `Alt-U`, a `bindkey`-ed widget — now hands the line to the shell
  so zle's keymap decides, instead of vanishing. Pressing any of them while
  scrolled up snaps the viewport back to the live prompt first, so a recalled
  line can't be edited off-screen. (#222)

- **One Dark Pro built-in theme** — a ninth built-in, slotted alphabetically
  among the dark themes. Two deliberate departures from the VS Code theme's
  terminal set, both caught by this repo's contrast tests: the accent is the
  editor's focus blue `#528bff` rather than the syntax blue, which sits at the
  same luminance as the switch knob it has to be legible against; and normal red
  stays the classic `#e06c75`, because the Pro terminal red lands too close to
  the warning orange to stay distinguishable. (#224)

- **Panes are told which terminal they're running in** — every pane now carries
  `TERM_PROGRAM=tty7` and `TERM_PROGRAM_VERSION`, the de-facto standard pair
  Apple Terminal introduced and iTerm2, WezTerm, Ghostty, VS Code and tmux all
  set. `TERM` names terminfo capabilities and can't answer "which program is
  this", so without the pair, capability probes (`supports-color`,
  `supports-hyperlinks`, and the CLI ecosystem built on them), editors applying
  terminal-specific workarounds, and shell prompts all fell back to their most
  conservative behaviour. tty7's own `TTY7` marker doesn't help them — it exists
  so globally-installed agent hooks stay silent in other terminals, and nothing
  third-party knows to look for it. Unlike `TERM` and `COLORTERM`, both new
  variables can be overridden from `env` in `config.json`: they name an
  identity, not a capability, and posing as another terminal is a legitimate way
  to get a tool that only recognises a fixed list to light up. Local panes only
  — ssh forwards environment variables solely by agreement between client and
  server, so a remote host still sees whatever it sets for itself. (#219)

- **Inactive panes only fade if you want them to** — a split tab dims every pane
  but the focused one so the active terminal reads as foreground. That is the
  right default, but it is not free: at 55% opacity a dim theme's comment color
  or a long-running build's output in the pane you are *watching* rather than
  typing into gets harder to read, and some people track panes by cursor alone
  and never needed the cue. Settings → Appearance → Transparency now carries a
  "Dim inactive panes" switch. On by default, so nothing changes for anyone who
  was happy; off renders every pane at full opacity. (#214)

### Changed

- **One fewer duplicate SVG stack in the build** — the resvg 0.47 bump (#227)
  left the gpui fork on 0.45, so the tree compiled two resvg/usvg/tiny-skia
  stacks. The fork now pins 0.47 too, re-unifying its stack with the one tty7
  uses for the tray icon. (#237)

- **The last duplicate SVG stack is gone** — after #227 and #237 unified tty7
  and the gpui fork on resvg 0.47, the gpui-component fork still declared its
  own `resvg = "0.45.1"`, keeping a legacy resvg/usvg/tiny-skia 0.45/0.11
  stack in the tree. That fork now pins 0.47 as well, so the whole build
  compiles a single resvg stack. (#238)

### Fixed

- **Box-drawing characters no longer break into dashes between rows** — they
  rendered as font glyphs, and a glyph only covers the font's own line height
  while the cell is `font_size × line_height` (1.4 by default). At any line
  height above 1.0 every vertical run of `│` `╭` `╰` was perforated at each row
  boundary: two-line prompts never closed their corners, TUI frames were dotted
  down both sides. U+2500–U+257F and U+2580–U+259F are now drawn as native
  geometry pinned to the cell's real edges — the same special case kitty,
  Alacritty, WezTerm and iTerm2 all ship, and what tty7's Powerline separators
  already did. Covers mixed light/heavy weights, the double-line set with its
  junctions left open, rounded corners, dashes, diagonals, block eighths and the
  ░▒▓ shades. (#229)

- **Underlines survived box-drawing characters, and strokes stay one weight at
  fractional DPI** — two things the native-drawing path changed without meaning
  to, both invisible at the integer device scale it was developed on. An `ESC[4m`
  span or a hovered URL showed a one-column hole wherever it crossed a box
  character, because the underline rides the shaped text run and the native cell
  returned before building one; the cell now shapes a space in its own style, so
  gpui draws the line from the same `UnderlineStyle` as its neighbours. And at
  125% / 150% / 175% display scaling a vertical rule alternated thin/thick across
  every column, since a logical stroke width that isn't a whole number of device
  pixels rounds differently per column; widths are now laid off the snapped near
  edge in whole device pixels. Nothing changes at 1x or 2x. (#234)

- **Thai SARA AM lost its vowel and rendered as a dotted circle** — `ำ` (U+0E33)
  is width 1 and correctly gets its own column, but it is not atomic to the
  shaper: rustybuzz decomposes it and moves the nikhahit backwards onto the base
  consonant, which needs the base in the same run. It was being segmented alone,
  so there was nothing to reorder onto. A following SARA AM is now absorbed into
  the preceding cell's cluster, so base, tone mark and vowel shape together. Lao
  SARA AM (U+0EB3) takes the same path and comes along. (#230)

- **Italic CJK rendered as unrelated CJK on Windows** — every character came out
  as a different character, one for one, consistently, so it read as a broken
  locale or a mangled encoding. It was neither. Hack, the bundled default, has no
  CJK, so those cells are shaped by the font-fallback chain; gpui's Windows
  backend then threw away the face DirectWrite shaped with and looked a fresh one
  up by family, weight and style. That round trip mapped DirectWrite's *italic*
  to *oblique* — the two are numbered the other way around in the API — and a
  family with no oblique face resolved to its upright one. The glyph indices were
  right; the outlines they were pointing into belonged to a different face. Fixed
  in our gpui fork by rasterizing the face DirectWrite actually chose, which also
  closes a latent use-after-free in the same cache: it keyed fonts by a raw
  pointer to a face nothing held a reference to. (#233)

## [26.7.5] - 2026-07-27

### Added

- **Tab completes remote paths in an SSH pane** — path candidates came off the
  local filesystem, so a native-SSH pane deliberately passed no cwd and Tab
  found nothing; a no-match hands the line to the shell, which costs the inline
  editor until the next prompt. Since paths are what people complete most, in
  practice every path Tab in an SSH session dropped the user back to the raw
  shell. Tab now lists the remote directory over the pane's own authenticated
  connection — the same request the SFTP panel browses with — so nothing is
  echoed into the scrollback and no prompt hooks are involved. `cd` still filters
  to directories. Command position and `~/` still fall through, as does a
  foreground `ssh` typed into a local shell or a WSL pane, neither of which has a
  tty7-owned connection to ask. (#217)

- **The last-window close confirmation can be turned off** — Settings → Window &
  Tabs gains **Confirm before closing the last window**, on by default. The
  prompt is a teaching device, not a safety net: ⌘Q, the tray's Quit and the
  palette all quit without asking, and nothing is lost either way since the panes
  keep running in the daemon. Once that model is learned, an extra dialog on
  every quit is friction. Brings the window-close prompt in line with the SSH
  close warning, which has had a toggle all along. (#206)

- **The detail panel and tab rail have a scrollbar** — Info / Outline / Changes,
  the file tree, the remote SFTP listing and the tab rail all scrolled with a
  container that painted nothing, so a deep tree or a long tab list gave no hint
  that there was more content or where in it you were. The bar takes its colours
  from the active theme rather than a stock grey, and follows the platform:
  auto-hiding on macOS, permanently visible on Windows and Linux. (#193)

- **Foreground applications can negotiate the Kitty keyboard protocol** — the
  embedded terminal config inherited `kitty_keyboard: false`, so the parser
  ignored the negotiation sequences and an application asking for progressive
  enhancement fell back to `modifyOtherKeys`, which tty7 doesn't implement. That
  collapsed distinct chords onto the same legacy byte — `Shift+Enter` reached the
  application as a plain carriage return, submitting a prompt instead of
  inserting a soft newline. Legacy input is unchanged until an application opts
  in. (#184)

- **The window's leading corner carries the app's mark off macOS** — macOS fills
  the top-left with the traffic lights; on Windows and Linux that corner was
  empty, with everything the caption row holds (the rail's "+" and collapse, the
  corner chrome, the window controls) pushed to a right edge. The "duo" mark now
  heads the tab rail on its content inset, the line the search box and every row
  label below it start on, and follows the rail's controls into the title strip
  when the sidebar is collapsed — so the corner never falls back to nothing.
  Drawn, never clicked: it takes no hover capsule and no hit box, leaving the
  strip grabbable through it. (#202)

### Changed

- **Interaction state and status colours are derived from the active theme**
  ([#197](https://github.com/l0ng-ai/tty7/issues/197)) — a segmented control's
  selected option was indistinguishable from its neighbours on *every* bundled
  theme (Dracula worst at 1.03:1), because the theme had a colour model but no
  state model: fixed blend ratios scattered across the code, plus every
  gpui-component field nobody had noticed still carrying stock greys and Tailwind
  hues. Each painting surface now carries a state ladder derived to hit a
  contrast *ratio* against that surface, so the result no longer drifts with the
  theme — selected-vs-resting goes from 1.20–1.47:1 to 1.70–1.72:1, anchored so
  Dracula's already-signed-off highlight is a no-op. Ladders are per surface, so
  a menu row anchors to the popover it actually sits on. Taking a fill now also
  takes its paired label colour, instead of each site hand-fixing that
  separately. `danger`/`warning`/`success`/`info`/`link` come from the theme's
  own ANSI-16 at 33 call sites — Dracula's delete button is literally the
  `#ff5555` the terminal in the same window paints with. Switches were inverted
  on every dark theme (a near-black knob on a light track, invisible on the dark
  one) and now take the light end of the theme's axis with the accent on the
  checked track. (#205)

- **The bundled icon set is redrawn to one humanist spec** — the previous set
  accumulated one exception at a time: a heavier `plus` because a bare cross
  looked frail, a `stock/` prefix to undo overrides that were too heavy at 16px,
  two names sharing one drawing. All 17 glyphs now hold to a single spec — 2.1
  stroke throughout, corner radii matching the app's 10px panels, near-square
  boxes of equal apparent area, round caps and joins everywhere. Metaphors are
  untouched, so nothing needs relearning; `folder-closed` gains an inner rule
  that distinguishes it from `folder`, and `info` a title bar that stops it
  colliding with `panel-left`. (#190)

- **The title bar spans the detail panel off macOS** — the panel was a
  full-height column *beside* the bar, and the bar lays out ─ ▢ ✕ at its own
  right end, so opening the panel on Windows or Linux stranded the window
  controls mid-window with the panel's grey to their right. Layout moves to
  `[rail | col(bar / row(body, panel))]` so the bar reaches the corner, with the
  caption row over the panel painted in the panel's own surface — the column
  reads as one continuous sidebar from the very top instead of starting 40px down
  in a different colour. The panel's tab tiles move into the section header it
  was already drawing, since a tile row of its own made three stacked headers
  before a single line of content. macOS is untouched. (#188)

### Fixed

- **Splitting or un-maximizing a pane you were hovering no longer kills the app**
  ([#201](https://github.com/l0ng-ai/tty7/issues/201)) — a pane remembers the
  cell under the pointer (that's what makes ⌘-hover underline links), and nothing
  invalidated it when the grid shrank underneath. The remembered row then named a
  line the grid no longer had, and the next modifier press indexed the grid with
  it — inside a gpui input callback, which is `extern "C"` and cannot unwind, so
  the process aborted with the message lost. `⌘⇧D`, `⌘⇧⏎` and dragging the window
  smaller were all reliable ways to hit it. The hovered cell is dropped on resize
  and the row is validated before indexing. Two other aborts went with it: `⌘T` /
  `⌘D` against a dead daemon now restarts it and retries once, and session
  restore with an unreachable daemon restores what it can instead of dying. A
  panic anywhere in the GUI now also lands in `<config-dir>/crash.log` with its
  message, location and backtrace — the OS report only ever kept the abort.
  (#204)

- **Emoji written with a variation selector take their real width and
  presentation** ([#203](https://github.com/l0ng-ai/tty7/issues/203)) — `🗂️`,
  `❤️` and `⚠️` were budgeted one column, so the glyph bled over its neighbour
  and every following cell on the line shifted left by one, taking selection and
  click hit-testing with it. The selector is zero-width and lands *after* the
  base's column budget is spent, and nothing revisited the decision. The same
  gap made `❤️` render as the black text-presentation heart, identical to bare
  `❤`, because only `cell.c` reached the shaper — dropping every combining mark,
  not just variation selectors. The terminal now re-scores the sequence and
  widens the cell, and carries the marks through to be shaped with their base:
  `❤️` and `⚠️` come up in colour while their bare forms stay monochrome, and
  `e` + U+0301 renders as `é`. Narrowing (`U+FE0E` on an already-wide emoji) is
  still not implemented — it would have to free a column and reflow the line.
  (#210, #216)

- **Untrusted terminal output can no longer freeze a pane** — with Kitty
  keyboard negotiation enabled, an upstream `alacritty_terminal` bug became
  reachable: `push_keyboard_mode` caps its stack by removing from the *title*
  stack, a copy-paste slip that compiles because both are `Vec`s. With an empty
  title stack that panics and kills the reader thread, freezing the pane; with a
  non-empty one it silently drops a saved title, so a later restore returns the
  wrong one. It doesn't take hostile output — a TUI that pushes without popping
  reaches the 4096 cap on its own in a long session, and `cat` of a crafted file
  or a remote host over SSH gets there in one ~20KB burst. Pinned to a patched
  fork, with a regression test that guards the pin. (#194)

- **`theme_follow_system` no longer panics every launch on Linux** — flipping
  "sync with system" on in Settings persists immediately, so the app then aborted
  on every start until `config.json` was hand-edited back, with a backtrace
  pointing at gpui internals and nothing connecting it to the theme toggle. gpui's
  Wayland and X11 backends dispatch the appearance-changed callback while the
  platform client's `RefCell` is already mutably borrowed, and reading
  `cx.window_appearance()` from that callback re-borrows the same cell. The OS
  appearance is now cached in a global that the observer fills from the window's
  own cell, so nothing on the re-entrant path touches the client. macOS and
  Windows were never affected. (#181)

- **A config file saved with a UTF-8 BOM no longer wipes your settings** — every
  config-dir file is read by a loader that treats any parse error as "there is
  no file", falling back to defaults. `serde_json` rejects the U+FEFF a BOM puts
  before the opening brace, so a BOM didn't report a broken config — it reported
  an absent one, and tty7 came up on defaults with nothing in the log to explain
  it. Windows makes this easy to hit by accident: PowerShell's `>`, `Out-File`
  and `Set-Content -Encoding utf8` all write a BOM, so editing `config.json`
  from a shell was enough to lose every setting. `config.json`, `session.json`
  (which lost every workspace the same way) and hand-authored `themes/*.yaml`
  now skip a leading BOM. (#215)

- **New tabs and splits open in the right directory even when the shell can't be
  instrumented** ([#187](https://github.com/l0ng-ai/tty7/issues/187)) — a pane
  learned its directory from `OSC 7`, which only shells tty7 injects its
  integration into ever emit. A shell that `exec`s into another one from its rc
  file (`exec fish` at the end of `.zshrc`), a nested shell started by hand, or
  any shell with no integration at all emitted none — and because a pane's
  directory is seeded with the one it was spawned in, such a pane didn't report
  *no* directory, it reported a permanently stale one. New tabs, splits, the git
  probe and path completion all followed it to the wrong place, with nothing on
  screen to say why. The pane now also reads its directory from the process
  table, on the same half-second foreground poll that already detects SSH
  sessions and coding agents. A shell that does emit `OSC 7` keeps its own
  spelling of the path: `$PWD` preserves the symlinked route the user walked in
  through, and that is the one a new tab should open in. macOS and Linux;
  Windows has no equivalent process query and is unchanged. (#207)

- **Links open on Ctrl+click on Windows and Linux**
  ([#183](https://github.com/l0ng-ai/tty7/issues/183)) — the link modifier was
  the platform key, which gpui maps to ⌘ on macOS but to Win/Super elsewhere, a
  key the OS mostly swallows. Off macOS neither the hover underline nor
  click-to-open could be triggered at all, and the only way to follow a URL was
  to select and copy it — while `config.json`'s own docs had promised
  "⌘/Ctrl-click" all along. Now the same portable secondary modifier the
  keybindings already use, so ⌃-click keeps meaning right-click on macOS.
  Settings copy and the docs follow the platform. (#192)

- **Option-as-Meta works with a CJK input source**
  ([#177](https://github.com/l0ng-ai/tty7/issues/177)) — macOS gives Option two
  jobs, and routes the chord before the terminal sees it. With a non-ASCII input
  source active, ⌥F went to the IME, which committed `ƒ` and consumed the event,
  so the code that turns the chord into `ESC f` never got a say. The setting
  worked on ASCII layouts and silently did nothing on CJK ones, which is why it
  read as intermittent rather than broken. gpui can now decide per keystroke
  rather than once per view, so Meta chords are claimed by the terminal while
  ordinary text, dead keys and Pinyin composition still reach the IME. gpui
  accordingly moves to the `l0ng-ai/zed` fork. (#191)

- **A whole command cycle arriving in one read reports both prompt states** —
  the OSC sniffer folded a chunk's shell-integration marks into one state and
  sent a single frame, hiding the prompt boundary the client counts to tell a
  fresh prompt from a same-prompt redraw. Routine over SSH, where a fast
  command's start mark, output and completion mark leave the remote in one
  packet. Marks on the same side of a boundary still fold, so an ordinary prompt
  draw costs one frame as before. (#217)

- **The detail-panel toggle stops drawing itself as selected, and the Files
  dotfile switch moves into the tree's right-click menu** — follow-ups to the
  caption-row rework, which left the toggle permanently lit and the dotfile
  control competing for room on the header line. (#188)

- **The editor and diff overlays keep their header on the caption line when the
  detail panel is open** — off macOS the title bar is hoisted above
  `[terminal | panel]` so the ─ ▢ ✕ group can reach the window's corner, which
  left both overlays — anchored to the terminal column — starting 40px down.
  Their headers are drawn to *be* the title bar while they're up (its height,
  its insets, a full chrome tile for their one control), and instead landed a
  row low, level with the panel's tab row. They now hang on the row that owns
  the bar, inset by the panel's width, so the corner chrome keeps its surface
  and its clicks. (#202)
- **Those headers became real title bars** — dragging one now moves the window
  and double-clicking zooms it. Both covered the caption row and neither did
  either, with the panel open or closed, so opening a file turned the top of the
  window into a 40px strip that looked exactly like a title bar and answered
  nothing. Their controls (the ✕, the diff's back-to-all-files chip) are
  `occlude()`d to keep taking clicks: a drag region on Windows is HTCAPTION, and
  the OS claims the press before the app hit-tests. Double-clicking a stand-in
  title bar zooms the window on Linux too. (#202)
- **The rail's top zone lines up with the title bar to the pixel** — the bar
  reserves a hairline inside its own height that the rail's stand-in row didn't,
  so everything in that row sat half a pixel low. Invisible on the line-art
  tiles; not on the mark, which visibly hopped as collapsing the rail handed it
  over to the bar. (#202)
- **CJK and emoji stop falling through to the OS on Windows and Linux** — the
  default `font_fallbacks` named only faces that ship with macOS (Menlo, Apple
  Color Emoji), so off macOS the entire chain matched nothing and every
  ideograph and pictograph was resolved by the platform's own cascade instead.
  The bundled Hack primary carries no CJK at all, so on Windows this was every
  Chinese character in every pane. Defaults are now chosen per platform —
  PingFang SC / Apple Color Emoji on macOS, Microsoft YaHei / Segoe UI Emoji on
  Windows, Noto on Linux — and those stock names are appended to a
  hand-written list as well, so a `config.json` that predates this change (or
  was copied from another machine) is repaired at use time without being
  rewritten. Maple Mono NF CN stays first on every platform: its 1.2em CJK
  advance is the only exact fit for the two-column slot Hack's 0.60205em cell
  produces, so a 1.0em stock face is left-aligned there with the remaining
  ~0.2em showing as a gap on the right of each character. (#195)

## [26.7.4] - 2026-07-26

### Added

- **Grok Build gets the full hook integration** — Settings → Agents grows a
  sixth row, installing tty7's hooks to `~/.grok/hooks/tty7.json`. A grok pane
  now carries the same live status the other agents do (blue "working" → green
  "done", amber "needs you" when grok asks a question), and after a restart it
  relaunches `grok --resume <id>` with the original launch flags instead of a
  bare shell. Panes that only ever had the Claude Code hooks installed are
  relabeled to grok rather than reporting as "Claude Code", and grok's brand
  mark replaces the generic robot avatar. (#174)
- **Edit, File and Help menus** — copy/cut/paste/select-all, undo/redo and
  find/find-next now live in a real Edit menu; `About tty7`, `Check for
  Updates…`, `Hide tty7` and Services sit in the app menu; Docs, Discord and
  `Report an Issue` are reachable from Help; and tab actions that used to exist
  only in a context menu (Rename Tab, New Worktree Tab, Close Other Tabs, Close
  Tabs to the Right, Copy Working Directory) are real actions you can also find
  in the palette and rebind in Settings. (#175)

### Changed

- **The menu bar follows the HIG** — `tty7 · File · Edit · View · Window ·
  Help` replaces `tty7 · Shell · Window · View`. The `Shell` menu is gone: its
  new / close / split / rename items are File's job everywhere else, and the
  name collided with Settings → Shell, which configures something entirely
  different. View gained the layout toggles and pane commands that were
  chord-only, and `Delete Workspace…` moved behind its own rule at the bottom
  of File. (#175)
- **The command palette is ranked, banded, and says what it will do** — results
  are scored rather than filtered, so word-initials and prefixes outrank
  scattered letters; the idle list opens on `Recent` (frecency) followed by
  Tabs & Panes · Workspaces · View · Terminal · SSH · Agents · Application
  instead of 47 flat rows. Names describe the outcome and the current state
  (`Hide Left Sidebar`, `Tab Bar: Move to Left Sidebar`) rather than the switch,
  one namespace prefix per subsystem, and the naming grammar is written down at
  the top of `palette.rs`. (#175)
- **Settings re-sectioned around what you're configuring** — the orphan `Shell`
  page folds into Terminal's first group, a new `Input` page collects the prompt
  editor, selection/clipboard and keyboard settings, Terminal drops from seven
  groups to four, and notifications moved to Window & Tabs where the rest of the
  app-level behaviour lives. The search index grew from 14 entries to 51 with
  titles pinned to the rendered row labels, so `opacity`, `blur`, `completion`,
  `ctrl-r` and `grouping` now find their rows. (#175)
- **The tab bar defaults to the left sidebar** — a fresh install opens with the
  vertical tab rail instead of the horizontal title-bar strip. Anyone who has
  flipped the setting or hit `ToggleTabSidebar` has an explicit value persisted
  and keeps their layout. (#171)
- **Idle chrome tiles paint at the rail's ink weight** — title-bar toggles,
  panel tabs, `+` and their siblings now draw their glyph in
  `sidebar_foreground` instead of the near-black `foreground`, with full
  strength reserved for the selected tile. The chrome no longer reads a step
  darker than everything it neighbours, and "on" gets a second cue beyond the
  grey capsule. (#176)

### Fixed

- **Non-ASCII output survives a Finder/Dock launch** — macOS GUI launches can
  omit every locale variable, leaving the shell in the C locale: `ls` printed
  one `?` per non-ASCII byte, so a CJK filename came out as `??????`, and tmux
  replaced each Unicode cell with `_`. New shells now receive a UTF-8 character
  locale (`LC_CTYPE` only, so message/date/number localization is untouched)
  derived from the system locale the way Terminal.app does it, and only when no
  inherited or configured locale exists — explicit locale choices still win.
  The derived name is checked against the locales actually installed before it
  is exported, so it stays loadable on remote hosts too, where ssh forwards
  `LC_*` by default.
  ([#178](https://github.com/l0ng-ai/tty7/issues/178), #173, #180)
- **Right-click Paste dropped images, and Copy looked disabled** — copy, cut,
  paste and undo each had two implementations, a chord handler and an action
  handler, which had drifted: the context menu's Paste skipped the image branch
  ⌘V had, so pasting a screenshot into an agent pane worked by keyboard but not
  by menu, and Copy rendered disabled whenever the selection lived in the prompt
  editor rather than the terminal grid. Both now route through one method each.
  (#175)
- **The palette named the other window's sidebar** — the stateful palette titles
  read the config copies of `sidebar_collapsed` / `right_panel_visible`, which
  only record whichever window toggled them last. With two windows in different
  states, one window's palette offered "Show Left Sidebar" while its rail was
  already out. Each window now labels the toggles from its own chrome state.
  (#175)
- **No more update prompts for a half-built release** — the release step ran
  inside the build matrix, so the first platform to finish published a release
  carrying only its own assets, and that release became `/releases/latest`
  immediately. The in-app update check polls exactly that endpoint, so users
  were prompted to download a version whose `.dmg` was still notarizing. All
  four platforms now upload artifacts and a single gated job assembles one
  **draft** release with every asset. (#172)

## [26.7.3] - 2026-07-25

### Added

- **Multiple windows, one workspace each** — `New Workspace` (⌘⇧N) opens a
  second window with its own tabs, splits, and chrome state. A workspace is
  the persistent thing: closing its window puts it away with its panes still
  running in the daemon, and the title bar's workspace menu, the command
  palette, and the home page's picker all bring it back. `Stop Workspace`
  ends one for real (no default chord — it kills sessions), `Delete
  Workspace` also forgets the layout, and a workspace can be renamed from
  the title-bar chip. (#169)
- **Workspace switcher, in the two places you'd look** — a title-bar chip
  (monogram + chevron) whose menu lists every workspace with a monogram badge
  and a corner dot for the ones whose shells are still running, and the macOS
  **Window** menu listing the same set: on screen first, then the detached ones
  with how long ago you left them. Both show the first nine. (#169)
- **Detail panel** — a docked right-hand column that shows what the active
  pane *is* rather than what it's printing: **Info** (cwd, branch, shell,
  agent, the process tree and the TCP ports the pane is listening on),
  **Outline**, **Changes**, and **Files** (a local file tree). Opening a file
  from it lifts a code editor overlay over the terminal. (#158)
- **Port forwarding and SFTP live in the detail panel** — a native-SSH pane's
  forwards are listed, added and removed from the Info tab, and its remote
  filesystem browses from the Files tab, with transfers reported in a footer
  that rides under every tab. (#166)
- **Shell integration in native SSH panes** — OSC 133 prompt marks, exit
  codes and cwd reporting now reach zsh, bash and fish on the far side, so
  the inline line editor works in an SSH pane exactly as it does locally.
  (#152)
- **Live drag-to-reorder** — tabs rearrange under the cursor the way Chrome
  and Warp do (the dragged tab stays in the list and the ones it passes slide
  out of the way) in the strip, the sidebar rows, and whole repo groups.
  (#151)
- **The rail's top strip drags the window** — the sidebar's title-bar-height
  top zone now moves the window on drag and zooms it on double-click, like
  the title bar it sits level with. (#153)
- **The ⌃R history menu can be switched off** — Settings → Terminal →
  Keyboard, or `history_search` in `config.json`. With it off, the prompt
  line is handed to the shell and the raw `^R` follows it, so a binding
  of your own (fzf, percol, plain reverse-i-search) answers instead of
  tty7's menu. (#163, #170)

### Changed

- **Sidebar and right-panel visibility are per-window** — toggling one
  window's chrome leaves the others alone. The config value is now what a
  *newly opened* window starts with; panel width stays shared. (#169)
- **Chrome icons draw at the size they were asked to** — every tile glyph in
  the window (title bar, rail, panel tabs, overlay headers) had been rendering
  at 12px regardless of the size its call site set, because the button widget
  overwrites its icon's size from its own. The tile rhythm is now stated once
  and applied from one helper, so the marks read at their intended weight and
  the hover capsule keeps a consistent gap from the window edge. (#169)
- **The panel and chrome glyphs are drawn to one spec** — the detail panel's
  tab marks and the title bar's own were redrawn so a row of icons stops
  reading as a row of icons from different sets, and every icon tile shares
  one soft hover fill. (#161)
- **Multi-line commands submit as one bracketed paste** — a 30-line block
  costs one prompt cycle instead of thirty, so it no longer retypes itself
  down the screen on Enter. (#164)
- **The file tree lists directories off the render thread** — `read_dir`, the
  `.gitignore` chain and the search walk left the paint, so a cold cache or a
  large repo no longer stalls a frame. (#160)
- **The LSP client is gone** — opening a `.rs` file in the code panel silently
  spawned rust-analyzer, which indexed the whole workspace for hundreds of
  megabytes of RAM. A terminal shouldn't do that to you on a click, and the
  fix is removal rather than a switch for something nobody asked for. (#159)
- **Sidebar rows float their close button** — the ✕ no longer reserves a
  permanent trailing column, so labels and branch lines get the full rail
  width, and it stays clear of a row's `+n −n` counts. (#145)
- **Daemon wire protocol is now v2** — WSL panes carry a remote-context kind
  that a v1 client can't decode, so it would drop the pane's connection
  instead of ignoring the unknown value. The version handshake now sees that
  skew and offers to restart the daemon, rather than letting a downgraded
  build lose panes silently. (#169)
- **`Cargo.lock` is checked against `Cargo.toml` in CI** — release builds now
  run `--locked`, so a drifted lockfile fails the build instead of quietly
  resolving something else. (#150)

### Fixed

- **⌃J and ⌃M submit the line again** — accept-line's control codes were
  swallowed at the prompt as unrecognized Ctrl chords, so the keys did
  nothing. They now take the same path Enter does, completion picker and
  history menu included. (#163, #170)
- **The sidebar's git counts keep up** — `+N −N` refreshes as an agent's tool
  calls land (throttled per repo) and when the window regains focus, instead
  of going stale until you happened to run a command in that pane. (#149)
- **`ToggleSftp` earns its name** — pressing it while the panel is already on
  Files closes the panel rather than being a dead press, and the remote
  browser's 500ms transfer poll now ends itself when the panel closes instead
  of depending on the render path to retire it. (#167)
- **Closing the last window from the home page quits** — it used to leave a
  windowless process in the Dock that no longer responded to being clicked.
  (#147, #148)
- **Windows titlebar chrome** — the `⋯` menu sits back on the native
  window-control rhythm, and the sidebar's collapse / new-tab buttons take
  their clicks again instead of being swallowed by the drag region. (#162)
- **Windows: the theme panel's close button did nothing** — and the repo group
  header showed a plain arrow where every other row shows a pointer. (#155,
  #156)
- **Settings keeps its stock glyphs** — the page's own icons stopped being
  restyled by the chrome tile pass, and its close button is sized to the title
  bar's rhythm rather than reading undersized beside the window controls.
  (#157, #165)

## [26.7.2] - 2026-07-21

### Added

- **WSL shell integration** — prompt marks, cwd tracking, and agent
  detection now work inside WSL distros. The integration tags the shell
  it actually launches, never blocks the spawn path while probing the
  distro's shell, and declines untranslatable cwds rather than
  misreporting them. (#134)
- **Git Bash shell integration on Windows** — Git Bash panes get the
  full integration, reporting proper Windows paths for cwd-dependent
  features. (#130)
- **Smart double-click selection** — double-click selects the word under
  the cursor with language-aware boundaries, including CJK segmentation
  (the OS tokenizer on macOS, jieba elsewhere); angle-bracket pairs must
  hug their contents and contraction apostrophes stay out of quote
  pairing. (#128)
- **Configurable file-link command** — file links can open with a custom
  command instead of the default editor. Contributed by @ayamir. (#143)
- **Agent resume keeps launch flags** — resuming an agent session
  carries the flags the agent was originally launched with onto the
  resume command. (#144)
- **Sidebar git line follows the agent's cwd** — the branch/status line
  under an agent tab tracks the directory the agent is actually working
  in, not just the pane's shell. (#127)

### Changed

- **Tab close button floats over the label** — the close affordance no
  longer reserves a slot in every tab, so labels get the full width
  until hovered. (#126)
- **Dependency updates** — resvg 0.47, sha2 0.11, a cargo minor-patch
  group bump, and newer CI artifact actions. (#137, #138, #139, #140,
  #141)

### Fixed

- **Tab completion falls through to the shell** — a Tab tty7 has no
  candidates for now hands the line to the shell's own completion
  instead of being swallowed (remote panes included); `cd`/`pushd` stop
  offering files; and the whole feature can be turned off with
  `tab_completion` in config or Settings → Terminal → Keyboard. (#146)
- **Remote panes never leak local filesystem operations** — a remote
  pane's cwd is no longer used for local path lookups, and the agent-cwd
  bypass validates inherited directories. (#133)
- **macOS text input goes through the IME** — synthesized keystrokes
  keep their text (remote/automated input no longer degrades), and Kitty
  full mode stays off the IME path. (#132)
- **No more console flashes on Windows git probes** — git status probes
  and ssh ProxyCommand children no longer flash console windows. (#129)
- **Windows titlebar menu spacing** — the titlebar "..." aligns with the
  window-control rhythm. (#131)
- **Codex avatar and macOS tray sizing** — the Codex tab avatar uses its
  black brand field, and the tray icon renders at 22pt instead of the
  hardcoded 18pt. (#142)
- **Agents settings layout** — rows stay aligned and notes terse. (#125)

## [26.7.1] - 2026-07-17

### Added

- **Follow the OS appearance** — a "Sync with system" mode with separate
  light and dark theme slots: the theme flips live when the OS switches
  appearance, native chrome follows along, and picking a theme while
  syncing writes the slot matching the current mode. Old configs are
  unchanged (sync defaults off). (#121)
- **Mark as Unread on tabs** — the tab context menu can re-flag a finished
  agent result you've already looked at, re-arming the unread badge on the
  Done dot. Agent tabs only; disabled while the agent is still working.
  (#120)

### Changed

- **"Duo" logo refresh** — a new brand mark (two offset session panes,
  mint behind ink, with a prompt chevron) replaces the orange window
  identity across every icon asset: app icon, logo, tray glyph, favicon,
  and social preview. Bare macOS binaries (`cargo run`) now show the icon
  in the Dock too. (#124)

### Fixed

- **Linked git worktrees group under their main repository** — the sidebar
  keys groups on the repository home instead of each worktree's own root,
  so a repo and its worktrees share one header while branch status stays
  per-worktree. (#118)
- **macOS tray icon stays a template image in every state** — the
  attention state no longer swaps in a grey non-template glyph that was
  illegible on many menu-bar appearances; agent status lives in the
  tooltip and tray menu. (#122)
- **No more console-window flashes from Windows agent hooks** — each hook
  emitter frees its throwaway console before it can paint (debug builds
  only; release builds were unaffected). (#119)

## [26.7.0] - 2026-07-16

First CalVer release: versions are now `YY.M.PATCH`, so the number says when
it shipped rather than what changed.

### Added

- **Coding-agent detection on Windows** — the agent status dot now works on
  Windows: agents are detected from the shell-integration command capture
  (no `/proc` there), hook status events reach the daemon via the agent's
  ancestor console, and mark-derived detection only re-fires when the
  command capture actually changes. (#115)
- **Clipboard image paste for agents off macOS** — pasting a screenshot into
  a coding-agent pane on Windows/Linux stages the image to a temp file and
  pastes its shell-escaped path (the same route drag-and-drop uses); Windows
  BMP screenshots are transcoded to PNG since agent vision rejects BMP.
  macOS keeps the higher-fidelity pasteboard read. (#117)
- **Nightly build channel** — `main` is built every night into a rolling
  `nightly` prerelease with all six platform artifacts; the in-app update
  check is prerelease-aware so nightly users ride the channel while stable
  users never see it. (#114, #116)
- **Sidebar groups tabs by git repository** — vertical tabs cluster under a
  repo header, groups persist across restarts, ⌘N numbering follows visual
  order, and same-named repos are disambiguated by their parent directory.
  (#110)
- **System tray icon with agent status menu** — a tray/menu-bar icon
  summarizes agent activity across sessions and jumps to a pane from its
  menu. (#105, #109)
- **Gradient and image theme backgrounds** — themes can render gradient or
  image backgrounds with global window opacity and blur, hot-reloaded like
  the rest of the theme. (#106)
- **Double-click a tab to zoom the window** — matching titlebar behavior;
  rename moved to the context menu. (#103)

### Changed

- **Settings sheet refinements** — controls right-align in their rows with
  hover feedback, the column is tighter, and the theme card is richer.
  (#108)
- **Settings copy polish** — clearer wording throughout, and the SSH
  security defaults stay visible instead of hiding behind a toggle. (#104)

### Fixed

- **Ctrl+C after a copy sends SIGINT again** — copying with Ctrl+C and
  pasting to the PTY now consume the selection, so the next Ctrl+C reaches
  the program instead of copying the same text twice. (#113)
- **Shell vi mode is supported** — vi-mode prompts are detected via durable
  signals and no longer confuse the prompt gap hold. (#102, thanks @ayamir)
- **App cursor shape is respected** — programs that set the cursor shape
  (e.g. vim) see it honored. (#101, thanks @ayamir)

## [0.17.0] - 2026-07-16

### Added

- **Per-tab context menu with worktree tabs** — right-clicking a tab chip or
  sidebar row opens a menu: rename, splits, copy working directory, and a
  close group. *New Worktree Tab* creates a git worktree under the repo's own
  `.tty7/worktrees/<name>` (kept out of `git status` by a self-ignoring
  `.gitignore`), with editable name / branch / start point and a live path
  preview. Closing a tab that sat in a managed worktree offers to remove the
  checkout — unless another pane still lives in it, and dirty checkouts
  default to keeping. (#96)
- **Unread pane count in the tab status dot** — a split tab can finish
  several agent turns while you're away; the green Done dot now swells into
  a badge counting the unread panes (clamped at 9) and shrinks back once
  every pane has been seen. (#98)

### Changed

- **The sidebar diff overlay is per-tab and side-by-side** — the overlay now
  lives on its tab: switching away hides it, switching back restores it
  (re-probing when the status cache disagrees), closing the tab drops it,
  and Esc keeps working. The body switches from a unified diff to a
  GitHub-PR-style side-by-side view with positionally aligned
  removed / added columns. (#100)
- **macOS-style popup panels** — menus get a rounded pill highlight, inset
  hairline separators, a 10px panel radius, and a floatier shadow;
  searchable lists get a taller Spotlight-style search row, and the palette
  viewport holds a whole number of rows so the last visible row is never
  cut mid-height. (#97)
- **README rewritten as a minimal index** — feature details, keybindings,
  and performance notes moved to `docs/features.md` (en + zh-CN); the
  tagline now positions tty7 as a terminal workbench.

### Fixed

- **The agent dot no longer sticks on Waiting after you approve a
  permission** — Claude Code has no "permission replied" hook, so a new
  PostToolUse hook emits a tool-complete event and the first tool that
  finishes after approval flips the dot back to Working. Existing hook
  installs surface as Outdated in Settings → Agents with an Update
  button. (#99)

## [0.16.1] - 2026-07-15

### Fixed

- **Sidebar diff overlay only opens from the `+N`/`−N` counts** — clicking
  anywhere on a tab row's git line (branch icon, branch name) used to toggle
  the diff overlay, hijacking ordinary clicks on the lower half of the row.
  Now only the diff counts are the click target; the rest of the line
  activates the tab like the rest of the row. (#95)

## [0.16.0] - 2026-07-15

### Added

- **Click a sidebar git line to open a working-tree diff overlay** — the
  branch/diff row in the sidebar is now clickable and opens an in-app overlay
  showing the working-tree diff against `HEAD`, file by file with expandable
  hunks. It rides the shared git-status signal: when fresh numbers land that
  disagree with what it shows, it re-probes the full diff so the overlay stays
  live. (#92)
- **Window size and position are remembered across launches** — tty7 saves
  the window geometry on quit and restores it next launch, re-centering if the
  saved bounds no longer overlap any display. Can be toggled off with the
  `remember_window_size` config key. (#94)

### Fixed

- **Attach replay no longer duplicates TUI output into scrollback** — the
  daemon's replay ring is now segmented by the geometry its bytes were
  recorded under, and attach replays a `Size` → `Snapshot` pair per segment.
  Previously the whole ring replayed at the final PTY width, so any resize
  during a session (a pane split, a window drag) re-wrapped older output and
  a TUI's cursor-up redraws (Claude Code's inline renderer, most visibly)
  landed mid-frame, flooding the reattached pane's scrollback with stale
  frame copies that never existed live. The ring also caps its segment count
  so a long-lived session with many resizes can't grow it without bound. (#91)
- **Agent hooks no longer hang when stdin is a terminal** — the hook runner
  skips the stdin read when fd 0 is a tty, so a hook invoked interactively
  (rather than with a piped payload) emits the bare event instead of blocking
  forever on a read that never returns. (#93)

## [0.15.0] - 2026-07-15

### Added

- **Daemon protocol version handshake** — the GUI asks a running daemon
  which wire protocol it speaks before reusing it; after an app upgrade a
  mismatched daemon is kept alive and a prompt offers Keep Sessions or
  Restart Daemon instead of silently killing every persisted session. (#90)

### Changed

- **Tab close affordance** hides until hover on the active tab too, so the
  sidebar and tab strip read clean. (#90)
- **Command palette** no longer offers the Claude-only hook install entry;
  Settings → Agents owns hook installs with per-agent state.

### Fixed

- **SSH connection state** shows as a corner status dot on the tab avatar
  (amber connecting, green connected, red failed) in the same semantic
  colors as agent dots, replacing a theme-grey border ring that read as no
  state at all. (#90)
- **Sidebar git branch/diff line** is shared per repository: panes in one
  work tree read one snapshot refreshed by whichever pane probed last, so
  rows for the same directory no longer show stale or missing +/− counts.
  (#90)
- **Command palette** no longer pins Connect/Save rows above command
  matches for bare words like `java`; QuickConnect rows require a
  host-like query (`@`, `:` or `.`). (#90)

## [0.15.0-beta.1] - 2026-07-15

### Added

- Per-agent hook integrations (Claude Code, Codex, Copilot CLI, OpenCode,
  Pi) with install state and actions in Settings → Agents. (#87)
- CLI coding agents and the git branch are recognized and shown in the
  sidebar. (#85)
- Multi-line prompt editor, plus an I-beam mouse pointer over text. (#80)
- SSH: Unix GSSAPI (Kerberos) authentication. (#81)

### Changed

- Splitting an SSH pane opens another SSH pane on the same host. (#83)

### Fixed

- The grid shifts up when wrapped command input overflows the bottom of
  the screen, keeping the caret visible. (#86)
- Each tab keeps its own active pane across tab switches. (#84)
- The theme panel stays on-screen on narrow windows. (#82)

## [0.14.0] - 2026-07-14

### Added

- SSH connection manager: a native russh client with saved connection
  profiles, password and public-key auth, port forwarding, and SFTP. (#74)
- Buffer search overhaul — richer in-terminal search with rebindable,
  cross-platform shortcuts. (#75)
- Vertical tab sidebar, and Settings reworked into a full-window page. (#70)
- Tab title now follows the active pane. (#73)
- New Settings controls: bell, notify threshold, mouse reporting, and
  session restore. (#68)

### Changed

- SSH saved profiles are now the single source of truth for connections. (#77)
- Bump memchr 2.8.2 → 2.8.3. (#69)

### Fixed

- SSH auth-sheet polish and softer primary buttons. (#79)
- The Cmd+F find bar now owns the top-right slot over the SSH action
  icons. (#76, #78)
- Windows: stop the daemon before install/uninstall so it can replace
  `tty7.exe`. (#72)
- Moved SSH forwards into the pane context. (#71)

## [0.13.0] - 2026-07-13

### Added

- SSH loopback forwarding for links. (#58)
- Editable keybindings: rebindable shortcuts, pane/tab actions, and a tmux
  preset. (#65)

### Fixed

- Trim the first tab's left gap flush to the traffic-light reserve. (#62)
- Remove the active-pane corner indicator dot. (#63)
- CI: format code and platform-gate the ctrl glyph in the keymap test. (#67)

## [0.12.0] - 2026-07-13

### Added

- Ship a Linux AppImage alongside the tarball. (#55)
- Title-bar overflow menu for the command palette and settings. (#57)

### Changed

- Redesigned theme picker with a slide-in panel. (#56)

### Fixed

- Stop the title-bar strip from clipping the Windows close button. (#60)
- Restore the original terminal-window logo, reverting the branding
  change. (#59)

## [0.11.0] - 2026-07-12

### Added

- File-based themes, an in-app theme editor, and a UI/branding refresh. (#54)

## [0.10.0] - 2026-07-11

### Added

- Tab completion now executes the completion specs' *dynamic generators*:
  positions whose candidates come from the live system get real values — git
  branches on `git checkout <Tab>`, container names for docker/podman,
  `package.json` scripts for npm/pnpm/bun/yarn, cargo/rustup/tmux/brew/apt/pip
  listings, and more. Scripts run off the main thread in the session's cwd
  (800 ms timeout, output capped, short-lived cache) and their results merge
  into the already-open menu as they arrive; a slow or failing generator just
  contributes nothing. (#52)
- When shell integration never engages in a pane, pressing Ctrl+R now explains
  why the history menu can't appear (once per pane, dismissed by the next
  keystroke) instead of failing silently — naming the wrapper when a
  figterm-style PTY shim (`kiro-cli-term`, `figterm`, `qterm`) is intercepting
  the shell's OSC 133 reports. The chord still reaches the shell, so its own
  reverse-i-search keeps working. (#46)

### Fixed

- `ssh <Tab>` (and scp/sftp/rsync) now completes host aliases from
  `~/.ssh/config` — `Include` files honored, wildcard patterns skipped — and
  hosts from `known_hosts`, instead of falling back to listing the current
  directory. (#51)

## [0.9.0] - 2026-07-10

### Changed

- Ctrl+R history search is now a browsable menu: matching is fuzzy
  (subsequence with word-boundary/consecutive bonuses; space-separated terms
  must all match) blended with frecency, and the ranked candidates float
  beside the prompt — matched characters highlighted, selection moved by
  Ctrl+R/↓ and Ctrl+S/↑, Enter to edit, Cmd+Enter to run outright. An empty
  query lists the whole history by frecency, so bare Ctrl+R is a "recent &
  relevant" browser. The classic `(reverse-i-search)` line stays. (#45)
- History records now carry when the command ran and its exit code: new
  entries are `<ts>\t<exit>\t<cwd>\t<command>`, written when the command
  *finishes* (zsh `INC_APPEND_HISTORY_TIME`-style, exit code sniffed from
  OSC 133;D daemon-side); older formats still load. The Ctrl+R menu shows
  "ran 3h ago" and a `✗` badge on commands whose last run failed; timestamps
  from zsh/bash history files are carried over when seeding. (#45)

## [0.8.0] - 2026-07-10

### Added

- Copy on select: an opt-in Settings → Terminal → Clipboard toggle (config
  key `copy_on_select`) that copies a mouse selection — drag, double-click
  word, or triple-click line, over terminal output or the prompt's command
  editor — to the clipboard the moment the gesture ends, no ⌘C needed. Off
  by default so a stray selection never overwrites the clipboard. (#34)

### Fixed

- The held-⌘ tab-number badges no longer stick on after the window loses key
  status mid-hold (⌘-Tab, Spotlight, a click into another app). The ⌘ release
  goes to whatever app is key by then, so the window never saw it; the badges
  — and any pending reveal — are now dismissed on the activation flip itself.

## [0.7.0] - 2026-07-10

### Added

- Terminal ANSI colors (`color0`–`color15`) can now be overridden individually
  via `ansi_colors.*` in `config.json`, layered on top of the active theme
  preset — with a color picker per slot under Settings → Appearance → ANSI
  Colors. Malformed values are ignored, and clearing an override falls back to
  the preset's palette. (#37)

- Font ligatures can now be enabled for terminal text. A new optional
  `font_features` config passes OpenType features (e.g. `{"calt": true}`)
  through to the renderer, and Settings → Appearance grows a toggle for the
  common `calt`/`liga` pair. Ligatures stay disabled by default for cell-grid
  safety, and changes hot-apply to open panes. (#38)

### Fixed

- Ctrl+L now clears the screen while the prompt-local line editor is active.
  The readline dispatcher used to swallow it as an unrecognized chord; it now
  forwards the same form-feed byte the raw terminal path sends, so the shell
  repaints its prompt as expected. (#36)

## [0.6.2] - 2026-07-08

### Changed

- Context menus and the "+" dropdown now highlight the hovered row with the
  same soft fill the command palette uses for its selected row, instead of the
  stock saturated accent that snapped hard against the rest of the UI. The
  hover text stays at the normal foreground so it reads clearly on the quieter
  fill.

### Fixed

- On Windows, a pane no longer hangs open when its shell exits on its own.
  Typing `exit`, pressing Ctrl-D, or a shell crash ends the shell, but ConPTY's
  output pipe never reports EOF on a natural exit — and tty7 detected a shell's
  death solely from that EOF — so the pane was left wedged open, dead but
  visible. A Windows-only monitor now waits on the shell process directly and
  reports the exit through the same path a Unix `read()` EOF drives, so the pane
  closes as it does everywhere else. Closing a tab from the UI was already
  unaffected; macOS and Linux are unchanged. (#30)

- Nerd Font prompt icons no longer render sliced off on the right. A non-Mono
  Nerd Font (and the proportional `➜`/`❯` the OS cascade hands back when nothing
  in your font list covers them) gives an icon a single-cell *advance* but draws
  ink up to ~1.9 cells wide, and tty7 clipped every lone glyph to exactly one
  cell — severing the overflow into the half-icons and cut-off arrow from the
  report. A single glyph now paints into a two-cell window, so it renders whole
  (bleeding into the trailing blank the way iTerm2 and Terminal.app do), bounded
  at two cells so a stray face can't smear across the row. Pairs with the native
  powerline separators from #19; Mono Nerd Fonts are unchanged. (#17)

- New tabs and splits no longer stall for seconds while a zsh plugin manager
  reinstalls itself. tty7 launches zsh through a throwaway `ZDOTDIR` (so it can
  layer its shell integration on top of your config), but it used to leave
  `ZDOTDIR` pointing at that empty temp dir the whole time — so tools that find
  their own state via `${ZDOTDIR:-$HOME}` (Zim, oh-my-zsh, `compinit`'s
  `.zcompdump`) looked in the wrong place and rebuilt from scratch on every
  pane, e.g. Zim reprinting `modules/…: Installed` and hanging for ~3s. Each
  redirector now points `ZDOTDIR` back at your real config dir while your
  startup files run, and restores it for the live session, so plugin managers
  and completion caches resolve correctly and load instantly. As a bonus this
  also fixes the classic relocated-config layout (a tiny `~/.zshenv` that sets
  `ZDOTDIR=~/.config/zsh`), which previously loaded your config but silently
  dropped tty7's integration. (#15)

## [0.6.1] - 2026-07-08

### Fixed

- Tab completion (and other line editing) now stays out of the way over `ssh`.
  A remote shell that emits its own prompt marks — fish 4.x on a Linux server,
  most visibly, which ships OSC 133 on by default — used to engage tty7's
  *local* line editor, so Tab ran completion against the local machine's
  filesystem instead of reaching the remote shell. tty7 now only drives the
  inline editor while the shell it launched is itself idle at its prompt;
  whenever a foreground command (ssh, a TUI, a nested shell) owns the terminal,
  keystrokes pass straight through to it. (#26, follow-up to #18)

## [0.6.0] - 2026-07-08

### Added

- The "+" button now opens a shell picker: tty7 detects the shells installed
  on this machine (on Unix the login shell, `/etc/shells`, plus well-known
  shells found on `PATH` — fish, nushell, pwsh and friends installed by
  Homebrew/nix are never registered in `/etc/shells`; on Windows PowerShell 7,
  Windows PowerShell, Command Prompt, Git Bash and WSL distributions)
  and lists them in a dropdown, so opening a tab in a different shell
  no longer requires editing `config.json`. The default entry leads the menu,
  ⌘T / Ctrl+T still opens a default tab in one keystroke, and splitting a pane
  inherits its shell — a fish tab splits into more fish, not back to the
  default. Shells picked this way aren't remembered across restarts (restored
  panes re-attach to their still-running shells anyway).

### Changed

- The Windows default shell now prefers PowerShell 7 (`pwsh.exe`) when
  installed — probed across Program Files (x64/x86/ARM), the Microsoft Store,
  scoop, dotnet tools and `PATH` — and falls back to Windows PowerShell as
  before. Set `shell` in `config.json` to override, as ever.

### Fixed

- Powerline prompt separators (powerlevel10k, oh-my-posh, oh-my-zsh) now render
  pixel-perfect at any font, size and line-height: the eight solid separators
  (sharp triangles, round caps, slants) are drawn natively as fill paths sized
  to the exact cell instead of relying on a Nerd Font, so segments meet their
  backgrounds cleanly with no gaps, narrow wedges or tofu. The bundled Hack font
  is also appended to every font-fallback chain, so common prompt glyphs (➜, ❯,
  box drawing) no longer render truncated or missing when no Nerd Font is
  installed. (#17)
- A URL glued directly to a full-width bracket with no space — e.g.
  `…/pull/343（fix/… → dev）` — no longer swallows the bracket text into the
  link. URL detection now stops at the first non-URL character (a CJK glyph,
  full-width bracket, arrow or emoji), while interior ASCII parens
  (Wikipedia-style URLs) are still preserved.
- Fish, nushell, pwsh and other shells installed by Homebrew or nix now appear
  in the "+" shell picker even when they aren't registered in `/etc/shells`
  (which those package managers leave to the user): a curated set of well-known
  shells is now probed on `PATH` as a catch-all, after the `/etc/shells`
  entries. (#18)
- Upgrading tty7 while an older daemon is still running in the background no
  longer breaks new tabs. A stale daemon that accepts the connection but can't
  serve the new client's request is now restarted once and retried
  automatically; on macOS the GUI also forwards the shell it was launched with
  to the detached daemon, so panes use the right shell instead of a stale
  `$SHELL` inherited from LaunchServices.

## [0.5.0] - 2026-07-07

### Added

- Windows releases now ship an Inno Setup installer
  (`tty7-<version>-windows-x86_64-setup.exe`) alongside the portable zip. It
  installs per-user by default (no admin prompt, with an all-users option),
  adds a Start Menu shortcut and an "Apps" uninstall entry, and offers an
  optional desktop icon. Still unsigned, so SmartScreen warns on first launch.
- Startup update check: tty7 asks GitHub once, in the background, whether a
  newer release has shipped. If so, it pops a one-time "Update available" dialog
  (once per version — remembered in `update.json`, so it never nags twice for
  the same release) and keeps a persistent "Download" prompt in Settings →
  About. Both open the Releases page; tty7 never downloads or updates itself —
  you still install by hand. Turn the check off with `check_for_updates` in
  `config.json` or the "Check for updates on launch" toggle in About. A failed
  or offline check is silent.
- ⌘K (Ctrl+K on Windows/Linux) clears the screen and scrollback — the same
  "Clear" the right-click menu already offered, now on the keyboard shortcut
  Terminal.app, iTerm2, and Ghostty users expect. Also available from the
  command palette, and remappable as `ClearScrollback` in `keybindings`.
- ⌘⏎ toggles window fullscreen (new `ToggleFullscreen` action, also in the
  View menu and command palette), matching the Ghostty/iTerm2 default. It
  previously toggled pane maximize — which silently did nothing in a
  single-pane tab, so the chord felt dead.
- The right-click menu now shows each item's keyboard shortcut. Copy, Paste,
  Select All, and Find previously showed nothing (they're dispatched inline,
  with no bound key for the menu to read a hint from) while the other items
  did, so the menu looked half-labelled. ⌘A / ⌘F stay hint-less on
  Windows/Linux, where those chords keep their readline meaning.

### Changed

- Maximize / restore pane moved from ⌘⏎ to ⌘⇧⏎ (Ghostty's `toggle_split_zoom`
  default), making room for fullscreen on the bare chord. An existing
  `ToggleMaximizePane` override in `keybindings` still wins.

### Fixed

- Windows: launching tty7 no longer opens a stray console window behind the
  app. Release builds are now linked with the `windows` subsystem; debug
  builds keep the console so `println!` output stays visible. (#10)
- The right-click "Select All" now matches the ⌘A shortcut: at the prompt it
  selects the edited command line, otherwise the whole terminal buffer. It
  previously always selected the whole buffer, so click and keystroke behaved
  differently at the prompt.
- Ctrl+R reverse-search now accepts plain ASCII keystrokes. The query only
  took text from the IME commit path, so a non-CJK input source on macOS — and
  all typing on Linux — was swallowed: the search box opened but ate every key.
  Reported on V2EX.

## [0.3.0] - 2026-07-07

### Added

- PowerShell shell integration: `powershell.exe` and `pwsh` now emit the OSC 133
  semantic-prompt marks and OSC 7 cwd that zsh/bash/fish already do, injected
  via `-EncodedCommand` after the user's profile loads (their config is never
  touched). This turns on the inline line editor at the PowerShell prompt — so
  clicking positions the caret and new tabs/splits inherit the working
  directory — which is what previously made mouse clicks a no-op at the prompt
  on Windows.

### Fixed

- Typing `exit` (or Ctrl-D) left a dead "process exited" pane behind instead
  of closing it. A pane whose shell genuinely ends now closes itself —
  collapsing its split, or closing the tab when it was the only pane (the
  last tab falls back to the home page), like every other terminal. A pane
  that merely *lost its daemon connection* still stays visible: auto-closing
  those would silently discard — and kill — sessions that may still be alive
  daemon-side. Panes that died while detached clean themselves up on the next
  attach the same way.

- A full-screen TUI dying without restoring the terminal — the canonical case
  being an ssh session dropping mid-`htop`/`vim` — left the pane stranded on
  the alt screen with a hidden cursor and live mouse reporting: a visible
  prompt with no cursor anywhere, mouse clicks echoing `0;19;42M`-style junk,
  and broken scrollback. The client now scrubs this residue the moment the
  shell reports its next prompt (OSC 133): it leaves the stranded alt screen,
  re-shows the DECTCEM-hidden cursor, and disables stale mouse/focus reporting
  and kitty keyboard flags — each reset only when its mode is actually set.
  Reattach self-heals the same way, since the daemon replays the prompt state
  after the ring.

- Windows shell integration never engaged even for the default shell: detection
  keyed off `portable-pty`'s `get_shell()`, which reports `%ComSpec%` (cmd.exe)
  regardless of what's actually spawned, so the PowerShell default was mistaken
  for an unsupported shell. It now resolves to `powershell.exe` directly.

## [0.2.0] - 2026-07-04

### Added

- Underline styles: undercurl, double, dotted, and dashed underlines render distinctly.
- `config.json` hot reload — edits apply to the running app without a restart.
- Desktop notifications driven by OSC 9 / OSC 777 escape sequences.
- Kitty keyboard protocol (CSI u progressive enhancement) for TUI apps like Neovim and Helix.
- Shell integration for bash and fish, alongside the existing zsh support.
- Windows support: cross-platform daemon, PowerShell as the default shell, embedded app icon.
- Linux support: builds against gpui's x11/wayland backends, `/proc`-based foreground cwd + pane-title tracking, Linux CI job, and documented build dependencies.
- Downloadable builds for every platform: the release workflow now packages and uploads all four targets — signed/notarized macOS DMGs (arm64 + x86_64) plus unsigned archives for Windows (`.zip`) and Linux (`.tar.gz`), each via its own `.github/scripts/bundle-<os>` script.
- Settings UI: terminal / appearance / behavior options are configurable from the GUI, with a searchable font-family dropdown and a wider theme gallery.
- Configurable default shell.

### Changed

- Project renamed to **tty7**.
- macOS releases ship as drag-to-Applications DMGs instead of zips, and the
  Intel build moved to the `macos-15-intel` runner (`macos-13` was retired,
  which had silently kept x86_64 assets from ever publishing).
- Pixel-smooth scrollback: scrolling carries a sub-line fraction and shifts the paint instead of jumping whole lines.
- Smoother scrolling on dense screens: glyph shaping is batched and wakeups are coalesced.
- CJK-dense screens paint ~2.4× faster: consecutive wide glyphs batch into single shaped runs (two columns per glyph) instead of painting cell-by-cell; the grid snapshot buffer is reused across frames and the selection/search overlay scans are skipped when nothing is highlighted. Release builds now use thin LTO.
- Type-ahead is integrated into the line editor instead of being stranded on zle's line.
- New tabs open next to the active tab instead of at the end.
- Terminal throughput ~12× faster (11 MB `cat`: ~2.0 s → ~0.16 s; DOOM-fire: ~47 fps → ~920 fps, both at 155×40 on an M1 Pro — now ahead of Alacritty/Ghostty on the same machine): the daemon's replay ring is a `VecDeque` so a full ring no longer memmoves 8 MiB per ~1 KiB PTY read, and the per-connection writer coalesces queued `Output` frames (≤256 KiB) so a flood reaches the client as a few large frames instead of thousands of tiny ones. A backpressure gate (4 MiB high-water) pauses the PTY reader while the client catches up, so a runaway `yes` can't grow daemon memory without bound. `TTY7_TRACE=1` prints per-second reader-loop accounting on both sides for future diagnosis.
- Second throughput pass, another ~1.4× on bulk output (11 MB `cat`: ~160 ms → ~100 ms; sustained plaintext drain 124 → 148 MB/s, vs ~170 MB/s for a raw do-nothing PTY reader on the same machine; DOOM-fire is unchanged — it is producer-bound at ~96 MB/s): the backpressure high-water grows to 16 MiB so a big burst drains at PTY speed while the client parses in its own time; daemon⇄GUI socket buffers grow from macOS's 8 KiB default to 256 KiB; the client applies consecutive `Output` frames as one batched parser pass (one term-lock + wakeup per burst, latency-free — the batch never waits for unarrived bytes); the shared OSC tokenizer skips Ground/Ignore runs with SIMD `memchr`; the gate's hot path is a lock-free atomic (previously a Mutex plus an unconditional `notify_all` per socket write); and the four threads on the interactive output path ask macOS for `USER_INTERACTIVE` QoS to stay off the efficiency cores (`TTY7_NO_QOS=1` opts out).

### Fixed

- A long `--config-dir` path crashed the GUI at startup ("path must be shorter than SUN_LEN"): when `<config>/daemon.sock` would exceed the OS socket-path limit (104 bytes on macOS), the endpoint now falls back to a short per-user path keyed by a stable hash of the config dir ($XDG_RUNTIME_DIR, else the OS temp dir). Short paths keep the original layout, so existing daemons stay reachable.
- Typing right after a command finished could leave a stray echoed character plus zsh's reverse-video `%` in the scrollback: the "command finished" mark (OSC 133;D) is now emitted the instant the command exits — prepended ahead of the user's precmd hooks (zsh/bash) — instead of after slow prompt frameworks (oh-my-zsh git status, conda), so the local input editor takes keystrokes back hundreds of milliseconds sooner.
- Typing while a command was still running stranded those keystrokes on zle's line at the next prompt — un-editable and double-drawn under the line editor's overlay. Type-ahead adoption (wipe the shell's line, seed the editor) now runs at every prompt, not just the shell's first, and the wipe waits until zle is actually reading (the live `133;B` mark) so it is consumed silently instead of being kernel-echoed into the scrollback as a literal `^U`.
- Typing ahead of a fast command left kernel-echoed debris in the scrollback (`ls` plus zsh's reverse-video `%`). Reconstructable gap input is now held client-side for up to 150 ms: a command that finishes inside the window hands the keystrokes straight to the line editor with the PTY untouched — zero echo; a longer command (or one reading stdin) gets the bytes released verbatim, so REPLs and password prompts still work.
- fish shell integration silently never installed, so fish users got no prompt marks or cwd tracking.
- **Security:** pasted clipboard content is stripped of ESC bytes, closing a bracketed-paste escape that could inject auto-executing commands.
- Crash when copying/cutting right after a forward word/line delete left a stale selection anchor.
- `Ctrl+Alt+<letter>` was indistinguishable from `Ctrl+<letter>` because the legacy key encoder dropped the Alt ESC prefix.
- Plain Enter/Tab/Backspace were wrongly CSI-u-encoded at the kitty-keyboard DISAMBIGUATE level, which could wedge the shell after a crashed TUI.
- No-op edits (e.g. Backspace at the start of the line) no longer swallow the first undo.
- OSC scanners (daemon-side and notification-side) dropped a well-formed sequence that directly followed an unterminated one.
- Daemon pane teardown is hardened: process-group kill, bounded join, dead panes are reclaimed.
- New shells default to `$HOME` when launched from the app bundle with cwd `/`.

## [0.1.0] - 2026-06-30

Initial release.

- Sessions live in a persistent daemon and survive window close / app restart.
- GPU-rendered terminal grid on [gpui], backed by Zed's `alacritty_terminal` fork.
- Tabs and pane splits (split right/down, maximize, focus movement).
- Command palette with fuzzy search over every action.
- Smart line editing: inline completion, syntax highlighting, history, in-terminal search.
- zsh shell integration (OSC 7 cwd + OSC 133 prompt marks) via a throwaway `ZDOTDIR`.
- Native macOS light/dark themes that follow the system appearance.

[Unreleased]: https://github.com/l0ng-ai/tty7/compare/v26.7.6...HEAD
[0.10.0]: https://github.com/l0ng-ai/tty7/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/l0ng-ai/tty7/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/l0ng-ai/tty7/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/l0ng-ai/tty7/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/l0ng-ai/tty7/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/l0ng-ai/tty7/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/l0ng-ai/tty7/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/l0ng-ai/tty7/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/l0ng-ai/tty7/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/l0ng-ai/tty7/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/l0ng-ai/tty7/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/l0ng-ai/tty7/releases/tag/v0.1.0
[gpui]: https://github.com/zed-industries/zed
