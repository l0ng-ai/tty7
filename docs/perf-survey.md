# Performance survey: what would make tty59 faster, snappier, more efficient

Research and measurement only (2026-10-29, v26.10.1 @ ee9c1ba, release
builds, this Linux desktop: Arch, Wayland/niri, 24×80 default grid unless
noted). Nothing here is implemented; every claim is measured or cited to
code. The input-latency instrumentation that produced §1 is in the tree
env-gated (`src/terminal/inputtrace.rs`, `TTY59_INPUT_TRACE=1`) — research
tooling, uncommitted.

## 1. Measurements

### Input latency (keystroke → grid, env-gated probe + wtype)

Typing `a`/`s` at a zsh prompt (17 paired keystrokes) and in nvim
insert/normal (10):

| stage | shell p50 | shell p90 | nvim p50 | nvim p90 |
|---|---:|---:|---:|---:|
| key handler → pty write | 25 µs | 34 µs | 38 µs | 50 µs |
| write → echo/redraw fed to the engine | 372 µs | 640 µs | 1.9 ms | 7.1 ms |
| feed → frame built + grid walked | 3.6 ms | 4.4 ms | 2.1 ms | 6.2 ms |
| **key → pixels (pre-present)** | **4.2 ms** | **4.9 ms** | **4.2 ms** | **13.3 ms** |

Reading: input dispatch is free; the pty/shell round trip is sub-millisecond
at a prompt (nvim's redraw generation is its own cost); **the dominant term
is the feed→frame wake — 2–3.7 ms of event-loop latency between the reader
feeding the engine and the UI thread's next render pass.** The engine's
steady fill is 0.005 ms (frame gate); the 3.7 ms is scheduling, not work.
Latency is already excellent; the wins below are throughput and efficiency,
not key-to-pixel.

### The DOOM-fire gap (2372 vs 2841 fps against the old engine)

Traced fire run (TTY59_TRACE, same session as the 6a numbers), steady
seconds: the GUI reader drains **93–99 MB/s** in **253 KB batches**, 371
delivered batches/s, engine `advance` = **592 ms/s (59% of one core)**,
lock waits 0.15–0.3 ms/s, socket read waits ~8 ms/s. The reader's own
untraced loop work (protocol decode, buffer copies, batch dispatch) is the
remaining ~400 ms/s — the **second-largest slice** and unattributed to
this day.

The gap is *not* parse capacity: at the fire's ~42.5 KB/frame the engine
could sustain ~157 MB/s ≈ 3 700 fps. The pipeline delivers 99 MB/s and the
reader sits 40% idle — the ceiling is upstream of the engine, most likely
the daemon's pty-read→socket relay pacing interacting with 253 KB batch
delivery. **Follow-up: the same per-second trace accounting on the daemon
pane reader** (`[trace daemon]` exists, pane.rs:2327) plus a µs-level split
of the GUI reader's non-advance time. The old engine's advantage was
per-batch cost shape, not per-byte parse.

### Steady state, memory, attach

| scenario | CPU | wakeups | RSS |
|---|---:|---:|---:|
| app, 1 idle pane | ~0% | 14/s | 116 MB |
| app, 10 idle panes | 1.7% of a core | 27/s | 120 MB (+0 kB/pane marginal) |
| 1 pane tailing at 20 lines/s | 0.8% of a core | 33/s | 118 MB |

Idle efficiency is already excellent (idle compress from 6a keeps
scrollback cheap; daemon RSS 4 MB). Two soft spots, both tty59-side, not
engine:

- **Cursor-blink frames repaint everything.** Idle-traced app: 407
  frame/grid passes in ~145 s (~2.8/s), each a full row walk + shape +
  paint submission even though only the cursor changed — no row was dirty.
- **`capture` costs ~308 ms wall on a 10k-line pane** (CLI connect alone:
  3 ms; raw == `--plain`, so the cost is the daemon's ring→newest-segment
  extraction, before any engine parse). The attach replay path shares this
  ring-walking shape. Startup-to-first-frame needs the probe extended to
  the window layer (niri exposes no cheap hook); `ls`-to-daemon is 3 ms,
  so startup cost is GUI-side.

### What `-Dcpu=baseline` costs (distribution decision, not a change)

Archive rebuilt with `-Dcpu=x86-64-v3`, same-session A/B of the parse
benches: plaintext **76 → 117 MB/s (+54%)**; TUI frames flat (492 vs 478,
noise). The scalar/older-ISA paths in the parser's hot loops (utf8 scan,
width lookup) are what baseline gives up. A v3 or native build would not
help the fire workload (TUI-shaped), but plain-text throughput (build
output, logs) gains materially.

## 2. The native surface vs what we do in Rust

What libghostty-vt offers but tty59 doesn't use, or duplicates:

| capability | tty59 today | native | expected gain | cost/risk |
|---|---|---|---|---|
| **snapshot.h** state encode/decode | attach = daemon byte-ring replay; capture = ring→segment walk (308 ms measured); hibernate = replay on wake | `ghostty_snapshot_encode*` / decoder: whole-terminal state as bytes | attach/replay in ms not hundreds; kills the ring-walk cost; smaller wire than raw bytes | daemon must link the engine (zig builds musl — CI already cross-builds msvc/gnu); replaces term_modes mirror + replay prelude. Big architectural win. |
| **render dirty end-to-end** | frame fill skips clean rows (render.h, 6a) but `build_grid` (element.rs:1828) walks + `shape_line`s (element.rs:1575 et al) every row every paint, blink included | per-row dirty is already in the render state | blink-only frames drop to cursor-only work; per-paint cost scales with dirty rows, not grid size | medium: plumb the dirty flag through the model Frame rows and skip walk/shape/paint for clean rows |
| **key.h encoder** | hand-rolled `keystroke_to_bytes` (input.rs, ~5 call sites) | `ghostty_key_encoder_encode` + `setopt_from_terminal` (reads the engine's own kitty flags) | correctness first: kitty fidelity follows the engine's live mode stack, incl. push/pop edge cases hand-encoding misses; small CPU win (encoding is tiny anyway) | medium; behavior change risk is low (golden-testable) |
| **mouse.h / paste.h / focus.h / size_report.h encoders** | all hand-encoded | one-call encoders, mode-aware | correctness/tidiness; zero measurable CPU | small each |
| **search.h incremental** | regex/case via a Rust matcher over extracted text; plain via search.h | native incremental search state (`ghostty_search_run` already used for plain) | at 10k lines: text extraction alone is 12.4 ms per full scan; incremental would be sub-ms on scroll | text matcher would go only if native grew regex — it hasn't; keep Rust matcher, but feed it incrementally off the render state instead of re-extracting |
| **kitty_graphics.h** | daemon lifts APC/Kitty frames out of the stream (decision 5); GUI places via tracked anchors (Phase 4) | native placement iterator + render info + `animation_tick` | placements track reflow natively (no anchor glue), animations ticked by the engine, one fewer daemon pass | large; contradicts decision 5's memory boundary — user call |
| **osc.h tokenizer** | daemon's OscTokenizer + term_modes mirror reimplement OSC/mode tracking for replay | `ghostty_osc_*` state machine | deleted daemon code, one parser | subsumed by the snapshot.h item (if the daemon links the engine, the mirror dies wholesale) |
| **memory: page size / compress cadence** | 5 s idle compress (6a: 29× on 10k lines); MAX_LINES prunes at page granularity | tunable in the engine | marginal — RSS is already flat per pane | nothing pressing |

## 3. Ranked table

Ranked by expected user-visible gain ÷ cost × risk (all measured where
marked):

| # | item | gain | cost | risk | behavior change |
|---|---|---|---|---:|---|
| 1 | Daemon links the engine; attach/replay/capture via **snapshot.h** | capture 308 ms → ms-class (measured ceiling); attach and hibernate-wake the same path; deletes the replay prelude + OscTokenizer + term_modes mirror | high (daemon arch; musl archive in CI — already proven shape) | medium | none visible |
| 2 | **Dirty-tracking end to end** through paint (blink-only frames; walk/shape only dirty rows) | idle/blink CPU and per-paint cost drop with grid size (measured: 407 full paints in an idle 145 s window) | medium (element.rs plumbing; frame rows carry dirty) | low | none |
| 3 | **Reader-loop µs accounting** (split the untraced 40% during fire; daemon-side trace) | finds the fire gap's 17% and the 253 KB batch pacing; likely small fixes with big benchmark swing | low (instrumentation, then targeted) | none | none |
| 4 | **key.h encoder** (and mouse/paste/focus/size_report one-liners) | kitty-protocol correctness from the engine's own mode state; deletes ~5 hand-encoding sites | medium | low | slightly different bytes in protocol edge cases (golden-testable) |
| 5 | **-Dcpu choice per release channel** (v3/native alongside baseline) | plaintext parse +54% measured | low (CI matrix + packaging) | low | distribution policy, not code |

## 4. Top 5, one sketch each

1. **Snapshot-based attach.** Zig builds `libghostty-vt` for
   `x86_64-linux-musl` (the CI composite action already cross-builds msvc
   and gnu — add the musl target to the server-musl job). Link it into
   tty59-core behind a small `-sys`-style seam; `ghostty_snapshot_encode`
   after every idle (compress cadence, 5 s) and on hibernate; attach =
   decode + one scrollback-fill, replacing the ring replay. The 308 ms
   ring→segment walk disappears from capture's path too (`capture` asks
   the engine's own state). The daemon's byte ring stays as the raw log
   (it must, for `capture` raw), but stops being the *replay* source.
2. **Dirty paint.** `Frame`'s rows gain a dirty bit stamped by the fill
   (the render state already tracks it — `row_iterator_next_dirty`).
   `build_grid` skips clean rows entirely (no `RenderCell` fill, no
   `shape_line`, no paint call — gpui needs the element painted once; keep
   the last frame's pixels via a per-row cache keyed on the dirty
   generation). Blink frames touch one cursor row. Risk: the gpui paint
   model wants a full scene per frame — measure whether skipping is even
   beneficial vs. gpui's own damage tracking; the win may be mostly the
   shape/walk CPU, not the paint.
3. **Fire-gap attribution.** Add µs marks inside the reader loop
   (protocol decode / `pending` copy / `feed_grid` split / event dispatch)
   and run the daemon's `[trace daemon]` pane accounting during the same
   fire bench. One afternoon of instrumentation; the fix it points at is
   probably batch-size or relay-buffer shape (253 KB reads vs the old
   engine's cadence), not engine work.
4. **Native key encoder.** `VtTerm` exposes `encode_key(event, mods)` via
   `ghostty_key_encoder_new` + `setopt_from_terminal` on the live engine;
   input.rs's five `keystroke_to_bytes` call sites collapse into it.
   Golden-test the byte outputs across mode stacks (the corpus already
   covers the mode churn). Mouse/paste/focus/size-report follow the same
   shape when touched.
5. **CPU-feature tier.** CI builds a second archive with
   `-Dcpu=x86-64-v3`; release ships baseline + a `tty59-v3` flavor (or
   AppImage variant) and the README states the measured +54% plaintext
   number. No code change — packaging and policy only.

## Instrumentation left in the tree

`src/terminal/inputtrace.rs` + five marks (view key handler, remote write,
reader feed, element frame/grid) behind `TTY59_INPUT_TRACE=1`. Zero cost
when off (one atomic load per mark site). Keep for the §3/3 follow-ups or
drop with the probe.
