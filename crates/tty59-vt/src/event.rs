//! Effect events, translated to owned Rust data.
//!
//! One enum covers everything both engines emit. The synchronous shapes —
//! title changes, PTY writes, the bell — arrived with the Ghostty wrapper in
//! Phase 1; the request/reply shapes a live GUI pane additionally raises
//! (clipboard access, color queries, pixel-size queries, child exit) joined
//! in Phase 2e, when the engine's owner moved onto the seam and the
//! [`crate::alacritty`] backend began translating the engine being replaced
//! into these. Effects with request/reply callbacks of their own that
//! libghostty leaves unset still join here as the consumers that need them
//! arrive (Phase 4); unset means the engine silently ignores the query,
//! which is always safe.

use std::fmt;
use std::sync::Arc;

/// What a running program asked the terminal to do.
///
/// The owned payload is the rule: bytes the engine handed over as a borrowed
/// buffer or its own types arrive as plain Rust, so no engine detail escapes
/// a event and the consumer's match never names an engine crate.
///
/// `Eq` stops at the reply formatters: two closures are never `Eq`, only
/// pointer-equal, so equality of an event carrying one is `PartialEq`'s
/// pointer comparison — what a differential harness wants anyway.
#[derive(Clone)]
pub enum VtEvent {
    /// New terminal content is available — repaint.
    Wakeup,

    /// The window title changed (OSC 0 / OSC 2). Lossy-decoded, since
    /// programs may set arbitrary bytes.
    Title(String),

    /// Reset to the embedder's default window title.
    ResetTitle,

    /// Bytes the terminal wants written back to the PTY: replies to DA/DSR
    /// queries, mode reports, and the like. Owned, because the buffer they
    /// came from is only valid for the call's duration.
    PtyWrite(Vec<u8>),

    /// The engine asked for the pane to be shut down.
    Exit,

    /// The child process exited. The status is carried for whoever wants it;
    /// the GUI treats it exactly like [`Exit`](VtEvent::Exit).
    ChildExit(std::process::ExitStatus),

    /// Store a string in the system clipboard (OSC 52).
    ClipboardStore(ClipboardType, String),

    /// Write the clipboard back to the PTY. The attached function formats
    /// the clipboard text into the escape sequence the requesting program
    /// expects.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Send + Sync>),

    /// Write the RGB value of a color to the PTY (OSC 10/11/12 replies). The
    /// formatter takes the model's [`Rgb`](crate::model::Rgb).
    ColorRequest(usize, Arc<dyn Fn(crate::model::Rgb) -> String + Send + Sync>),

    /// Report the terminal's pixel size (`CSI 14 t`). The formatter takes
    /// the model's [`WindowSize`](crate::model::WindowSize).
    TextAreaSizeRequest(Arc<dyn Fn(crate::model::WindowSize) -> String + Send + Sync>),

    /// BEL (0x07).
    Bell,

    /// The working directory changed (OSC 7 / OSC 9 / OSC 1337). The raw
    /// stored value — typically a `file://` URI for OSC 7, a bare path
    /// otherwise — exactly as the engine keeps it; decoding the URI is the
    /// consumer's job.
    PwdChanged(String),

    /// The program posted a desktop notification (OSC 9 / OSC 777).
    DesktopNotification { title: String, body: String },

    /// The program reported progress (OSC 9;4). `progress` is `None` when
    /// the report carried no percentage.
    ProgressReport { state: ProgressState, progress: Option<u8> },
}

/// Hand-written, as the engine being replaced's own event Debug is: a
/// closure cannot be printed, so the reply formatters print by name and
/// everything else by value.
impl fmt::Debug for VtEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtEvent::Wakeup => write!(f, "Wakeup"),
            VtEvent::Title(title) => write!(f, "Title({title:?})"),
            VtEvent::ResetTitle => write!(f, "ResetTitle"),
            VtEvent::PtyWrite(bytes) => write!(f, "PtyWrite({bytes:?})"),
            VtEvent::Exit => write!(f, "Exit"),
            VtEvent::ChildExit(status) => write!(f, "ChildExit({status:?})"),
            VtEvent::ClipboardStore(kind, text) => write!(f, "ClipboardStore({kind:?}, {text})"),
            VtEvent::ClipboardLoad(kind, _) => write!(f, "ClipboardLoad({kind:?})"),
            VtEvent::ColorRequest(index, _) => write!(f, "ColorRequest({index})"),
            VtEvent::TextAreaSizeRequest(_) => write!(f, "TextAreaSizeRequest"),
            VtEvent::Bell => write!(f, "Bell"),
            VtEvent::PwdChanged(pwd) => write!(f, "PwdChanged({pwd:?})"),
            VtEvent::DesktopNotification { title, body } => {
                write!(f, "DesktopNotification({title:?}, {body:?})")
            }
            VtEvent::ProgressReport { state, progress } => {
                write!(f, "ProgressReport({state:?}, {progress:?})")
            }
        }
    }
}

/// Hand-written for the same reason: a closure is not `PartialEq`, so the
/// reply formatters compare by pointer — identical events are equal, and a
/// harness that wants to diff replies should read what the formatter
/// produces, not the formatter itself.
impl PartialEq for VtEvent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (VtEvent::Wakeup, VtEvent::Wakeup)
            | (VtEvent::ResetTitle, VtEvent::ResetTitle)
            | (VtEvent::Exit, VtEvent::Exit)
            | (VtEvent::Bell, VtEvent::Bell) => true,
            (VtEvent::Title(a), VtEvent::Title(b))
            | (VtEvent::PwdChanged(a), VtEvent::PwdChanged(b)) => a == b,
            (VtEvent::PtyWrite(a), VtEvent::PtyWrite(b)) => a == b,
            (VtEvent::ChildExit(a), VtEvent::ChildExit(b)) => a == b,
            (VtEvent::ClipboardStore(kind_a, a), VtEvent::ClipboardStore(kind_b, b)) => {
                kind_a == kind_b && a == b
            }
            (VtEvent::ClipboardLoad(kind_a, a), VtEvent::ClipboardLoad(kind_b, b)) => {
                kind_a == kind_b && Arc::ptr_eq(a, b)
            }
            (VtEvent::ColorRequest(index_a, a), VtEvent::ColorRequest(index_b, b)) => {
                index_a == index_b && Arc::ptr_eq(a, b)
            }
            (VtEvent::TextAreaSizeRequest(a), VtEvent::TextAreaSizeRequest(b)) => Arc::ptr_eq(a, b),
            (
                VtEvent::DesktopNotification { title: a, body: c },
                VtEvent::DesktopNotification { title: b, body: d },
            ) => a == b && c == d,
            (
                VtEvent::ProgressReport { state: a, progress: c },
                VtEvent::ProgressReport { state: b, progress: d },
            ) => a == b && c == d,
            _ => false,
        }
    }
}

/// Which clipboard an OSC 52 access means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipboardType {
    /// The system clipboard.
    Clipboard,
    /// The selection buffer (X11 PRIMARY and friends).
    Selection,
}

/// Lifecycle of an OSC 9;4 progress report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// Remove the progress indication.
    Remove,
    /// Set determinate progress.
    Set,
    /// The tracked operation failed.
    Error,
    /// Progress is indeterminate.
    Indeterminate,
    /// Progress is paused.
    Pause,
}

impl ProgressState {
    /// Maps the C enum by its generated constants, so a header renumber
    /// cannot silently mismatch.
    pub(crate) fn from_raw(raw: i32) -> Self {
        use tty59_vt_sys as sys;
        match raw {
            x if x == sys::GhosttyTerminalProgressState_GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE => {
                Self::Remove
            }
            x if x == sys::GhosttyTerminalProgressState_GHOSTTY_TERMINAL_PROGRESS_STATE_SET => {
                Self::Set
            }
            x if x == sys::GhosttyTerminalProgressState_GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => {
                Self::Error
            }
            x if x
                == sys::GhosttyTerminalProgressState_GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE =>
            {
                Self::Indeterminate
            }
            x if x == sys::GhosttyTerminalProgressState_GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE => {
                Self::Pause
            }
            // A state this build doesn't know; the header reserves room for
            // new values and tells callbacks to ignore what they don't
            // recognize. There is nothing to carry that meaning with, so
            // this arm is unreachable for the pinned tag.
            _ => Self::Set,
        }
    }
}

/// Receives [`VtEvent`]s while a write runs.
///
/// Every method has a default no-op, so implementors declare only what they
/// care about. The sink is `&mut` borrowed for the whole `write_with` call:
/// the events of one write arrive in order, on the same thread, and a sink
/// that panics only loses that event (the panic is caught at the boundary).
pub trait EventSink {
    fn event(&mut self, event: VtEvent);
}

/// `()` is the no-op sink — `write` uses it, since effects only fire for
/// embedders that asked to observe them.
impl EventSink for () {
    fn event(&mut self, _event: VtEvent) {}
}

/// Collect every event into a vector, for tests and differential harnesses.
#[derive(Default)]
pub struct EventLog {
    pub events: Vec<VtEvent>,
}

impl EventSink for EventLog {
    fn event(&mut self, event: VtEvent) {
        self.events.push(event);
    }
}

/// Receives [`VtEvent`]s the moment the engine emits them.
///
/// This is the `&self`-shaped counterpart of [`EventSink`]: an engine fires
/// events from inside its own parsing methods, where only a shared borrow of
/// its listener exists, so the method an engine-owned listener implements
/// cannot be `&mut`. The GUI's reader proxy implements it (forwarding into
/// the channel the view drains); the backends bridge it onto whatever
/// listener protocol their engine has.
pub trait VtEventListener {
    fn send_event(&self, event: VtEvent);
}

/// `()` is the no-op listener — the owning facade's default, for consumers
/// that create a terminal to parse with and never observe effects.
impl VtEventListener for () {
    fn send_event(&self, _event: VtEvent) {}
}
