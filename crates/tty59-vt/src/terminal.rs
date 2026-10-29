//! The terminal handle.

use std::ffi::c_void;

use tty59_vt_sys as sys;

use crate::event::EventSink;
use crate::model::{Cursor, Screen, Snapshot};
use crate::snapshot;

// The generated names are C-shaped on purpose (a -sys crate mirrors its
// header); importing the handful this file uses under the option/data enum's
// own short names keeps the FFI calls below readable.
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN as DATA_ACTIVE_SCREEN;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS as DATA_COLS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP as DATA_CURSOR_PENDING_WRAP;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE as DATA_CURSOR_VISIBLE;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_X as DATA_CURSOR_X;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_Y as DATA_CURSOR_Y;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MODE as DATA_MODE;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_PWD as DATA_PWD;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ROWS as DATA_ROWS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TITLE as DATA_TITLE;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS as DATA_TOTAL_ROWS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE as DATA_VIEWPORT_ACTIVE;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_BELL as OPT_BELL;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ as OPT_CLIPBOARD_READ;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE as OPT_CLIPBOARD_WRITE;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES as OPT_DEVICE_ATTRIBUTES;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SIZE as OPT_SIZE;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_XTVERSION as OPT_XTVERSION;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION as OPT_DESKTOP_NOTIFICATION;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_MODE as OPT_MODE;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT as OPT_PROGRESS_REPORT;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PWD_CHANGED as OPT_PWD_CHANGED;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES as OPT_SCROLLBACK_MAX_LINES;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_TITLE_CHANGED as OPT_TITLE_CHANGED;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA as OPT_USERDATA;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY as OPT_WRITE_PTY;

/// An error from libghostty-vt, with its raw result code attached for
/// matching on. The codes are negative `c_int`s from `tty59-vt-sys`
/// (`GHOSTTY_OUT_OF_MEMORY` and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    code: sys::GhosttyResult,
}

impl Error {
    pub(crate) fn new(code: sys::GhosttyResult) -> Self {
        Self { code }
    }

    /// The raw `GhosttyResult` code.
    pub fn code(&self) -> sys::GhosttyResult {
        self.code
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.code {
            x if x == sys::GhosttyResult_GHOSTTY_OUT_OF_MEMORY => "out of memory",
            x if x == sys::GhosttyResult_GHOSTTY_INVALID_VALUE => "invalid value",
            x if x == sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE => "out of space",
            x if x == sys::GhosttyResult_GHOSTTY_NO_VALUE => "no value",
            x if x == sys::GhosttyResult_GHOSTTY_IO_ERROR => "I/O error",
            x if x == sys::GhosttyResult_GHOSTTY_LIMIT_EXCEEDED => "limit exceeded",
            x if x == sys::GhosttyResult_GHOSTTY_REJECTED => "rejected",
            _ => "unknown error",
        };
        write!(f, "libghostty-vt: {name} ({})", self.code)
    }
}

impl std::error::Error for Error {}

/// Where `Terminal::scroll` moves the viewport to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTarget {
    /// The top of the scrollback.
    Top,
    /// The bottom — the live, active area.
    Bottom,
    /// Rows relative to the viewport; negative is up.
    DeltaRows(isize),
    /// Absolute row from the top of the scrollable area; the row becomes
    /// the viewport's first visible row.
    RowOffset(usize),
    /// Prompts relative to the viewport (needs OSC 133 marks).
    DeltaPrompts(isize),
}

/// A packed ANSI/DEC terminal mode, as `ghostty_terminal_set`/`get` take it:
/// bits 0–14 the mode number, bit 15 the ANSI flag. libghostty packs these
/// with a `static inline` helper the header defines, which bindgen cannot
/// evaluate — so the same two-instruction packing lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode(pub(crate) u16);

impl Mode {
    /// A DEC private mode (`CSI ? N h`/`l`).
    pub const fn dec(value: u16) -> Self {
        Self(value & 0x7FFF)
    }

    /// An ANSI mode (`CSI N h`/`l`).
    pub const fn ansi(value: u16) -> Self {
        Self((value & 0x7FFF) | (1 << 15))
    }

    /// Auto-wrap (DECAWM, `CSI ? 7 h`).
    pub const WRAPAROUND: Self = Self::dec(7);
    /// Bracketed paste (`CSI ? 2004 h`).
    pub const BRACKETED_PASTE: Self = Self::dec(2004);
    /// Alternate screen (`CSI ? 1047 h`).
    pub const ALT_SCREEN: Self = Self::dec(1047);
    /// Alternate screen + save cursor + clear (`CSI ? 1049 h`).
    pub const ALT_SCREEN_SAVE: Self = Self::dec(1049);
    /// Synchronized output (`CSI ? 2026 h`).
    pub const SYNC_OUTPUT: Self = Self::dec(2026);
}

/// The effect-delivery side shared with the C callbacks.
///
/// The C API is callback-shaped: one `userdata` pointer for everything,
/// passed to every registered trampoline. This is what it points at — boxed
/// once at construction so the address is stable. `sink` is armed only for
/// the duration of a `write_with`/`reset`/`resize` call, and only read on
/// that same call stack, so the raw pointer never outlives the `&mut`
/// borrow that produced it.
struct Shared {
    /// The caller's sink for the duration of a call that can fire effects;
    /// `None` outside one. An `Option`, because a null *fat* pointer has no
    /// `null_mut()` spelling in today's std.
    sink: Option<*mut dyn EventSink>,
    /// Set when a sink panic was caught; a one-way flag, because after a
    /// caught panic the sink's state is whatever the panic left behind.
    panicked: bool,
    /// While a synchronized-output hold (DEC mode 2026) is open, when it
    /// started plus the flush deadline. Written by the render-hold
    /// trampoline inside `vt_write`, read by [`Terminal::sync_deadline`] —
    /// both on the writer's thread or behind the caller's terminal lock, so
    /// plain mutation needs no synchronization.
    hold_until: Option<std::time::Instant>,
    /// The cell pixel size the last resize carried, for the XTWINOPS
    /// callback to report. Lives here (not on `Terminal`) because the
    /// trampolines only ever see `Shared`.
    cell_px: (u32, u32),
}

impl Shared {
    unsafe fn deliver(&mut self, event: crate::event::VtEvent) {
        // SAFETY: armed by `arm` for the duration of the call that borrowed
        // the sink, and this runs inside that call, on the same thread.
        if let Some(sink) = self.sink {
            let sink = unsafe { &mut *sink };
            // A Rust panic unwinding through the C frames below (Zig, in
            // libghostty) is undefined behavior, so the boundary catches it.
            // The sink is left logically inconsistent — that is what
            // `panicked` reports — but the terminal keeps parsing.
            let caught =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.event(event)));
            if caught.is_err() {
                self.panicked = true;
            }
        }
    }
}

/// A libghostty-vt terminal: parser, grid, scrollback, modes.
///
/// All methods take `&mut self` (or `&self` for pure reads) — the handle is
/// moved between threads, never shared. See the crate docs for the
/// threading contract.
pub struct Terminal {
    handle: sys::GhosttyTerminal,
    shared: Box<Shared>,
}

// SAFETY: libghostty-vt holds no thread-local or thread-affine state — its
// callbacks run on the `vt_write` caller's thread and its docs require the
// caller to serialize all access, which every method here enforces through
// `&mut self`/`&self`. `Terminal` owns the handle exclusively and the
// `Shared` box it hands to C contains no resources of its own, so moving
// the whole `Terminal` to another thread is as safe as calling it there.
// `Sync` stays unimplemented on purpose: sharing `&Terminal` across threads
// would need interior locking that belongs to the consumer (the GUI's
// `Arc<FairMutex<...>>`), not this crate.
unsafe impl Send for Terminal {}

impl Terminal {
    /// A terminal of `cols`×`rows` cells keeping up to `scrollback_lines`
    /// physical lines of history.
    ///
    /// Zero dimensions are rejected with [`Error`] (code
    /// `GHOSTTY_INVALID_VALUE`) — the wrapper does not guess a size.
    ///
    /// The line limit is an estimate on libghostty's side — it prunes at
    /// page granularity, so a bit *more* history survives than asked for,
    /// never less (see `GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES`).
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Result<Self, Error> {
        let mut handle: sys::GhosttyTerminal = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer; zero dimensions are the
        // one precondition violation, and the C call reports them as
        // `GHOSTTY_INVALID_VALUE`, mapped to `Err` right below.
        let r = unsafe { sys::ghostty_terminal_new(std::ptr::null(), &mut handle, cols, rows) };
        if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return Err(Error::new(r));
        }
        let shared = Box::new(Shared {
            sink: None,
            panicked: false,
            hold_until: None,
            cell_px: (0, 0),
        });
        let mut term = Self { handle, shared };
        // SAFETY: every set below passes the documented value type for its
        // option, and the shared pointer stays valid for the terminal's
        // life (it lives in the Box the Terminal owns; Drop frees the
        // terminal first).
        unsafe {
            term.set_raw(
                OPT_USERDATA,
                &*term.shared as *const Shared as *const c_void,
            )?;
            term.set_raw(OPT_WRITE_PTY, trampoline_write_pty as *const c_void)?;
            term.set_raw(OPT_BELL, trampoline_bell as *const c_void)?;
            term.set_raw(OPT_TITLE_CHANGED, trampoline_title_changed as *const c_void)?;
            term.set_raw(OPT_PWD_CHANGED, trampoline_pwd_changed as *const c_void)?;
            term.set_raw(
                OPT_DESKTOP_NOTIFICATION,
                trampoline_desktop_notification as *const c_void,
            )?;
            term.set_raw(OPT_PROGRESS_REPORT, trampoline_progress_report as *const c_void)?;
            // The query/reply effects a live pane must answer so programs
            // don't hang (Phase 3): device attributes, XTVERSION, XTWINOPS
            // sizes, OSC 52 clipboard, and the synchronized-output hold the
            // reader parks on. The color-scheme query (CSI ? 996 n) is
            // deliberately left unset: tty59's own daemon-sniffed answer
            // owns that reply, and a second one from the engine would
            // double-answer. ENQ is left unset too — the engine being
            // replaced never answered it either.
            term.set_raw(
                OPT_DEVICE_ATTRIBUTES,
                trampoline_device_attributes as *const c_void,
            )?;
            term.set_raw(OPT_XTVERSION, trampoline_xtversion as *const c_void)?;
            term.set_raw(OPT_SIZE, trampoline_size as *const c_void)?;
            term.set_raw(
                OPT_CLIPBOARD_WRITE,
                trampoline_clipboard_write as *const c_void,
            )?;
            term.set_raw(OPT_CLIPBOARD_READ, trampoline_clipboard_read as *const c_void)?;
            term.set_raw(
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_RENDER_HOLD,
                trampoline_render_hold as *const c_void,
            )?;
            term.set_raw(OPT_SCROLLBACK_MAX_LINES, &scrollback_lines as *const usize as *const c_void)?;
            // libghostty-vt ships a default scrollback *byte* cap of a mere
            // 10 KB, which prunes history long before any sane line limit
            // is reached (measured: a 4 MB plaintext stream left 466 rows
            // of a 10 000-line budget). The engine being replaced bounds
            // scrollback in lines only, so the byte cap is removed here and
            // `scrollback_lines` governs alone; an embedder that wants a
            // memory ceiling sets `GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES`
            // itself through a future options API.
            term.set_raw(
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                std::ptr::null(),
            )?;
            // Grapheme cluster mode (`CSI ? 2027 h`), on by default. Three
            // reasons, in order of weight:
            //
            // 1. Ghostty itself clusters by default in its own terminal —
            //    this is the behavior the engine was extracted from, and
            //    the embedder-visible mode only exists because libghostty
            //    starts headless-neutral.
            // 2. The engine being replaced has no 2027 at all: alacritty
            //    attaches combining marks to their base cell
            //    unconditionally, and our fork's VS16 patch (#2, fixed
            //    upstream by this very mode) widened emoji-presentation
            //    sequences by hand. Mode-off is the *divergent* setting.
            // 3. Off, a VS16 emoji takes one column and a mark takes a
            //    cell of its own — text that would misalign against every
            //    other terminal the user has.
            //
            // Intended difference to carry in the migration notes: with
            // this on, a program that queries DECRQM 2027 sees "set" where
            // the alacritty build did not recognize the mode at all.
            let grapheme_mode = sys::GhosttyTerminalModeConfig {
                mode: Mode::dec(2027).0,
                value: true,
            };
            term.set_raw(
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_MODE,
                &grapheme_mode as *const _ as *const c_void,
            )?;
        }
        Ok(term)
    }

    /// Set an option with a value pointer, mapping failure to [`Error`].
    ///
    /// # Safety
    /// `value` must point at (or be) the input type documented for `option`.
    pub(crate) unsafe fn set_raw(
        &mut self,
        option: sys::GhosttyTerminalOption,
        value: *const c_void,
    ) -> Result<(), Error> {
        let r = unsafe { sys::ghostty_terminal_set(self.handle, option, value) };
        if r == sys::GhosttyResult_GHOSTTY_SUCCESS {
            Ok(())
        } else {
            Err(Error::new(r))
        }
    }

    /// Feed PTY bytes through the parser, dropping any effects they cause.
    ///
    /// Parsing never fails: the input is untrusted by contract, and
    /// libghostty keeps the terminal consistent through arbitrary garbage —
    /// which is why `write` returns nothing.
    pub fn write(&mut self, data: &[u8]) {
        self.write_with(data, &mut ());
    }

    /// [`write`](Self::write), delivering the effects the bytes cause to
    /// `sink` as owned [`VtEvent`]s, in stream order.
    pub fn write_with(&mut self, data: &[u8], sink: &mut dyn EventSink) {
        self.arm(sink, |this| {
            // SAFETY: `data` is a valid read buffer for `len` bytes. The
            // call may re-enter through the trampolines, which only touch
            // `this.shared` — never re-entering `vt_write`.
            unsafe {
                sys::ghostty_terminal_vt_write(
                    this.handle,
                    data.as_ptr(),
                    data.len(),
                );
            }
        });
    }

    /// Full reset (RIS), as `ESC c` performs: modes, scrollback, scrolling
    /// region and screen contents. Dimensions are preserved.
    pub fn reset(&mut self) {
        // A held synchronized-output mode ends here, which fires the render
        // hold effect — so this goes through the sink-arming path too.
        self.arm(&mut (), |this| {
            // SAFETY: the handle is live; reset on it is always valid.
            unsafe { sys::ghostty_terminal_reset(this.handle) };
        });
    }

    /// Resize to `cols`×`rows` cells with the given cell pixel size. The
    /// pixel size feeds image protocols and XTWINOPS reports; `0, 0` is
    /// fine for headless use (the CLI's capture path).
    ///
    /// The primary screen reflows when wraparound is on; the alternate
    /// screen never does.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), Error> {
        self.shared.cell_px = (cell_width_px, cell_height_px);
        // A hold ends on resize (spec-allowed), same as reset.
        self.arm(&mut (), |this| {
            // SAFETY: live handle, positive dimensions; the four-arg form
            // is the only resize in the C API.
            let r = unsafe {
                sys::ghostty_terminal_resize(
                    this.handle,
                    cols,
                    rows,
                    cell_width_px,
                    cell_height_px,
                )
            };
            if r == sys::GhosttyResult_GHOSTTY_SUCCESS {
                Ok(())
            } else {
                Err(Error::new(r))
            }
        })
    }

    /// Move the viewport. Scrolling is view-only — it changes what a
    /// snapshot of the viewport shows, never the active area or the PTY.
    pub fn scroll(&mut self, target: ScrollTarget) {
        use sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_BOTTOM as TAG_BOTTOM;
        use sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA as TAG_DELTA;
        use sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT as TAG_DELTA_PROMPT;
        use sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_ROW as TAG_ROW;
        use sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_TOP as TAG_TOP;

        // The C side is a tagged union whose payload is one integer (delta,
        // absolute row, or prompt delta) padded to u64; each arm below sets
        // the field the docs pair with its tag.
        let value = |i: isize| sys::GhosttyTerminalScrollViewportValue {
            delta: i,
        };
        let behavior = match target {
            ScrollTarget::Top => sys::GhosttyTerminalScrollViewport {
                tag: TAG_TOP,
                value: value(0),
            },
            ScrollTarget::Bottom => sys::GhosttyTerminalScrollViewport {
                tag: TAG_BOTTOM,
                value: value(0),
            },
            ScrollTarget::DeltaRows(d) => sys::GhosttyTerminalScrollViewport {
                tag: TAG_DELTA,
                value: value(d),
            },
            ScrollTarget::RowOffset(row) => sys::GhosttyTerminalScrollViewport {
                tag: TAG_ROW,
                value: value(row as isize),
            },
            ScrollTarget::DeltaPrompts(d) => sys::GhosttyTerminalScrollViewport {
                tag: TAG_DELTA_PROMPT,
                value: value(d),
            },
        };
        // SAFETY: live handle, well-formed tagged union.
        unsafe { sys::ghostty_terminal_scroll_viewport(self.handle, behavior) };
    }

    /// The cursor, in viewport coordinates.
    pub fn cursor(&self) -> Cursor {
        let mut visible = false;
        let mut pending_wrap = false;
        let mut x: u16 = 0;
        let mut y: u16 = 0;
        let mut screen: sys::GhosttyTerminalScreen = 0;
        // SAFETY: every get below passes the output type its enum entry
        // documents, into valid memory.
        unsafe {
            sys::ghostty_terminal_get(self.handle, DATA_CURSOR_X, &mut x as *mut _ as *mut c_void);
            sys::ghostty_terminal_get(self.handle, DATA_CURSOR_Y, &mut y as *mut _ as *mut c_void);
            sys::ghostty_terminal_get(
                self.handle,
                DATA_CURSOR_VISIBLE,
                &mut visible as *mut _ as *mut c_void,
            );
            sys::ghostty_terminal_get(
                self.handle,
                DATA_CURSOR_PENDING_WRAP,
                &mut pending_wrap as *mut _ as *mut c_void,
            );
            sys::ghostty_terminal_get(
                self.handle,
                DATA_ACTIVE_SCREEN,
                &mut screen as *mut _ as *mut c_void,
            );
        }
        Cursor {
            x,
            y,
            visible,
            pending_wrap,
            screen: if screen
                == sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE
            {
                Screen::Alternate
            } else {
                Screen::Primary
            },
        }
    }

    /// Grid size in cells, `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.get_u16(DATA_COLS), self.get_u16(DATA_ROWS))
    }

    /// Total rows including scrollback, and how many of those are scrollback
    /// (everything above the viewport).
    pub fn scrollback(&self) -> (usize, usize) {
        let total = self.get_usize(DATA_TOTAL_ROWS);
        let scrollback = self.get_usize(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
        );
        (total, scrollback)
    }

    /// Whether the viewport follows the active area (as opposed to being
    /// scrolled into history).
    pub fn viewport_active(&self) -> bool {
        self.get_bool(DATA_VIEWPORT_ACTIVE)
    }

    /// The current value of a mode.
    pub fn mode(&self, mode: Mode) -> bool {
        let mut cfg = sys::GhosttyTerminalModeConfig {
            mode: mode.0,
            value: false,
        };
        // SAFETY: `DATA_MODE` documents `GhosttyTerminalModeConfig*` as its
        // in/out type, with `mode` initialized by the caller.
        unsafe {
            sys::ghostty_terminal_get(self.handle, DATA_MODE, &mut cfg as *mut _ as *mut c_void);
        }
        cfg.value
    }

    /// Set the current value of a mode, as `CSI h`/`l` would. This does not
    /// change the value a reset restores (see the C API's
    /// `GHOSTTY_TERMINAL_OPT_MODE_DEFAULT` for that).
    pub fn set_mode(&mut self, mode: Mode, value: bool) -> Result<(), Error> {
        let cfg = sys::GhosttyTerminalModeConfig { mode: mode.0, value };
        // SAFETY: `OPT_MODE` takes `GhosttyTerminalModeConfig*`.
        unsafe { self.set_raw(OPT_MODE, &cfg as *const _ as *const c_void) }
    }

    /// An owned snapshot of the viewport. See [`Snapshot`].
    pub fn snapshot(&self) -> Snapshot {
        snapshot::take(self.handle)
    }

    /// The whole grid — scrollback plus screen — as plain text, rows joined
    /// the way the text was written: a soft wrap continues its row, a real
    /// line break starts a new one. See `text` module docs for the exact
    /// rules, which match the alacritty path this replaces.
    pub fn text_full(&self) -> String {
        crate::text::full(self.handle)
    }

    /// Whether any sink panic has been caught since construction.
    pub fn callback_panicked(&self) -> bool {
        self.shared.panicked
    }

    /// How long a synchronized-output hold is allowed to run before the
    /// reader flushes it — the engine being replaced's own constant
    /// (`vte`'s `SYNC_UPDATE_TIMEOUT`), kept so the reader's pacing
    /// doesn't change with the engine.
    pub(crate) const SYNC_HOLD: std::time::Duration = std::time::Duration::from_millis(150);

    /// While a synchronized-output hold (DEC mode 2026) is open, the
    /// deadline after which the reader flushes it (see the render-hold
    /// callback). `None` when no hold is open.
    pub fn sync_deadline(&self) -> Option<std::time::Instant> {
        self.shared.hold_until
    }

    /// The cell pixel size the terminal last resized with — what XTWINOPS
    /// pixel reports and image protocols answer. Updated by [`Self::resize`]
    /// and [`Self::set_cell_px`]; `0, 0` until an embedder that knows its
    /// cells says otherwise (a headless capture path, for instance).
    pub(crate) fn cell_px(&self) -> (u32, u32) {
        self.shared.cell_px
    }

    /// Record the cell pixel size without resizing the grid — a display
    /// scale change that leaves the cell count alone still has to reach
    /// size reports.
    pub(crate) fn set_cell_px(&mut self, width: u32, height: u32) {
        self.shared.cell_px = (width, height);
    }

    /// The raw C handle, for the seam's backend (render state, search, the
    /// grid-ref reads) — never a public shape, and never valid past the
    /// next mutating call the owner makes through those reads.
    pub(crate) fn handle(&self) -> sys::GhosttyTerminal {
        self.handle
    }

    fn get_u16(&self, data: sys::GhosttyTerminalData) -> u16 {
        let mut v: u16 = 0;
        // SAFETY: `u16*` matches each data kind this helper is called with.
        unsafe { sys::ghostty_terminal_get(self.handle, data, &mut v as *mut _ as *mut c_void) };
        v
    }

    fn get_usize(&self, data: sys::GhosttyTerminalData) -> usize {
        let mut v: usize = 0;
        // SAFETY: `usize*` matches each data kind this helper is called with.
        unsafe { sys::ghostty_terminal_get(self.handle, data, &mut v as *mut _ as *mut c_void) };
        v
    }

    fn get_bool(&self, data: sys::GhosttyTerminalData) -> bool {
        let mut v = false;
        // SAFETY: `bool*` matches each data kind this helper is called with.
        unsafe { sys::ghostty_terminal_get(self.handle, data, &mut v as *mut _ as *mut c_void) };
        v
    }

    /// Run `f` with `sink` armed as the effect target.
    ///
    /// Effects fire only inside calls that can produce them (`vt_write`,
    /// `reset`, `resize`), always synchronously on this thread — so a
    /// plain set/restore around the call is the whole lifetime story for
    /// the raw pointer.
    fn arm<T>(&mut self, sink: &mut dyn EventSink, f: impl FnOnce(&mut Self) -> T) -> T {
        // The `Shared` field is typed `*mut (dyn EventSink + 'static)`, but
        // the sink only needs to live as long as `f`. Raw pointers are
        // invariant, so the shorter lifetime cannot coerce — it is erased
        // here instead. Sound because the stored pointer is dereferenced
        // only inside `f`, within the original borrow, and cleared before
        // `arm` returns; the 'static lie never escapes that window.
        let sink: *mut (dyn EventSink + 'static) =
            unsafe { std::mem::transmute::<*mut (dyn EventSink + '_), _>(sink) };
        self.shared.sink = Some(sink);
        let out = f(self);
        self.shared.sink = None;
        out
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: the handle is live and owned, and nothing else holds it
        // (the `Shared` box C points at is dropped right after, with no C
        // call in between that could dereference it).
        unsafe { sys::ghostty_terminal_free(self.handle) };
    }
}

// ---- The C trampolines -----------------------------------------------------
//
// Each translates one effect into a `Shared::deliver`. They only run inside
// `write_with`/`reset`/`resize`, on the same thread, and never call back
// into `vt_write` — the two things the C docs demand of an effect callback.

unsafe fn string_data(handle: sys::GhosttyTerminal, data: sys::GhosttyTerminalData) -> String {
    let mut s = sys::GhosttyString { ptr: std::ptr::null(), len: 0 };
    // SAFETY: `GhosttyString*` is the documented output type for TITLE and
    // PWD; the returned bytes are borrowed until the next mutating call,
    // and are copied out before this function returns.
    let r = unsafe { sys::ghostty_terminal_get(handle, data, &mut s as *mut _ as *mut c_void) };
    if r != sys::GhosttyResult_GHOSTTY_SUCCESS || s.ptr.is_null() {
        return String::new();
    }
    // SAFETY: the pointer and length come from the terminal, valid until
    // the next mutating call — which cannot happen before this returns
    // (we're inside one of its own callbacks).
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe extern "C" fn trampoline_write_pty(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if data.is_null() && len > 0 {
        return;
    }
    // SAFETY: `data`/`len` are a valid borrow for the call's duration;
    // copied out because the sink outlives it.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    // SAFETY: see fn docs.
    unsafe { shared.deliver(crate::event::VtEvent::PtyWrite(bytes)) };
}

unsafe extern "C" fn trampoline_bell(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    // SAFETY: see fn docs.
    unsafe { shared.deliver(crate::event::VtEvent::Bell) };
}

unsafe extern "C" fn trampoline_title_changed(
    terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    // The C callback carries no payload: the header's contract is to read
    // the new title from the terminal. `get` is a read, allowed from here.
    // SAFETY: `get` from an effect callback is a read, allowed here.
    let title = unsafe { string_data(terminal, DATA_TITLE) };
    // SAFETY: see fn docs.
    unsafe { shared.deliver(crate::event::VtEvent::Title(title)) };
}

unsafe extern "C" fn trampoline_pwd_changed(
    terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    // SAFETY: `get` from an effect callback is a read, allowed here.
    let pwd = unsafe { string_data(terminal, DATA_PWD) };
    // SAFETY: see fn docs.
    unsafe { shared.deliver(crate::event::VtEvent::PwdChanged(pwd)) };
}

unsafe extern "C" fn trampoline_desktop_notification(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const sys::GhosttyTerminalDesktopNotification,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    // SAFETY: `notification` points at a sized struct the terminal owns
    // for the duration of this call; both strings are copied out below.
    let n = unsafe { &*notification };
    let read = |s: &sys::GhosttyString| -> String {
        if s.ptr.is_null() {
            return String::new();
        }
        // SAFETY: valid for the callback's duration, per the struct docs.
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    let event = crate::event::VtEvent::DesktopNotification {
        title: read(&n.title),
        body: read(&n.body),
    };
    // SAFETY: see fn docs.
    unsafe { shared.deliver(event) };
}

unsafe extern "C" fn trampoline_progress_report(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const sys::GhosttyTerminalProgressReport,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    // SAFETY: `report` points at a sized struct the terminal owns for the
    // duration of this call.
    let r = unsafe { &*report };
    let event = crate::event::VtEvent::ProgressReport {
        state: crate::event::ProgressState::from_raw(r.state),
        progress: (r.progress >= 0).then_some(r.progress as u8),
    };
    // SAFETY: see fn docs.
    unsafe { shared.deliver(event) };
}

// ---- The query/reply trampolines (Phase 3) ---------------------------------
//
// These answer the identity and capability queries a live pane receives, so
// programs don't wait on a terminal that never speaks. All of them run inside
// `vt_write` on the writer's thread, deliver through `Shared`, and never
// re-enter it — the C contract every trampoline here follows.

/// The primary device attributes tty59 reports: a bare VT102 (`ESC[?6c`)
/// with no feature bits — the same answer the daemon gives a detached pane
/// (`daemon/pane.rs::detached_reply`) and the engine being replaced gave an
/// attached one, so a program sees one terminal whichever side answered.
/// Secondary attributes mirror the replaced engine's shape (device type 0,
/// ROM cartridge 1) with a zero firmware number; tertiary reports unit 0.
unsafe extern "C" fn trampoline_device_attributes(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    out_attrs: *mut sys::GhosttyDeviceAttributes,
) -> bool {
    // SAFETY: userdata is the `Shared` pointer installed at construction;
    // `out_attrs` points at the struct the terminal owns for this call.
    let _shared = unsafe { &mut *(userdata as *mut Shared) };
    let attrs = unsafe { &mut *out_attrs };
    attrs.primary = sys::GhosttyDeviceAttributesPrimary {
        conformance_level: 6,
        features: [0; 64],
        num_features: 0,
    };
    attrs.secondary = sys::GhosttyDeviceAttributesSecondary {
        device_type: 0,
        firmware_version: 0,
        rom_cartridge: 1,
    };
    attrs.tertiary = sys::GhosttyDeviceAttributesTertiary { unit_id: 0 };
    true
}

/// XTVERSION (`CSI > q`): the embedder's name and version. A compile-time
/// literal, so the `GhosttyString` borrows `'static` memory as the callback
/// contract requires.
unsafe extern "C" fn trampoline_xtversion(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
) -> sys::GhosttyString {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let _shared = unsafe { &mut *(userdata as *mut Shared) };
    const VERSION: &[u8] = concat!("tty59 ", env!("CARGO_PKG_VERSION")).as_bytes();
    sys::GhosttyString {
        ptr: VERSION.as_ptr(),
        len: VERSION.len(),
    }
}

/// XTWINOPS size queries and mode-2048 reports, answered from the
/// terminal's own geometry — the same numbers the GUI used to answer with,
/// because they are the ones its resize calls set.
unsafe extern "C" fn trampoline_size(
    terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    out_size: *mut sys::GhosttySizeReportSize,
) -> bool {
    // SAFETY: userdata is the `Shared` pointer installed at construction;
    // `get` from a callback is a read, allowed here.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    let (width, height) = shared.cell_px;
    let out = unsafe { &mut *out_size };
    let mut cols: u16 = 0;
    let mut rows: u16 = 0;
    // SAFETY: both gets pass the output type their enum entry documents.
    unsafe {
        sys::ghostty_terminal_get(terminal, DATA_COLS, &mut cols as *mut _ as *mut c_void);
        sys::ghostty_terminal_get(terminal, DATA_ROWS, &mut rows as *mut _ as *mut c_void);
    }
    *out = sys::GhosttySizeReportSize {
        rows,
        columns: cols,
        cell_width: width,
        cell_height: height,
    };
    true
}

/// A borrowed C string as an owned `String` (empty on null).
unsafe fn cstr(s: &sys::GhosttyString) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    // SAFETY: the bytes are valid for the callback's duration.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// The clipboard location an OSC 52 access means, in the model's spelling.
fn clipboard_kind_of(location: sys::GhosttyClipboardLocation) -> crate::event::ClipboardType {
    use sys::GhosttyClipboardLocation_GHOSTTY_CLIPBOARD_LOCATION_STANDARD as STANDARD;
    if location == STANDARD {
        crate::event::ClipboardType::Clipboard
    } else {
        crate::event::ClipboardType::Selection
    }
}

/// OSC 52 writes (and the clipboard half of OSC 1337/5522): handed to the
/// embedder as a store event, answered to the program as done. tty59's
/// permission posture is unchanged from the engine being replaced — an OSC
/// 52 store simply happened — so the reply is success, not a prompt.
unsafe extern "C" fn trampoline_clipboard_write(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const sys::GhosttyClipboardWrite,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction;
    // `write` points at a sized struct the terminal owns for the call.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    let request = unsafe { &*write };
    // The first text/plain representation is the value OSC 52 carries; the
    // MIME-aware protocols' non-text representations have no model shape
    // yet, so a write that carries none of them stores nothing.
    let mut text = None;
    for i in 0..request.contents_len {
        let entry = unsafe { &*request.contents.add(i) };
        // SAFETY: the entry's strings are borrowed for the callback, which
        // this loop runs inside.
        if unsafe { cstr(&entry.mime) } == "text/plain" {
            text = Some(unsafe { cstr(&entry.data) });
            break;
        }
    }
    // SAFETY: the reply function is the request's own, valid for the
    // callback's duration; calling it once is the documented contract.
    unsafe {
        let mut reply: sys::GhosttyClipboardWriteReply = std::mem::zeroed();
        reply.size = std::mem::size_of::<sys::GhosttyClipboardWriteReply>();
        reply.result =
            sys::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
        if let Some(reply_fn) = request.reply {
            reply_fn(request, &reply);
        }
    }
    // SAFETY: see fn docs.
    if let Some(text) = text {
        unsafe {
            shared.deliver(crate::event::VtEvent::ClipboardStore(
                clipboard_kind_of(request.location),
                text,
            ))
        };
    }
}

/// OSC 52 reads. The system clipboard lives on the UI thread, so the read
/// is surfaced as a load event the embedder answers asynchronously — the
/// same flow the engine being replaced had. The C callback wants a
/// synchronous answer this flow cannot give, so the program also receives
/// the engine's empty-clipboard reply; the embedder's real answer follows
/// it. (Known, allow-listed difference: two replies where the old engine
/// produced one.)
unsafe extern "C" fn trampoline_clipboard_read(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    read: *const sys::GhosttyClipboardRead,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction;
    // `read` points at a sized struct the terminal owns for the call.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    let request = unsafe { &*read };
    let kind = clipboard_kind_of(request.location);
    // SAFETY: see fn docs.
    unsafe {
        shared.deliver(crate::event::VtEvent::ClipboardLoad(
            kind,
            std::sync::Arc::new(move |text: &str| osc52_paste_reply(kind, text)),
        ))
    };
}

/// The OSC 52 answer a read arms: the clipboard text, base64'd, in the
/// selection the program asked for. Base64 by hand — the encoding is 20
/// lines and the crate picks up no dependency for it.
fn osc52_paste_reply(kind: crate::event::ClipboardType, text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    let selection = match kind {
        crate::event::ClipboardType::Clipboard => "c",
        crate::event::ClipboardType::Selection => "s",
    };
    format!("\x1b]52;{selection};{out}\x07")
}

/// Synchronized output (DEC mode 2026) begins and ends here: the deadline
/// the reader parks on opens with the hold and closes with it. The reader
/// also flushes early through `CSI ? 2026 l` when the deadline passes.
unsafe extern "C" fn trampoline_render_hold(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    held: bool,
) {
    // SAFETY: userdata is the `Shared` pointer installed at construction.
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    shared.hold_until = held.then(|| std::time::Instant::now() + Terminal::SYNC_HOLD);
}
