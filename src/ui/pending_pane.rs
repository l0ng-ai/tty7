use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, EventEmitter, FocusHandle, Focusable, SharedString,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::ShellSpec;
use crate::terminal::PaneWorkspace;

#[derive(Clone)]
pub struct PendingSpawn {
    pub workspace: Option<PaneWorkspace>,
    pub working_directory: Option<std::path::PathBuf>,
    pub restore_pane: Option<u64>,
    pub shell: Option<ShellSpec>,
    pub agent: Option<crate::core::cli_agent::CLIAgent>,
    pub agent_session_id: Option<String>,
    pub agent_launch_argv: Option<Vec<String>>,
    pub owner: Option<crate::core::session::WorkspaceId>,
    pub font_size: f32,
}

pub enum PendingState {
    Connecting,
    Failed(SharedString),
}

pub struct RetryRequested;

pub struct PendingPane {
    pub focus_handle: FocusHandle,
    pub machine: SharedString,
    pub state: PendingState,
    pub spawn: PendingSpawn,
}

impl EventEmitter<RetryRequested> for PendingPane {}

impl PendingPane {
    pub fn new(
        machine: impl Into<SharedString>,
        spawn: PendingSpawn,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            machine: machine.into(),
            state: PendingState::Connecting,
            spawn,
        }
    }

    pub fn fail(&mut self, reason: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.state = PendingState::Failed(reason.into());
        cx.notify();
    }

    pub fn retrying(&mut self, cx: &mut Context<Self>) {
        self.state = PendingState::Connecting;
        cx.notify();
    }
}

impl Focusable for PendingPane {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PendingPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, dim) = (theme.muted_foreground, theme.muted_foreground.opacity(0.75));

        let body = match &self.state {
            PendingState::Connecting => v_flex()
                .items_center()
                .gap(px(10.))
                .child(
                    Icon::new(IconName::LoaderCircle)
                        .size(px(18.))
                        .text_color(dim)
                        .with_animation(
                            "pending-pane-spin",
                            Animation::new(Duration::from_millis(900)).repeat(),
                            |icon, delta| {
                                icon.transform(gpui::Transformation::rotate(gpui::percentage(
                                    delta,
                                )))
                            },
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(format!("Connecting to {}…", self.machine)),
                )
                .into_any_element(),
            PendingState::Failed(reason) => v_flex()
                .items_center()
                .gap(px(10.))
                .max_w(px(420.))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(format!("Couldn't reach {}", self.machine)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_center()
                        .text_color(muted)
                        .child(reason.clone()),
                )
                .child(
                    Button::new("pending-pane-retry")
                        .label("Try Again")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.retrying(cx);
                            cx.emit(RetryRequested);
                        })),
                )
                .into_any_element(),
        };

        h_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .child(body)
    }
}
