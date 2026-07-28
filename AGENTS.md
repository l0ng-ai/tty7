# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.
- **gpui's `overflow_hidden` does not round-clip.** Its content mask is an
  axis-aligned rectangle applied as a hard per-fragment discard, so a child that
  paints a background into a rounded container's corner squares that corner off
  with no anti-aliasing. Any such child must carry its own radius, inset one
  border-width. `src/ui/rounding.rs` holds the rule, the constants and the
  tests; read it before adding a filled band or segment to a rounded track.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
