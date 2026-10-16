mod icon;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;
#[cfg(target_os = "linux")]
mod sni;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use native::Backend;
#[cfg(target_os = "linux")]
use sni::Backend;

use std::sync::Mutex;

use crate::core::cli_agent::AgentStatus;
use crate::core::config::{Config, NotifyMode};
use crate::ui::i18n::{L10nKey, t};
use gpui::{App, AppContext};

const POLL: std::time::Duration = std::time::Duration::from_millis(1000);

/// Handed out to callers that live outside the UI thread — notification click
/// handlers, which run on a platform callback thread and can only enqueue.
///
/// The dispatch loop is created once, for the whole process. A window can be
/// built and retired many times (the tray keeps the app alive windowless), so
/// the channel must survive as long as the app does — it does, because the
/// loop outlives every window.
static SENDER: Mutex<Option<smol::channel::Sender<TrayAction>>> = Mutex::new(None);

/// Whether an icon is actually on the bar right now.
///
/// `show_tray_icon` is a request, not an outcome: `Backend::create` can fail
/// for the whole run (a Linux session with no StatusNotifier host is the
/// ordinary case), and after `MAX_ATTEMPTS` the loop gives up and logs. Asking
/// the config alone whether closing the last window may retire the app would
/// then leave a process with no window and no icon — running, unreachable, and
/// still holding the daemon.
static ICON_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the app still has a visible presence once its last window closes.
pub(crate) fn icon_is_up() -> bool {
    ICON_UP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Actions that arrived before the dispatch loop existed, replayed by `init`.
///
/// A toast click can reach a process before it has a window: Windows launches
/// tty7 over COM to deliver a click on a toast in notification history, and
/// calls in as soon as the class object is registered — well before the first
/// window, and with it this loop, is up. Bounded so a process whose loop never
/// starts cannot grow it without limit; the oldest click is the one to lose.
static PENDING: Mutex<std::collections::VecDeque<TrayAction>> =
    Mutex::new(std::collections::VecDeque::new());
const MAX_PENDING: usize = 8;

/// Hand `action` to the tray dispatch loop, or hold it until the loop starts.
// Only the platform notification callbacks call this, and Linux has none.
#[cfg_attr(
    any(test, not(any(target_os = "macos", target_os = "windows"))),
    allow(dead_code)
)]
pub(crate) fn send_or_queue(action: TrayAction) {
    // Checked under the SENDER lock, which `init` also holds while it drains
    // the queue, so an action can never land in the queue after the drain.
    let Ok(sender) = SENDER.lock() else { return };
    if let Some(tx) = sender.as_ref() {
        let _ = tx.try_send(action);
        return;
    }
    if let Ok(mut pending) = PENDING.lock() {
        if pending.len() == MAX_PENDING {
            pending.pop_front();
        }
        pending.push_back(action);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    ShowWindow,
    RevealPane { leaf_id: u64 },
    SetNotifyMode(NotifyMode),
    OpenSettings,
    CheckForUpdates,
    Quit,
}

pub(crate) fn urgency(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Waiting => 3,
        AgentStatus::Working => 2,
        AgentStatus::Done => 1,
        AgentStatus::Idle => 0,
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AgentRow {
    pub leaf_id: u64,
    pub agent: crate::core::cli_agent::CLIAgent,
    pub status: AgentStatus,
    pub detail: String,
    /// When the agent entered `status`, by its daemon's clock.
    pub since_ms: Option<u64>,
    /// A finished turn whose result nobody has looked at yet.
    pub unread: bool,
    /// "waiting 3m" on a waiting row, already rendered: in whole minutes, so
    /// the snapshot (and with it the menu) changes once a minute at most.
    pub waited: Option<String>,
}

/// The leaf to reveal next when the user asks for the agent that needs them.
///
/// Waiting agents come first, the one that has waited longest at the head;
/// then turns that finished with nobody looking. Pressing again from one of
/// them moves to the one after it, wrapping, so every row in the queue is
/// reached in turn. From anywhere else (a pane outside the queue, or one that
/// just left it because looking at it marked its turn read) it starts over at
/// the head, which is the most overdue row still left.
///
/// `rows` come in window, tab and pane order. That order breaks ties, and a
/// row with no `since_ms` (from a daemon that predates it) queues behind the
/// stamped ones of its kind.
pub(crate) fn next_needing_attention(rows: &[AgentRow], current: Option<u64>) -> Option<u64> {
    let mut queue: Vec<&AgentRow> = rows
        .iter()
        .filter(|r| match r.status {
            AgentStatus::Waiting => true,
            AgentStatus::Done => r.unread,
            AgentStatus::Working | AgentStatus::Idle => false,
        })
        .collect();
    queue.sort_by_key(|r| {
        (
            r.status != AgentStatus::Waiting,
            r.since_ms.is_none(),
            r.since_ms,
        )
    });
    let at = current.and_then(|id| queue.iter().position(|r| r.leaf_id == id));
    let next = match at {
        Some(i) => queue.get(i + 1).or(queue.first()),
        None => queue.first(),
    };
    next.map(|r| r.leaf_id)
}

/// Reveal a pane in whichever window owns it, the way picking it from the
/// tray menu does.
pub(crate) fn reveal_pane(leaf_id: u64, cx: &mut App) {
    dispatch(TrayAction::RevealPane { leaf_id }, cx);
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct TraySnapshot {
    pub agents: Vec<AgentRow>,
    pub notify_mode: NotifyMode,
}

impl TraySnapshot {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn attention(&self) -> bool {
        self.agents.iter().any(|a| a.status == AgentStatus::Waiting)
    }

    pub(crate) fn tooltip(&self) -> String {
        let count = |s: AgentStatus| self.agents.iter().filter(|a| a.status == s).count();
        let mut parts = Vec::new();
        for (n, word) in [
            (
                count(AgentStatus::Waiting),
                t(crate::ui::i18n::L10nKey::PanelAgentWaiting),
            ),
            (
                count(AgentStatus::Working),
                t(crate::ui::i18n::L10nKey::PanelAgentWorking),
            ),
            (
                count(AgentStatus::Done),
                t(crate::ui::i18n::L10nKey::PanelAgentDone),
            ),
        ] {
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        if parts.is_empty() {
            "tty7".to_string()
        } else {
            crate::ui::i18n::t_fmt(
                L10nKey::TrayTooltipAgents,
                &[("parts", &parts.join(t(L10nKey::TrayAgentSep)))],
            )
        }
    }
}

pub(crate) enum SpecItem {
    Item {
        id: String,
        label: String,
        checked: Option<bool>,
        avatar: Option<(crate::core::cli_agent::CLIAgent, AgentStatus)>,
    },
    Separator,
    Submenu {
        label: String,
        items: Vec<SpecItem>,
    },
}

pub(crate) fn menu_spec(snap: &TraySnapshot) -> Vec<SpecItem> {
    let item = |id: &str, label: &str| SpecItem::Item {
        id: id.to_string(),
        label: label.to_string(),
        checked: None,
        avatar: None,
    };
    let mut items = vec![item("show", t(L10nKey::TrayShowTty7)), SpecItem::Separator];
    for a in &snap.agents {
        let state = match a.status {
            AgentStatus::Waiting => {
                let needs = t(crate::ui::i18n::L10nKey::TrayAgentNeedsInput);
                match &a.waited {
                    Some(waited) => format!(" — {needs} · {waited}"),
                    None => format!(" — {needs}"),
                }
            }
            AgentStatus::Working => {
                format!(" — {}", t(crate::ui::i18n::L10nKey::PanelAgentWorking))
            }
            AgentStatus::Done => format!(" — {}", t(crate::ui::i18n::L10nKey::PanelAgentDone)),
            AgentStatus::Idle => String::new(),
        };
        items.push(SpecItem::Item {
            id: format!("agent:{}", a.leaf_id),
            label: format!("{} · {}{state}", a.agent.display_name(), a.detail),
            checked: None,
            avatar: Some((a.agent, a.status)),
        });
    }
    if !snap.agents.is_empty() {
        items.push(SpecItem::Separator);
    }
    let notify = |id: &str, label: &str, mode: NotifyMode| SpecItem::Item {
        id: id.to_string(),
        label: label.to_string(),
        checked: Some(snap.notify_mode == mode),
        avatar: None,
    };
    items.push(SpecItem::Submenu {
        label: t(L10nKey::TrayNotifications).to_string(),
        items: vec![
            notify(
                "notify:never",
                t(L10nKey::NotifyModeNever),
                NotifyMode::Never,
            ),
            notify(
                "notify:unfocused",
                t(L10nKey::NotifyModeUnfocused),
                NotifyMode::Unfocused,
            ),
            notify(
                "notify:always",
                t(L10nKey::NotifyModeAlways),
                NotifyMode::Always,
            ),
        ],
    });
    items.push(item("settings", t(L10nKey::AppMenuSettings)));
    items.push(item("updates", t(L10nKey::AppMenuCheckForUpdates)));
    items.push(SpecItem::Separator);
    // One exit path with one meaning: quit the app and stop the server. The
    // window-close button is the keep-everything exit — it retires to the tray.
    items.push(item("quit", t(L10nKey::AppMenuQuit)));
    items
}

pub(crate) fn action_from_id(id: &str) -> Option<TrayAction> {
    match id {
        "show" => Some(TrayAction::ShowWindow),
        "settings" => Some(TrayAction::OpenSettings),
        "updates" => Some(TrayAction::CheckForUpdates),
        "quit" => Some(TrayAction::Quit),
        "notify:always" => Some(TrayAction::SetNotifyMode(NotifyMode::Always)),
        "notify:unfocused" => Some(TrayAction::SetNotifyMode(NotifyMode::Unfocused)),
        "notify:never" => Some(TrayAction::SetNotifyMode(NotifyMode::Never)),
        _ => {
            let leaf_id = id.strip_prefix("agent:")?.parse().ok()?;
            Some(TrayAction::RevealPane { leaf_id })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_agent(status: AgentStatus) -> TraySnapshot {
        TraySnapshot {
            agents: vec![AgentRow {
                leaf_id: 42,
                agent: crate::core::cli_agent::CLIAgent::Claude,
                status,
                detail: "tty7 @ main".into(),
                since_ms: None,
                unread: false,
                waited: None,
            }],
            notify_mode: NotifyMode::Unfocused,
        }
    }

    fn row(leaf_id: u64, status: AgentStatus, since_ms: Option<u64>, unread: bool) -> AgentRow {
        AgentRow {
            leaf_id,
            agent: crate::core::cli_agent::CLIAgent::Claude,
            status,
            detail: String::new(),
            since_ms,
            unread,
            waited: None,
        }
    }

    #[test]
    fn the_longest_wait_comes_first_then_unread_results() {
        let rows = [
            row(1, AgentStatus::Done, Some(10), true),
            row(2, AgentStatus::Waiting, Some(500), false),
            row(3, AgentStatus::Working, Some(1), false),
            row(4, AgentStatus::Waiting, Some(100), false),
            row(5, AgentStatus::Done, Some(5), false),
            row(6, AgentStatus::Idle, None, false),
        ];
        // From a pane outside the queue, or from nowhere: the oldest wait.
        assert_eq!(next_needing_attention(&rows, Some(3)), Some(4));
        assert_eq!(next_needing_attention(&rows, None), Some(4));
        // Then the newer wait, then the unread result, then around again.
        assert_eq!(next_needing_attention(&rows, Some(4)), Some(2));
        assert_eq!(next_needing_attention(&rows, Some(2)), Some(1));
        assert_eq!(next_needing_attention(&rows, Some(1)), Some(4));
    }

    #[test]
    fn a_result_read_on_arrival_hands_back_to_the_head() {
        // Revealing pane 1 marked its turn read, so it left the queue. The
        // next press starts again from the oldest wait rather than stalling.
        let rows = [
            row(1, AgentStatus::Done, Some(10), false),
            row(2, AgentStatus::Waiting, Some(500), false),
        ];
        assert_eq!(next_needing_attention(&rows, Some(1)), Some(2));
        // A queue of one stays where it is.
        assert_eq!(next_needing_attention(&rows, Some(2)), Some(2));
    }

    #[test]
    fn an_unstamped_wait_queues_behind_stamped_ones_in_pane_order() {
        let rows = [
            row(7, AgentStatus::Waiting, None, false),
            row(8, AgentStatus::Waiting, None, false),
            row(9, AgentStatus::Waiting, Some(900), false),
        ];
        assert_eq!(next_needing_attention(&rows, None), Some(9));
        assert_eq!(next_needing_attention(&rows, Some(9)), Some(7));
        assert_eq!(next_needing_attention(&rows, Some(7)), Some(8));
    }

    #[test]
    fn nothing_to_attend_to_reveals_nothing() {
        let rows = [
            row(1, AgentStatus::Working, Some(1), false),
            row(2, AgentStatus::Done, Some(2), false),
        ];
        assert_eq!(next_needing_attention(&rows, None), None);
        assert_eq!(next_needing_attention(&[], Some(1)), None);
    }

    #[test]
    fn a_waiting_row_says_how_long() {
        crate::ui::i18n::set_locale("en");
        let mut snap = snapshot_with_agent(AgentStatus::Waiting);
        snap.agents[0].waited = Some("waiting 3m".into());
        let label = menu_spec(&snap)
            .into_iter()
            .find_map(|i| match i {
                SpecItem::Item { id, label, .. } if id == "agent:42" => Some(label),
                _ => None,
            })
            .unwrap();
        assert!(label.ends_with("needs input · waiting 3m"), "{label}");
    }

    #[test]
    fn every_menu_id_decodes_to_an_action() {
        fn check(items: &[SpecItem]) {
            for item in items {
                match item {
                    SpecItem::Item { id, label, .. } => assert!(
                        action_from_id(id).is_some(),
                        "menu item {label:?} has undecodable id {id:?}"
                    ),
                    SpecItem::Separator => {}
                    SpecItem::Submenu { items, .. } => check(items),
                }
            }
        }
        check(&menu_spec(&snapshot_with_agent(AgentStatus::Waiting)));
        check(&menu_spec(&TraySnapshot::default()));
    }

    #[test]
    fn agent_rows_decode_to_reveal_with_their_leaf_id() {
        assert_eq!(
            action_from_id("agent:42"),
            Some(TrayAction::RevealPane { leaf_id: 42 })
        );
        assert_eq!(action_from_id("agent:nope"), None);
        assert_eq!(action_from_id("bogus"), None);
    }

    #[test]
    fn attention_follows_waiting_and_tooltip_counts() {
        crate::ui::i18n::set_locale("en");
        assert!(snapshot_with_agent(AgentStatus::Waiting).attention());
        assert!(!snapshot_with_agent(AgentStatus::Working).attention());
        assert!(!snapshot_with_agent(AgentStatus::Done).attention());
        assert_eq!(
            snapshot_with_agent(AgentStatus::Waiting).tooltip(),
            format!(
                "tty7 — 1 {}",
                t(crate::ui::i18n::L10nKey::PanelAgentWaiting)
            )
        );
        assert_eq!(TraySnapshot::default().tooltip(), "tty7");
    }

    #[test]
    fn menu_spec_shape() {
        crate::ui::i18n::set_locale("en");
        let empty = menu_spec(&TraySnapshot::default());
        let labels: Vec<_> = empty
            .iter()
            .filter_map(|i| match i {
                SpecItem::Item { label, .. } => Some(label.as_str()),
                SpecItem::Submenu { label, .. } => Some(label.as_str()),
                SpecItem::Separator => None,
            })
            .collect();
        assert_eq!(
            labels,
            [
                t(L10nKey::TrayShowTty7),
                t(L10nKey::TrayNotifications),
                t(L10nKey::AppMenuSettings),
                t(L10nKey::AppMenuCheckForUpdates),
                t(L10nKey::AppMenuQuit),
            ]
        );
        assert!(
            !empty
                .windows(2)
                .any(|w| matches!(w, [SpecItem::Separator, SpecItem::Separator]))
        );

        let with_agent = menu_spec(&snapshot_with_agent(AgentStatus::Waiting));
        assert!(with_agent.iter().any(|i| matches!(
            i,
            SpecItem::Item { id, avatar: Some(_), .. } if id == "agent:42"
        )));
    }
}

fn app_snapshot(cx: &mut App) -> TraySnapshot {
    let windows = crate::ui::windows::WindowRegistry::open_windows(cx);
    let mut agents = Vec::new();
    for (_, weak) in windows {
        let Some(app) = weak.upgrade() else { continue };
        agents.extend(app.read(cx).agent_rows(cx));
    }
    agents.sort_by_key(|a| std::cmp::Reverse(urgency(a.status)));
    TraySnapshot {
        agents,
        notify_mode: cx.global::<Config>().notify_on_command_finish,
    }
}

fn dispatch(action: TrayAction, cx: &mut App) {
    use crate::ui::windows::WindowRegistry;

    let target = match action {
        TrayAction::RevealPane { leaf_id } => WindowRegistry::open_windows(cx)
            .into_iter()
            .find(|(_, weak)| {
                weak.upgrade()
                    .is_some_and(|app| app.read(cx).owns_leaf(leaf_id))
            })
            .map(|(workspace, _)| workspace),
        _ => None,
    }
    .or_else(|| WindowRegistry::most_recent(cx));

    let Some(workspace) = target else {
        // No window on screen. The tray is then the app's only presence, so
        // bring the most recent workspace back the way a pathless launch would
        // and let the action run against it.
        //
        // Quit especially: retiring to the tray leaves every shell running, so
        // by the time the tray menu is the only way in there can be a whole
        // session behind it. `quit_stop_sessions` — "anything still running in
        // your shells is terminated" — is the one thing standing between this
        // menu item and all of it, and a confirmation needs a window to appear
        // in. Stopping the server here instead would take the shells with no
        // way to say no.
        let restore = crate::ui::windows::restore_target(cx, None);
        crate::ui::windows::open_at(cx, restore.map(|(id, _)| id), None);
        match WindowRegistry::most_recent(cx) {
            Some(restored) => deliver(action, restored, cx),
            // The window would not open, so nothing can be confirmed in it.
            // Quit still has to work — a tray whose only exit is inert strands
            // the app — but every other action wanted the window it failed to
            // get.
            None if matches!(action, TrayAction::Quit) => {
                log::warn!("no window to confirm the quit in; stopping the server anyway");
                cx.spawn(async move |cx| {
                    cx.background_spawn(async { crate::daemon::spawn::stop() })
                        .await;
                    let _ = cx.update(|cx| cx.quit());
                })
                .detach();
            }
            None => {}
        }
        return;
    };
    deliver(action, workspace, cx);
}

/// Hand an action to the window that owns `workspace`.
fn deliver(action: TrayAction, workspace: tty7_core::core::session::WorkspaceId, cx: &mut App) {
    use crate::ui::windows::WindowRegistry;

    let (Some(handle), Some(weak)) = (
        WindowRegistry::window_for(cx, workspace),
        WindowRegistry::app_for(cx, workspace),
    ) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        if let Some(app) = weak.upgrade() {
            app.update(cx, |app, cx| app.handle_tray_action(action, window, cx));
        }
    });
}

pub(crate) fn init(cx: &mut App) {
    // One tray per process. Every window's constructor calls this, and the
    // tray-persist flow builds and retires windows without the app ever
    // exiting — a second backend loop here would add a second icon that the
    // window close cannot remove. The channel and both loops are created
    // exactly once; later calls must not touch them.
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }

    let (tx, rx) = smol::channel::unbounded::<TrayAction>();
    if let Ok(mut slot) = SENDER.lock() {
        if let Ok(mut pending) = PENDING.lock() {
            for action in pending.drain(..) {
                let _ = tx.try_send(action);
            }
        }
        *slot = Some(tx.clone());
    }

    cx.spawn(async move |cx| {
        while let Ok(action) = rx.recv().await {
            cx.update(|cx| dispatch(action, cx));
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        let mut backend: Option<Backend> = None;
        let mut shown: Option<TraySnapshot> = None;
        const MAX_ATTEMPTS: u32 = 10;
        const RETRY_EVERY: u32 = 30;
        let mut attempts = 0u32;
        let mut cooldown = 0u32;
        loop {
            cx.background_executor().timer(POLL).await;
            let (enabled, snap) =
                cx.update(|cx| (cx.global::<Config>().show_tray_icon, app_snapshot(cx)));
            if !enabled {
                backend = None;
                shown = None;
                attempts = 0;
                cooldown = 0;
                ICON_UP.store(false, std::sync::atomic::Ordering::Relaxed);
                // Closing the last window only keeps the app alive because the
                // tray is its window. If the icon is turned off while no
                // window is up (a config edit is the only way there), staying
                // would leave an invisible process that can never be reached.
                if cx.update(|cx| crate::ui::windows::WindowRegistry::count(cx)) == 0 {
                    let _ = cx.update(|cx| cx.quit());
                    continue;
                }
                continue;
            }
            if backend.is_none() && attempts < MAX_ATTEMPTS {
                if cooldown > 0 {
                    cooldown -= 1;
                    continue;
                }
                attempts += 1;
                backend = Backend::create(tx.clone(), cx).await;
                ICON_UP.store(backend.is_some(), std::sync::atomic::Ordering::Relaxed);
                if backend.is_none() {
                    cooldown = RETRY_EVERY;
                    if attempts == MAX_ATTEMPTS {
                        log::warn!(
                            "tray icon unavailable after {MAX_ATTEMPTS} attempts; \
                             running without one — the last window closing now quits"
                        );
                    }
                }
                shown = None;
            }
            if let Some(backend) = backend.as_mut()
                && shown.as_ref() != Some(&snap)
            {
                backend.update(&snap);
                shown = Some(snap);
            }
        }
    })
    .detach();
}
