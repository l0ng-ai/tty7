---
name: tty7
description: >-
  Drive the tty7 terminal workbench from the shell with the `tty7` binary — list workspaces/tabs/panes, split a pane, send keystrokes into one, capture what is on a pane's screen, run a command in a real PTY and pass its exit code through, see which coding agents are running and which ports a pane is listening on. Use this whenever tty7, panes, workspaces, or `%42`/`@7`/"the other pane"/"the other agent" come up; whenever you need to start something long-running or interactive (dev server, REPL, ssh session, `tail -f`, a TUI) that should not sit blocking your Bash tool; whenever a program needs a real terminal to behave the way the user sees it; and whenever you need to look at or report on what is running in some *other* terminal on this machine. Cheap to check: if `$TTY7_PANE` is set you are already inside tty7 and every command here works with no setup.
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
whether a server is reachable, whether the dialect matches, and whether
`TTY7_CONFIG_DIR` / `TTY7_WS` / `TTY7_PANE` are set — i.e. whether you are
running *inside* a tty7 pane.

Being inside a pane matters for two reasons: the address-taking verbs
(`split`, `send`, `capture`, `procs`) default to `$TTY7_PANE`, and `run --keep`
files its pane into `$TTY7_WS`. Outside a tty7 shell you must name a target
explicitly, and the error will say so rather than guessing.

If `tty7 doctor` says the server is unreachable, stop and tell the user — do
not run `tty7 server start` on your own initiative. Starting a server they
didn't ask for changes what their GUI attaches to.

## When to use this instead of the Bash tool

The Bash tool is right for anything that starts, does its job, and exits.
Reach for tty7 when one of these is true:

- **It shouldn't block you.** A dev server, a watcher, `tail -f`, a long test
  run you want to check on later. Put it in a pane, come back and read it.
- **It's interactive or stateful.** A REPL, `ssh`, a database shell, anything
  where you send one thing, read the answer, then send the next. A pane keeps
  the session alive between your turns; a Bash call cannot.
- **It needs a real TTY.** Programs that detect a pipe and change behaviour —
  colour, progress bars, TUIs, `top`, anything using raw mode. `tty7 run`
  gives a genuine PTY at 120×30.
- **The user should be able to watch it.** Anything in a pane shows up in their
  tty7 window, live. That is often the whole point.
- **You're being asked about something you didn't start.** "What's running in
  that pane?", "why is port 3000 taken?", "what are my agents doing?" — you can
  answer those from here without touching anything.

## Addresses

| Shape | Means | Stable? |
|---|---|---|
| `%42` | a pane | yes — a pane keeps its id for its whole life |
| `@7` | a tab, numbered across the **whole machine** in tree order | **no** — it shifts whenever a workspace or tab appears or disappears |
| `api` / `76698a44` / a full UUID | a workspace, by name, by unique id prefix, or by id | yes |

Re-resolve `@N` right before you use it; never cache one across a step that
creates or removes a tab. Pane ids and workspace ids are safe to remember.

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

Two things to know. `--keep` needs a workspace, so it only works inside a tty7
shell or with `--ws <workspace>`. And with `--json`, the streamed output comes
first and the JSON object last — the combined stream is *not* parseable as
JSON, so read the last line.

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
the carriage return. It does not wait and it does not tell you what happened —
reading is a separate step.

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

```bash
tty7 procs %83
```

lists the process tree inside the pane, indented, with `*` on the foreground
process — plus any ports those processes are listening on. When the only entry
left is the depth-0 shell, the command is done. That is a far more reliable
"finished?" signal than grepping the screen, where your sentinel string can get
line-wrapped or echoed twice.

Poll it on an interval rather than in a tight loop — a few seconds between
checks. In Claude Code, use the Monitor tool with an until-condition instead of
a bare foreground `sleep`.

The whole shape, end to end:

```bash
tty7 send "$PANE" 'cargo test > /tmp/t.log 2>&1; echo $? > /tmp/t.rc' --enter
# poll until only the shell is left
until [ "$(tty7 procs "$PANE" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["procs"]))')" = 1 ]; do sleep 3; done
cat /tmp/t.rc /tmp/t.log
tty7 pane close "$PANE"
```

The ports half also stands alone: `tty7 procs %62` answers "what is this pane
serving, and on which port" without any guessing.

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
coding agent as `running` / `waiting` / `idle`. If you are one of them, you are
in that list too.

Add `--json` to any of these to parse instead of eyeball. `-q` suppresses
output on success but never suppresses errors.

## Don't break the user's session

The panes on this machine are the user's real work, and some of them are other
coding agents mid-task. Treat anything you did not create as read-only:

- **Never `send` into a pane you didn't open.** Keystrokes into another agent's
  pane, or into a shell the user is typing in, land in the middle of whatever
  is happening there. Check `tty7 agents` before you touch a pane.
- **Never `pane close` / `tab close` / `ws rm` something you didn't create.**
- **Never `server stop` or `server restart`.** Every pane on the machine dies
  with the server, including yours. If the server genuinely seems wedged, say
  so and let the user decide.
- **Clean up what you did create.** `tty7 pane close %83` when you're done with
  a scratch pane. Note that `ws rm` does *not* kill the panes inside it — they
  survive as orphans, visible under `tty7 pane ls --all` with no workspace, and
  you have to close them individually.

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

`ws stop`, `machine connect`, `machine disconnect`, and bare `tty7 <path>` (GUI
launch) all exit with a message saying they're not implemented. Don't build a
plan around them.

## Full command reference

`references/commands.md` has every verb, subcommand and flag in one table, plus
the JSON shape each one emits. Read it when you need a verb that isn't above,
or when you're about to parse `--json` output and want to know the field names.
