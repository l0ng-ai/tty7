#!/bin/zsh
# Driver: launch ONE terminal running ONE benchmark script as its shell, wait
# for the "done" marker in the result file, then clean up and print results.
#
#   run_one.sh <tty7|alacritty|ghostty|kitty> <io|fire> [cols rows]
#
# Grid fairness: run tty7 first — its result file records the grid it opened
# at ("grid: <rows> <cols>"), and later alacritty/ghostty runs of the same
# test default to that grid (both accept a cell-based window size at launch;
# tty7 has no such flag, so it is the reference). Pass cols/rows explicitly to
# override.
#
# Only processes whose argv references this harness's paths are ever killed —
# a daily-driver tty7/Ghostty/Alacritty running alongside is never touched.
set -u
SELF=${0:A}
HERE=${SELF:h}
REPO=${HERE:h:h}
WORK=${TTY7_BENCH_DIR:-$REPO/.bench}
TTY7_BIN=${TTY7_BIN:-$REPO/target/release/tty7}
TERM_NAME=$1
TEST=$2
COLS=${3:-}
ROWS=${4:-}
R=$WORK/results
mkdir -p $R
res=$R/$TEST-$TERM_NAME.txt

# Default the grid to tty7's recorded one for the same test.
if [ -z "$COLS" ] && [ "$TERM_NAME" != tty7 ] && [ -f $R/$TEST-tty7.txt ]; then
  ROWS=$(awk '/^grid:/ {print $2; exit}' $R/$TEST-tty7.txt)
  COLS=$(awk '/^grid:/ {print $3; exit}' $R/$TEST-tty7.txt)
fi

# Stale processes from a previous aborted run (scoped to harness paths).
pkill -f -- "$WORK/cfg-" 2>/dev/null
pkill -f -- "$HERE/io.sh" 2>/dev/null
pkill -f -- "$HERE/fire.sh" 2>/dev/null
sleep 1
rm -f $res

case $TERM_NAME in
  tty7)
    cfg=$WORK/cfg-$TEST-tty7
    rm -rf $cfg && mkdir -p $cfg
    printf '{"shell":{"program":"%s","args":["tty7"]}}\n' "$HERE/$TEST.sh" > $cfg/config.json
    $TTY7_BIN --config-dir $cfg >/dev/null 2>&1 &
    pid=$!
    ;;
  alacritty)
    opts=()
    [ -n "$COLS" ] && opts=(-o "window.dimensions.columns=$COLS" -o "window.dimensions.lines=$ROWS")
    : > $WORK/alacritty-empty.toml # default config, isolated from the user's
    /Applications/Alacritty.app/Contents/MacOS/alacritty \
      --config-file $WORK/alacritty-empty.toml "${opts[@]}" \
      -e $HERE/$TEST.sh alacritty >/dev/null 2>&1 &
    pid=$!
    ;;
  ghostty)
    opts=()
    [ -n "$COLS" ] && opts=("--window-width=$COLS" "--window-height=$ROWS")
    # --window-save-state=never: macOS window restoration otherwise overrides
    # the requested size with the last session's.
    /Applications/Ghostty.app/Contents/MacOS/ghostty \
      --config-default-files=false --window-save-state=never "${opts[@]}" \
      -e $HERE/$TEST.sh ghostty >/dev/null 2>&1 &
    pid=$!
    ;;
  kitty)
    opts=()
    # remember_window_size defaults to yes even under --config NONE, and the
    # restored size would override initial_window_*; the "c" suffix means cells.
    [ -n "$COLS" ] && opts=(-o remember_window_size=no \
      -o "initial_window_width=${COLS}c" -o "initial_window_height=${ROWS}c")
    /Applications/kitty.app/Contents/MacOS/kitty \
      --config NONE "${opts[@]}" \
      $HERE/$TEST.sh kitty >/dev/null 2>&1 &
    pid=$!
    ;;
  *)
    echo "unknown terminal: $TERM_NAME (want tty7|alacritty|ghostty|kitty)" >&2
    exit 1
    ;;
esac

# Wait for the in-terminal script to write its "done" marker.
deadline=$((SECONDS + 200))
while (( SECONDS < deadline )); do
  if [ -f $res ] && grep -q '^done' $res; then
    break
  fi
  sleep 2
done

kill $pid 2>/dev/null
pkill -f -- "$WORK/cfg-$TEST-tty7" 2>/dev/null # tty7 GUI + its daemon
pkill -f -- "$HERE/$TEST.sh" 2>/dev/null
sleep 1
pkill -9 -f -- "$WORK/cfg-$TEST-tty7" 2>/dev/null

echo "=== $res ==="
cat $res 2>/dev/null || echo "(no result file — did the window open and stay visible?)"
