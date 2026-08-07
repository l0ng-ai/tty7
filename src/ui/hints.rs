use gpui::{Context, ModifiersChangedEvent, Window};

use crate::ui::app::Tty7App;

const BADGE_DELAY_MS: u64 = 200;

pub(crate) fn tab_badge_label(index: usize) -> String {
    (index + 1).to_string()
}

impl Tty7App {
    pub(crate) fn on_modifiers_changed(
        &mut self,
        ev: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &ev.modifiers;
        self.switcher_hold_changed(m, window, cx);
        self.set_link_modifier(m.secondary(), cx);

        let extra_platform = if cfg!(target_os = "macos") {
            m.control
        } else {
            m.platform
        };
        let bare_secondary = m.secondary() && !m.alt && !m.shift && !extra_platform;

        self.mod_hint_gen = self.mod_hint_gen.wrapping_add(1);
        if !bare_secondary {
            self.dismiss_mod_hint(cx);
            return;
        }

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

    pub(crate) fn set_link_modifier(&mut self, down: bool, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                leaf.update(cx, |view, cx| {
                    view.refresh_link_hover(down, cx);
                });
            }
        }
    }

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

    #[test]
    fn tab_badge_label_is_the_bare_digit() {
        assert_eq!(tab_badge_label(0), "1");
        assert_eq!(tab_badge_label(8), "9");
    }
}

#[cfg(test)]
mod gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};

    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Tty7App>, VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let window = cx.add_window(|window, cx| {
            Tty7App::with_session(None, Some(Session::default()), window, cx)
        });
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

    #[gpui::test]
    fn deactivation_dismisses_visible_badges(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        vcx.simulate_modifiers_change(Modifiers::secondary_key());
        wait_for_badges(&window, cx);

        vcx.deactivate_window();

        assert!(
            !badges_shown(&window, cx),
            "deactivation must dismiss the badges — the modifier release will never reach this window"
        );
    }

    #[gpui::test]
    fn deactivation_cancels_a_pending_reveal(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        vcx.simulate_modifiers_change(Modifiers::secondary_key());
        vcx.deactivate_window();

        std::thread::sleep(std::time::Duration::from_millis(400));
        cx.background_executor.run_until_parked();

        assert!(
            !badges_shown(&window, cx),
            "a reveal scheduled before deactivation must not fire after it"
        );
    }
}
