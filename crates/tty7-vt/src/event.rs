//! Effect callbacks, translated to owned Rust events.

/// What a running program asked the terminal to do, delivered through the
/// caller's [`EventSink`] while [`crate::Terminal::write_with`] runs.
///
/// This starts as the Phase 1 subset of libghostty-vt's effects — the ones
/// with a plain synchronous notification shape. Query-response effects
/// (device attributes, XTWINOPS sizes, color scheme, clipboard reads and
/// writes) have request/reply callbacks of their own and join here as the
/// consumers that need them arrive (Phase 4 of the migration); leaving them
/// unset makes libghostty silently ignore the query, which is always safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtEvent {
    /// Bytes the terminal wants written back to the PTY: replies to DA/DSR
    /// queries, mode reports, and the like. Owned, because the C buffer is
    /// only valid for the callback's duration.
    PtyWrite(Vec<u8>),

    /// BEL (0x07).
    Bell,

    /// The window title changed (OSC 0 / OSC 2). Read from the terminal at
    /// callback time; lossy-decoded, since programs may set arbitrary bytes.
    TitleChanged(String),

    /// The working directory changed (OSC 7 / OSC 9 / OSC 1337). The raw
    /// stored value — typically a `file://` URI for OSC 7, a bare path
    /// otherwise — exactly as libghostty keeps it; decoding the URI is the
    /// consumer's job.
    PwdChanged(String),

    /// The program posted a desktop notification (OSC 9 / OSC 777).
    DesktopNotification { title: String, body: String },

    /// The program reported progress (OSC 9;4). `progress` is `None` when
    /// the report carried no percentage.
    ProgressReport { state: ProgressState, progress: Option<u8> },
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
    pub(crate) fn from_raw(raw: i32) -> Self {
        use tty7_vt_sys as sys;
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
