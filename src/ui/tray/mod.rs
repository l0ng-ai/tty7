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

/// A sender for the current tray dispatch loop, if one is running.
// Only the platform notification callbacks call this, and those are compiled
// out of test builds so unit tests never raise a real toast.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn sender() -> Option<smol::channel::Sender<TrayAction>> {
    SENDER.lock().ok()?.clone()
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
                format!(" — {}", t(crate::ui::i18n::L10nKey::TrayAgentNeedsInput))
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
                    cx.update(|cx| cx.quit());
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
                if cx.update(crate::ui::windows::WindowRegistry::count) == 0 {
                    cx.update(|cx| cx.quit());
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
            }],
            notify_mode: NotifyMode::Unfocused,
        }
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
