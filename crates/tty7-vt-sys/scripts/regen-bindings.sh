#!/usr/bin/env bash
# Regenerate src/bindings.rs from the pinned libghostty-vt headers.
#
# Run from anywhere; mise resolves bindgen (and Zig for `vt:build`) from this
# crate's mise.toml. The headers come from $TTY7_GHOSTTY_DIR — the prefix of
# the *built* library, not the Ghostty source checkout — so bindings and the
# linked archive can never describe different versions.
#
# CI runs this and fails if `git diff` shows a change: a re-pinned Ghostty
# that forgot to regenerate is an ABI mismatch waiting to corrupt a grid.
set -euo pipefail

cd "$(dirname "$0")/.."

# mise exports TTY7_GHOSTTY_DIR from the repo root; fall back to the default
# location `mise run vt:build` writes to, for shells without mise hooks.
if [[ -z "${TTY7_GHOSTTY_DIR:-}" ]]; then
    TTY7_GHOSTTY_DIR="$(cd ../.. && pwd)/../ghostty-vt-out"
    export TTY7_GHOSTTY_DIR
fi

if [[ ! -f "$TTY7_GHOSTTY_DIR/include/ghostty/vt.h" ]]; then
    echo "error: $TTY7_GHOSTTY_DIR/include/ghostty/vt.h not found." >&2
    echo "       Run 'mise run vt:build' from the repo root first." >&2
    exit 1
fi

mise exec -- bindgen wrapper.h \
    -o src/bindings.rs \
    --no-doc-comments \
    -- \
    -I"$TTY7_GHOSTTY_DIR/include"

echo "regenerated src/bindings.rs from $TTY7_GHOSTTY_DIR"
