---
name: tty7
description: >-
  Drive the tty7 terminal workbench from the shell with the `tty7` binary — list workspaces/tabs/panes, split a pane, send text or keystrokes into one, capture what is on a pane's screen, run a command in a real PTY and pass its exit code through, block until a pane finishes or needs input, see which coding agents are running and which ports a pane is listening on. Use this whenever tty7, panes, workspaces, or `%42`/`@7`/"the other pane"/"the other agent" come up; whenever you want to hand work to another agent and collect the result ("get Claude/Codex to do X", "派个活", "let another agent handle this", running several agents in parallel and merging what they produce); whenever you need to start something long-running or interactive (dev server, REPL, ssh session, `tail -f`, a TUI) that should not sit blocking your Bash tool; whenever a program needs a real terminal to behave the way the user sees it; and whenever you need to look at or report on what is running in some *other* terminal on this machine. Cheap to check: if `$TTY7_PANE` is set you are already inside tty7 and every command here works with no setup.
---

# Driving tty7 from the command line

`tty7` is a thin, non-interactive client of the tty7 server. Every verb returns
and exits; `--json` makes the output machine-readable. The GUI never has to be
running — the server is what owns the panes.

## First: where are you?

```bash
tty7 doctor
```

One table, and it answers everything you need before doing anything else:
whether a server is reachable, whether the dialect matches, whether each agent's
status hooks are installed, and whether `TTY7_CONFIG_DIR` / `TTY7_WS` /
`TTY7_PANE` are set — i.e. whether you are running *inside* a tty7 pane.

Being inside a pane matters for two reasons: the address-taking verbs
(`split`, `send`, `capture`, `procs`, `wait`, `pane close`) default to
`$TTY7_PANE`, and `run --keep` files its pane into `$TTY7_WS`. Outside a tty7
shell you must name a target explicitly, and the error will say so rather than
guessing.

The hooks row matters if you intend to delegate to another agent: without them
an agent reports no status, so `tty7 wait` on it will only ever time out.

If `tty7 doctor` says the server is unreachable, stop and tell the user — do
not run `tty7 server start` on your own initiative. Starting a server they
didn't ask for changes what their GUI attaches to.

## What are you here to do?

Four jobs, four shapes:

1. **Run something that shouldn't block you or needs a real TTY** — a dev
   server, a long test run, a TUI. [Running a command](#running-a-command-two-shapes).
2. **Talk to something stateful over time** — a REPL, `ssh`, a debugger.
   Same primitives: [send](#non-blocking-a-pane-you-talk-to-over-time),
   [read](#reading-a-pane), repeat.
3. **Look at what this machine is doing** — other panes, other agents, ports.
   [Looking around](#looking-around), strictly read-only.
4. **Hand work to another coding agent** — one worker or a fan-out of several.
   Read `references/delegation.md` first; the short version is
   [below](#handing-work-to-another-agent).

The Bash tool remains right for anything that starts, does its job, and exits
without needing a terminal or an audience. A pane earns its keep when the
process outlives your turn, needs a real PTY, or should be visible to the user
in their tty7 window — that last one is often the whole point.

## Addresses

| Shape | Means | Stable? |
|---|---|---|
| `%42` | a pane | yes — a pane keeps its id for its whole life |
| `@7` | a tab, numbered across the **whole machine** in tree order | **no** — it shifts whenever a workspace or tab appears or disappears |
| `@<full tab UUID>` | that same tab, by id | yes |
| `api` / `76698a44` / a full UUID | a workspace, by name, by unique id prefix, or by id | yes |

Re-resolve `@N` right before you use it; never cache one across a step that
creates or removes a tab. Pane ids, tab ids and workspace ids are safe to
remember — so when you create a tab and mean to address it again later, keep the
id `tty7 tab new --json` hands back rather than counting `@N` a second time.

The sigils are optional wherever an address is expected: `%42` and `42` are the
same pane, `@7` and `7` the same tab. Ids copied out of `--json` paste straight
back in.

Omitting the address inside a tty7 shell means "this pane" / "this workspace".
An explicit address always wins over the environment.

## Running a command: two shapes

### Blocking, with a real exit code

```bash
tty7 run -- cargo test          # streams to your stdout, exits with cargo's code
tty7 run --cwd /path -- make
tty7 run --keep -- cargo build  # leaves the pane as a new tab afterwards
```

The command's output streams to your stdout as it happens, and `tty7` exits
with the command's own exit code. This is the closest thing to a Bash call —
the difference is the PTY and the fact that the user can see it.

Three things to know. `--keep` needs a workspace, so it only works inside a tty7
shell or with `--ws <workspace>`. With `--json`, the streamed output comes first
and the JSON object last — the combined stream is *not* parseable as JSON, so
read the last line. And the pane is 120 columns wide with no way to change it,
so output that assumes a wider terminal wraps.

### Non-blocking: a pane you talk to over time

This is the one that makes tty7 worth reaching for. Get a pane, send it work,
come back later.

```bash
PANE=$(tty7 split --v)                  # or --h; splits $TTY7_PANE, prints "%83"
tty7 send "$PANE" 'npm run dev' --enter
```

`split` prints the new pane's address on stdout, which is what you capture into
a variable. Without an axis it is a usage error — `--v` stacks the new pane
below, `--h` puts it to the right.

Splitting `$TTY7_PANE` changes the user's visible layout, which is usually the
point: they can watch the dev server you started. Say that you did it, and close
the pane when you're done with it.

If you are *not* inside a tty7 pane there is nothing to split, so make your own
place to work first. `tty7 new --json /path/to/repo` hands you both ids at
once — don't go digging through `ws tree` for the pane:

```bash
read -r WS PANE < <(tty7 new --json /path/to/repo \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], "%%%d" % d["pane"])')
```

`send` types text into the pane exactly as a keyboard would; `--enter` appends
the carriage return, or presses Enter on its own when you give it no text
(`tty7 send "$PANE" --enter` runs what is already typed there). It does not
wait and it does not tell you what happened — reading is a separate step, and
waiting is `tty7 wait`.

For keystrokes rather than characters — Ctrl-C, Escape, the arrow keys — use
`--key`: it takes `enter escape tab backtab space backspace delete up down
right left home end pageup pagedown`, plus `C-<char>` for Ctrl and `M-<char>`
for Alt. Repeat it for a sequence; text and keys compose, text first. Typing
`^C` as text does nothing — it arrives as two characters; `--key C-c` is the
real interrupt.

**A brand-new pane can swallow the Enter.** A shell still working through its
startup files — a prompt framework, `fastfetch`, anything that paints on login —
takes the text you send but loses the carriage return that follows it, and the
command just sits on the prompt line unexecuted. Nothing reports this: the
`send` succeeded, and the pane looks like a worker that has not got going yet.
So after sending the first command into a pane you just created, read the screen
back and check it actually left the prompt:

```bash
tty7 capture "$PANE" --plain | tail -3   # command still sitting on the prompt?
tty7 send "$PANE" --enter                # then give it the Enter it lost
```

Cheaper than diagnosing it later, and only the first `send` into a fresh pane
needs the check.

## Reading a pane

### If you want the screen, use `--plain`

```bash
tty7 capture %83 --plain
```

`capture` hands back what the daemon stored — the pane's bytes, escapes and
all — and `--plain` replays them through a terminal grid and prints the
resulting text instead. Not a stripper: colour and cursor escapes are gone, but
also a line the shell wrapped at column 249 comes back as one line, a progress
bar that rewrote itself with `\r` reads as its final value, and a TUI's screen
lands where it was drawn. Use it whenever a human would want to read the output.

Two details about what you get back either way: capture returns a *snapshot*,
not a stream — call it again for a newer one. And by default it prints the
newest scrollback segment (the ring splits on resize); `--scrollback` prints the
whole ring, which for a pane that was never resized is the same thing.

### If you want the result, redirect to a file

`--plain` gives you the screen, and a screen is a rectangle: whatever scrolled
past the top of a long build log is gone, and the exit code was never on screen
at all. So when what you want is the *answer* rather than the view, have the
shell write it somewhere clean:

```bash
tty7 send "$PANE" 'cargo test > /tmp/t.log 2>&1; echo $? > /tmp/t.rc' --enter
# ...wait for it to finish (below), then:
cat /tmp/t.rc /tmp/t.log
```

Complete output, a real exit code, no terminal in the middle.

### Knowing when a command has finished

Don't poll the screen and don't write your own loop — block on it:

```bash
tty7 wait "$PANE" --until free --changed --timeout 900
```

`free` means the foreground command has exited and the pane is back to its bare
shell. `--changed` adds "and something actually ran while I watched", which is
what you want on the line right after a `send`: without it, a command that has
not started yet leaves the pane looking finished.

The whole shape, end to end:

```bash
tty7 send "$PANE" 'cargo test > /tmp/t.log 2>&1; echo $? > /tmp/t.rc' --enter
tty7 wait "$PANE" --until free --changed --timeout 900
cat /tmp/t.rc /tmp/t.log
tty7 pane close "$PANE"
```

Exit codes are built for this: `0` means a state you asked for was reached,
`124` means the timeout ran out (the `timeout(1)` convention, so "not yet" is
distinguishable from "broken"), `1` means the pane died first.

One trap in `--changed`: a command that finishes inside a single poll (500ms by
default) is never *seen* running, so the wait keeps going until it times out.
For something that quick, `--interval 100`, or drop `--changed` and read the
`.rc` file. The timeout message says so when it happens.

If you want the process tree itself — "what is running in there", "which port is
this pane serving" — that is `tty7 procs %83`: indented by depth, `*` on the
foreground process, then the ports those processes are listening on. It is not
the way to check on a coding agent, though: `procs` reports `nothing running in
this pane` for a pane with a busy agent in it, so reading it as "the worker
died" is wrong. Ask `tty7 agents` about those.

## Handing work to another agent

A pane can hold another coding agent, and every primitive above works on it —
plus one that only agents have: status hooks report `working` / `waiting` /
`done`, so `tty7 wait` can block on the *agent* rather than its process tree.

**Delegation has a playbook — `references/delegation.md`. Read it before you
spawn a worker.** It covers the whole arc: giving the worker its own git
worktree and workspace, handing the task over with the delivery contract in the
prompt, proving the command actually started, babysitting the states, collecting
the result out of git, fanning out several workers, and cleaning up.

Four rules from it survive even if you read nothing else:

- **Interactive mode, never `-p`.** `claude -p` draws no TUI: the pane stays
  blank, `capture --plain` returns nothing, the user watches an empty
  rectangle, and the session dies after one turn so you cannot follow up. Hand
  the task as an argument to the interactive command instead.
- **A worker that writes files gets its own git worktree.** Two agents in one
  checkout trample each other and the user's working tree.
- **Collect results from git, not from the screen.** Tell the worker to commit;
  read the diff. A screen is a rectangle and the top of it is gone.
- **After the first send into a new pane, confirm the command left the
  prompt** — the swallowed-Enter check above.

## Looking around

```bash
tty7 ls                    # every workspace: tabs, panes, who's attached
tty7 ws tree api           # one workspace as a tree — tabs, splits, panes, cwds
tty7 pane ls               # panes with their workspace, tab, cwd, live flag
tty7 pane ls --all         # + orphans: panes the server runs that no workspace holds
tty7 agents                # every coding agent on the machine and its status
tty7 status                # server pid, uptime, pane count, build, socket
tty7 machine ls            # this machine plus any linked remotes
tty7 events                # stream server events, one per line, until interrupted
```

`tty7 agents` is worth knowing about: it reports each pane running a recognised
coding agent as `idle` / `working` / `waiting` / `done`, with the agent's own
message beside it. If you are one of them, you are in that list too. It also
prints a diagnostic — `diagnostics` in the JSON — when it can see an agent
running whose status hooks are missing or outdated, which is the explanation for
any agent that appears frozen.

Add `--json` to any of these to parse instead of eyeball. `-q` suppresses
output on success but never suppresses errors.

## Don't break the user's session

The panes on this machine are the user's real work, and some of them are other
coding agents mid-task. Treat anything you did not create as read-only:

- **Never `send` into a pane you didn't open.** Keystrokes into another agent's
  pane, or into a shell the user is typing in, land in the middle of whatever
  is happening there. Check `tty7 agents` before you touch a pane. This goes
  double for `--key`: a stray `C-c` kills somebody's work.
- **Never `pane close` / `tab close` / `ws rm` something you didn't create.**
- **Never `pane close --orphans`.** It closes every abandoned pane on the
  machine, and an abandoned pane can still be running a real command. It is the
  user's broom; point them at it, don't swing it.
- **Never `server stop` or `server restart`.** Every pane on the machine dies
  with the server, including yours. If the server genuinely seems wedged, say
  so and let the user decide.
- **Clean up what you did create.** `tty7 pane close %83` when you're done with
  a scratch pane; it takes several ids at once. `ws rm` hangs up the panes the
  workspace held, so removing a scratch workspace is enough on its own. What
  does leak is an interrupted `tty7 run` — that pane keeps running with nothing
  referencing it, and shows up under `tty7 pane ls --all`.

## Remote machines

`-m <machine>` routes any command over a link the local server already holds:

```bash
tty7 -m devbox ls
tty7 -m devbox run -- cargo test
```

The name matches the full link key (`me@devbox:22`) or just the host. The CLI
will not dial a fresh connection — if the link is down, or it's a jump/proxy
chain, it says so and you should hand that back to the user, who can connect it
from the GUI.

## Not wired up yet

`ws stop`, `machine connect` and `machine disconnect` exit with a message saying
they're not implemented. Don't build a plan around them.

## References

- `references/delegation.md` — the delegation playbook: worktrees, handover,
  babysitting, collection, fan-out, cleanup. Read it whenever another agent is
  about to do the work.
- `references/commands.md` — every verb, subcommand and flag in one table, plus
  the JSON shape each one emits. Read it when you need a verb that isn't above,
  or when you're about to parse `--json` output and want to know the field
  names.
