#!/bin/bash
# Usage: assert-static.sh <path-to-elf>
# Fail unless the binary is a fully static ELF — no dynamic loader, no shared
# library dependencies.
#
# This is the mechanical guard behind decision D10: one `tty7-server` binary
# is pushed to arbitrary remote machines and must run
# there regardless of what libc, and what *version* of it, that machine has. A
# build that silently picked up a dynamic dependency would still pass a
# compile-only CI job and then fail on the first old box a user connects to —
# far from the change that caused it. Cheap to assert, expensive to discover.
set -euo pipefail

BIN="$1"

if [ ! -f "$BIN" ]; then
  echo "::error::assert-static.sh: $BIN does not exist"
  exit 1
fi

echo "--- file ---"
file "$BIN"
echo "--- readelf -d ---"
readelf -d "$BIN" || true

fail=0

# `file` says "statically linked" for a classic static binary and "static-pie
# linked" for a position-independent one. Rust's musl targets have shipped both
# shapes depending on toolchain version, and both are equally self-contained, so
# accept either — but nothing else.
if ! file "$BIN" | grep -Eq 'statically linked|static-pie linked'; then
  echo "::error::$BIN is not statically linked (D10 requires a self-contained binary)"
  fail=1
fi

# The decisive check: a static binary has no PT_INTERP segment, i.e. no
# request for /lib/ld-musl-*.so or ld-linux-*.so. This catches the case `file`
# alone would not, where a dynamic loader is still required.
if readelf -l "$BIN" | grep -q 'Requesting program interpreter'; then
  echo "::error::$BIN requires a dynamic loader (PT_INTERP present) — not a static build"
  fail=1
fi

# Belt and braces: no DT_NEEDED entries, i.e. no shared libraries to resolve.
if readelf -d "$BIN" 2>/dev/null | grep -q 'NEEDED'; then
  echo "::error::$BIN has shared-library dependencies (DT_NEEDED) — not a static build"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "✅ $BIN is a static binary ($(du -h "$BIN" | cut -f1))"
