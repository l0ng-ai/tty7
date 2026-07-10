//! Hold-the-modifier tab-shortcut badges.
//!
//! Hold the bare `secondary` modifier (⌘ on macOS, Ctrl on Windows/Linux) for
//! a beat and every tab chip shows its switch digit (1…9 — the held modifier
//! itself is implied).
//! Releasing the modifier, adding another modifier, pressing any real key
//! (a chord like ⌘C), or the window changing active status all hide them
//! immediately — the chord dismissal lives in the keystroke interceptor
//! registered in `Tty7App::new` (it fires even for keys the terminal
//! consumes), and the activation dismissal in the observer beside it (a
//! window that deactivates mid-hold never receives the release).
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
    /// shows them, and on every window-activation flip (the observer next to
    /// it) because deactivating mid-hold — ⌘-Tab, Spotlight, a click into
    /// another app — sends the modifier release to whatever app is key by
    /// then, so this window would otherwise show the badges forever.
    /// Re-arming always requires releasing and holding ⌘ afresh.
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

/// gpui-harness tests: a real (headless) App + Window around a `Tty7App`
/// restored to the zero-tab home page — no terminal panes, so no daemon —
/// with the modifiers listener, the reveal timer, and the window-activation
/// wiring running exactly as in production.
#[cfg(test)]
mod gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};

    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Tty7App>, VisualTestContext) {
        // The badge reveal is a real `smol::Timer` on smol's reactor thread —
        // outside the deterministic executor — so waiting on it parks the
        // test thread, exactly what `allow_parking` exists for.
        cx.executor().allow_parking();
        cx.update(|cx| {
            // Same globals `main` installs: the component theme and the
            // user config.
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        // Inject the zero-tab session (the persisted home-page state) so the
        // app builds without spawning a terminal — and without reading the
        // on-disk `session.json`.
        let window =
            cx.add_window(|window, cx| Tty7App::with_session(Some(Session::default()), window, cx));
        // `add_window` alone doesn't make this the platform's active window,
        // and `deactivate_window` below is a no-op on a non-active one — so
        // activate it for real, like the OS does when the app opens.
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (window, vcx)
    }

    fn badges_shown(window: &WindowHandle<Tty7App>, cx: &mut TestAppContext) -> bool {
        window
            .update(cx, |app, _, _| app.mod_hint_badges)
            .expect("the app window stays open")
    }

    /// The reveal timer is real time (~200ms), so poll — bounded — until the
    /// badges land.
    fn wait_for_badges(window: &WindowHandle<Tty7App>, cx: &mut TestAppContext) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            cx.background_executor.run_until_parked();
            if badges_shown(window, cx) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "badges never appeared within 5s of the bare-secondary hold"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Regression: ⌘-Tab, Spotlight, or a click into another app steals key
    /// status mid-hold, so the ⌘ release lands in the other app and this
    /// window never sees the `ModifiersChanged`. The activation flip is the
    /// only signal left — it must dismiss the badges, or they stick until
    /// some later keypress.
    #[gpui::test]
    fn deactivation_dismisses_visible_badges(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        vcx.simulate_modifiers_change(Modifiers::secondary_key());
        wait_for_badges(&window, cx);

        // The window deactivates with the modifier still down; its eventual
        // release is delivered to whatever app is key by then, not to us.
        vcx.deactivate_window();

        assert!(
            !badges_shown(&window, cx),
            "deactivation must dismiss the badges — the modifier release will never reach this window"
        );
    }

    /// Same steal, faster: the modifier goes down and the window deactivates
    /// within the reveal delay. The pending timer must not pop the badges up
    /// in a window the user has already left.
    #[gpui::test]
    fn deactivation_cancels_a_pending_reveal(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        vcx.simulate_modifiers_change(Modifiers::secondary_key());
        vcx.deactivate_window();

        // Give the now-stale reveal timer ample real time to fire.
        std::thread::sleep(std::time::Duration::from_millis(400));
        cx.background_executor.run_until_parked();

        assert!(
            !badges_shown(&window, cx),
            "a reveal scheduled before deactivation must not fire after it"
        );
    }
}
