//! The pane that is not a terminal yet.
//!
//! # Why this exists
//!
//! Building a pane is one blocking call — [`TerminalView::spawn_shell_terminal_in`]
//! — and for a *local* pane that is a unix-socket round trip to a daemon on the
//! same machine: microseconds, invisible, and it has been called straight from
//! the UI thread since before remote workspaces existed.
//!
//! For a **remote** pane the identical call opens a connection to the local
//! daemon, hands it a `RouteHeader`, and then waits while the daemon opens an
//! SSH channel to another computer (doing the whole handshake if nothing is
//! pooled yet) and the remote `tty7-server` answers a `Spawn` or `Attach`.
//! `connect_routed` says as much in its own doc — *"blocks for as long as the
//! setup takes … callers are already on a background thread"* — and the caller
//! that mattered, `ui::app::new_terminal`, was not. It runs inside a gpui input
//! callback. Restoring a four-pane remote workspace froze the whole window four
//! times over, and every remote new-tab and split froze it once.
//!
//! So a remote pane is now built in two halves: the slot goes into the tree
//! immediately holding one of these, the blocking half runs on the background
//! executor, and the slot is swapped for the real [`TerminalView`] when it
//! lands. See [`PaneSlot`](crate::ui::pane::PaneSlot).
//!
//! # What it does *not* change
//!
//! **A local pane still takes the old path, synchronously.** Not for lack of
//! generality — for honesty about what the user sees: a pane that is ready in
//! under a millisecond would otherwise paint one frame of a spinner on every
//! ⌘T, which is a flicker introduced to solve a problem local panes do not
//! have. The split is on [`PaneRoute`](crate::terminal::PaneRoute), the same
//! value that decides whether the connection carries a route header at all.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, EventEmitter, FocusHandle, Focusable, SharedString,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::ShellSpec;
use crate::terminal::PaneWorkspace;

/// Everything needed to (re)run the blocking half of a pane spawn.
///
/// Kept whole rather than as loose fields so a retry runs the *same* attempt
/// the first one did: a retry that quietly spawned a fresh pane where the first
/// tried to re-attach to a running one would silently abandon the user's
/// session — which is the single thing session restore exists to prevent.
#[derive(Clone)]
pub struct PendingSpawn {
    pub workspace: Option<PaneWorkspace>,
    pub working_directory: Option<std::path::PathBuf>,
    pub restore_pane: Option<u64>,
    pub shell: Option<ShellSpec>,
    /// The coding agent this leaf was last seen running, its native session id
    /// and the argv it was launched with — carried verbatim from the saved
    /// session.
    ///
    /// Two jobs, both of which need the *fields* rather than a precomputed
    /// resume line:
    ///
    /// 1. If `restore_pane` turns out to be gone, `land_pane` builds the
    ///    `--resume` command from them. Whether it is needed is only known on
    ///    the machine, one round trip away.
    /// 2. A save that happens while this pane is still connecting writes them
    ///    straight back out ([`pane_to_session`](crate::ui::app)). Without
    ///    that, every such save silently erased the agent from the record —
    ///    and a workspace whose sessions were then ended had nothing left to
    ///    resume *from*, which is what made the resume look intermittent.
    pub agent: Option<crate::core::cli_agent::CLIAgent>,
    pub agent_session_id: Option<String>,
    pub agent_launch_argv: Option<Vec<String>>,
    /// The workspace this pane is being created for — carried so the spawn that
    /// finally lands (and any retry) names the same owner the synchronous local
    /// path would have.
    pub owner: Option<crate::core::session::WorkspaceId>,
    /// Inherited by the terminal this becomes, so a pane that arrives late
    /// still matches the ones already on screen.
    pub font_size: f32,
}

/// Where a pending pane is up to.
pub enum PendingState {
    Connecting,
    /// The attempt failed, with the reason as the user should read it. A
    /// resting state: the slot keeps its place in the layout and
    /// offers the next move rather than collapsing the split under the user.
    Failed(SharedString),
}

/// Emitted when the user asks a failed pane to try again. The app owns the
/// respawn because only it can put the result back into the tree.
pub struct RetryRequested;

/// A pane slot whose terminal has not arrived yet.
pub struct PendingPane {
    /// Its own handle, so focus, ⌘W and directional pane movement treat this
    /// slot exactly like a terminal — the tab is not half-broken while one of
    /// its panes is still connecting.
    pub focus_handle: FocusHandle,
    /// What to say we are waiting on. The machine's name, when there is one;
    /// a pane with no machine to name is one this view should not be showing.
    pub machine: SharedString,
    pub state: PendingState,
    /// The attempt, kept for a retry.
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

    /// Land a failure. The reason is shown verbatim — it is the one produced by
    /// `connect_routed`, which is written to name *which* of the four hops gave
    /// up, and paraphrasing it here would throw that away.
    pub fn fail(&mut self, reason: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.state = PendingState::Failed(reason.into());
        cx.notify();
    }

    /// Back to waiting, for a retry.
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

        // On the window background, not a card: this *is* the pane, and a
        // floating panel inside a split would read as a dialog over a terminal
        // that isn't there.
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
                // The hop that gave up, in full: a failure says which of
                // "the daemon isn't running", "that machine refused us" and
                // "the server over there is too old" it was, because they want
                // completely different things from the user.
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
