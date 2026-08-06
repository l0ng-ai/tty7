//! Windows taskbar overlay badge: a status dot stamped on each window's
//! taskbar button, so a glance at the taskbar answers "is anything running /
//! finished / stuck waiting on me" while tty7 is in the background.
//!
//! Three states, sharing the in-window dot palette (`AgentStatus::dot_rgb`)
//! so a color never means two things:
//! - **amber** — a coding agent in this window is blocked on the user
//!   (same signal as the tray's attention badge);
//! - **blue** — a shell command or agent is still working;
//! - **green** — work finished while the window was unfocused; cleared the
//!   moment the window is activated (once you've seen it, it's done its job).
//!
//! Data flow copies `ui::tray`: a foreground task polls once a second,
//! aggregates each window's panes into an [`Overlay`], diffs against what the
//! taskbar currently shows, and only calls into COM on a change. The config
//! flag is re-read every tick, so the Settings toggle and a `config.json`
//! hot-reload both apply within a second (off clears any stamped badge).
//!
//! Windows-only by nature — `SetOverlayIcon` is a taskbar concept. macOS has
//! the Dock badge as a natural follow-up; on Linux there is no portable
//! equivalent and the tray badge already covers attention there.

use std::collections::HashMap;

use gpui::App;

use crate::core::cli_agent::AgentStatus;
use crate::core::config::Config;
use crate::ui::i18n::{L10nKey, t};

/// Same cadence as the tray poll: agent status reaches the views on a 300 ms
/// timer, so 1 s here keeps the taskbar a hair behind the in-window dots at
/// negligible cost.
const POLL: std::time::Duration = std::time::Duration::from_millis(1000);

/// What the badge shows, least to most urgent. Attention outranks Busy
/// outranks Done, matching the tray's `urgency` ordering — when panes
/// disagree, the one that needs the user wins the single overlay slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Overlay {
    #[default]
    None,
    /// Work finished while the window was unfocused (green).
    Done,
    /// A shell command or agent is still working (blue).
    Busy,
    /// An agent is blocked on the user (amber).
    Attention,
}

impl Overlay {
    /// Dot color, borrowed from the in-window status dots so the taskbar
    /// never invents a fourth meaning for a color.
    fn rgb(self) -> Option<u32> {
        match self {
            Overlay::None => None,
            Overlay::Done => AgentStatus::Done.dot_rgb(),
            Overlay::Busy => AgentStatus::Working.dot_rgb(),
            Overlay::Attention => AgentStatus::Waiting.dot_rgb(),
        }
    }

    /// Screen-reader text for the overlay (`SetOverlayIcon`'s accessibility
    /// description). Localized like every other user-visible string, and
    /// borrowed from the panel and tray so the badge reads the same as the
    /// status it mirrors.
    fn description(self) -> &'static str {
        match self {
            Overlay::None => "",
            Overlay::Done => t(L10nKey::PanelAgentDone),
            Overlay::Busy => t(L10nKey::PanelAgentWorking),
            Overlay::Attention => t(L10nKey::TrayAgentNeedsInput),
        }
    }
}

/// Pick the overlay for one window from its aggregated signals.
fn overlay_for(attention: bool, busy: bool, done_pending: bool) -> Overlay {
    if attention {
        Overlay::Attention
    } else if busy {
        Overlay::Busy
    } else if done_pending {
        Overlay::Done
    } else {
        Overlay::None
    }
}

/// Per-window memory for the "finished while unfocused" state: `Done` is an
/// *edge* (busy stopped while the user was elsewhere), not a level, so
/// someone has to remember the edge until the user looks.
#[derive(Default)]
struct Track {
    was_busy: bool,
    done_pending: bool,
}

impl Track {
    /// Advance one poll tick. Activation clears a pending Done (seen = done);
    /// a busy→idle transition while unfocused sets it. Ordering matters: an
    /// active window never *accumulates* a Done, even on the same tick the
    /// command finishes — the user is looking right at the result.
    fn advance(&mut self, busy: bool, active: bool) {
        if active {
            self.done_pending = false;
        } else if self.was_busy && !busy {
            self.done_pending = true;
        }
        self.was_busy = busy;
    }
}

/// Wire the overlay up: one app-wide poll task, same lifetime story as
/// `tray::init` (called once, for the first window; lives for the app).
pub(crate) fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        // COM object + what each taskbar button currently shows. Both live
        // here, on the foreground executor: `ITaskbarList3` is !Send and the
        // taskbar wants to be called from the UI thread anyway (gpui already
        // ran `OleInitialize` on it).
        let mut taskbar = Taskbar::default();
        let mut tracks: HashMap<isize, Track> = HashMap::new();
        let mut shown: HashMap<isize, Overlay> = HashMap::new();
        loop {
            cx.background_executor().timer(POLL).await;
            let wanted = cx.update(|cx| snapshot(&mut tracks, cx));
            for (hwnd, overlay) in &wanted {
                // Only remember a badge the taskbar actually took. A failed
                // stamp (Explorer restarting takes every overlay with it)
                // leaves the cache alone, so the next tick sees the same diff
                // and tries again instead of believing a badge it never drew.
                if shown.get(hwnd).copied().unwrap_or_default() != *overlay
                    && taskbar.stamp(*hwnd, *overlay)
                {
                    shown.insert(*hwnd, *overlay);
                }
            }
            // Windows that closed take their taskbar button (and overlay)
            // with them — just forget them.
            shown.retain(|hwnd, _| wanted.iter().any(|(h, _)| h == hwnd));
        }
    })
    .detach();
}

/// One poll tick: every open window's HWND and the overlay it should show.
/// With the setting off, every window maps to `Overlay::None` — going through
/// the normal diff is what *clears* badges stamped before the toggle flipped.
fn snapshot(tracks: &mut HashMap<isize, Track>, cx: &mut App) -> Vec<(isize, Overlay)> {
    use crate::ui::windows::WindowRegistry;

    let enabled = cx.global::<Config>().taskbar_status_icon;
    let mut wanted = Vec::new();
    for (workspace, weak) in WindowRegistry::open_windows(cx) {
        let Some(app) = weak.upgrade() else { continue };
        let Some(handle) = WindowRegistry::window_for(cx, workspace) else {
            continue;
        };
        let Ok((hwnd, active)) = handle.update(cx, |_, window, _| {
            (win32_hwnd(window), window.is_window_active())
        }) else {
            continue;
        };
        let Some(hwnd) = hwnd else { continue };
        let (attention, busy) = app.read(cx).taskbar_signals(cx);
        let track = tracks.entry(hwnd).or_default();
        track.advance(busy, active);
        let overlay = if enabled {
            overlay_for(attention, busy, track.done_pending)
        } else {
            Overlay::None
        };
        wanted.push((hwnd, overlay));
    }
    tracks.retain(|hwnd, _| wanted.iter().any(|(h, _)| h == hwnd));
    wanted
}

/// The window's Win32 handle, unwrapped from gpui's `HasWindowHandle`. `None`
/// never actually happens on the Windows backend, but degrading to "no badge"
/// beats unwrapping someone else's platform invariant.
fn win32_hwnd(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Fully qualified: gpui's inherent `Window::window_handle` (the
    // `AnyWindowHandle`) shadows the `HasWindowHandle` trait method.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// The COM half: owns the lazily-created `ITaskbarList3` and turns an
/// [`Overlay`] into a `SetOverlayIcon` call.
#[derive(Default)]
struct Taskbar {
    list: Option<windows::Win32::UI::Shell::ITaskbarList3>,
    /// Create-failure backoff, same shape as `tray::init`'s: Explorer may not
    /// be up yet when the first window opens, and it can restart later, so a
    /// failure is a cooldown rather than a life sentence.
    attempts: u32,
    cooldown: u32,
}

/// Give up on the overlay after this many failed creations, and wait this
/// many stamps between tries. Both copied from `ui::tray`.
const MAX_ATTEMPTS: u32 = 10;
const RETRY_EVERY: u32 = 30;

impl Taskbar {
    /// Stamp one window's badge. Returns whether the taskbar took it, so the
    /// caller knows not to cache a badge that was never drawn.
    fn stamp(&mut self, hwnd: isize, overlay: Overlay) -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
        use windows::core::PCWSTR;

        // Cloned (COM refcount) rather than borrowed: a failed call has to
        // drop `self.list` below, which the borrow would forbid.
        let Some(list) = self.list().cloned() else {
            return false;
        };
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // Null icon + null description == clear the overlay slot.
        let icon = overlay.rgb().and_then(dot_icon);
        // Keep the wide string alive across the call.
        let desc_buf: Vec<u16> = overlay.description().encode_utf16().chain([0]).collect();
        let desc = if icon.is_some() {
            PCWSTR(desc_buf.as_ptr())
        } else {
            PCWSTR::null()
        };
        let stamped = unsafe {
            let stamped = list.SetOverlayIcon(hwnd, icon.unwrap_or_default(), desc);
            // The taskbar copies the icon during the call, so ours is free to
            // go immediately (per the SetOverlayIcon contract) — holding it
            // longer would leak one GDI handle per status change. Unconditional
            // on the result: a rejected icon is still ours to destroy.
            if let Some(icon) = icon
                && icon != HICON::default()
            {
                let _ = DestroyIcon(icon);
            }
            stamped
        };
        if let Err(e) = stamped {
            // Either a dead HWND (window closed between snapshot and stamp —
            // its button is gone anyway) or an interface outlived by an
            // Explorer restart. Both are cheapest to treat the same way: drop
            // the interface and let the next tick re-create and re-stamp.
            log::debug!("taskbar overlay stamp failed: {e}");
            self.list = None;
            return false;
        }
        // A dot the taskbar accepted means the interface is live; forgive the
        // creation failures that came before it.
        self.attempts = 0;
        true
    }

    fn list(&mut self) -> Option<&windows::Win32::UI::Shell::ITaskbarList3> {
        use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
        use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};

        if self.list.is_none() {
            if self.attempts >= MAX_ATTEMPTS {
                return None;
            }
            if self.cooldown > 0 {
                self.cooldown -= 1;
                return None;
            }
            self.attempts += 1;
            // gpui's Windows platform already ran `OleInitialize` on this
            // thread, so plain creation is all that's needed.
            let created: Result<ITaskbarList3, _> =
                unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_ALL) };
            match created.and_then(|list| unsafe { list.HrInit() }.map(|()| list)) {
                Ok(list) => self.list = Some(list),
                Err(e) => {
                    // Explorer not running (custom shells) is the realistic
                    // cause; the app is fine without a badge.
                    self.cooldown = RETRY_EVERY;
                    if self.attempts == MAX_ATTEMPTS {
                        log::warn!(
                            "taskbar overlay unavailable after {MAX_ATTEMPTS} attempts: {e}"
                        );
                    }
                }
            }
        }
        self.list.as_ref()
    }
}

/// Render the status dot as a 32×32 `HICON`: a solid antialiased disc on a
/// transparent field. Drawn by hand — a circle needs no SVG pipeline, and the
/// alpha channel of a 32-bit icon carries the antialiasing.
///
/// `SetOverlayIcon` asks for 16×16 *at 96 dpi*, so at 150%/200% scaling the
/// shell wants 24/32 — rendering at 32 and letting it downscale beats handing
/// it 16 to blow up, the same reason `tray::icon` renders at 32 off macOS.
fn dot_icon(rgb: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::UI::WindowsAndMessaging::CreateIcon;

    const S: usize = 32;
    const CENTER: f32 = S as f32 / 2.0 - 0.5;
    // Leaves a pixel of breathing room at 96 dpi so the dot never touches the
    // icon's edge once the shell has scaled it.
    const RADIUS: f32 = S as f32 * 7.0 / 16.0;
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    // BGRA, straight (non-premultiplied) alpha — the layout `CreateIcon`
    // expects for 32-bit XOR bits.
    let mut xor = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - CENTER;
            let dy = y as f32 - CENTER;
            // Solid to `RADIUS`, with alpha ramping linearly over the last
            // pixel of distance to antialias the rim.
            let a = (RADIUS - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if a > 0.0 {
                let i = (y * S + x) * 4;
                xor[i] = b;
                xor[i + 1] = g;
                xor[i + 2] = r;
                xor[i + 3] = (a * 255.0) as u8;
            }
        }
    }
    // The AND mask is vestigial for 32-bit icons (alpha wins), but CreateIcon
    // still demands one bit per pixel.
    let and = [0u8; S * S / 8];
    unsafe { CreateIcon(None, S as i32, S as i32, 1, 32, and.as_ptr(), xor.as_ptr()) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_priority_is_attention_busy_done() {
        assert_eq!(overlay_for(true, true, true), Overlay::Attention);
        assert_eq!(overlay_for(false, true, true), Overlay::Busy);
        assert_eq!(overlay_for(false, false, true), Overlay::Done);
        assert_eq!(overlay_for(false, false, false), Overlay::None);
    }

    #[test]
    fn done_arms_only_on_a_busy_edge_while_unfocused() {
        let mut t = Track::default();
        // Command runs and finishes with the window focused: never Done.
        t.advance(true, true);
        t.advance(false, true);
        assert!(!t.done_pending);
        // Runs focused, finishes unfocused: Done.
        t.advance(true, true);
        t.advance(true, false);
        t.advance(false, false);
        assert!(t.done_pending);
        // Sticks while unfocused, even as new work starts and stops…
        t.advance(true, false);
        assert!(t.done_pending);
        // …and clears the moment the window is activated.
        t.advance(false, true);
        assert!(!t.done_pending);
    }

    #[test]
    fn idle_unfocused_window_never_arms_done() {
        let mut t = Track::default();
        t.advance(false, false);
        t.advance(false, false);
        assert!(!t.done_pending);
    }

    #[test]
    fn overlay_colors_track_the_shared_dot_palette() {
        assert_eq!(Overlay::Busy.rgb(), AgentStatus::Working.dot_rgb());
        assert_eq!(Overlay::Attention.rgb(), AgentStatus::Waiting.dot_rgb());
        assert_eq!(Overlay::Done.rgb(), AgentStatus::Done.dot_rgb());
        assert_eq!(Overlay::None.rgb(), None);
    }
}
