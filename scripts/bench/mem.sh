#!/bin/zsh
# Memory benchmark: cold-launch each terminal with its DEFAULT shell, idle 6 s
# at the prompt, record RSS via ps, kill, repeat. $1 = runs per terminal
# (default 3). tty7 is reported as GUI + daemon — both processes are part of
# delivering one window.
set -u
SELF=${0:A}
HERE=${SELF:h}
REPO=${HERE:h:h}
WORK=${TTY7_BENCH_DIR:-$REPO/.bench}
TTY7_BIN=${TTY7_BIN:-$REPO/target/release/tty7}
RUNS=${1:-3}
R=$WORK/results
mkdir -p $R

rss_kb() { ps -o rss= -p ${1:-0} 2>/dev/null | awk '{print $1+0}' }

out=$R/mem-tty7.txt
: > $out
for i in $(seq 1 $RUNS); do
  cfg=$WORK/cfg-mem
  rm -rf $cfg && mkdir -p $cfg && print -- '{}' > $cfg/config.json
  $TTY7_BIN --config-dir $cfg >/dev/null 2>&1 &
  gpid=$!
  sleep 6
  dpid=$(pgrep -f -- "--daemon --config-dir $cfg" | head -1)
  g=$(rss_kb $gpid)
  d=$(rss_kb ${dpid:-0})
  print -- "run $i: gui ${g} KB, daemon ${d} KB, total $((g + d)) KB" >> $out
  kill $gpid 2>/dev/null
  [ -n "${dpid:-}" ] && kill $dpid 2>/dev/null
  sleep 1
  pkill -9 -f -- "$cfg" 2>/dev/null
  sleep 1
done
print -- done >> $out

out=$R/mem-alacritty.txt
: > $out
for i in $(seq 1 $RUNS); do
  : > $WORK/alacritty-empty.toml
  /Applications/Alacritty.app/Contents/MacOS/alacritty --config-file $WORK/alacritty-empty.toml >/dev/null 2>&1 &
  pid=$!
  sleep 6
  print -- "run $i: $(rss_kb $pid) KB" >> $out
  kill $pid 2>/dev/null
  sleep 2
done
print -- done >> $out

out=$R/mem-ghostty.txt
: > $out
for i in $(seq 1 $RUNS); do
  /Applications/Ghostty.app/Contents/MacOS/ghostty --config-default-files=false >/dev/null 2>&1 &
  pid=$!
  sleep 6
  print -- "run $i: $(rss_kb $pid) KB" >> $out
  kill $pid 2>/dev/null
  sleep 2
done
print -- done >> $out

out=$R/mem-kitty.txt
: > $out
for i in $(seq 1 $RUNS); do
  /Applications/kitty.app/Contents/MacOS/kitty --config NONE >/dev/null 2>&1 &
  pid=$!
  sleep 6
  print -- "run $i: $(rss_kb $pid) KB" >> $out
  kill $pid 2>/dev/null
  sleep 2
done
print -- done >> $out

echo "=== mem results ==="
for f in $R/mem-*.txt; do
  echo "--- $f"
  cat $f
done
