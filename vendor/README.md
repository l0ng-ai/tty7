# Terminal checkpoint dependencies

These are source-only copies of the versions already pinned by tty7:

- `alacritty_terminal`: l0ng-ai/alacritty, commit `1276f128fbaa8832cbc66210675cbe8aeb570499`.
- `vte`: crates.io `0.15.0`.

Original licenses are retained in each directory. Large upstream recording
fixtures and unrelated workspace packages are not vendored. The package metadata
uses explicit edition/MSRV values so these crates build outside their original
workspaces.

Local changes add validated, transport-neutral checkpoints for both terminal
grids, cursors, modes, tabs, colors and stacks, and the in-flight VT parser state
(partial CSI/OSC/UTF-8, REP and synchronized output). UI selection, configuration,
focus, callbacks and local clock instants are not serialized. tty7-core and the
GUI must use these same versions. Keep changes here narrow and upstream the API
before returning to remote dependency pins; never patch the Cargo cache at build
time.
