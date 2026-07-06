#!/bin/zsh
# DOOM-fire FPS benchmark. Runs INSIDE the terminal under test as its shell
# (run_one.sh arranges that); $1 names the terminal for the result file.
#
# 5 runs; each lets the fire burn ~14 s, then SIGINTs it. The binary is built
# by setup.sh with doom-fire-fps.patch, which dumps the cumulative average fps
# to $DOOM_FPS_FILE every 30 frames — the file's last value IS the run's
# average, so nothing has to record the output stream. (The obvious
# alternative, script(1), wrote multi-GB recordings on fast terminals and the
# disk writes throttled later runs by 4-8×.) stdout stays the real pty:
# DOOM-fire sizes itself via ioctl(stdout), and stdin may be a pipe — the
# printf feeds the "press return" intro pauses.
SELF=${0:A}
HERE=${SELF:h}
REPO=${HERE:h:h}
WORK=${TTY7_BENCH_DIR:-$REPO/.bench}
T=${1:-unknown}
R=$WORK/results
DOOM=$WORK/DOOM-fire-zig/zig-out/bin/DOOM-fire
mkdir -p $R
out=$R/fire-$T.txt
: > $out
print -- "grid: $(stty size 2>/dev/null)" >> $out
sleep 1
for i in 1 2 3 4 5; do
  fpsfile=$WORK/doom-fps-$T-$i.txt
  rm -f $fpsfile
  ( printf '\n\n\n\n\n\n'; sleep 60 ) | DOOM_FPS_FILE=$fpsfile $DOOM &
  sp=$!
  sleep 15
  pkill -INT -f 'zig-out/bin/DOOM-fire' 2>/dev/null
  sleep 1
  pkill -9 -f 'zig-out/bin/DOOM-fire' 2>/dev/null
  wait $sp 2>/dev/null
  fps=$(head -1 $fpsfile 2>/dev/null)
  print -- "run $i: ${fps:-NA} fps" >> $out
  printf '\e[0m\e[2J\e[H'
  sleep 1
done
print -- done >> $out
sleep 5
