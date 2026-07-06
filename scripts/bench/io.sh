#!/bin/zsh
# Plaintext-IO benchmark. Runs INSIDE the terminal under test as its shell
# (run_one.sh arranges that); $1 names the terminal for the result file.
#
# Methodology (moktavizen/terminal-benchmark): `time cat` an 11 MB text file,
# 5 runs. Timed with zsh's EPOCHREALTIME instead of `time` so the driver can
# collect results from a file instead of reading the screen.
zmodload zsh/datetime
SELF=${0:A}
HERE=${SELF:h}
REPO=${HERE:h:h}
WORK=${TTY7_BENCH_DIR:-$REPO/.bench}
T=${1:-unknown}
R=$WORK/results
mkdir -p $R
out=$R/io-$T.txt
: > $out
print -- "grid: $(stty size 2>/dev/null)" >> $out
sleep 1
for i in 1 2 3 4 5; do
  t0=$EPOCHREALTIME
  command cat $WORK/shakespeare.txt
  t1=$EPOCHREALTIME
  printf 'run %d: %.0f ms\n' $i $(( (t1 - t0) * 1000 )) >> $out
done
print -- done >> $out
sleep 5
