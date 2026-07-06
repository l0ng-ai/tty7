#!/bin/zsh
# One-time setup for the terminal benchmark harness (see README.md).
# Fetches everything into the gitignored work dir ($TTY7_BENCH_DIR, default
# <repo>/.bench): the 11 MB plaintext corpus, DOOM-fire-zig (patched to dump
# its fps for collection), and a zig 0.14 toolchain if the system one is newer
# (DOOM-fire-zig pins 0.14; the build API changed in 0.15+). macOS only.
set -e
SELF=${0:A}
HERE=${SELF:h}
REPO=${HERE:h:h}
WORK=${TTY7_BENCH_DIR:-$REPO/.bench}
mkdir -p $WORK

# 1) 11 MB Shakespeare corpus — the plaintext-IO payload from the methodology
#    source, moktavizen/terminal-benchmark.
if [ ! -f $WORK/shakespeare.txt ]; then
  echo "fetching shakespeare.txt (11 MB)…"
  curl -fsSL -o $WORK/shakespeare.txt \
    https://raw.githubusercontent.com/moktavizen/terminal-benchmark/main/test/shakespeare.txt
fi

# 2) DOOM-fire-zig, with the fps-dump patch applied (doom-fire-fps.patch).
if [ ! -d $WORK/DOOM-fire-zig ]; then
  git clone --depth 1 https://github.com/const-void/DOOM-fire-zig.git $WORK/DOOM-fire-zig
  git -C $WORK/DOOM-fire-zig apply $HERE/doom-fire-fps.patch
fi

# 3) zig 0.14.x. Use the system zig when it matches, else download 0.14.1.
ZIG=zig
if ! zig version 2>/dev/null | grep -q '^0\.14'; then
  arch=$(uname -m)
  [ "$arch" = arm64 ] && arch=aarch64
  zdir=$WORK/zig-$arch-macos-0.14.1
  if [ ! -x $zdir/zig ]; then
    echo "downloading zig 0.14.1 ($arch)…"
    curl -fsSL https://ziglang.org/download/0.14.1/zig-$arch-macos-0.14.1.tar.xz | tar -xJ -C $WORK
  fi
  ZIG=$zdir/zig
fi

# 4) Build the fire.
if [ ! -x $WORK/DOOM-fire-zig/zig-out/bin/DOOM-fire ]; then
  (cd $WORK/DOOM-fire-zig && $ZIG build -Doptimize=ReleaseFast)
fi

echo "bench setup complete: $WORK"
