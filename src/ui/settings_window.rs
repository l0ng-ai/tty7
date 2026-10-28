//! Settings in a window of its own.
//!
//! The settings page used to cover the workspace it was opened from, which hid
//! the terminal a setting was being tried out on. It now opens beside it. The
//! state and the page itself still belong to the workspace's `Tty59App` — this
//! window only borrows it to draw — so every setting keeps the one code path it
//! had, and a change repaints both windows.

use gpui::{
    App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowDecorations, WindowOptions, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, TitleBar};

use crate::core::actions::{CloseActiveTab, OpenSettings};
use crate::core::config::Config;
use crate::ui::app::Tty59App;

/// Big enough for the navigation column and a comfortable page beside it.
const DEFAULT_SIZE: (f32, f32) = (960., 700.);
const MIN_SIZE: (f32, f32) = (720., 480.);

pub(crate) fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
            cx,
        ))),
        app_id: Some("tty59".to_owned()),
        titlebar: Some(TitlebarOptions {
            // Not drawn (the title bar is the window's own), but it is the
            // name the Window menu, Mission Control and VoiceOver give it.
            title: Some(crate::ui::i18n::t(crate::ui::i18n::L10nKey::SettingsWindowTitle).into()),
            traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
            ..TitleBar::title_bar_options()
        }),
        window_decorations: Some(WindowDecorations::Client),
        window_background: crate::ui::theme::background_appearance(cx),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        ..Default::default()
    }
}

pub(crate) struct SettingsWindow {
    pub(crate) app: WeakEntity<Tty59App>,
    _observe: Subscription,
    _release: Subscription,
}

impl SettingsWindow {
    pub(crate) fn new(app: &Entity<Tty59App>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Every settings change notifies the app, not this view; without this
        // the page would change underneath and never repaint here.
        let observe = cx.observe(app, |_, _, cx| cx.notify());
        // The workspace that owns the page is gone, so the page is too.
        let release = cx.observe_release_in(app, window, |_, _, window, _| window.remove_window());
        // The close button asks the same question Escape does: a half-edited
        // form or theme draft gets its prompt, and the window only goes once
        // it is answered.
        let weak = app.downgrade();
        window.on_window_should_close(cx, move |window, cx| match weak.upgrade() {
            Some(app) => {
                app.update(cx, |this, cx| this.close_settings_checked(window, cx));
                false
            }
            None => true,
        });
        Self {
            app: app.downgrade(),
            _observe: observe,
            _release: release,
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |this, cx| this.close_settings_checked(window, cx));
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The workspace window sets this in its own render; a second window
        // has to, or the page lays out against the default 16px rem.
        window.set_rem_size(px(cx.global::<Config>().ui_font_size));
        crate::ui::settings::kit::note_scale(window);
        let page = self.app.upgrade().map(|app| {
            let page = app.update(cx, |this, cx| {
                // The state can be gone for a frame between closing and the
                // window being removed.
                this.has_settings()
                    .then(|| this.render_settings(window, cx).into_any_element())
            });
            if page.is_none() {
                // Closed without passing through `close_settings` — nothing
                // is left to draw, so the window goes too.
                app.update(cx, |this, cx| this.forget_settings_window(cx));
                window.remove_window();
            }
            page
        });
        gpui::div()
            .size_full()
            .bg(crate::ui::theme::overlay_background(cx))
            .text_color(cx.theme().foreground)
            // ⌘W closes this window the way it closes a tab in a workspace.
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| this.close(window, cx)))
            // ⌘, lands here while this window has focus; it is already open.
            .on_action(cx.listener(|_, _: &OpenSettings, window, _| window.activate_window()))
            .when_some(page.flatten(), |root, page| root.child(page))
            // This window is a `Root` of its own, and a `Root` only shows the
            // toasts it is asked to draw: without this, everything the page
            // reports — an ssh_config import, a passphrase it could not store
            // — was pushed into a layer nobody rendered.
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}
