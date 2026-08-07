use gpui::{Context, Window, div, prelude::*, px};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex};

use crate::core::config::SidebarMode;
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::tab_strip::chrome_tile;

const ACTIVITY_BAR_HEIGHT: f32 = 36.0;

impl Tty7App {
    pub(crate) fn activity_bar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mode = self.sidebar_mode;
        let modes = [
            (
                SidebarMode::Workspaces,
                Icon::empty().path("icons/machine-remote.svg"),
                t(L10nKey::SidebarActivityWorkspacesTooltip),
            ),
            (
                SidebarMode::Outline,
                Icon::empty().path("icons/list.svg"),
                t(L10nKey::SidebarActivityOutlineTooltip),
            ),
        ];

        let mut buttons = modes
            .into_iter()
            .map(|(m, icon, tooltip)| {
                div()
                    .occlude()
                    .flex_shrink_0()
                    .child(
                        chrome_tile(
                            Button::new(("activity-bar", m as usize)).icon(icon),
                            mode == m,
                            cx,
                        )
                        .rounded_lg()
                        .tooltip(tooltip)
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.set_sidebar_mode(m, cx);
                            },
                        )),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        buttons.push(div().flex_1().into_any_element());
        buttons.push(
            div()
                .occlude()
                .flex_shrink_0()
                .child(
                    chrome_tile(
                        Button::new("activity-bar-collapse")
                            .icon(Icon::empty().path("icons/panel-left.svg")),
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(t(L10nKey::TabTooltipHideSidebar))
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                )
                .into_any_element(),
        );
        buttons.push(
            div()
                .occlude()
                .flex_shrink_0()
                .child(
                    self.attach_new_tab_menu(
                        chrome_tile(
                            Button::new("activity-bar-new").icon(Icon::new(IconName::Plus)),
                            false,
                            cx,
                        )
                        .rounded_lg()
                        .tooltip(t(L10nKey::SidebarNewWorkspaceTooltip)),
                        cx,
                    ),
                )
                .into_any_element(),
        );

        h_flex()
            .id("activity-bar")
            .items_center()
            .h(px(ACTIVITY_BAR_HEIGHT))
            .px(px(6.))
            .gap(px(2.))
            .border_b_1()
            .border_color(cx.theme().sidebar_border)
            .children(buttons)
    }
}
