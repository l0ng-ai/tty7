//! The terminal handle.

use std::ffi::c_void;

use tty7_vt_sys as sys;

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
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION as OPT_DESKTOP_NOTIFICATION;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_MODE as OPT_MODE;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT as OPT_PROGRESS_REPORT;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PWD_CHANGED as OPT_PWD_CHANGED;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES as OPT_SCROLLBACK_MAX_LINES;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_TITLE_CHANGED as OPT_TITLE_CHANGED;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA as OPT_USERDATA;
use sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY as OPT_WRITE_PTY;

/// An error from libghostty-vt, with its raw result code attached for
/// matching on. The codes are negative `c_int`s from `tty7-vt-sys`
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
pub struct Mode(u16);

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
    unsafe fn set_raw(
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

#[cfg(test)]
impl Terminal {
    /// Test-only: the raw handle, for probing C API details the wrapper
    /// does not model (the default scrollback byte limit, for one).
    pub(crate) fn raw(&self) -> sys::GhosttyTerminal {
        self.handle
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
