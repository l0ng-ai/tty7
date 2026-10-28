# Migrating the VT core from `alacritty_terminal` to libghostty-vt

Status: plan · Target: Ghostty **v1.4.0** (released 2026-10-27) · Owner: TBD

## Scope

tty7 renders on gpui and keeps doing so. What changes is the **VT parser,
grid and scrollback**: today `alacritty_terminal` (our fork, pinned in the
workspace `Cargo.toml`); after this, **libghostty-vt**, the embeddable,
renderer-free half of Ghostty. Ghostty's full app-embedding `libghostty` is out
of scope: it would replace gpui rather than the engine.

The daemon is unaffected. It parses nothing (it is a byte pipe with its own
mode sniffer in `tty7-core/src/core/term_modes.rs`), so `tty7-core` and the
static `tty7-server` gain no new native dependency.

### Who parses VT today

| Consumer | Where | Size | Use |
|---|---|---|---|
| GUI | `src/terminal/remote.rs` + `view.rs`, `element.rs`, `search.rs`, `smart_select.rs`, `prompt_reflow.rs`, `view/copy_unwrap.rs`, `input.rs`, `composer.rs`, … | ~45k lines touching it | the live, rendered mirror |
| Mobile gateway | `crates/tty7-gateway/src/mirror.rs` | 356 | screen snapshots for phones |
| CLI | `crates/tty7-cli/src/screen.rs` | 257 | `tty7 capture --plain` |

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
   - `crates/tty7-vt-sys`: bindgen output from `include/ghostty/vt.h` at
     `v1.4.0`, checked in, plus a freshness check in CI.
   - `crates/tty7-vt`: our safe wrapper. It owns the handle, asserts `Send`
     (never `Sync`) and is only ever reached through our mutex. Callbacks are
     wrapped in `catch_unwind`.
   - Upstream bindings stay a reference to borrow ideas and tests from, not a
     dependency.
2. **No Zig for contributors.** CI builds `libghostty-vt.a`/`.lib` per target
   from the pinned tag with Zig 0.17 (`ReleaseFast`, baseline CPU), and
   publishes them with SHA-256s. `build.rs` fetches nothing: it links the
   checked archive, or a local build via `TTY7_GHOSTTY_DIR` for hacking on
   Ghostty itself. Release packaging rejects any binary that links a shared
   `libghostty`.
3. **Seam first, swap second.** Every consumer moves onto tty7-owned types
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
   case-sensitive queries, which `search.h` lacks, run through tty7's matcher
   over formatter text, anchored with `grid_ref_tracked` so the matches
   survive pruning and reflow.
7. **tty7's OSC tokenizers stay authoritative** for 133, 7, 0/2, 9/777 and 52
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
- [ ] Link it into `tty7-app` on each, next to gpui. Check for symbol clashes
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
- [ ] ConPTY resize prototype (see Phase 5, fork patch #3) on Windows with
      PSReadLine.
- **Exit:** builds and links everywhere, throughput within ~5%, a written gap
  list, and the ConPTY approach chosen.

#### Phase 0 results: Linux x86_64, 2026-10-28

Measured with `cargo test --release -p tty7-cli --features vt-ghostty -- --ignored --nocapture`
(`crates/tty7-cli/src/bench_vt.rs`): 11.5 MB per workload, 64 KiB chunks,
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
- A tab is stored as pen movement, not a literal `\t`. `tty7 capture --plain`
  output differs in spacing, never content. Decide in Phase 4 whether to
  reconstruct tabs.
- Intended difference: grapheme clustering (mode 2027) is on by default, so
  DECRQM 2027 now reports it as set.

### Phase 1: `tty7-vt-sys` + `tty7-vt` (1–2 weeks)

- [ ] `tty7-vt-sys`: bindgen from the pinned header, `build.rs` links the
      checked archive, plus a CI freshness check.
- [ ] `tty7-vt::Terminal`: `new`, `write`, `resize(cols, rows, cell_px)`,
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

Add `src/terminal/vt/` (shared with the gateway and CLI through `tty7-vt`)
holding tty7's own `GridPoint { line: i32, col: u16 }`, `Side`, `CellFlags`,
`Modes` (bitflags), `CursorShape`, `Rgb`, `SelectionSpan`, `Snapshot`, and a
`VtTerm` facade whose first backend is alacritty.

Port in order of size, one PR each:

- [ ] `crates/tty7-cli/src/screen.rs`
- [ ] `crates/tty7-gateway/src/mirror.rs`
- [ ] `palette.rs`, `color_scheme.rs`, `size.rs`, `command_cursor.rs`
- [ ] `prompt_reflow.rs`, `parked_cursor.rs`, `view/copy_unwrap.rs`
- [ ] `smart_select.rs` (word, URL and path patterns run over snapshot text,
      not grid internals)
- [ ] `search.rs`: tty7's own regex over row text with a line↔offset map,
      replacing `RegexSearch`. Use Ghostty's `search.h` instead if Phase 0
      shows it fits.
- [ ] `element.rs` (render reads a `Snapshot`, never the live grid)
- [ ] `remote.rs` (reader loop, `EventProxy`)
- [ ] `view.rs`, split by area: selection → scroll and display offset → modes
      → hit-testing and hyperlinks → the rest
- **Exit:** `rg alacritty_terminal` matches only
  `src/terminal/vt/alacritty.rs` and the backend's `Cargo.toml` lines.

### Phase 3: Ghostty backend behind `vt-ghostty` (2 weeks)

- [ ] Implement `VtTerm` over `tty7-vt`.
- [ ] Switch consumers in the same order: CLI → gateway → GUI.
- [ ] **Differential harness**: identical byte streams into both engines,
      split at random chunk sizes, comparing normalized snapshots, cursor,
      modes and ordered events. Corpus: daemon replay rings, `scripts/bench`,
      vim/nvim, htop, tmux-in-pane, Claude Code/Codex sessions, PSReadLine.
      Known, intended differences (see Phase 5) are allow-listed explicitly.
- [ ] Fuzz: garbage input, resizes mid-stream, splits mid-sequence, very deep
      push stacks.

### Phase 4: Behavior parity (2 weeks)

- [ ] **Replay suppression.** `EventProxy` drops `PtyWrite`, `Bell`,
      `ClipboardStore/Load`, `ColorRequest` and `TextAreaSizeRequest` while a
      replay runs. The callbacks honor the same flag.
- [ ] **Identity replies** must match what the daemon answers while detached
      (`daemon/pane.rs::detached_reply`, currently VT102 `ESC[?6c`). Either set
      the `DEVICE_ATTRIBUTES`/`XTVERSION` callbacks to the same answer, or
      update `detached_reply` to Ghostty's, but never let the two disagree.
- [ ] Selection semantics: simple, word (with `word_separators` as
      `semantic_escape_chars` today), line, block, and selections across
      scrollback pruning.
- [ ] Viewport: display offset, scroll anchoring while output streams, the
      scrollbar (`scrollbar.rs`), hibernated tabs.
- [ ] Hyperlinks: OSC 8 ids per cell for hover and underline
      (`element/osc8_underline.rs`).
- [ ] Image anchors and prompt marks survive scrollback pruning and reflow,
      using `grid_ref_tracked`.
- [ ] Column widths: the input bar (`terminal/view`, today `unicode-width`)
      measures with Ghostty's `unicode.h`, so bar and grid cannot drift.

### Phase 5: Carrying the four alacritty-fork patches forward (1 week, plus the Phase 0 prototype)

| # | Fork patch | Plan on Ghostty |
|---|---|---|
| 1 | `push_keyboard_mode` truncated the title stack (panic after 4097 pushes) | Not an issue in Ghostty. Port guard test `deep_keyboard_mode_pushes_leave_the_reader_alive`. Make `term_modes.rs`'s stack cap match Ghostty's, which is now the reference the daemon mirrors. |
| 2 | VS16 emoji reserves 2 columns (#203) | Fixed upstream in 1.4.0 (VS15/VS16 apply to the preceding codepoint). Port `emoji_presentation_sequences_reserve_two_columns`. |
| 3 | `Config::conpty_resize`: resize the primary grid the way conhost re-anchors (#415) | **Highest risk.** 1.4.0 preserves the cursor across reflow, and `RESIZE_PULL_SCROLLBACK` exists, but neither is conhost's anchoring. Options, in order: (a) disable reflow and pull-back for ConPTY panes and re-anchor in `tty7-vt`; (b) carry a small patch in our built archive; (c) upstream an option. Gate: PSReadLine prompt redraws land on the right rows after grow and shrink. |
| 4 | `Term::reset_keyboard_modes` when the prompt returns (#1155) | Look for a mode/option call that clears both screens' Kitty stacks. Otherwise patch as in #3, and propose upstream. Port `a_prompt_forgets_kitty_flags_a_killed_program_left_on_either_screen`. |

### Phase 6: Performance, rollout, deletion (1–2 weeks)

- [ ] Profile the reader loop and snapshot path. Thin LTO no longer inlines
      across the engine boundary, so batch `vt_write` calls and copy only
      dirty rows.
- [ ] Call `ghostty_terminal_compress` on idle and hibernated panes.
- [ ] Re-run `scripts/bench`, and update the README benchmark numbers honestly.
- [ ] Release N: `vt-ghostty` on by default, with a hidden fallback setting.
- [ ] Release N+1: delete the alacritty backend, the `l0ng-ai/alacritty` fork
      pin and its `[profile.dev.package]` entry.
- [ ] Docs: README ("VT core from Alacritty" → Ghostty) in both languages,
      `docs/terminal/*`, and third-party notices (Ghostty is MIT) in the
      DMG, installer and AppImage.

### Phase 7: Re-pin to libghostty 1.0 (≈1 week, when tagged)

- [ ] Expected before March 2027, with one more ABI break. Regenerate
      `tty7-vt-sys`, fix `tty7-vt`, rebuild the archives, and run the
      differential harness against the previous pin.
- [ ] From then on, pin only tagged libghostty releases.

### Phase 8: Optional follow-ups

- Ghostty `key.h`/`mouse.h` encoders in place of `input.rs`'s hand-rolled
  encoding.
- Native Kitty graphics in Ghostty, retiring the daemon's APC lifting.
- `formatter.h` for `tty7 capture` (plain, VT and HTML output).

## Risks

| Risk | Mitigation |
|---|---|
| C API breaks again before 1.0 | Pin the exact tag and archive SHA-256. All FFI lives in `tty7-vt`. Phase 7 is budgeted. |
| ConPTY resize parity (#415) | Prototyped in Phase 0. Patch-carrying fallback. |
| Throughput below today (README claims ~2×) | Measured in Phase 0 and Phase 6. Dirty-row snapshots. |
| `Send` soundness | Phase 0 audit. One mutex. `!Sync` kept. |
| `view.rs` merge churn | Small, mechanical seam PRs. Behavior changes only in Phase 3+. |
| Zig/toolchain drift | Prebuilt, checksummed archives. Contributors never need Zig. |

## Estimate

Phase 0: 1 wk · 1: 1–2 wk · 2: 3–4 wk · 3: 2 wk · 4: 2 wk · 5: 1 wk ·
6: 1–2 wk → **≈11–14 weeks**, plus about a week for Phase 7 when libghostty
1.0 lands.

**First PR:** the Phase 0 build spike and benchmark, together with the
`tty7-cli` `screen.rs` port behind `vt-ghostty`, the smallest consumer, which
proves the toolchain end to end.

## References

- Ghostty 1.4.0 release notes: <https://ghostty.org/docs/install/release-notes/1-4-0>
- libghostty API docs (tracks `main`): <https://libghostty.tip.ghostty.org>
- C headers: <https://github.com/ghostty-org/ghostty/tree/main/include/ghostty/vt>
- Prior art, Paneflow's migration: <https://paneflow.dev/blog/libghostty-linux>
- Rust bindings, for reference: <https://github.com/uzaaft/libghostty-rs>, <https://docs.rs/libghostty-vt>
