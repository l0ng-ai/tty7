use gpui::{Bounds, Pixels, point, px};

pub use tty7_core::core::window_state::WindowState;

pub trait WindowGeometry: Sized {
    fn from_bounds(bounds: Bounds<Pixels>) -> Self;
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
