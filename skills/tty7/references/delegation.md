# Delegating work to another coding agent

A worker in a pane is a real agent session the user can watch, interrupt, and
take over — that is what makes a pane better than an API call. This playbook is
the arc of one delegation, in order:

1. [Give it a place to work](#1-a-place-to-work) — its own worktree and workspace
2. [Hand over the task](#2-handing-over-the-task) — interactive mode, delivery contract in the prompt
3. [Prove it started](#3-prove-it-started) — the three ways a launched worker silently isn't
4. [Wait, and babysit](#4-wait-and-babysit) — states, `--changed`, answering prompts
5. [Collect through git](#5-collect-through-git-not-the-screen) — the screen is a diagnostic, not a deliverable
6. [Clean up](#6-clean-up)

[Fan-out](#running-several-workers) builds on the same six steps, one worker at
a time.

## 1. A place to work

**A worker that will write files gets its own git worktree.** Two agents in one
checkout — or one agent in the checkout the user is editing — trample each
other: half-written files show up in each other's diffs, builds race, and
`git status` stops meaning anything. The whole industry of parallel-agent
tooling converged on the same answer: one task, one worktree, one branch.

```bash
REPO=/path/to/repo
WT=/tmp/agent-wt/parser-tests
git -C "$REPO" worktree add "$WT" -b agent/parser-tests
read -r WS PANE < <(tty7 new --json "$WT" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], "%%%d" % d["pane"])')
```

`tty7 new` rather than `split` for the same reason as the worktree, one level
up: a workspace of its own keeps the worker out of the user's window layout.
Splitting your own pane is fine for one short-lived worker the user wants to
watch; three splits deep the window is unusable.

Skip the worktree only when the worker will not write: a research question, a
code-reading task, a second opinion. For those, `split` in the current checkout
is fine. And if the user explicitly wants the worker operating on their live
checkout, say what the risk is and do as asked.

## 2. Handing over the task

Hand the task as an argument to the **interactive** command — never `-p`:

```bash
TASK="Add tests for the tokenizer edge cases in src/lexer.rs.
When you are done, commit to the current branch with a message
saying what you did and what you verified. Do not push."
tty7 send "$PANE" "claude --dangerously-skip-permissions \"$TASK\"" --enter
```

Why interactive: both modes run one turn and stop, so the difference is not
what the worker does — it is what anybody can see while it does it.
Interactive draws the TUI, so the pane fills with the worker's reasoning and
tool calls as they happen; that is what the user watches and what
`capture --plain` reads back. `-p` is the piped mode: it draws nothing, the
pane's screen stays **empty** until the turn ends — which reads exactly like a
hung worker — and the session is gone afterwards, so you cannot ask a
follow-up. Putting a `-p` worker in a pane throws away the only reason it is
in a pane. When you want an answer as a string and nobody needs to watch,
that's `tty7 run` or your own Bash tool, not a pane.

Two things belong in every task prompt:

- **The delivery contract.** "Commit to the current branch when done" turns
  the result into something git can hand you complete — see
  [step 5](#5-collect-through-git-not-the-screen). Without it you are left
  reading a 40-line tail of a screen and guessing.
- **The boundaries.** "Do not push", "stay in this directory", "ask before
  deleting" — a worker inherits none of your context, only its prompt.

## 3. Prove it started

Three independent ways a worker you just launched is silently not running, and
the checks that catch each:

| Failure | What you see | The check |
|---|---|---|
| A fresh shell swallowed the Enter | command sits on the prompt, never runs | `capture --plain \| tail -3`; still on the prompt → `tty7 send "$PANE" --enter` |
| `tty7 agents` reports stale state | a pane stuck at the prompt can still show `working` | the screen's last lines are the authority, not the status |
| `tty7 procs` says `nothing running` | looks like the worker died | it hasn't — `procs` cannot see agents; ignore it here |

The first check is mandatory after the first `send` into any pane you just
created. Thirty seconds here beats a 900-second `wait` that times out on a
worker that never began.

## 4. Wait, and babysit

```bash
tty7 wait "$PANE" --until waiting,done --changed --timeout 1800
```

### What the states mean

| State | The pane is |
|---|---|
| `working` | mid-turn |
| `waiting` | **stopped, needing you** — a permission prompt, a question |
| `done` | finished its turn |
| `idle` | an agent that has not started a turn |
| `free` | no agent: the foreground command exited |
| `no-agent` | nothing reports status here — a plain shell, or hooks not installed |
| `exit` | the pane is gone; ends every wait whether you asked for it or not |

`--until waiting,done,exit` is the default because those are the three that
mean "your turn again". `idle` is something an agent says about *itself* — a
pane running a build is `no-agent`, never `idle`, so `--until idle` is never
the way to ask "is the command finished"; that is `free`. Mixing agent states
with `free` is safe: `free` is only consulted when no named agent state
matched first.

### `--changed` on every wait that follows a send

The status is a **level, not an event**: `done` stands until the next turn
begins. A `wait` issued right after a `send` will happily answer with *last*
turn's `done` before the worker has even read the input, and you will read a
stale screen and think it failed. `--changed` refuses the state the pane was
already in. Every wait that follows input into the pane needs it; the JSON's
`stale` flag tells you when it mattered. A wait that follows *nothing* — the
harvest round in [fan-out](#running-several-workers) — is the case that must
not use it.

### The babysit loop

`waiting` means the worker stopped for a human. Be that human when you can:

```bash
tty7 capture "$PANE" --plain | tail -20   # what is it asking?
tty7 send "$PANE" --key down --key enter  # answer a menu / permission prompt
tty7 send "$PANE" 'yes, use the existing fixture file' --enter   # answer a question
```

Then go back to waiting. Answer what the task's boundaries already cover;
anything outside them — a destructive action, a scope change, credentials —
gets reported to the user instead, with the pane id so they can look
themselves. A worker you cannot safely answer is a worker the user takes over;
that handover is a feature, not a failure.

To stop a runaway worker: `tty7 send "$PANE" --key C-c` — typing `^C` as text
arrives as two harmless characters.

Exit codes from `wait`: `0` a state you asked for was reached, `124` timeout
(the `timeout(1)` convention — "not yet", distinguishable from broken), `1`
the pane died.

## 5. Collect through git, not the screen

The delivery contract from step 2 pays off here:

```bash
git -C "$WT" log --oneline main..HEAD    # what it says it shipped
git -C "$WT" diff main...HEAD            # the changes themselves
git -C "$WT" status --short              # anything it left uncommitted
```

Complete, unwrapped, with nothing scrolled away — and reviewable before a
single byte reaches the user's branch. You are the merge point: read the diff,
run the tests if the task warranted them, then merge or report.

`capture --plain | tail` is for *diagnosis* — what is it stuck on, what did it
just print — not for collecting results. A screen is a 120-column rectangle
and the interesting part has usually scrolled off the top of it.

If the worker finished but committed nothing, the screen is where you find out
why; that is the one time the tail is the deliverable.

## 6. Clean up

```bash
tty7 ws rm "$WS"                          # hangs up the workspace's panes
git -C "$REPO" worktree remove "$WT"      # refuses if dirty — that's a feature
git -C "$REPO" branch -D agent/parser-tests   # once merged, or rejected
```

`worktree remove` refusing means uncommitted work is sitting there — look at
it before deciding anything, and ask the user rather than `--force`-ing away
changes you have not read. A worker you opened with `split` instead is just
`tty7 pane close "$PANE"`.

## Running several workers

Fan-out is the six steps above per worker — **each with its own worktree and
branch** — plus a harvest loop that no single stuck worker can stall:

```bash
: > /tmp/agent-workers
for task in parser lexer codegen; do
  WT=/tmp/agent-wt/$task
  git -C "$REPO" worktree add "$WT" -b "agent/$task"
  read -r WS PANE < <(tty7 new --json "$WT" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], "%%%d" % d["pane"])')
  tty7 send "$PANE" "claude --dangerously-skip-permissions \"Add tests for the $task. Commit to the current branch when done; do not push.\"" --enter
  echo "$task $PANE $WT $WS" >> /tmp/agent-workers
done
# prove each one started (step 3) before settling in to wait

cp /tmp/agent-workers /tmp/agent-pending
while [ -s /tmp/agent-pending ]; do
  : > /tmp/agent-still
  while read -r task PANE WT WS; do
    tty7 wait "$PANE" --until waiting,done --timeout 120; rc=$?
    if [ $rc -eq 0 ]; then
      : # done → collect (step 5); waiting → babysit (step 4), then requeue
    elif [ $rc -eq 124 ]; then
      echo "$task $PANE $WT $WS" >> /tmp/agent-still   # not yet — come back
    else
      echo "$task: pane $PANE is gone" >&2             # 1: died; nothing to requeue
    fi
  done < /tmp/agent-pending
  mv /tmp/agent-still /tmp/agent-pending
done
```

The short per-worker timeout is the point: with one long `wait` per worker in
sequence, the first stuck worker blinds you to every worker behind it. Round
trips of 120 seconds keep you circulating — collecting the finished, answering
the stuck, and telling the user about the one that has moved nothing for three
rounds.

No `--changed` here, unlike step 4, and that is deliberate: a worker that
reached `done` while you were waiting on a *different* one is already standing
in that state when its own `wait` finally starts, and `--changed` would refuse
it — every round, forever. Each pane runs one turn, so a standing `done` is
this turn's. What that costs you is on the other side: after answering a
`waiting` worker, give it a `tty7 wait "$PANE" --until working --changed
--timeout 30` before you requeue it, or the next round hands you the same
prompt again.

What not to build: workers do not talk to each other, and their branches never
merge into each other. Keep the topology a star — you hand out tasks that do
not overlap, each worker delivers to its own branch, and every diff comes back
through you, serially. Cross-cutting conflicts between two workers' branches
are yours to resolve at merge time, which is exactly why the tasks should not
overlap in the first place.

## When a worker never moves

A `wait` that times out while `tty7 agents` shows a status that never changes
almost always means the agent's status hooks are missing or out of date — the
worker is fine, it just has no way to say so. `tty7 agents` names the agent
when it can see the gap, and `tty7 doctor` reports where every agent's hooks
stand. Hooks are installed from the GUI's **Settings → Agents**; tell the user
rather than trying to install them yourself.

Before concluding anything, check whether it is moving. Those same hooks emit
an OSC 777 line on every tool call, and `capture` **without** `--plain` shows
them — one of the few times the raw bytes beat the rendered screen:

```bash
tty7 capture "$PANE" | grep -c 'tool-complete'   # rising = alive and working
```

Two things that do *not* answer this question: `tty7 procs`, which reports
nothing running for a pane with a live agent in it, and an empty
`capture --plain` — a worker mistakenly launched with `-p` paints nothing all
turn while the event stream underneath is busy.
