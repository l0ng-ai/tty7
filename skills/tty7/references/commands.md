# `tty7` command reference

Every verb, its flags, and the JSON it emits under `--json`. Read the section
you need; the table of contents mirrors the top-level grammar.

- [Global flags](#global-flags)
- [Environment](#environment)
- [Exit codes](#exit-codes)
- [Top-level verbs](#top-level-verbs)
- [`ws` — workspaces](#ws--workspaces)
- [`tab` — tabs](#tab--tabs)
- [`pane` — panes](#pane--panes)
- [`machine` — remotes](#machine--remotes)
- [`server` — the daemon](#server--the-daemon)
- [Not implemented yet](#not-implemented-yet)

## Global flags

Accepted anywhere on the line, before or after the subcommand.

| Flag | Effect |
|---|---|
| `-m, --machine <MACHINE>` | Route the command to a linked machine over the local server's existing link. Matches the full link key (`me@devbox:22`) or the bare host (`devbox`). Ssh links only; a down link or a jump/proxy chain is refused with a reason rather than dialled fresh. |
| `--json` | One JSON object on stdout instead of the human table. |
| `-q, --quiet` | No output on success. Errors still go to stderr. |

## Environment

Set inside every tty7 pane, inherited by anything you launch from one.

| Variable | Meaning |
|---|---|
| `TTY7_PANE` | This pane's id, e.g. `71` or `%71` (both forms are accepted). The default target of `split`, `send`, `capture`, `procs`, `wait`, `pane close`. |
| `TTY7_WS` | This pane's workspace id. The default for `run --keep`, `tab new`, `ws tree`. |
| `TTY7_CONFIG_DIR` | The server's config dir. How the CLI finds the right server's sockets — you never pass a socket path. |

Outside a tty7 shell the address-taking verbs fail with
`not inside a tty7 shell — pass an explicit %pane/@tab/workspace`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | the command failed; the reason is one line on stderr, prefixed `tty7:` |
| 2 | usage error (clap) — unknown verb, missing argument, bad type |
| 124 | `tty7 wait` gave up — the `timeout(1)` convention, so "not yet" is distinguishable from "broken" |
| 141 | Unix only: the reader hung up (`| head -1`) and SIGPIPE ended it, exactly as it ends `cat`. Not a failure. Windows reports 0 for the same thing, having no signal to imitate. |
| *other* | only from `tty7 run`, which passes the child's exit code through |

If `run` cannot learn the child's code it prints a note to stderr and exits 1
with `"exit_code_known": false` in the JSON — that is how you tell a real 1
from a stand-in.

## Top-level verbs

### `tty7 [PATH]`
No subcommand means the GUI. A running window is asked to come forward and open
a tab at `PATH`; if none is registered, the app is launched instead. Without
`PATH` it just activates the app. `-m` is refused — this verb drives the GUI on
*this* machine. JSON: `{"path","delivered","launched"}`.

A word in this position that does not name a path is treated as a mistyped verb
and refused, rather than silently opening a window.

### `tty7 ls`
Same as `ws ls`. Table: `WORKSPACE NAME TABS PANES ATTACHED`.
JSON: `{"workspaces":[{"id","name","tabs","panes","attached"}]}`.

ATTACHED names the host holding the workspace — a GUI window, or another
client — and is `-` when nobody is. It is the hostname only; the token that
proves the hold never leaves the connection that owns it.

### `tty7 run [--keep] [--cwd DIR] [--ws WORKSPACE] -- CMD...`
Spawns a pane running `CMD`, streams its output to stdout, waits, and exits
with its code. The command must come after `--`; anything after `--` belongs to
the child, so `tty7 run -- cargo test --keep` passes `--keep` to cargo.

- `--keep` leaves the pane alive as a new tab afterwards. It needs a workspace,
  so it requires `--ws` or `$TTY7_WS`; without one it is an error, not a
  silent fallback.
- `--cwd` sets the working directory. `--ws` also sets the pane's `TTY7_WS`.
- Interrupting `run` can leave the pane behind as an orphan — `pane ls --all`.

JSON: `{"pane","exit","exit_code_known","kept"}`, printed **after** the streamed
output. The combined stream is not valid JSON; read the last line.

### `tty7 new [PATH] [--open]`
Creates a workspace plus its first tab and shell, at `PATH` if given. Prints
the workspace id. JSON: `{"id","pane","opened"}`.

`--open` also puts a window on it, if a GUI is running on this machine — say
so when you make a workspace for someone to look at. Without it the workspace
is still listed in the GUI's switcher; it just waits there to be opened.

### `tty7 split [%PANE] (--v|--h) [--ratio R]`
Alias of `pane split`. Splits `%PANE` (default `$TTY7_PANE`), spawning a shell
in the same cwd. Exactly one axis is required — `--v`/`--vertical` puts the new
pane below, `--h`/`--horizontal` to the right. `--ratio` (default 0.5) is the
share kept by the *existing* pane. Prints `%NN`. JSON: `{"pane"}`.

### `tty7 send [%PANE] [TEXT] [--enter] [--key KEY]…`
Types `TEXT` into the pane as keystrokes; `--enter` appends CR. With one
argument the text is the argument and the pane comes from `$TTY7_PANE` — but a
lone `%42` is rejected as a missing-text error rather than typed, unless a
`--key` gives it something to do.
JSON: `{"pane","sent","enter","keys"}`.

`--key` presses a key instead of typing characters — the arrow keys a
permission prompt wants, the `escape` that closes a TUI, the `C-c` that stops a
build. Repeatable, delivered in order, and composable with `TEXT` (text first).

| | |
|---|---|
| Named | `enter` `escape` `tab` `backtab` `space` `backspace` `delete` `up` `down` `right` `left` `home` `end` `pageup` `pagedown` |
| Chords | `C-<char>` (Ctrl: `C-c`, `C-d`, `C-z`, also `C-@ C-[ C-\ C-] C-^ C-_ C-?`), `M-<char>` (Alt = prefixed ESC) |
| Aliases | `return` `cr` `esc` `del` `bs` `shift-tab` `pgup` `pgdn` |

Case-insensitive. An unknown name is a usage error (exit 2) raised before
anything is written, so a bad key never lands half a sequence in a live pane.
Each keystroke goes out as its own event 200 ms after the last, which is what
keeps a raw-mode TUI from reading the sequence as a paste; the first write is
not delayed, so an interrupt is immediate.

### `tty7 capture [%PANE] [--plain] [--scrollback]`
The pane's replay. Two independent choices: **how much** — the newest scrollback
segment by default, the whole ring with `--scrollback` (the ring splits into
segments on resize, so for a pane that was never resized the two are identical)
— and **in what form**.

Without `--plain` you get the stored bytes, ANSI escapes intact, decoded as
UTF-8 (invalid bytes become U+FFFD). That is the faithful form: it is exactly
what the pane emitted.

With `--plain` those bytes are replayed through a terminal grid — the same
parser and rev the GUI renders panes with — and you get the text that produced.
The difference from stripping the escapes yourself:

- a line the shell wrapped at the pane's width comes back as **one** line, not
  split at an invented newline
- `\r` **overwrites** rather than breaking the line, so a progress bar reads as
  its final value and a syntax-highlighting shell doesn't echo as `eecho …echo`
- cursor addressing (`ESC[5;80H`) puts text **where the app put it**
- wide characters keep one character per cell pair; combining marks stay put
- each segment is rendered at **its own** width, which is why the size travels
  with it; the empty rows a grid leaves above and below the output are dropped

Reach for it whenever a human would want to read the output. It is still a
screen, though: what scrolled past the top is gone, and an exit code was never
on screen — redirect to a file when you want the answer rather than the view.

Either way it is a snapshot, not a stream: it collects the replay the server
sends, settles for ~300 ms, and returns. Call it again for a newer one.
JSON: `{"pane","text"}`, where `text` is whichever form was asked for.

### `tty7 procs [%PANE]`
The process tree inside the pane, indented by depth, `*` on the foreground
process — then a second table of ports those processes are listening on.
Prints `nothing running in this pane` when both are empty.

JSON: `{"procs":[{"pid","name","depth","foreground"}],"ports":[{"port","pid","name"}]}`.

Nothing below the depth-0 shell means the foreground command has exited — but
you rarely need to check that by hand, because that is exactly what
`tty7 wait --until free` blocks on.

### `tty7 agents`
Every pane running a recognised coding agent. Table: `PANE AGENT STATUS
MESSAGE`, status one of `idle` / `working` / `waiting` / `done`.
JSON: `{"agents":[...]}`, plus `"diagnostics"` when an agent is running whose
status hooks are missing or outdated — the reason an agent can sit in one status
forever. Each diagnostic is
`{"kind":"agent_status_hooks_unavailable","agent","hooks_state","action"}`.

### `tty7 wait [%PANE] [--until STATE,…] [--changed] [--timeout SECS] [--interval MS]`
Blocks until the pane reaches one of the named states. The orchestration
primitive: `tty7 wait %3 && tty7 capture %3 --plain`.

| Flag | Default | |
|---|---|---|
| `--until` | `waiting,done,exit` | Comma-separated; see the states below |
| `--changed` | off | Only wake on a state the pane moved into *after* the wait began |
| `--timeout` | none | Give up after N seconds, exiting 124 |
| `--interval` | 500 | Poll interval in ms (50–3,600,000) |

| State | Means |
|---|---|
| `idle` `working` `waiting` `done` | The agent's own status, from its hooks |
| `no-agent` | Nothing reports status here — a plain shell, or hooks not installed |
| `free` | The foreground command has exited; the pane is back to its bare shell |
| `exit` | The pane is gone. Ends every wait whether asked for or not |

JSON: `{"pane","status","matched","stale","activity","message","session_id"}`.
`stale: true` means the pane was already in that state when the wait began, so
the answer may belong to a previous turn — which is what `--changed` refuses.

Exit 0 = a requested state was reached; 124 = timed out; 1 = the pane exited
without reaching it (the JSON still comes, with `"matched": false`).

Notes that decide whether a loop works:

- **`idle` is not "the command finished".** It is something an *agent* says
  about itself. A pane running a build has no agent and reports `no-agent`.
  Use `free` for commands.
- **`free` costs a second request per poll**, so it is only checked when named,
  and only if none of the agent states you asked for matched first — pairing
  `waiting,done,free` never loses you a `waiting`.
- **`--changed` means something different for `free`**: a shell goes free →
  busy → free and ends where it started, so there is no new state to compare
  against. There it means "something ran while I watched" — exactly what you
  want on the line after a `send`. A command fast enough to finish inside one
  `--interval` is never seen running, so it times out instead; use
  `--interval 100` for those, or drop `--changed` and read a sentinel file.
- **`free` reads the process tree**, so a pane whose root process is the command
  itself (a `tty7 run` pane) looks free while it runs, and a backgrounded job
  keeps a pane busy after the foreground command is gone.

### `tty7 events`
Streams server events until interrupted, one per line — pane exits, agent
status changes, workspace preemption, layout deltas. `--json` makes it NDJSON.
Blocks forever; run it with a timeout or in the background.

### `tty7 status`
Same as `server status`: pid, uptime, pane count, dialect versions, build,
socket path. JSON is the `ServerStatus` object itself (`pid`, `uptime_secs`,
`panes`, `control_version`, `protocol_version`, `build`, `socket`).

### `tty7 doctor`
The install check: the three env vars, whether the server answers, whether its
control/protocol versions match this binary, pid/uptime/panes, how many machine
links exist, and where each agent's status hooks stand. Adds a note when you are
not inside a tty7 shell.
JSON: `{"context":{"config_dir","workspace","pane"},"server":{"reachable","dialect_ok","build","status","routes"},"hooks":{"installed","outdated","not_installed"}}`
— the context fields are booleans, not values; each `hooks` field is a list of
agent slugs.

The hooks row is what explains an agent that never moves: without hooks it
reports no status, so `tty7 agents` shows it frozen and `tty7 wait` only ever
times out. Hooks are a local install, so under `-m` the row reads `unknown`
rather than claiming a gap it cannot see.

## `ws` — workspaces

A workspace is a named tree of tabs and panes the server keeps alive. Address
one by name, by full id, or by a unique id prefix (the 8-char prefix `tty7 ls`
prints is what you normally use). An ambiguous name or prefix is an error that
lists the candidates.

| Command | Effect | JSON |
|---|---|---|
| `ws ls` | every workspace | `{"workspaces":[...]}` |
| `ws tree [WORKSPACE]` | one workspace as a tree: tabs, split axes and ratios, panes with cwds | the whole workspace object: `{"id","name","last_active","tabs":[{"id","name","sidebar_group","root",...}]}`, where `root` is the nested split tree |
| `ws new [NAME]` | an empty workspace (no tab, no pane) | `{"id","name"}` |
| `ws rename WORKSPACE NAME` | name or rename | `{"id","name"}` |
| `ws rm WORKSPACE` | delete the workspace | `{"removed"}` |
| `ws attach WORKSPACE` | become its controlling client | `{"attached","took_over_from"}` |
| `ws detach WORKSPACE` | let go without interrupting anything | `{"detached"}` |

`ws rm` hangs up the panes the workspace held, so removing a scratch workspace
is enough on its own. What does leak is an interrupted `tty7 run`: that pane
keeps running with nothing referencing it. `pane ls --all` finds those.

Prefer `tty7 new <path>` over `ws new` when you want something usable: `ws new`
leaves you with an empty workspace you then have to populate, while
`tty7 new --json <path>` hands back `{"id","pane"}` — both addresses in one go.

The `root` node in `ws tree --json` is externally tagged, so a leaf is
`{"Leaf":{"pane":31}}` and a split is `{"Split":{"axis","ratio","a","b"}}` with
`a`/`b` nested the same way. `d["tabs"][0]["root"]["pane"]` will not work.

## `tab` — tabs

`@N` numbers tabs across the **whole machine** in tree order, densely from `@1`.
The numbering shifts whenever any workspace or tab is created or removed, so
resolve it immediately before use. A full tab UUID also works: `@<uuid>`.

| Command | Effect | JSON |
|---|---|---|
| `tab ls [WORKSPACE]` | tabs of a workspace | `{"workspace","tabs":[{"ordinal","id","name","label","agent","group","panes":[..]}]}` |
| `tab new [WORKSPACE] [--cwd DIR]` | add a tab with a fresh shell | `{"tab","pane"}` |
| `tab close @TAB` | close the tab and every pane in it | `{"closed"}` |
| `tab rename @TAB NAME` | name or rename | `{"tab","name"}` |
| `tab move @TAB INDEX` | reposition within its workspace | `{"tab","to"}` |

GROUP is the heading the GUI's sidebar files the tab under, shown by its last
segment (`group` in the JSON is the whole value). Read-only from here: with the
default repo grouping the GUI recomputes it from the tab's working directory,
so anything written from outside would be overwritten on the next render.

Almost no tab has a `name`: the GUI's tab strip reads OSC titles, which the
machine tree never sees. So the NAME column — and `label` in the JSON — falls
back through the best evidence there is: the name if someone set one, else the
agent running in the tab ("Claude Code"), else the last segment of its cwd,
else the foreground process. `name` in the JSON stays literal, so a script can
still tell a real name from a stand-in.

## `pane` — panes

| Command | Effect | JSON |
|---|---|---|
| `pane ls [WORKSPACE]` | panes with their workspace, tab, cwd, live flag | `{"panes":[...]}` |
| `pane ls --all` | the server's whole pane registry, including orphans no workspace holds | `{"panes":[...],"orphans":N}` |
| `pane split ...` | identical to top-level `split` | `{"pane"}` |
| `pane close [%PANE…]` | close panes; their shells are hung up | `{"closed":[...]}` |
| `pane close --orphans` | close every pane no workspace holds | `{"closed":[...]}` |

`--all` is the one that shows leaks. Each entry is
`{"pane","workspace","orphan","owner","title","cwd","live"}`: `owner` is
`tty7-cli` for panes this CLI spawned (a workspace id otherwise), and
`orphan: true` means no workspace holds it. An interrupted `tty7 run` is what
leaves them.

`close` takes several ids at once and keeps going after a failure: the rest are
still attempted, and it exits 1 with `{"closed":[...],"failed":[...]}` so you
know what is left. `--orphans` closes exactly what `pane ls --all` marks
orphaned, and reports an empty list instead of an error when there is nothing
to do.

**`--orphans` is the user's broom, not yours.** It closes every abandoned pane
on the machine, and an abandoned pane may still be running someone's command.
Point the user at it; don't run it on your own initiative.

`title` is the pane's current title — usually the running command, so it reads
`claude`, `nvim`, `cargo` — which makes `pane ls --all --json` a quick way to
find "the pane running X" without capturing anything.

## `machine` — remotes

`machine ls` lists the local machine plus every link the server holds:
`MACHINE KIND CONNECTED`. JSON: `{"machines":[{"key","kind","connected"}]}`.

`machine connect` / `machine disconnect` are not implemented — links are
managed from the GUI's connection manager.

## `server` — the daemon

| Command | Effect |
|---|---|
| `server status` | same as `tty7 status` |
| `server logs` | tail the server log; prints the path, and says so when logging was never enabled (`TTY7_LOG=info` before the server starts) |
| `server start` | bring up a server on this machine |
| `server stop` | stop it — **every pane on the machine dies** |
| `server restart` | stop, then start — same consequence |

Do not run `start`, `stop` or `restart` on your own initiative. They change or
destroy what the user's GUI is attached to.

## Not implemented yet

These parse and then exit 1 with an explanation:

- `ws stop` — the control dialect has no workspace-stop request yet
- `machine connect` / `machine disconnect` — use the GUI
