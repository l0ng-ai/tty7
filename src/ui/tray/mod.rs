mod icon;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;
#[cfg(target_os = "linux")]
mod sni;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use native::Backend;
#[cfg(target_os = "linux")]
use sni::Backend;

use crate::core::cli_agent::AgentStatus;
use crate::core::config::{Config, NotifyMode};
use gpui::App;

const POLL: std::time::Duration = std::time::Duration::from_millis(1000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    ShowWindow,
    RevealPane { leaf_id: u64 },
    SetNotifyMode(NotifyMode),
    OpenSettings,
    CheckForUpdates,
    Quit,
    QuitStopSessions,
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
            (count(AgentStatus::Waiting), "waiting"),
            (count(AgentStatus::Working), "working"),
            (count(AgentStatus::Done), "done"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        if parts.is_empty() {
            "tty7".to_string()
        } else {
            format!("tty7 — {}", parts.join(", "))
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
    let item = |id: &str, label: String| SpecItem::Item {
        id: id.to_string(),
        label,
        checked: None,
        avatar: None,
    };
    let mut items = vec![item("show", "Show tty7".into()), SpecItem::Separator];
    for a in &snap.agents {
        let state = match a.status {
            AgentStatus::Waiting => " — needs input",
            AgentStatus::Working => " — working",
            AgentStatus::Done => " — done",
            AgentStatus::Idle => "",
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
        label: "Notifications".into(),
        items: vec![
            notify("notify:never", "Never", NotifyMode::Never),
            notify("notify:unfocused", "When Unfocused", NotifyMode::Unfocused),
            notify("notify:always", "Always", NotifyMode::Always),
        ],
    });
    items.push(item("settings", "Settings…".into()));
    items.push(item("updates", "Check for Updates…".into()));
    items.push(SpecItem::Separator);
    items.push(item("quit", "Quit tty7".into()));
    items.push(item("quit-stop", "Quit and Stop Daemon…".into()));
    items
}

pub(crate) fn action_from_id(id: &str) -> Option<TrayAction> {
    match id {
        "show" => Some(TrayAction::ShowWindow),
        "settings" => Some(TrayAction::OpenSettings),
        "updates" => Some(TrayAction::CheckForUpdates),
        "quit" => Some(TrayAction::Quit),
        "quit-stop" => Some(TrayAction::QuitStopSessions),
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
        assert!(snapshot_with_agent(AgentStatus::Waiting).attention());
        assert!(!snapshot_with_agent(AgentStatus::Working).attention());
        assert!(!snapshot_with_agent(AgentStatus::Done).attention());
        assert_eq!(
            snapshot_with_agent(AgentStatus::Waiting).tooltip(),
            "tty7 — 1 waiting"
        );
        assert_eq!(TraySnapshot::default().tooltip(), "tty7");
    }

    #[test]
    fn menu_spec_shape() {
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
                "Show tty7",
                "Notifications",
                "Settings…",
                "Check for Updates…",
                "Quit tty7",
                "Quit and Stop Daemon…"
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
        if matches!(action, TrayAction::Quit) {
            cx.quit();
        }
        return;
    };
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
    let (tx, rx) = smol::channel::unbounded::<TrayAction>();

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
                continue;
            }
            if backend.is_none() && attempts < MAX_ATTEMPTS {
                if cooldown > 0 {
                    cooldown -= 1;
                    continue;
                }
                attempts += 1;
                backend = Backend::create(tx.clone(), cx).await;
                if backend.is_none() {
                    cooldown = RETRY_EVERY;
                    if attempts == MAX_ATTEMPTS {
                        log::warn!(
                            "tray icon unavailable after {MAX_ATTEMPTS} attempts; \
                             running without one"
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
