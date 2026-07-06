# Terminal benchmark harness

Reproducible throughput/FPS/memory benchmarks for tty7 against Alacritty,
Ghostty and Kitty, following the methodology of
[moktavizen/terminal-benchmark](https://github.com/moktavizen/terminal-benchmark).
macOS only (drivers use `/Applications` paths, BSD `ps`, `pkill`).

## What it measures

| Test | Method | Collects |
| --- | --- | --- |
| **Plaintext IO** | `cat` an 11 MB text file inside the terminal, 5 runs | elapsed ms per run (lower = better) |
| **Frame rate** | [DOOM-fire-zig](https://github.com/const-void/DOOM-fire-zig), 5 runs × ~14 s | cumulative average fps per run (higher = better) |
| **Memory** | cold launch, default shell, idle 6 s | RSS in KB (tty7 = GUI + daemon) |

The methodology source also measures **input latency**, but with a high-speed
camera ([is-it-snappy](https://github.com/chadaustin/is-it-snappy)) — that
can't be automated here and is skipped.

## Usage

```bash
scripts/bench/setup.sh                  # once: corpus + patched DOOM-fire (+ zig 0.14 if needed)
cargo build --release                   # the tty7 under test

scripts/bench/run_one.sh tty7 io        # ALWAYS first: records the reference grid
scripts/bench/run_one.sh alacritty io   # matches tty7's grid automatically
scripts/bench/run_one.sh ghostty io
scripts/bench/run_one.sh kitty io
scripts/bench/run_one.sh tty7 fire      # same order for the fire test
scripts/bench/run_one.sh alacritty fire
scripts/bench/run_one.sh ghostty fire
scripts/bench/run_one.sh kitty fire
scripts/bench/mem.sh                    # all four terminals, 3 runs each
```

Results land in `.bench/results/` (override the work dir with
`$TTY7_BENCH_DIR`, the tty7 binary with `$TTY7_BIN`).

**While a run is up: don't type into the window, and don't hide or fully
occlude it** — macOS throttles occluded windows and the FPS numbers collapse
(observed: ~950 fps → ~360 fps when the window was hidden mid-run).

## How it drives each terminal

Every terminal opens a real window whose *shell* is the benchmark script, so
the measurement includes the full input path the user experiences:

- **tty7**: an isolated `--config-dir` under the work dir whose `config.json`
  sets `shell` to the script. GUI + daemon are launched fresh and killed
  (by config-dir-scoped `pkill`) after the run — a daily-driver tty7 and its
  daemon are never touched.
- **Alacritty**: `--config-file <empty>` (isolates the user's config) plus
  `-o window.dimensions.…` for the grid, `-e` for the script.
- **Ghostty**: `--config-default-files=false` plus `--window-width/height`
  (cells) and `-e`. `--window-save-state=never` matters: macOS window
  restoration otherwise overrides the requested size.
- **Kitty**: `--config NONE` plus `-o initial_window_width/height=<n>c` (the
  `c` suffix means cells); the script is passed as trailing args (no `-e`).
  `-o remember_window_size=no` matters: it defaults to yes even under
  `--config NONE`, and the restored size overrides `initial_window_*`.

Warp is installed here but not benchmarked: closed source, no CLI to run a
script as the shell, and config isn't file-isolatable. iTerm2/Terminal.app
would need AppleScript driving and can't cleanly isolate config either.

Grid fairness: tty7 has no size flag, so its default window is the reference —
run tty7 first, and the driver reads the recorded `grid:` line to size the
other terminals identically (cells, not pixels; fonts differ).

## Why DOOM-fire is patched

`doom-fire-fps.patch` (applied by `setup.sh`) makes DOOM-fire dump its
cumulative average fps to `$DOOM_FPS_FILE` every 30 frames. The upstream
binary only *paints* the number, and recording the output stream to parse it
back (`script(1)`) is a trap: at 500+ fps the recording grows to gigabytes in
seconds and its disk writes throttle later runs by 4-8×. The fps definition is
unchanged — total frames / elapsed seconds since the fire started, the same
number painted on screen.

## Recorded baseline (2026-07-03)

Apple M1 Pro, 32 GB, macOS 26.3.1, grid 155×40, release builds, defaults.
Optimization = the `VecDeque` replay ring + coalesced `Output` frames +
backpressure gate (see CHANGELOG "Terminal throughput ~12× faster").

| Test | tty7 (before) | tty7 (after) | Alacritty | Ghostty | Kitty |
| --- | ---: | ---: | ---: | ---: | ---: |
| Plaintext IO, 5-run avg | 2030 ms | **161 ms** | 232 ms | 183 ms | 217 ms |
| DOOM-fire, 5-run avg | 47 fps | **920 fps** | 542 fps | 533 fps | 546 fps |
| Memory (GUI+daemon) | 100 MB | **105 MB** | 86 MB | 112 MB | — |

Kitty (0.47.4) was measured the same day at the same 155×40 grid; its memory
run was skipped. Upstream's Kitty frame-rate dominance (Linux/Wayland) does
not reproduce on macOS — here it lands in the same band as Alacritty/Ghostty.

Diagnosis notes for posterity: macOS PTYs deliver ~1 KiB per read. Before the
fix, every read into a full 8 MiB `Vec` ring memmoved the whole ring
(`drain(..overflow)`), eating ~92% of the daemon reader's time — visible as
"run 1 fast, later runs slow" (the ring fills during run 1). `TTY7_TRACE=1`
on both the GUI and a foreground daemon prints the per-second accounting that
localized this.

## Recorded baseline (2026-07-04, second throughput pass)

Same machine and grid, all four terminals re-run the same day. Optimization =
the CHANGELOG "second throughput pass" batch (16 MiB gate, 256 KiB socket
buffers, client-side Output batching, memchr OSC fast paths, atomic gate,
QoS promotion).

All four terminals measured back-to-back in one quiet-machine session:

| Test | tty7 (before) | tty7 (after) | Alacritty | Ghostty | Kitty |
| --- | ---: | ---: | ---: | ---: | ---: |
| Plaintext IO, 5-run avg | 154 ms | **95 ms** | 239 ms | 179 ms | 185 ms |
| DOOM-fire, 5-run avg | ~760 fps¹ | **888 fps** | 485 fps | 552 fps | 616 fps |
| Memory (GUI+daemon) | 112 MB | 115 MB | 105 MB | 128 MB | 130 MB |

¹ The tty7-before numbers were measured while the machine was busy (builds +
tracing running alongside); on the later quiet machine the same pre-pass
pipeline would have landed near its 07-03 920 fps. The fire before/after
delta is therefore mostly ambient load, not the optimization — see the notes
below. After the pass, quiet-machine fire runs tightened to 882–894 (±0.7%).

Notes for interpreting these numbers, learned the hard way:

- **The day's fps band matters more than the run.** The same pre-pass binary
  that recorded 920 fps on 07-03 measured 728–857 on 07-04; competitors
  reproduced within ±3%. tty7's fire number is drain-rate-bound and therefore
  sensitive to ambient machine load in a way the (slower) competitors aren't.
  Only compare tty7-vs-tty7 fire numbers from the same session.
- **DOOM-fire is producer-bound, not terminal-bound, at this level.** Under a
  raw do-nothing PTY reader it produces ~96 MB/s at a constant ~87 KB/frame —
  i.e. ~1050–1100 fps is the machine's ceiling for *any* terminal, and fps
  scales linearly with drain rate (capped-drain probe: 95 MB/s → 1092 fps,
  60 MB/s → 690 fps). tty7's steady seconds already drain at 93–98 MB/s; the
  gap to the ceiling is whole seconds where the *producer* gets descheduled.
- **`cat` completion time is a drain benchmark, not a render benchmark.** The
  16 MiB gate lets an 11 MB burst leave the PTY at device speed while the
  client parses behind; sustained plaintext drain is 148 MB/s against a
  ~170 MB/s raw-reader ceiling (the client's VT parser, ~0.7 core, is the
  remaining sustained-throughput limit).
