//! The gpui-facing half of [`WindowState`].
//!
//! The struct itself, its `window.json` IO, and the "is this geometry sane"
//! guard live in `tty7-core` — `views.json` embeds the geometry in each
//! [`WindowView`](crate::core::session::WindowView), which is defined there.
//! What is left here is the only part that genuinely needs gpui: turning the
//! four stored `f32`s into a [`Bounds<Pixels>`] and back.

use gpui::{Bounds, Pixels, point, px};

pub use tty7_core::core::window_state::WindowState;

/// Conversions between the stored geometry and gpui's window bounds.
///
/// An extension trait rather than inherent methods because the type lives in
/// `tty7-core`; bring it into scope and `WindowState::from_bounds(..)` /
/// `state.bounds()` read exactly as they did before the crate split.
pub trait WindowGeometry: Sized {
    /// Capture a window's current bounds for persisting.
    fn from_bounds(bounds: Bounds<Pixels>) -> Self;
    /// The bounds to reopen a window at.
    fn bounds(&self) -> Bounds<Pixels>;
}

impl WindowGeometry for WindowState {
    fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(self.x), px(self.y)),
            size: gpui::size(px(self.width), px(self.height)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bounds() {
        let state = WindowState {
            x: -120.5,
            y: 42.0,
            width: 1440.0,
            height: 900.0,
        };
        assert_eq!(WindowState::from_bounds(state.bounds()), state);
    }
}
