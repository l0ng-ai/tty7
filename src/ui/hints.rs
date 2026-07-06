//! Hold-the-modifier tab-shortcut badges.
//!
//! Hold the bare `secondary` modifier (⌘ on macOS, Ctrl on Windows/Linux) for
//! a beat and every tab chip shows its switch digit (1…9 — the held modifier
//! itself is implied).
//! Releasing the modifier, adding another modifier, or pressing any real key
//! (a chord like ⌘C) hides them immediately — the chord dismissal lives in
//! the keystroke interceptor registered in `Tty7App::new`, which fires even
//! for keys the terminal consumes.
//!
//! The trigger is a *hold*, not a chord: ⌘+Tab is reserved by macOS for the
//! system app switcher and never reaches the app.

use gpui::{Context, ModifiersChangedEvent, Window};

use crate::ui::app::Tty7App;

/// Hold this long before the badges show. Practiced chords land their key
/// within ~200ms of the modifier, so ⌘C never even flashes (the interceptor
/// is the backstop for slower chords), while a deliberate pause to look at
/// the tabs still reads as instant — the "immediate response" perception
/// threshold sits around 100–200ms.
const BADGE_DELAY_MS: u64 = 200;

/// The badge label for tab `index`: just the digit ("1"…"9").
/// The modifier is redundant — it's the key the user is holding right now —
/// and a bare digit fits the exact footprint of the close button the badge
/// replaces, so revealing it can't change the chip's width (no strip jitter
/// when an ellipsized label would otherwise reflow). Only tabs 0..9 have a
/// switch shortcut; callers gate on `index < 9`.
pub(crate) fn tab_badge_label(index: usize) -> String {
    (index + 1).to_string()
}

impl Tty7App {
    /// Track the bare-secondary hold that drives the badges: shown while
    /// exactly "secondary held alone", hidden on any other modifier state.
    pub(crate) fn on_modifiers_changed(
        &mut self,
        ev: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &ev.modifiers;
        // Mirror `on_key_down`'s chord test: reject the other platform-ish key
        // (⌃ on macOS, Win/Super elsewhere), Alt, and Shift, so only the bare
        // secondary hold shows the badges.
        let extra_platform = if cfg!(target_os = "macos") {
            m.control
        } else {
            m.platform
        };
        let bare_secondary = m.secondary() && !m.alt && !m.shift && !extra_platform;

        // Every transition invalidates a previously scheduled reveal.
        self.mod_hint_gen = self.mod_hint_gen.wrapping_add(1);
        if !bare_secondary {
            self.dismiss_mod_hint(cx);
            return;
        }

        // Bare secondary went down: schedule the reveal. The timer re-checks
        // the generation so a release, added modifier, or chord keypress in
        // the meantime cancels it. The task dies with the app (update → Err).
        let generation = self.mod_hint_gen;
        cx.spawn(async move |this, cx| {
            smol::Timer::after(std::time::Duration::from_millis(BADGE_DELAY_MS)).await;
            let _ = this.update(cx, |this, cx| {
                if this.mod_hint_gen == generation {
                    this.mod_hint_badges = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Hide the badges and invalidate any pending reveal. Called on every real
    /// keypress (the interceptor in `Tty7App::new`) so a chord like ⌘C never
    /// shows them, and re-arming requires releasing and holding ⌘ afresh —
    /// which doubles as the stuck-state guard if the window ever misses a
    /// release (e.g. ⌘-tabbing away and back).
    pub(crate) fn dismiss_mod_hint(&mut self, cx: &mut Context<Self>) {
        self.mod_hint_gen = self.mod_hint_gen.wrapping_add(1);
        if self.mod_hint_badges {
            self.mod_hint_badges = false;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digit-only on every platform: the held modifier is implied, and a
    /// single digit is what keeps the badge inside the close button's exact
    /// footprint (the no-jitter guarantee).
    #[test]
    fn tab_badge_label_is_the_bare_digit() {
        assert_eq!(tab_badge_label(0), "1");
        assert_eq!(tab_badge_label(8), "9");
    }
}
