use super::{SpecItem, TrayAction, TraySnapshot, action_from_id, icon};
use gpui::{AppContext as _, AsyncApp};

pub(super) struct Backend {
    updates: smol::channel::Sender<TraySnapshot>,
}

impl Backend {
    pub(super) async fn create(
        tx: smol::channel::Sender<TrayAction>,
        cx: &AsyncApp,
    ) -> Option<Self> {
        let handle = cx
            .background_spawn(async move {
                use ksni::blocking::TrayMethods as _;
                let tray = SniTray {
                    tx,
                    snap: TraySnapshot::default(),
                };
                match tray.spawn() {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        log::warn!("failed to register StatusNotifierItem: {e}");
                        None
                    }
                }
            })
            .await?;
        let (updates, update_rx) = smol::channel::unbounded::<TraySnapshot>();
        cx.background_spawn(async move {
            while let Ok(mut snap) = update_rx.recv().await {
                while let Ok(later) = update_rx.try_recv() {
                    snap = later;
                }
                handle.update(move |tray| tray.snap = snap);
            }
            handle.shutdown();
        })
        .detach();
        Some(Backend { updates })
    }

    pub(super) fn update(&mut self, snap: &TraySnapshot) {
        let _ = self.updates.try_send(snap.clone());
    }
}

struct SniTray {
    tx: smol::channel::Sender<TrayAction>,
    snap: TraySnapshot,
}

impl SniTray {
    fn send(&self, id: &str) {
        if let Some(action) = action_from_id(id) {
            let _ = self.tx.try_send(action);
        }
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "tty7".into()
    }

    fn title(&self) -> String {
        "tty7".into()
    }

    fn status(&self) -> ksni::Status {
        if self.snap.attention() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Some((data, size)) = icon::render_argb(self.snap.attention()) else {
            return Vec::new();
        };
        vec![ksni::Icon {
            width: size as i32,
            height: size as i32,
            data,
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.snap.tooltip(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowWindow);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        super::menu_spec(&self.snap)
            .into_iter()
            .map(translate)
            .collect()
    }
}

fn translate(item: SpecItem) -> ksni::MenuItem<SniTray> {
    match item {
        SpecItem::Item {
            id,
            label,
            checked: None,
            avatar,
        } => ksni::menu::StandardItem {
            label,
            icon_data: avatar
                .and_then(|(agent, status)| icon::agent_avatar(agent, status))
                .and_then(|pm| pm.encode_png().ok())
                .unwrap_or_default(),
            activate: Box::new(move |tray: &mut SniTray| tray.send(&id)),
            ..Default::default()
        }
        .into(),
        SpecItem::Item {
            id,
            label,
            checked: Some(checked),
            ..
        } => ksni::menu::CheckmarkItem {
            label,
            checked,
            activate: Box::new(move |tray: &mut SniTray| tray.send(&id)),
            ..Default::default()
        }
        .into(),
        SpecItem::Separator => ksni::MenuItem::Separator,
        SpecItem::Submenu { label, items } => ksni::menu::SubMenu {
            label,
            submenu: items.into_iter().map(translate).collect(),
            ..Default::default()
        }
        .into(),
    }
}
