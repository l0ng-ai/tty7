#!/bin/bash
# Usage: assert-macho.sh <path-to-mach-o> <expected-arch>
# Fail unless the binary is a Mach-O executable for <expected-arch> that depends
# on nothing but the libraries every macOS already has, and carries a code
# signature.
#
# The macOS counterpart of assert-static.sh, and the same decision (D10) behind
# it: one `tty7-server` binary is pushed to an arbitrary remote Mac and has to
# run there with nothing installed alongside it. Static linking is not the
# instrument on macOS — Apple does not ship a static libSystem and linking one
# is unsupported — so the equivalent guarantee is "links only what the OS
# guarantees is present". A stray Homebrew dependency picked up from the runner
# would still compile, still pass a build-only job, and then fail on the first
# Mac that does not have /opt/homebrew — far from the change that caused it.
set -euo pipefail

BIN="$1"
WANT_ARCH="$2"

if [ ! -f "$BIN" ]; then
  echo "::error::assert-macho.sh: $BIN does not exist"
  exit 1
fi

echo "--- file ---"
file "$BIN"
echo "--- otool -L ---"
otool -L "$BIN"
echo "--- otool -l (build version) ---"
otool -l "$BIN" | grep -A 4 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX' || true

fail=0

# Each probe is captured into a variable and matched afterwards, never piped
# into `grep -q`. Under `pipefail` that pipeline is a coin toss: -q exits on the
# first match, the writer takes SIGPIPE, and the pipeline reports failure — so a
# binary that passes would be reported as failing, on the runs where grep
# happened to win the race.
FILE_SAYS=$(file "$BIN")
if [[ "$FILE_SAYS" != *"Mach-O 64-bit executable ${WANT_ARCH}"* ]]; then
  echo "::error::$BIN is not a 64-bit Mach-O executable for ${WANT_ARCH}"
  fail=1
fi

# Every dependency must live somewhere the OS owns. /usr/lib and
# /System/Library are the two prefixes shipped with macOS itself; anything else
# — /opt/homebrew, /usr/local, @rpath into a bundle we are not shipping — is a
# library the destination Mac has no reason to have.
#
# `tail -n +2` drops otool's first line, which is the binary's own path and
# would otherwise be judged as if it were a dependency.
STRAY=$(otool -L "$BIN" | tail -n +2 | awk '{print $1}' \
  | grep -Ev '^(/usr/lib/|/System/Library/)' || true)
if [ -n "$STRAY" ]; then
  echo "::error::$BIN links libraries that are not part of macOS:"
  echo "$STRAY"
  fail=1
fi

# arm64 refuses to execute an unsigned binary outright, so an unsigned build
# would not fail here but on the user's Mac, as "killed: 9" with no explanation.
#
# Asserted for both slices, not just arm64. The linker ad-hoc signs arm64 on its
# own and leaves x86_64 bare — which would be fine on an Intel Mac, but the
# x86_64 server is also what an Apple Silicon box gets when it asks through a
# Rosetta shell (`uname -sm` = "Darwin x86_64"), and that is not a machine to
# hand an unsigned binary to on a guess. The workflow signs it; this catches the
# day it stops.
SIGNING=$(codesign -dv "$BIN" 2>&1 || true)
if [[ "$SIGNING" != *"Signature="* ]]; then
  echo "::error::$BIN carries no code signature — arm64 macOS will refuse to run it"
  echo "$SIGNING"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "✅ $BIN is a self-contained ${WANT_ARCH} Mach-O ($(du -h "$BIN" | cut -f1))"
