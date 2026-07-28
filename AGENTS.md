# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Adding a user-facing action

Actions are declared once in `src/core/actions.rs` and then wired into every
surface by hand — miss one and the action silently exists nowhere. The full
set of places, each a plain table you extend by copying the row above:

| File | What |
|---|---|
| `src/core/actions.rs` | declare it in `actions!` |
| `src/ui/app.rs` | an `on_action` listener in `Render for Tty7App`, and a `run_command` arm if it is in the palette |
| `src/ui/keymap.rs` | `default_bindings()` (empty string = bindable, no default chord) **and** `make_binding()` |
| `src/ui/palette.rs` | `CommandKind` variant, `id()`, the action-name map, and a `Command::new` row |
| `src/ui/theme.rs` | the macOS menu-bar item |
| `src/ui/tab_strip.rs` | `tab_context_menu` — the tab-strip chips **and** the sidebar rows share it verbatim, so a row added here appears in both |
| `src/terminal/view.rs` | the pane right-click menu. It has no `Tty7App` handle, so rows here must dispatch actions, not closures; a submenu needs its own `action_context` (it does not inherit the parent menu's) |

## Third-party coding agents

`src/core/cli_agent.rs` is the whole registry: detection, per-agent resume /
fork commands, and the launch-flag replay that carries a pane's original flags
onto them. Per-agent behaviour is always a `match self` table returning `None`
for agents without the capability — follow that shape rather than special-casing
one agent. tty7 never reads or writes an agent's own session files; it shells
the agent's own subcommand, so an agent changing its on-disk format costs at
most a visible shell error. Only claim a flag you have checked against the
installed CLI's own `--help`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
