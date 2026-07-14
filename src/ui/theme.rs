//! The menu bar and theme application — the window "chrome" that sits outside
//! the tab/pane shell. `set_menus` (re)builds the macOS menu; `apply_theme`
//! paints gpui-component's `Theme` from the active color theme (see
//! `ui::presets`) and publishes the terminal-facing palette.

use gpui::{
    App, Hsla, Menu, MenuItem, Pixels, Point, Window, WindowBackgroundAppearance, point, px, rgb,
};
use gpui_component::{Theme, ThemeMode};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::ui::presets;

/// The traffic-light origin, nudged down from the macOS default so the buttons
/// stay vertically centred in our taller (40px) title bar. Shared between the
/// window's initial `TitlebarOptions` (see `main.rs`) and `apply_theme`, which
/// re-pins it after each theme change — macOS resets the buttons to their
/// default (higher) position when the app appearance changes, and gpui only
/// repositions them on the next resize/activation, so they'd briefly sit too
/// high until then.
pub(crate) fn traffic_light_position() -> Point<Pixels> {
    point(px(9.), px(13.))
}

/// (Re)build the macOS menu bar.
pub(crate) fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("tty7").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            // Force a fresh background daemon (so a newly granted macOS permission
            // such as Full Disk Access takes effect). The trailing "…" signals the
            // confirmation prompt; it ends every running session.
            MenuItem::action("Restart Daemon…", RestartDaemon),
            MenuItem::separator(),
            MenuItem::action("Quit tty7", Quit),
        ]),
        Menu::new("Shell").items([
            MenuItem::action("New Tab", NewTab),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
            MenuItem::separator(),
            MenuItem::action("Focus Next Pane", FocusNextPane),
            MenuItem::action("Focus Previous Pane", FocusPrevPane),
            MenuItem::action("Toggle Maximize Pane", ToggleMaximizePane),
            MenuItem::separator(),
            MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
            MenuItem::separator(),
            MenuItem::action("Close Pane / Tab", CloseActiveTab),
        ]),
        Menu::new("View").items([
            MenuItem::action("Increase Font Size", IncreaseFontSize),
            MenuItem::action("Decrease Font Size", DecreaseFontSize),
            MenuItem::action("Reset Font Size", ResetFontSize),
            MenuItem::separator(),
            MenuItem::action("Toggle Full Screen", ToggleFullscreen),
        ]),
    ]);
}

/// Paint gpui-component's `Theme` from the active color theme (selected by
/// `Config::theme_preset`). The theme's inferred `dark` brightness picks the
/// component `ThemeMode`; every shell surface is then derived from the theme's
/// background/foreground (see `Theme::neutrals`). Also publishes the
/// terminal-facing palette as the `ActivePalette` global so the renderer matches,
/// and applies the theme's window opacity/blur.
pub(crate) fn apply_theme(mut window: Option<&mut Window>, cx: &mut App) {
    let theme = presets::by_id(cx, &cx.global::<Config>().theme_preset.clone());
    let mode = if theme.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    // Force the native macOS chrome (traffic lights, system menus, scrollbars)
    // into the theme's own light/dark mode regardless of the OS setting.
    sync_native_appearance(theme.dark);
    let m = theme.neutrals();
    let active = theme.active_palette();
    let opacity = theme.opacity.filter(|o| *o < 1.0);

    // Window opacity / blur: only a theme that opts in makes the window
    // translucent; opaque themes leave the window fully opaque (the default).
    if let Some(window) = window.as_deref_mut() {
        let bg_appearance = if theme.blur {
            WindowBackgroundAppearance::Blurred
        } else if opacity.is_some() {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        };
        window.set_background_appearance(bg_appearance);
    }

    Theme::change(mode, window.as_deref_mut(), cx);
    // Publish the terminal palette before borrowing the theme mutably.
    cx.set_global(active);

    let t = Theme::global_mut(cx);
    // The window base carries the theme's opacity so a translucent/blurred theme
    // actually shows through; opaque themes (opacity None) stay fully opaque.
    let mut base: Hsla = rgb(m.background).into();
    if let Some(o) = opacity {
        base.a = o;
    }
    t.background = base; // terminal / window base
    t.foreground = rgb(m.foreground).into(); // default text
    t.border = rgb(m.border).into();
    t.secondary = rgb(m.secondary).into(); // hover chips (+ / tab)
    t.muted = rgb(m.muted).into();
    t.muted_foreground = rgb(m.muted_foreground).into(); // inactive tab text
    t.popover = rgb(m.popover).into(); // elevated surfaces
    // gpui-component paints popovers/menus (context menu, dropdowns) from
    // `tokens.popover` / `tokens.popover_foreground`, NOT the `popover*` fields —
    // so the menu background ignored our theme and fell back to the stock surface
    // (looking off-theme). Mirror the theme onto the tokens, same gotcha as the
    // sidebar below.
    t.tokens.popover = Hsla::from(rgb(m.popover)).into();
    t.tokens.popover_foreground = Hsla::from(rgb(m.foreground)).into();

    // Context menus and dropdowns highlight the hovered/selected row from
    // `tokens.accent` (fill) + `accent_foreground` (text) — see gpui-component's
    // `MenuItemElement`. Left unset, that highlight falls back to the stock
    // saturated accent, which snaps hard against this app's soft mix-based
    // palette (the "生硬" hover). Point it at the same soft fill the command
    // palette uses for its selected row (`list_active`, mix 0.17) so context
    // menu, dropdown and palette share one hover language; keep the text at
    // `foreground` so it stays legible on the low-contrast fill instead of the
    // stock inverted accent text. The plain `accent`/`accent_foreground` fields
    // feed the same highlight in the input completion / code-action popovers, so
    // mirror both the fields and the tokens to keep every menu surface in step.
    let accent_fill = rgb(m.list_active);
    let accent_text: Hsla = rgb(m.foreground).into();
    t.accent = accent_fill.into();
    t.accent_foreground = accent_text;
    t.tokens.accent = Hsla::from(accent_fill).into();
    t.tokens.accent_foreground = accent_text.into();

    // Primary buttons (Connect, Reconnect, Trust, Save…) fill from
    // `tokens.button_primary`; the raw default is the foreground — a pure
    // near-black in a light theme, which reads harsh. Nudge it toward the
    // background so it lands on a softer dark charcoal (and, symmetrically, a
    // slightly dimmed near-white in dark themes). We set the whole primary token
    // family (both the plain `primary*` fields — used for the border and outline
    // text — and the `button_primary*` tokens the fill actually reads) so every
    // primary button shifts together, hover/pressed included. The stock
    // `button_primary_foreground` stays legible on top either way.
    let primary_base: Hsla = rgb(presets::mix(m.foreground, m.background, 0.20)).into();
    let primary_hover: Hsla = rgb(presets::mix(m.foreground, m.background, 0.30)).into();
    let primary_active: Hsla = rgb(presets::mix(m.foreground, m.background, 0.10)).into();
    t.primary = primary_base;
    t.primary_hover = primary_hover;
    t.primary_active = primary_active;
    t.tokens.primary = primary_base.into();
    t.tokens.primary_hover = primary_hover.into();
    t.tokens.primary_active = primary_active.into();
    t.tokens.button_primary = primary_base.into();
    t.tokens.button_primary_hover = primary_hover.into();
    t.tokens.button_primary_active = primary_active.into();

    t.caret = rgb(m.caret).into();
    t.selection = rgb(m.selection).into(); // text selection highlight

    // Round every gpui-component widget (buttons, inputs, selects, switches,
    // segmented controls, menus) to match the shell's own hand-rolled chrome,
    // which uses `rounded_lg` (8px) for tab chips, title-bar tiles and the
    // settings steppers. gpui-component defaults to 6px, so stock controls read a
    // hair boxier than everything around them; pinning `radius` to 8 makes the
    // widgets and the chrome share one corner language instead of two. The
    // hand-rolled chrome sets explicit radii, so it's unaffected — this only
    // pulls the stock widgets into line.
    t.radius = px(8.);

    // Settings sidebar. NOTE: gpui-component's Sidebar paints its column from
    // `tokens.sidebar` (and the active chip from `tokens.sidebar_accent`), NOT
    // the `sidebar*` color fields — so those must be set on `tokens` or the
    // override is a no-op and the column falls back to the stock surface.
    let sidebar_bg = rgb(m.sidebar);
    let sidebar_sel = rgb(m.sidebar_sel);
    t.sidebar = sidebar_bg.into();
    t.tokens.sidebar = Hsla::from(sidebar_bg).into();
    t.sidebar_border = rgb(m.border).into();
    t.sidebar_foreground = rgb(m.sidebar_fg).into();
    t.sidebar_accent = sidebar_sel.into();
    t.tokens.sidebar_accent = Hsla::from(sidebar_sel).into();
    t.sidebar_accent_foreground = rgb(m.foreground).into();

    // Flatten gpui-component's list selection highlight (used by the command
    // palette) into a single soft fill — no blue ring, no accent tint — so it
    // matches this app's minimal aesthetic instead of the stock look. Keep
    // `active_highlight` on (the alternative path tints with the shared
    // `accent`), but make the ring colour equal the fill so the box disappears.
    t.list.active_highlight = true;
    t.list_active = rgb(m.list_active).into();
    t.list_active_border = rgb(m.list_active).into();
    t.list_hover = rgb(m.list_hover).into();

    // `sync_native_appearance` above may have flipped the macOS app appearance,
    // which resets the traffic-light buttons to their default (higher) position.
    // gpui doesn't reposition them on an appearance change (only on
    // resize/activation/title changes), so re-pin our centred position now —
    // otherwise the buttons briefly sit too high until the next such event. Same
    // immediate-re-move pattern gpui itself uses after `setRepresentedFilename`.
    #[cfg(target_os = "macos")]
    if let Some(window) = window.as_deref_mut() {
        window.set_traffic_light_position(traffic_light_position());
    }
}

/// Apply `Config::mouse_hide_while_typing` to GPUI's cursor-hide policy: hide the
/// pointer while typing when on, never when off. Called at startup and whenever
/// the config changes (setter + hot-reload) so the switch takes effect live.
pub(crate) fn apply_cursor_hide_mode(cx: &mut App) {
    let mode = if cx.global::<Config>().mouse_hide_while_typing {
        gpui::CursorHideMode::OnTypingAndAction
    } else {
        gpui::CursorHideMode::Never
    };
    cx.set_cursor_hide_mode(mode);
}

/// Force the macOS app appearance to match the active theme's light/dark mode
/// instead of following the OS `Appearance` setting.
///
/// macOS draws the native traffic-light buttons according to the window's
/// effective appearance. With a dark tty7 theme on a light-mode macOS, the
/// system paints the *light-style* inactive (unfocused) traffic lights — heavy
/// mid-grey circles that look filthy on the dark titlebar. gpui only ever
/// *reads* `effectiveAppearance` (`WindowAppearance::from_native`); it exposes
/// no setter, so we pin `NSApplication.appearance` ourselves via AppKit. This
/// also keeps system menus, context menus and scrollbars in the right mode.
#[cfg(target_os = "macos")]
fn sync_native_appearance(dark: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    // `apply_theme` is always invoked on the gpui app (main) thread; bail
    // defensively rather than panic if that ever stops holding.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: reading the framework-provided appearance-name statics.
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    if let Some(appearance) = NSAppearance::appearanceNamed(name) {
        NSApplication::sharedApplication(mtm).setAppearance(Some(&appearance));
    }
}

#[cfg(not(target_os = "macos"))]
fn sync_native_appearance(_dark: bool) {}
