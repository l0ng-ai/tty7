//! The settings page's own controls.
//!
//! Settings is drawn to one design, and that design is quieter than the rest
//! of the app: ink at a handful of fixed strengths instead of the theme's named
//! roles, hairline rings instead of borders, and controls that sit on the page
//! rather than on cards. Everything here derives from the active theme's
//! foreground and background, so the page follows every theme — light, dark,
//! and the tinted ones in between — without a palette of its own.
//!
//! Sizes are in rems off a 16px base, written as the pixel sizes the design
//! calls for, so the page still grows with the interface font.

use std::rc::Rc;

use gpui::{
    AnyElement, App, BoxShadow, ClickEvent, Div, ElementId, FontWeight, Hsla, IntoElement,
    MouseButton, Rems, SharedString, Stateful, Window, div, point, prelude::*, px, rems,
};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, Icon, h_flex, v_flex};

/// A size from the design, in pixels at the default 16px interface font.
pub(crate) fn fs(px_at_16: f32) -> Rems {
    rems(px_at_16 / 16.)
}

/// The page's ink at the strengths the design uses, plus the few surfaces it
/// lifts off the page.
#[derive(Clone, Copy)]
pub(crate) struct Tk {
    pub dark: bool,
    pub fg: Hsla,
    pub page: Hsla,
    pub k04: Hsla,
    pub k05: Hsla,
    pub k06: Hsla,
    pub k07: Hsla,
    pub k08: Hsla,
    pub k15: Hsla,
    pub k3: Hsla,
    pub k35: Hsla,
    pub k4: Hsla,
    pub k45: Hsla,
    pub k5: Hsla,
    pub k6: Hsla,
    /// A group heading: quieter than a label, but still read at a glance.
    pub heading: Hsla,
    /// A nav item at rest.
    pub nav: Hsla,
    /// A raised button's fill.
    pub btn: Hsla,
    /// A popover's fill.
    pub menu: Hsla,
    /// A switch or slider knob at rest.
    pub knob: Hsla,
    /// The chosen cell of a segmented control.
    pub chip: Hsla,
    pub warn: Hsla,
    pub warn_text: Hsla,
    pub danger: Hsla,
    pub ok: Hsla,
}

fn lift(c: Hsla, by: f32) -> Hsla {
    Hsla {
        l: (c.l + by).clamp(0., 1.),
        ..c
    }
}

impl Tk {
    pub(crate) fn of(cx: &App) -> Self {
        let theme = cx.theme();
        let dark = theme.is_dark();
        let fg = theme.foreground;
        let page = theme.background;
        let a = |light: f32, dark_a: f32| fg.opacity(if dark { dark_a } else { light });
        let white = gpui::white();
        Self {
            dark,
            fg,
            page,
            k04: a(0.04, 0.055),
            k05: a(0.05, 0.07),
            k06: a(0.06, 0.08),
            k07: a(0.07, 0.09),
            k08: a(0.08, 0.08),
            k15: a(0.15, 0.16),
            k3: a(0.3, 0.3),
            k35: a(0.35, 0.36),
            k4: a(0.4, 0.45),
            k45: a(0.45, 0.5),
            k5: a(0.5, 0.55),
            k6: a(0.6, 0.65),
            heading: a(0.9, 0.9),
            nav: a(0.82, 0.8),
            btn: if dark { fg.opacity(0.1) } else { white },
            menu: if dark { lift(page, 0.06) } else { white },
            knob: if dark { fg } else { white },
            chip: if dark { fg.opacity(0.16) } else { white },
            warn: gpui::rgb(0xe0a100).into(),
            warn_text: gpui::rgb(0xb07d00).into(),
            danger: gpui::rgb(0xd93025).into(),
            ok: gpui::rgb(0x34a853).into(),
        }
    }

    /// The hairline ring and the faint drop a raised control sits on.
    pub(crate) fn raised(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![ring(self.fg.opacity(0.1), 0.5, false)]
        } else {
            vec![
                ring(gpui::black().opacity(0.08), 0.5, false),
                drop(gpui::black().opacity(0.06), 1., 2., 0.),
            ]
        }
    }

    /// The same, a step firmer, for a hovered raised control.
    pub(crate) fn raised_hover(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![ring(self.fg.opacity(0.2), 0.5, false)]
        } else {
            vec![
                ring(gpui::black().opacity(0.16), 0.5, false),
                drop(gpui::black().opacity(0.08), 1., 2., 0.),
            ]
        }
    }

    pub(crate) fn popover(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![
                ring(self.fg.opacity(0.12), 0.5, false),
                drop(gpui::black().opacity(0.6), 16., 40., -8.),
            ]
        } else {
            vec![
                ring(gpui::black().opacity(0.12), 0.5, false),
                drop(gpui::black().opacity(0.22), 16., 40., -8.),
            ]
        }
    }

    pub(crate) fn mono(cx: &App) -> SharedString {
        cx.theme().mono_font_family.clone()
    }
}

pub(crate) fn ring(color: Hsla, width: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(width),
        inset,
    }
}

pub(crate) fn drop(color: Hsla, y: f32, blur: f32, spread: f32) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset: false,
    }
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type ToggleHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

/// An on/off switch: ink when on, a hairline track when off.
#[derive(IntoElement)]
pub(crate) struct Switch {
    id: ElementId,
    on: bool,
    disabled: bool,
    small: bool,
    handler: Option<ToggleHandler>,
}

pub(crate) fn switch(id: impl Into<ElementId>) -> Switch {
    Switch {
        id: id.into(),
        on: false,
        disabled: false,
        small: false,
        handler: None,
    }
}

impl Switch {
    pub(crate) fn checked(mut self, on: bool) -> Self {
        self.on = on;
        self
    }
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub(crate) fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub(crate) fn on_click(mut self, f: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.handler = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tk = Tk::of(cx);
        let (w, h, knob) = if self.small {
            (26., 16., 12.)
        } else {
            (30., 18., 14.)
        };
        let on = self.on;
        let left = if on { w - knob - 2. } else { 2. };
        div()
            .id(self.id)
            .flex_shrink_0()
            .relative()
            .w(px(w))
            .h(px(h))
            .rounded(px(h / 2.))
            .bg(if on { tk.fg } else { tk.k15 })
            .when(self.disabled, |d| d.opacity(0.4))
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(left))
                    .size(px(knob))
                    .rounded_full()
                    .bg(if on { tk.page } else { tk.knob })
                    .shadow(vec![drop(gpui::black().opacity(0.2), 1., 2., 0.)]),
            )
            .when_some(self.handler.filter(|_| !self.disabled), |d, handler| {
                d.cursor_pointer()
                    .on_click(move |_, window, cx| handler(&!on, window, cx))
            })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BtnKind {
    /// A raised button on the page.
    Secondary,
    /// Solid ink: the one action a block exists for.
    Primary,
    /// Text that darkens on hover, for actions beside other actions.
    Link,
    /// A link in red, for the destructive one.
    Danger,
}

#[derive(IntoElement)]
pub(crate) struct Btn {
    id: ElementId,
    label: SharedString,
    kind: BtnKind,
    disabled: bool,
    dimmed: bool,
    icon: Option<SharedString>,
    size: Option<Rems>,
    handler: Option<ClickHandler>,
}

pub(crate) fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: BtnKind,
) -> Btn {
    Btn {
        id: id.into(),
        label: label.into(),
        kind,
        disabled: false,
        dimmed: false,
        icon: None,
        size: None,
        handler: None,
    }
}

impl Btn {
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    /// Clickable, but drawn at rest — a Save with nothing to save yet.
    pub(crate) fn dimmed(mut self, dimmed: bool) -> Self {
        self.dimmed = dimmed;
        self
    }
    pub(crate) fn icon(mut self, path: impl Into<SharedString>) -> Self {
        self.icon = Some(path.into());
        self
    }
    pub(crate) fn text_size(mut self, size: Rems) -> Self {
        self.size = Some(size);
        self
    }
    pub(crate) fn on_click(
        mut self,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.handler = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Btn {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tk = Tk::of(cx);
        let kind = self.kind;
        let base = h_flex()
            .id(self.id)
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .whitespace_nowrap();
        let el = match kind {
            BtnKind::Secondary => base
                .h(px(26.))
                .px(px(12.))
                .when(self.icon.is_some(), |b| b.pl(px(9.)).pr(px(11.)))
                .rounded(px(6.))
                .bg(tk.btn)
                .shadow(tk.raised())
                .text_size(self.size.unwrap_or(fs(12.5)))
                .text_color(tk.fg)
                .when(!self.disabled, |b| {
                    b.hover(move |s| s.shadow(tk.raised_hover()))
                        .active(move |s| s.bg(tk.k05))
                }),
            BtnKind::Primary => base
                .h(px(26.))
                .px(px(12.))
                .rounded(px(6.))
                .bg(tk.fg)
                .text_size(self.size.unwrap_or(fs(12.5)))
                .font_weight(FontWeight::MEDIUM)
                .text_color(tk.page)
                .when(!self.disabled, |b| b.hover(|s| s.opacity(0.88))),
            BtnKind::Link | BtnKind::Danger => base
                .text_size(self.size.unwrap_or(fs(12.)))
                .text_color(if kind == BtnKind::Danger {
                    tk.danger
                } else {
                    tk.k6
                })
                .when(!self.disabled, |b| {
                    b.hover(move |s| {
                        s.text_color(if kind == BtnKind::Danger {
                            tk.danger
                        } else {
                            tk.fg
                        })
                    })
                }),
        };
        el.when(self.disabled, |b| b.opacity(0.45))
            .when(self.dimmed && !self.disabled, |b| b.opacity(0.55))
            .when_some(self.icon, |b, path| {
                b.child(Icon::empty().path(path).size(px(10.)).text_color(tk.k6))
            })
            .child(self.label)
            .when_some(self.handler.filter(|_| !self.disabled), |b, handler| {
                b.cursor_pointer()
                    .on_click(move |ev, window, cx| handler(ev, window, cx))
            })
    }
}

/// A key on a keycap: `⌘`, `T`, `Space`.
pub(crate) fn keycap(key: impl Into<SharedString>, tk: &Tk) -> Div {
    div()
        .flex_shrink_0()
        .min_w(px(20.))
        .h(px(20.))
        .px(px(5.))
        .rounded(px(4.))
        .bg(tk.k05)
        .shadow(vec![BoxShadow {
            color: tk.k15,
            offset: point(px(0.), px(-0.5)),
            blur_radius: px(0.),
            spread_radius: px(0.),
            inset: true,
        }])
        .flex()
        .items_center()
        .justify_center()
        .text_size(fs(11.5))
        .text_color(tk.k6)
        .child(key.into())
}

/// A search glass the size the design draws it beside a field.
pub(crate) fn search_glass(size: f32, tk: &Tk) -> Icon {
    Icon::empty()
        .path("icons/settings/search.svg")
        .size(px(size))
        .text_color(tk.k35)
}

/// A search field with a hairline ring, for filtering a list on the page.
pub(crate) fn search_field(input: &gpui::Entity<InputState>, tk: &Tk) -> Div {
    h_flex()
        .h(px(26.))
        .px(px(8.))
        .gap(px(6.))
        .items_center()
        .rounded(px(6.))
        .shadow(vec![ring(tk.k15, 0.5, true)])
        .child(search_glass(10., tk))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(fs(12.5))
                .child(Input::new(input).appearance(false).px_0().py_0().h(px(24.))),
        )
}

/// A text field the way the page draws one: a hairline ring on the page, in
/// the terminal's monospace, with a firmer ring while it has focus.
pub(crate) fn text_field(
    input: &gpui::Entity<InputState>,
    focused: bool,
    invalid: bool,
    tk: &Tk,
    cx: &App,
) -> Div {
    let ring_color = if invalid {
        tk.danger
    } else if focused {
        tk.k45
    } else {
        tk.k15
    };
    div()
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .rounded(px(6.))
        .shadow(vec![ring(
            ring_color,
            if focused || invalid { 1. } else { 0.5 },
            true,
        )])
        .font_family(Tk::mono(cx))
        .text_size(fs(12.))
        .text_color(tk.fg)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(input).appearance(false).px_0().py_0().h(px(24.))),
        )
}

/// A group's heading and, when it has one, the sentence under it.
pub(crate) fn section_head(title: &str, desc: Option<String>, tk: &Tk) -> Div {
    v_flex()
        .gap(px(3.))
        .pb(px(8.))
        .child(
            div()
                .text_size(fs(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tk.heading)
                .child(title.to_string()),
        )
        .when_some(desc.filter(|d| !d.is_empty()), |col, desc| {
            col.child(
                div()
                    .text_size(fs(12.))
                    .line_height(px(17.))
                    .text_color(tk.k6)
                    .child(desc),
            )
        })
}

/// The trigger of a dropdown: a raised button with the current value, an
/// optional mark before it, and a chevron.
pub(crate) fn select_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    leading: Option<AnyElement>,
    open: bool,
    tk: &Tk,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h(px(26.))
        .min_w(px(148.))
        .max_w(px(260.))
        .pl(px(if leading.is_some() { 8. } else { 10. }))
        .pr(px(7.))
        .gap(px(8.))
        .items_center()
        .rounded(px(6.))
        .bg(if open { tk.k05 } else { tk.btn })
        .shadow(tk.raised())
        .hover(move |s| s.shadow(tk.raised_hover()))
        .cursor_pointer()
        .text_size(fs(12.5))
        .text_color(tk.fg)
        .whitespace_nowrap()
        .children(leading)
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .child(chevron_down(tk))
}

pub(crate) fn chevron_down(tk: &Tk) -> Icon {
    Icon::empty()
        .path("icons/settings/chevron-down.svg")
        .size(px(10.))
        .text_color(tk.k45)
}

pub(crate) fn check_mark(visible: bool, tk: &Tk) -> Div {
    div().size(px(12.)).flex_shrink_0().when(visible, |d| {
        d.child(
            Icon::empty()
                .path("icons/settings/check.svg")
                .size(px(12.))
                .text_color(tk.fg),
        )
    })
}

/// A popover's surface.
pub(crate) fn menu_panel(tk: &Tk) -> Div {
    v_flex()
        .p(px(4.))
        .gap(px(1.))
        .rounded(px(8.))
        .bg(tk.menu)
        .shadow(tk.popover())
        .text_size(fs(12.5))
        .text_color(tk.fg)
}

/// One pickable row in a popover menu.
pub(crate) fn menu_row(id: impl Into<ElementId>, highlighted: bool, tk: &Tk) -> Stateful<Div> {
    h_flex()
        .id(id)
        .flex_shrink_0()
        .h(px(26.))
        .pl(px(6.))
        .pr(px(12.))
        .gap(px(6.))
        .items_center()
        .rounded(px(5.))
        .whitespace_nowrap()
        .cursor_pointer()
        .when(highlighted, |r| r.bg(tk.k06))
        .hover(move |s| s.bg(tk.k06))
}

pub(crate) fn menu_heading(label: impl Into<SharedString>, tk: &Tk) -> Div {
    div()
        .h(px(24.))
        .px(px(8.))
        .flex()
        .items_center()
        .text_size(fs(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(tk.k45)
        .child(label.into())
}

pub(crate) fn menu_separator(tk: &Tk) -> Div {
    div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(tk.k08)
}

/// Where a trigger's popover hangs: under the trigger, its right edge on the
/// trigger's (or its left on the trigger's), painted over the rest of the page
/// and pushed back inside the window when it would run off it.
pub(crate) fn popover_below(align_right: bool, gap: f32, content: impl IntoElement) -> Div {
    let anchor = if align_right {
        gpui::Anchor::TopRight
    } else {
        gpui::Anchor::TopLeft
    };
    div()
        .absolute()
        .top(px(26. + gap))
        .when(align_right, |d| d.right_0())
        .when(!align_right, |d| d.left_0())
        .child(
            gpui::deferred(
                gpui::anchored()
                    .anchor(anchor)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .occlude()
                            // A click inside the popover is not a click on the
                            // page under it, which would close it.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(content),
                    ),
            )
            .with_priority(2),
        )
}

/// The stepper: − value +, one raised control with hairline dividers.
pub(crate) struct Stepper {
    pub value: String,
    pub unit: String,
    pub can_dec: bool,
    pub can_inc: bool,
}

pub(crate) fn stepper(
    id: &'static str,
    s: Stepper,
    tk: &Tk,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let end = |suffix: &'static str, glyph: &'static str, enabled: bool| {
        div()
            .id(SharedString::from(format!("{id}-{suffix}")))
            .w(px(28.))
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(fs(13.))
            .text_color(tk.k6)
            .when(!enabled, |d| d.opacity(0.35))
            .when(enabled, |d| {
                d.cursor_pointer().active(move |s| s.bg(tk.k05))
            })
            .child(glyph)
    };
    let divider = || div().h(px(14.)).w(px(1.)).bg(tk.k15);
    h_flex()
        .id(id)
        .h(px(26.))
        .items_center()
        .rounded(px(6.))
        .bg(tk.btn)
        .shadow(tk.raised())
        .child(
            end("dec", "−", s.can_dec)
                .rounded_l(px(6.))
                .when(s.can_dec, |d| d.on_click(on_dec)),
        )
        .child(divider())
        .child(
            h_flex()
                .min_w(px(52.))
                .px(px(6.))
                .justify_center()
                .text_size(fs(12.5))
                .text_color(tk.fg)
                .child(s.value)
                .child(div().text_color(tk.k4).child(s.unit)),
        )
        .child(divider())
        .child(
            end("inc", "+", s.can_inc)
                .rounded_r(px(6.))
                .when(s.can_inc, |d| d.on_click(on_inc)),
        )
        .into_any_element()
}

/// What a slider is being dragged by; the id tells two sliders apart.
#[derive(Clone)]
pub(crate) struct SliderDrag(pub SharedString);

impl Render for SliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A thin track with an ink fill and a raised knob, 148 wide, with its value
/// read out beside it. `on_set` takes the fraction along the track the pointer
/// is at; `on_release` is where a drag's value gets written down for good.
pub(crate) fn slider(
    id: impl Into<SharedString>,
    fraction: f32,
    readout: String,
    tk: &Tk,
    on_set: Rc<dyn Fn(f32, &mut Window, &mut App)>,
    on_release: Rc<dyn Fn(&mut Window, &mut App)>,
) -> AnyElement {
    let id: SharedString = id.into();
    let f = fraction.clamp(0., 1.);
    const W: f32 = 148.;
    let frac_at = |bounds: gpui::Bounds<gpui::Pixels>, x: gpui::Pixels| -> f32 {
        let w = bounds.size.width.as_f32().max(1.);
        ((x - bounds.origin.x).as_f32() / w).clamp(0., 1.)
    };
    let set_down = on_set.clone();
    let set_move = on_set;
    let drag_id = id.clone();
    let release_up = on_release.clone();
    let release_out = on_release;
    let bounds = Rc::new(std::cell::Cell::new(gpui::Bounds::<gpui::Pixels>::default()));
    h_flex()
        .gap(px(8.))
        .items_center()
        .child(
            div()
                .id(ElementId::Name(id.clone()))
                .relative()
                .w(px(W))
                .h(px(18.))
                .cursor_pointer()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(8.))
                        .h(px(2.))
                        .rounded(px(1.))
                        .bg(tk.k15),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(8.))
                        .h(px(2.))
                        .w(px(W * f))
                        .rounded(px(1.))
                        .bg(tk.fg),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .left(px(W * f - 7.))
                        .size(px(14.))
                        .rounded_full()
                        .bg(tk.knob)
                        .shadow(vec![
                            ring(gpui::black().opacity(0.18), 0.5, false),
                            drop(gpui::black().opacity(0.18), 1., 3., 0.),
                        ]),
                )
                .child({
                    // The track's bounds for a click that never becomes a
                    // drag: a mouse-down event carries a position but not the
                    // box it landed in.
                    let bounds = bounds.clone();
                    gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                        .absolute()
                        .size_full()
                })
                .on_mouse_down(MouseButton::Left, {
                    let bounds = bounds.clone();
                    move |ev: &gpui::MouseDownEvent, window, cx| {
                        set_down(frac_at(bounds.get(), ev.position.x), window, cx);
                    }
                })
                .on_drag(SliderDrag(drag_id), |drag, _, _, cx| {
                    cx.new(|_| drag.clone())
                })
                .on_drag_move({
                    let id = id.clone();
                    move |ev: &gpui::DragMoveEvent<SliderDrag>, window, cx| {
                        if ev.drag(cx).0 != id {
                            return;
                        }
                        set_move(frac_at(ev.bounds, ev.event.position.x), window, cx);
                    }
                })
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    release_up(window, cx)
                })
                .on_mouse_up_out(MouseButton::Left, move |_, window, cx| {
                    release_out(window, cx)
                }),
        )
        .child(
            div()
                .w(px(40.))
                .flex_shrink_0()
                .text_right()
                .whitespace_nowrap()
                .text_size(fs(12.5))
                .text_color(tk.k6)
                .child(readout),
        )
        .into_any_element()
}
