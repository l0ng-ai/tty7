# Migrating the VT core from `alacritty_terminal` to libghostty-vt

Status: plan · Target: Ghostty **v1.4.0** (released 2026-10-27) · Owner: TBD

## Scope

tty59 renders on gpui and keeps doing so. What changes is the **VT parser,
grid and scrollback**: today `alacritty_terminal` (our fork, pinned in the
workspace `Cargo.toml`); after this, **libghostty-vt**, the embeddable,
renderer-free half of Ghostty. Ghostty's full app-embedding `libghostty` is out
of scope: it would replace gpui rather than the engine.

The daemon is unaffected. It parses nothing (it is a byte pipe with its own
mode sniffer in `tty59-core/src/core/term_modes.rs`), so `tty59-core` and the
static `tty59-server` gain no new native dependency.

### Who parses VT today

| Consumer | Where | Size | Use |
|---|---|---|---|
| GUI | `src/terminal/remote.rs` + `view.rs`, `element.rs`, `search.rs`, `smart_select.rs`, `prompt_reflow.rs`, `view/copy_unwrap.rs`, `input.rs`, `composer.rs`, … | ~45k lines touching it | the live, rendered mirror |
| Mobile gateway | `crates/tty59-gateway/src/mirror.rs` | 356 | screen snapshots for phones |
| CLI | `crates/tty59-cli/src/screen.rs` | 257 | `tty59 capture --plain` |

Coupling hot spots: `view.rs` (69 lock sites, 43 grid reads, 31 `TermMode`
checks, 38 scroll/`display_offset` uses, 15 selection uses) and `remote.rs`
(55 lock sites, 31 grid reads, 25 mode checks).

Alacritty surface in use: `Term`, `vte::ansi::Processor`, `sync::FairMutex`,
`EventListener`/`Event`, `index::{Point, Line, Column, Side, Direction,
Boundary}`, `grid::{Dimensions, Scroll}`, `selection::{Selection,
SelectionRange, SelectionType}`, `term::search::{RegexSearch, Match}`,
`term::cell::{Cell, Flags, Hyperlink}`, `TermMode`, `CursorShape`/`CursorStyle`,
`Rgb`/`Color`/`NamedColor`, `MIN_COLUMNS`/`MIN_SCREEN_LINES`.

## What Ghostty 1.4.0 means for this plan

- **libghostty is not tagged with 1.4.0.** It has its own release cycle; the C
  API "has broken compatibility several times" this cycle, with one more ABI
  break planned before a libghostty 1.0 tag, expected before Ghostty 1.5
  (March 2027). → We pin the **`v1.4.0` tag** now and budget one re-pin to
  libghostty 1.0 (Phase 7).
- **Windows is a supported libghostty-vt target** (MSVC and GNU), along with
  macOS, Linux, FreeBSD, Android, iOS and wasm, all verified in CI. It no
  longer needs libc++ and can build freestanding. → Removes the biggest
  platform risk; Phase 0 still proves it inside our build.
- **Zig 0.17** builds Ghostty.
- Packaging: pkg-config, CMake, Nix, source tarball, xcframework. API docs:
  <https://libghostty.tip.ghostty.org> (tracks `main`; check against the tag).
- Emulation changes we inherit for free:
  - VS15/VS16 apply to the preceding codepoint (covers our fork patch #2,
    issue #203), and combining marks after the last column attach to it.
  - Reflow is 3–6× faster on large or wide-char scrollback; cursor and saved
    cursor are preserved across reflow; `CSI 2 K` clears soft-wrap state.
  - Idle scrollback is compressed (`ghostty_terminal_compress`, 70–90% less
    resident memory), which matters for many-pane workbenches.
  - Scrollback limits exist in both bytes and lines
    (`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` / `_MAX_LINES`).

## Decisions

1. **Bindings: our own `-sys`, generated from the pinned header.** Third-party
   Rust bindings (`libghostty-vt` 0.2.x on crates.io, `Uzaaft/libghostty-rs`,
   `ghostty-vt-rs`) trail the C API and make every type `!Send`/`!Sync`, while
   our design shares one terminal between the reader thread and the UI thread
   (`Arc<FairMutex<Term>>`). So:
   - `crates/tty59-vt-sys`: bindgen output from `include/ghostty/vt.h` at
     `v1.4.0`, checked in, plus a freshness check in CI.
   - `crates/tty59-vt`: our safe wrapper. It owns the handle, asserts `Send`
     (never `Sync`) and is only ever reached through our mutex. Callbacks are
     wrapped in `catch_unwind`.
   - Upstream bindings stay a reference to borrow ideas and tests from, not a
     dependency.
2. **No Zig for contributors.** CI builds `libghostty-vt.a`/`.lib` per target
   from the pinned tag with Zig 0.17 (`ReleaseFast`, baseline CPU), and
   publishes them with SHA-256s. `build.rs` fetches nothing: it links the
   checked archive, or a local build via `TTY59_GHOSTTY_DIR` for hacking on
   Ghostty itself. Release packaging rejects any binary that links a shared
   `libghostty`.
3. **Seam first, swap second.** Every consumer moves onto tty59-owned types
   while alacritty is still underneath. The Ghostty backend then lands behind
   a cargo feature `vt-ghostty`, so both engines run side by side for
   differential tests. Alacritty is deleted last.
4. **Input encoding stays ours** (`terminal/input.rs`). Ghostty's
   `key.h`/`mouse.h` encoders are an optional follow-up (Phase 8).
5. **Kitty graphics keep their current path.** The daemon lifts APC frames out
   of the stream (`core/kitty_graphics.rs`), and the client decode worker
   (`terminal/images.rs`) places them. Ghostty's own image handling is
   disabled in the client (APC limits set to 0) so there is exactly one image
   path. Native Ghostty graphics are Phase 8.
6. **Search is shaped like Ghostty's, with a hybrid backend.** Ghostty's
   `search.h` owns its matches: it tracks them across page pruning,
   compression, reflow, resets and screen switches, and advances
   incrementally through `feed`/`tick`. So the seam's search is an
   engine-owned session (set the query, feed, read the selected match and the
   matches in the viewport, select next/prev). It is not a `Vec` of grid
   points that `view.rs` rebases as scrollback moves. On alacritty, the
   backend wraps `RegexSearch` and absorbs today's rebasing (`anchor_row`,
   `rebase_matches_after_scroll`, `refresh_matches_after_output`). On Ghostty
   (Phase 3), plain case-insensitive queries run through `search.h`. Regex and
   case-sensitive queries, which `search.h` lacks, run through tty59's matcher
   over formatter text, anchored with `grid_ref_tracked` so the matches
   survive pruning and reflow.
7. **tty59's OSC tokenizers stay authoritative** for 133, 7, 0/2, 9/777 and 52
   (`core/osc.rs`). Ghostty's matching callbacks are left unset or ignored, so
   nothing is handled twice.

## Phases

Each phase ends in mergeable PRs with existing tests green. Estimates assume
one engineer.

### Phase 0: Spike and go/no-go (1 week)

- [ ] Build `libghostty-vt` at `v1.4.0` with Zig 0.17 for:
      `aarch64-apple-darwin`, `x86_64-apple-darwin`,
      `x86_64-unknown-linux-gnu` (AppImage baseline glibc),
      `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`.
- [ ] Link it into `tty59-app` on each, next to gpui. Check for symbol clashes
      and CRT mismatches on MSVC.
- [ ] Throughput: a bare `vt_write` loop vs `Processor::advance` on the
      `scripts/bench` workloads. The README claims ~2× Alacritty/Ghostty/Kitty,
      so regressions must be known now. Measure snapshot-copy cost separately.
- [ ] Threading audit: confirm libghostty-vt holds no thread-local or
      thread-affine state, so `Send` behind a mutex is sound. Callbacks run on
      the `vt_write` caller, and must not re-enter `vt_write`.
- [ ] API coverage check against the `v1.4.0` headers: `terminal.h`,
      `render.h`, `screen.h`, `selection.h`, `search.h`, `grid_ref_tracked.h`,
      `formatter.h`, `modes.h`, `snapshot.h`, `unicode.h`.
- [x] ConPTY resize prototype (see Phase 5, fork patch #3) on Windows with
      PSReadLine. **Chosen and implemented in Phase 5** (option (a), the
      engine's own `RESIZE_PULL_SCROLLBACK`); the live-PSReadLine gate itself
      stays a Windows manual check — see Phase 5's status section.
- **Exit:** builds and links everywhere, throughput within ~5%, a written gap
  list, and the ConPTY approach chosen.

#### Phase 0 results: Linux x86_64, 2026-10-28

Measured with `cargo test --release -p tty59-cli --features vt-ghostty -- --ignored --nocapture`
(`crates/tty59-cli/src/bench_vt.rs`): 11.5 MB per workload, 64 KiB chunks,
200×50 grid, 10k-line scrollback.

| Measurement | alacritty | ghostty |
|---|---|---|
| Plain-text throughput | 85 MB/s | 87 MB/s (1.03×, exit criterion met) |
| TUI frames (`2J` + CUP + SGR churn) | 63 MB/s | 377 MB/s (upper bound: Ghostty's ED2 skips scrollback) |
| Viewport snapshot, 200×50 | — | 0.62 ms mean, 0.78 ms worst (per-cell `grid_ref`, no buffer reuse) |
| Plain text of a 10k-row scrollback | 8.9 ms | 80.5 ms (per-cell FFI; batch via `formatter.h` or render state in Phase 6) |

Gaps found in the v1.4.0 headers, each now assigned to a phase:

- No way to query the cursor shape (`DEFAULT_CURSOR_STYLE` can only be set).
  The wrapper tracks what it set.
- `SCROLLBACK_MAX_BYTES` defaults to 10 KB, which makes the line limit
  meaningless. The wrapper clears it.
- Hyperlinks come back as a URI per cell, with no numeric id. The wrapper
  dedupes them into a side table.
- `grid_ref` is documented as not meant for render loops, so the GUI (Phase 3)
  must snapshot through `render.h`.
- The archive references `arc4random_buf`, which needs **glibc ≥ 2.36**. Check
  this against the AppImage's baseline glibc in the cross-build.
- Query/reply effects (DA1–3, XTVERSION, XTWINOPS size, color scheme, ENQ,
  clipboard read/write) exist but aren't wrapped yet. While unset, queries go
  unanswered (Phase 4).
- ED2 (`CSI 2 J`) doesn't push the screen into scrollback; alacritty's does.
  Capture is unaffected, but this is a GUI parity item (Phase 4).
- A tab is stored as pen movement, not a literal `\t`. `tty59 capture --plain`
  output differs in spacing, never content. Decide in Phase 4 whether to
  reconstruct tabs.
- Intended difference: grapheme clustering (mode 2027) is on by default, so
  DECRQM 2027 now reports it as set.

### Phase 1: `tty59-vt-sys` + `tty59-vt` (1–2 weeks)

- [ ] `tty59-vt-sys`: bindgen from the pinned header, `build.rs` links the
      checked archive, plus a CI freshness check.
- [ ] `tty59-vt::Terminal`: `new`, `write`, `resize(cols, rows, cell_px)`,
      `scroll(ScrollTarget)`, `reset`, `set_options`, mode get/set, cursor.
- [ ] Callbacks become one `VtEvent` enum shaped like today's `AlacEvent`
      handling: `PtyWrite`, `TitleChanged`, `Bell`, `ClipboardStore/Load`,
      `ColorRequest`, `SizeRequest`, `Wakeup`, plus `PwdChanged`,
      `Notification`, `Progress`.
- [ ] `Snapshot`: an owned copy of the viewport (rows, cells, graphemes, style,
      hyperlink id, wrap flag, cursor, modes, dirty rows) taken under the lock.
      **No C pointer outlives the lock**, since rows and cells are invalidated
      by the next mutation. Reuse buffers and copy only dirty rows.
- [ ] Options mapping (replaces `terminal_config_from_user` in `remote.rs`):
  - `scrollback_limit` → `SCROLLBACK_MAX_LINES` (approximate; document it)
  - `cursor_style` → `DEFAULT_CURSOR_STYLE`/`_BLINK`
  - theme → `COLOR_FOREGROUND/BACKGROUND/CURSOR/PALETTE`, so OSC 4/10/11
    replies come from Ghostty with our colors
  - Kitty keyboard on
  - APC/Kitty image limits → 0 (decision 5)
- [ ] Unit tests for the wrapper itself: callback panics, resize to the
      minimum size, empty writes, split UTF-8 and split escape sequences.

### Phase 2: The seam, with no behavior change (3–4 weeks)

Add `src/terminal/vt/` (shared with the gateway and CLI through `tty59-vt`)
holding tty59's own `GridPoint { line: i32, col: u16 }`, `Side`, `CellFlags`,
`Modes` (bitflags), `CursorShape`, `Rgb`, `SelectionSpan`, `Snapshot`, and a
`VtTerm` facade whose first backend is alacritty.

Port in order of size, one PR each:

- [ ] `crates/tty59-cli/src/screen.rs`
- [ ] `crates/tty59-gateway/src/mirror.rs`
- [ ] `palette.rs`, `color_scheme.rs`, `size.rs`, `command_cursor.rs`
- [ ] `prompt_reflow.rs`, `parked_cursor.rs`, `view/copy_unwrap.rs`
- [ ] `smart_select.rs` (word, URL and path patterns run over snapshot text,
      not grid internals)
- [ ] `search.rs`: tty59's own regex over row text with a line↔offset map,
      replacing `RegexSearch`. Use Ghostty's `search.h` instead if Phase 0
      shows it fits.
- [ ] `element.rs` (render reads a `Snapshot`, never the live grid)
- [ ] `remote.rs` (reader loop, `EventProxy`)
- [ ] `view.rs`, split by area: selection → scroll and display offset → modes
      → hit-testing and hyperlinks → the rest
- **Exit:** `rg alacritty_terminal` matches only
  `src/terminal/vt/alacritty.rs` and the backend's `Cargo.toml` lines.

**Phase 2 status: done (2026-10-28).** Commits b1af977 (2a), 5ca9e6f (2b),
d921d75 (2c), 079278a (2d), 8bb4b93 (2e), 3d312da (2f-1) and the 2f-2 commit.
Outside `tty59-vt`'s alacritty backend, its copy of alacritty's `FairMutex` and
the `cfg(test)` engine benches, nothing names `alacritty_terminal`.

Phase 3 contracts recorded along the way, which the Ghostty backend must keep:

- `has_selection()` is true for a drag's unresolved first click (2f-1).
- `cursor_cell()` keeps reporting the cursor row while the viewport is
  scrolled away from it (2f-2). A render-state cursor that goes silent outside
  the viewport can't back it; derive the row from the active cursor plus the
  viewport offset.
- The wide-tail cursor maps to the lead cell
  (`a_cursor_on_a_wide_tail_reports_the_lead_cell`, 2d).
- The frame-build gate below.

Known flake, pre-existing and not caused by the seam:
`remote::replay_tests::the_later_of_two_racing_attaches_keeps_the_screen_and_the_seat`
fails about 0.5% of the time under load on `main` and on the branch alike
(9/2080 and 10/2080, 8 copies in parallel). It reads the width as soon as
`RACED-OUTPUT` appears, which can be before the replay's geometry is applied.

### Phase 3: Ghostty backend behind `vt-ghostty` (2 weeks)

- [x] Implement `VtTerm` over `tty59-vt` (Phase 3a).
- [x] Switch consumers in the same order: CLI → gateway → GUI (Phase 3a —
      the GUI flips with the feature, no consumer code changed).
- [x] **Differential harness** (Phase 3b): identical byte streams into both
      engines, split at random chunk sizes, comparing normalized frames,
      cursor, modes and ordered events. Corpus: hand-written streams (SGR,
      wide/emoji, alt screen, scroll regions, DECSCUSR, OSC, wraps, tabs,
      queries), the `bench_vt` workload shapes, and recorded tool sessions
      (`tests/fixtures/`: vim, nvim, htop, less, tmux-in-pane, plus a
      hand-written agent-TUI stream). Known, intended differences are
      allow-listed by name with a reason (see "Phase 3 status" below).
- [x] Fuzz (Phase 3b): `tests/fuzz.rs` — seeded garbage, escape-dense
      bytes, fragments cut mid-sequence, resizes mid-stream (floors to
      120×40), deep kitty/title/SGR stacks; invariants (cursor in grid,
      frame shape, reads) checked per step on both engines.
- [x] **Frame-build gate.** Ghostty's `Frame` fill goes through `render.h`
      and copies only dirty rows: **0.218 ms** full-dirty (1.04× of the
      0.209 ms pre-seam baseline), 0.005 ms steady (one row changed), both
      at 200×50 mixed SGR in release. The BCE tag probe (Phase 3b) is
      skipped on cells with text — an engine cell holds a character or an
      erase fill, never both — which is what keeps it there.

**Phase 3 status: done (2026-10-29).** Phases 3a+3b as 400b0ac and the
3b changes. `cargo test --features vt-ghostty` at the root: **2546 pass,
0 fail** (same count as the default build). Two engine-semantics finds of
note:

- **Kitty stack `CSI =` semantics differ.** alacritty applies a set to the
  flags in effect and leaves its stack verbatim (a pop reveals the last
  push); Ghostty rewrites the current stack entry (a pop reveals the set).
  `tty59-core`'s `term_modes` tracker models the engine its replay prelude
  feeds, selected by the root's `vt-ghostty` feature (tty59-core's own
  `vt-ghostty` cfg; the static `tty59-server` never turns it on).
- **A synchronized-output hold (mode 2026) freezes differently.** The
  replaced engine buffers the bytes (its grid is the pre-hold frame);
  Ghostty parses live and the *renderer* holds. The backend follows
  render.h's recipe — capture at hold-begin, don't refresh until the hold
  lifts — so painted frames agree mid-hold; live grid reads (the cursor)
  legitimately differ until then.

Intended differences carried in the harness allow-list: XTVERSION answered
(`tty59 <version>`), DA2/DA3 shapes, kitty always spoken, DECRQM values
(2027 on by design; 69/2031 defaults), color queries answered from
configured options (the theme push through `VtOptions`), XTWINOPS/mode-2048
answered by the engine, title restore on alt-screen exit (Phase 4), mouse
reporting modeled as one enum (the `MOUSE_MODE` union is the contract),
tabs as pen movement, ED2 not pushing the screen into scrollback, SGR 53
overline supported, OSC 7 pwd reported (informational; tty59's tokenizer
stays authoritative), OSC 52 read double-answered (engine empty reply +
the embedder's real one).

### Phase 4: Behavior parity (2 weeks)

- [ ] **Replay suppression.** `EventProxy` drops `PtyWrite`, `Bell`,
      `ClipboardStore/Load`, `ColorRequest` and `TextAreaSizeRequest` while a
      replay runs. The callbacks honor the same flag.
      *Status: the sink-side filter covers both engines (a backend emits the
      same model events either way); the Ghostty engine additionally answers
      DA/XTWINOPS/color queries itself through `PtyWrite`, which the filter
      drops — covered by remote.rs's replay-suppression test under the
      feature. Done in all but a dedicated both-engine callback-level pin.*
- [ ] **Identity replies** must match what the daemon answers while detached
      (`daemon/pane.rs::detached_reply`, currently VT102 `ESC[?6c`). Either set
      the `DEVICE_ATTRIBUTES`/`XTVERSION` callbacks to the same answer, or
      update `detached_reply` to Ghostty's, but never let the two disagree.
      *Status: the Ghostty backend answers DA1 as `ESC[?6c` — the daemon's
      answer unchanged, agreement pinned by a tty59-vt test. Done.*
- [x] Selection semantics: simple, word (with `word_separators` as
      `semantic_escape_chars` today), line, block, and selections across
      streaming. Word gestures expand through selection.h's own walker
      with the configured separators; whole-row and rectangle kinds pinned
      engine-against-engine in the diff harness.
- [x] Viewport: display offset, scroll anchoring while output streams
      (pinned in the harness: an anchored viewport holds through streaming
      and the live one follows), the scrollbar and hibernated tabs ride
      the same frame facts (both suites green on both engines).
- [x] Hyperlinks: OSC 8 hover/underline grouping — the URI-per-cell rule
      both engines report, with one side-table index per URI, pinned in
      the harness (adjacent same-URI runs group identically).
- [x] Image anchors and prompt marks survive scrollback pruning and reflow,
      using `grid_ref_tracked`. *Done as the tracked-anchor seam:* the
      Ghostty backend tracks the placement cell
      (`tty59_vt::TrackedAnchor`, `Session::track_point`); the reader
      creates one per placed image and the paint path resolves it (exact
      through pruning and reflow, `None` — no paint — once the row is
      gone). Engines without tracking (alacritty, the default build)
      answer `None` and keep the absolute-row anchors they always had.
      Prompt marks need no seam: both engines carry their own row
      semantics (OSC 133), and the reflow repair reads them through the
      session as before. Note: the engine prunes scrollback at *page*
      granularity, so small line limits keep more than asked (measured: a
      5-line cap held 19 rows) — anchors are what make that observable
      honestly.
- [x] Hibernated tabs: a sleeping tab lets go of its panes entirely and a
      wake rebuilds each one through the daemon's attach replay — the
      engine never survives a hibernate, so there is nothing
      engine-specific to hold; the replay paths run green on both engines
      (the racing-attach and replay-size suites).
- [x] Column widths: the input bar measures with the compiled-in engine's
      own table (`tty59_vt::codepoint_width` — libghostty-vt's table
      behind `vt-ghostty`, the fork's `unicode-width` otherwise) and the
      VS16 cluster rule through `tty59_vt::grapheme_width`, so bar and
      grid cannot drift.
- [x] **Tabs in `capture --plain`: accepted, not reconstructed.** The
      replaced engine stores a literal `\t`; Ghostty moves the pen, so
      extraction reads spaces-to-tab-stop (`"a\tb"` → `"a       b"`).
      Content is identical, spacing differs; reconstructing tabs from the
      grid would guess (real spaces and pen-jumps are indistinguishable
      after the fact), so capture keeps what the engine actually holds.
      The differential harness normalizes whitespace content-equal for
      exactly this reason. If formatter.h ever grows a tab-aware output
      mode, revisit.
- [x] **ED2 scrollback parity.** Implemented in the Ghostty backend
      (`ghostty_ed2.rs`): ground-state `CSI 2 J` on the primary screen is
      rewritten to the engine's own scroll-clear `CSI 22 J` — a
      parser-state scanner (payload-aware across chunks, split-tolerant,
      screen-tracking within the buffer), so an SSH session's remote shell
      keeps `clear`/`^L` history the way the alacritty build always did.
      At-prompt clears were already native (OSC 133 marks reach the
      engine; Ghostty's own at-prompt heuristic, #905). Pinned in the
      harness: ssh-like clear (both engines keep history, split-proof),
      the full `ESC[H ESC[2J ESC[3J` ends empty on both, payloads and
      near-misses untouched. The scanner's hot-path cost is one memchr
      per chunk: 0.7 ms per 11.5 MB of plaintext (measured, `#[ignore]`d
      bench in the module).

**Phase 4 status: done (2026-10-29).** Both suites green on both engines
(2547 GUI default and feature, 0 failures). Real fixes landed: the ED2
parity rewriter (ground-state `CSI 2 J` → the engine's own scroll-clear
`CSI 22 J`, payload-aware and split-tolerant; at-prompt clears were
already native through OSC 133), word-selection expansion through
selection.h with the configured separators, BCE backgrounds folded into
both cell-read paths, the input bar on the compiled-in engine's own width
table with cluster-level VS16 scoring, and the tracked-anchor seam for
image placements. Engine differences found and allow-listed with reasons
(see the harness's allow-list and Phase 3's): kitty-stack `CSI =`
semantics, mode-2026 hold capture timing, ESC-inside-OSC abort handling,
mouse-mode granularity, DECRQM values, XTVERSION/DA2/DA3 shapes, color
queries answered from configured options, title restore on alt-screen
exit, tabs as pen movement, ED2 history (now parity), SGR 53 overline.

### Phase 5: Carrying the four alacritty-fork patches forward (1 week, plus the Phase 0 prototype)

| # | Fork patch | Status on Ghostty |
|---|---|---|
| 1 | `push_keyboard_mode` truncated the title stack (panic after 4097 pushes) | **Done.** Not an issue in Ghostty: the kitty stack is a fixed ring of eight (`terminal/kitty/key.zig` `FlagStack.len`), and there is no title stack at all. `deep_keyboard_mode_pushes_leave_the_reader_alive` runs on both engines through the seam; `term_modes.rs`'s cap mirrors the compiled engine (8 under `vt-ghostty`, 4096 on the default build), and both caps are pinned by a backend test pushing past their own. |
| 2 | VS16 emoji reserves 2 columns (#203) | **Done.** Fixed upstream in 1.4.0. `emoji_presentation_sequences_reserve_two_columns` runs on both engines, and the harness corpus gained a variation-selectors case (VS15 one column, VS16 two, a pair of each, VS16 at the line edge) compared per chunk with the cursor landing. |
| 3 | `Config::conpty_resize`: resize the primary grid the way conhost re-anchors (#415) | **Done, option (a).** `VtOptions::conpty_resize` maps to the engine's `GHOSTTY_TERMINAL_OPT_RESIZE_PULL_SCROLLBACK = false` (its docs name ConPTY as the motivating case): grow opens blank rows with the cursor pinned, and a column reflow keeps the active-area top on the same content. The fork's three grid tests are ported engine-side, and the harness compares both engines with the option on for the grow and PSReadLine-shrink shapes. One spelled difference, see the status notes. |
| 4 | `Term::reset_keyboard_modes` when the prompt returns (#1155) | **Done (Phase 3b).** The DEC-47 round trip clears both screens' stacks; `a_prompt_forgets_kitty_flags_a_killed_program_left_on_either_screen` runs on both engines (verified under the feature suite). |

**Phase 5 status: done (2026-10-29).** Both root suites green on both
engines; `tty59-vt` green with and without the feature (unit, diff, fuzz);
clippy clean on both; the frame fill untouched, so the Phase 3 gate carries.

The #3 ConPTY detail: Ghostty's shrink is bottom-anchored (conhost's own
model — the rows above spill into history) where the fork's patch protects a
high cursor by dropping bottom rows instead. The shapes differ only when
anchoring the content would push the cursor's own row out of the viewport,
and there the new engine moves the cursor to the viewport's top-left. That is
invisible on a live ConPTY: the program repaints the whole viewport after a
resize, so the moved cursor is overwritten before anything reads it. Pinned
engine-side (`conpty_shrink_bottom_anchors_when_the_cursor_sits_high`) rather
than harness-compared.

Windows manual checks still owed (Linux cannot run them):

- The #3 gate: PSReadLine prompt redraws land on the right rows after grow
  and shrink on a real ConPTY pane with the Ghostty engine.
- The cursor-high shrink edge above, watched once on that same pane.
- Archive cross-build feasibility, investigated only: `zig build
  -Demit-lib-vt -Dtarget=x86_64-windows-gnu` succeeds from Linux (ar-format
  archive, usable by a `*-pc-windows-gnu` Rust target but not tty59's msvc
  ones), while `-Dtarget=x86_64-windows-msvc` from Linux fails in the C
  dependencies (wuffs/simdutf/highway want Windows-SDK headers zig does not
  ship). The msvc archive needs a Windows host with MSVC — where zig
  auto-detects the SDK — matching CI's `windows-latest` model. Ghostty's
  build already names the Windows static artifact `ghostty-vt-static.lib`,
  exactly what `tty59-vt-sys`'s build.rs links on `CARGO_CFG_WINDOWS`.

### Phase 6: Performance, rollout, deletion (1–2 weeks)

- [x] Profile the reader loop and snapshot path. Thin LTO no longer inlines
      across the engine boundary, so batch `vt_write` calls and copy only
      dirty rows. *(Phase 6a: the reader already feeds in 64 KiB batches —
      `MAX_LOCKED_FEED`, the same size the benches drive — so no batching
      change was needed; the snapshot path was already the render-state
      fill. The profile's real finding was text extraction; see the status
      notes.)*
- [x] Call `ghostty_terminal_compress` on idle and hibernated panes. *(6a:
      idle only — Phase 4 established no engine state survives
      hibernation. Five seconds of quiet, activity-token-gated; see the
      status notes.)*
- [x] Re-run `scripts/bench`, and update the README benchmark numbers honestly.
      *(6a: engine-level benches re-run and recorded below; the README text
      is drafted as a pending block below and lands with the rollout commit
      — the shipped numbers stay until the default engine flips.)*
- [ ] Release N: `vt-ghostty` on by default, with a hidden fallback setting.
- [ ] Release N+1: delete the alacritty backend, the `l0ng-ai/alacritty` fork
      pin and its `[profile.dev.package]` entry.
- [x] Docs: README ("VT core from Alacritty" → Ghostty) in both languages,
      `docs/terminal/*`, and third-party notices (Ghostty is MIT) in the
      DMG, installer and AppImage. *(6a: notices ship everywhere packages
      stage files — see the status notes; the README swap itself is the
      pending block below.)*

**Phase 6a status (2026-10-29). Not the default engine yet — no flip, no CI
changes, no deletion; those wait on the user.**

*Text extraction (the ~9× gap).* `text_full` — `tty59 capture --plain`'s
engine, and every whole-grid read — moved from a per-cell `grid_ref` walk
onto the engine's own plain formatter (`formatter.h`, one bulk `format_buf`
call), with one reconciliation: the formatter trims all trailing blank
lines where the seam's contract keeps a line per row, so the blank tail is
re-appended after the last text-occupied row. Measured (release,
`bench_vt::bench_text_extraction_at_scrollback_depth`, 10k-row scrollback):

| | before (per-cell) | after (formatter) |
|---|---:|---:|
| ghostty `text_full` | 80.5 ms (Phase 0) | **12.4 ms** |
| alacritty `bounds_to_string` | 8.9 ms (Phase 0) | 6.4 ms |

From ~9× slower to 1.9× — with two parity fixes the switch pinned and the
differential harness now carries as corpus cases: a wide character
soft-wrapped to the next row is no longer emitted twice, and a row filled
only by an erase's background extracts as an empty line (alacritty's
spelling) instead of a run of spaces. Tabs remain the one accepted
difference (literal `\t` vs spaces-to-stop). Small extractions got faster
too (a 6.5k-char grid reads in 0.17 ms).

*Idle compression.* The reader loop's existing 500 ms wake (the QUIT_POLL)
is the idle tick: after 5 s of quiet — measured from the engine's own
activity token, so a scroll through history restarts the delay exactly like
new output does — each wake runs one bounded incremental
`ghostty_terminal_compress` step under the engine's fair mutex, which is
the same lock a paint takes, so it can never run under a paint or over a
borrowed frame. Five seconds because compression is only worthwhile once
the pane is parked (pages touched again decompress transparently, so
compressing under an active reader or scroller just buys the restore), and
because a step is idle-callback-sized — a wrong guess costs one bounded
step, never a stall. Measured on a pane holding 10k rows of scrollback
(`idle_compress_shrinks_a_10k_row_scrollback`): engine primary resident
17.3 MB → 0.6 MB (**29×**, 43 pages), process RSS 24.2 MB → 8.6 MB
(**15.6 MB freed**), contents byte-identical afterwards. The default
build's facade answers "no compressor" and the whole pass is skipped.

*Benchmarks, engine level* (this machine, release; the harness's own
numbers, not the README's — see the pending block): plaintext parse
11.5 MB workloads ghostty 121 MB/s vs alacritty 65; TUI frames 613 vs 79
(the ED2 caveat: Ghostty's clear skips scrollback push, an upper bound);
viewport snapshot 200×50 0.58 ms. The reader facade's own overhead stays
noise-level on the default build (0.99–1.01× raw); under the feature the
bench's "raw" arm is still alacritty's, so its ratios are engine-vs-engine,
not facade cost. The frame gate holds: 0.220 ms full-dirty (1.05× the
pre-migration 0.209), 0.005 ms steady.

*scripts/bench*, the GUI+pty harness: its competitor arms are macOS-only,
so this Linux run covers the tty59 arms on both engines, same session,
same 24×80 grid (tty59's default window here; the README baseline machine
and grid are the flip-commit re-run's job):

| tty59 arm, 5-run avg | alacritty build | ghostty build |
|---|---:|---:|
| Plaintext I/O — 11 MB `cat` | 126.6 ms | **118.6 ms** |
| DOOM-fire frame rate | **2841 fps** | 2372 fps |

Plaintext through the whole stack favors the ghostty build; DOOM-fire at
this small grid — per-frame escape churn over 1 920 cells, where per-frame
costs dominate batch-parsing wins — favors the alacritty build by ~17%.
The engine-level benches say the trade flips with grid size (TUI-parse
4–8× in ghostty's favor at 200×50), so the flip commit's README numbers
must come from the baseline machine at the baseline grid.

*Third-party notices.* The alacritty notice never shipped anywhere (the
repo's gap, pre-migration): only ConPTY's did, in the Windows installer.
Now `assets/licenses/` carries alacritty's dual license (Apache-2.0 and
MIT texts) and Ghostty's MIT, and every packager that stages files ships
them: the Linux tarball, the macOS app bundle's Resources, and the Windows
stage + installer (+ the verifier asserts them, and the updater's managed
list accepts them — a missing notice is a CI bug, never an update-abort,
same rule as ConPTY's). Both stay until the backend deletion drops
alacritty's pair.

*README pending block* — lands with the rollout commit, with numbers from
the baseline machine, not this one:

> Tagline: `VT core from Alacritty` → `VT core from Ghostty` (zh-CN:
> `VT 内核来自 Alacritty` → `VT 内核来自 Ghostty`).
>
> Footer: "Built on gpui and `alacritty_terminal`" → "Built on
> [gpui](…) and [libghostty-vt](https://github.com/ghostty-org/ghostty),
> Ghostty's renderer-free VT engine" (zh-CN equivalent), keeping
> Apache-2.0 as the project license.
>
> Benchmarks table: re-measured on the README's machine with the flip
> build — engine-level numbers say the Ghostty build parses plaintext
> ~1.9× faster than the alacritty one and full-screen clears far faster;
> the table's honest move is to re-run scripts/bench on both the flip
> build and the competitors the same day, not to carry this machine's
> numbers over.

*Rollout checklist — what flipping the default needs* (research only, none
of it done in 6a):

1. **Per-target archives in CI.** Neither ci.yml nor release.yml knows
   `vt-ghostty` today. A flip needs a job per target building the archive
   from the pinned tag with the pinned Zig — `zig build -Demit-lib-vt
   -Doptimize=ReleaseFast -Dcpu=baseline` — on each runner: Linux
   (x86_64 + aarch64, `libghostty-vt.a`), macOS (the xcframework/universal
   shape), and Windows (`ghostty-vt-static.lib`; build on the windows
   runner, where Zig auto-detects MSVC — cross-building the msvc archive
   from Linux dies in the C dependencies wanting Windows-SDK headers, per
   Phase 5's investigation; the gnu target cross-builds fine from Linux
   but tty59 ships msvc). Archives publish with SHA-256s and jobs export
   `TTY59_GHOSTTY_DIR` at the published prefix.
2. **Contributors without Zig.** `build.rs` never fetches (deliberate — a
   build must not download native code). The contributor path is a fetch
   script: download the CI-published archive for the host triple, verify
   the SHA-256, unpack into a gitignored prefix, export
   `TTY59_GHOSTTY_DIR`. `mise run vt:build` stays the from-source path
   (Zig via mise, no network beyond the tag clone).
3. **The hidden fallback setting.** Today the engine is a compile-time
   choice; the default build links no engine-switching code at all. A
   runtime fallback means both backends compiled together (they already
   coexist under `vt-ghostty`) behind an enum that implements the seam —
   the two facades expose ~22 inherent methods each plus the engine.rs
   traits, so the enum is ~20 thin match arms once, mostly mechanical;
   the real costs are elsewhere: binary size (both engines + the archive),
   `text_full`'s engine-specific reconciliation living per-arm, and every
   future engine method growing two arms + a test. A cheaper fallback:
   keep the compile-time choice and ship the alacritty build as the
   fallback binary — a setting that re-execs into it, not one that links
   both. Decide at flip time; the enum estimate is "a day of mechanical
   work, permanent per-method tax".
4. **glibc ≥ 2.36 vs the AppImage floor.** The archive references
   `arc4random_buf` (glibc ≥ 2.36; Ubuntu 24.04 = 2.39 ✓, 22.04 = 2.35 ✗).
   The AppImage's glibc floor is the release runner's Ubuntu, so
   `ubuntu-latest` drifting below 2.36 is impossible today but the floor
   is implicit: the flip should assert it (`ldd --version` gate, or link
   check for `arc4random_buf` in CI) so a runner image change can't
   silently ship an AppImage that won't load on the floor it claims.
   Alternative if the floor must drop lower: ask upstream (Ghostty's own
   Linux baseline), or `--export-dynamic-symbol`-style shimming is *not*
   available for a static archive — it would need a source patch.

### Phase 7: Re-pin to libghostty 1.0 (≈1 week, when tagged)

- [ ] Expected before March 2027, with one more ABI break. Regenerate
      `tty59-vt-sys`, fix `tty59-vt`, rebuild the archives, and run the
      differential harness against the previous pin.
- [ ] From then on, pin only tagged libghostty releases.

### Phase 8: Optional follow-ups

- Ghostty `key.h`/`mouse.h` encoders in place of `input.rs`'s hand-rolled
  encoding.
- Native Kitty graphics in Ghostty, retiring the daemon's APC lifting.
- `formatter.h` for `tty59 capture` (plain, VT and HTML output).

## Risks

| Risk | Mitigation |
|---|---|
| C API breaks again before 1.0 | Pin the exact tag and archive SHA-256. All FFI lives in `tty59-vt`. Phase 7 is budgeted. |
| ConPTY resize parity (#415) | Closed in Phase 5 via the engine's own `RESIZE_PULL_SCROLLBACK` (no patch carried). Live-PSReadLine gate stays a Windows manual check. |
| Throughput below today (README claims ~2×) | Measured in Phase 0 and Phase 6. Dirty-row snapshots. |
| `Send` soundness | Phase 0 audit. One mutex. `!Sync` kept. |
| `view.rs` merge churn | Small, mechanical seam PRs. Behavior changes only in Phase 3+. |
| Zig/toolchain drift | Prebuilt, checksummed archives. Contributors never need Zig. |

## Estimate

Phase 0: 1 wk · 1: 1–2 wk · 2: 3–4 wk · 3: 2 wk · 4: 2 wk · 5: 1 wk ·
6: 1–2 wk → **≈11–14 weeks**, plus about a week for Phase 7 when libghostty
1.0 lands.

**First PR:** the Phase 0 build spike and benchmark, together with the
`tty59-cli` `screen.rs` port behind `vt-ghostty`, the smallest consumer, which
proves the toolchain end to end.

## References

- Ghostty 1.4.0 release notes: <https://ghostty.org/docs/install/release-notes/1-4-0>
- libghostty API docs (tracks `main`): <https://libghostty.tip.ghostty.org>
- C headers: <https://github.com/ghostty-org/ghostty/tree/main/include/ghostty/vt>
- Prior art, Paneflow's migration: <https://paneflow.dev/blog/libghostty-linux>
- Rust bindings, for reference: <https://github.com/uzaaft/libghostty-rs>, <https://docs.rs/libghostty-vt>
