use super::{SpecItem, TrayAction, TraySnapshot, action_from_id, icon};
use gpui::AsyncApp;
use tray_icon::menu::{
    CheckMenuItem, IconMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub(super) struct Backend {
    tray: TrayIcon,
    #[cfg(not(target_os = "macos"))]
    attention: bool,
}

impl Backend {
    pub(super) async fn create(
        tx: smol::channel::Sender<TrayAction>,
        _cx: &AsyncApp,
    ) -> Option<Self> {
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some(action) = action_from_id(&event.id().0) {
                let _ = tx.try_send(action);
            }
        }));

        #[cfg(target_os = "macos")]
        let img = icon::render()?;
        #[cfg(not(target_os = "macos"))]
        let img = icon::render(false)?;
        let icon = Icon::from_rgba(img.data, img.width, img.height).ok()?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_tooltip("tty7")
            .with_menu(Box::new(build_menu(&TraySnapshot::default())))
            .with_menu_on_left_click(true)
            .build();
        let tray = match tray {
            Ok(tray) => tray,
            Err(e) => {
                log::warn!("failed to create tray icon: {e}");
                return None;
            }
        };

        #[cfg(target_os = "macos")]
        if let Some(status_item) = tray.ns_status_item() {
            if let Some(mtm) = objc2::MainThreadMarker::new() {
                if let Some(button) = status_item.button(mtm) {
                    if let Some(nsimage) = button.image() {
                        let target_h: f64 = 22.0;
                        let aspect = nsimage.size().width / nsimage.size().height;
                        nsimage.setSize(objc2_foundation::NSSize::new(target_h * aspect, target_h));
                    }
                }
            }
        }
        Some(Self {
            tray,
            #[cfg(not(target_os = "macos"))]
            attention: false,
        })
    }

    pub(super) fn update(&mut self, snap: &TraySnapshot) {
        self.tray.set_menu(Some(Box::new(build_menu(snap))));
        let _ = self.tray.set_tooltip(Some(snap.tooltip()));
        #[cfg(not(target_os = "macos"))]
        {
            let attention = snap.attention();
            if attention != self.attention {
                self.attention = attention;
                if let Some(img) = icon::render(attention)
                    && let Ok(icon) = Icon::from_rgba(img.data, img.width, img.height)
                {
                    let _ = self.tray.set_icon(Some(icon));
                }
            }
        }
    }
}

fn build_menu(snap: &TraySnapshot) -> Menu {
    let menu = Menu::new();
    for item in super::menu_spec(snap) {
        append(&menu, &item);
    }
    menu
}

fn append(menu: &Menu, item: &SpecItem) {
    let appended = match item {
        SpecItem::Item { .. } => menu.append(leaf_item(item).as_ref()),
        SpecItem::Separator => menu.append(&PredefinedMenuItem::separator()),
        SpecItem::Submenu { label, items } => {
            let sub = Submenu::new(label, true);
            for child in items {
                let child: Box<dyn IsMenuItem> = match child {
                    SpecItem::Item { .. } => leaf_item(child),
                    _ => Box::new(PredefinedMenuItem::separator()),
                };
                if let Err(e) = sub.append(child.as_ref()) {
                    log::warn!("tray submenu item failed to append: {e}");
                }
            }
            menu.append(&sub)
        }
    };
    if let Err(e) = appended {
        log::warn!("tray menu item failed to append: {e}");
    }
}

fn leaf_item(item: &SpecItem) -> Box<dyn IsMenuItem> {
    let SpecItem::Item {
        id,
        label,
        checked,
        avatar,
    } = item
    else {
        return Box::new(PredefinedMenuItem::separator());
    };
    if let Some(checked) = checked {
        return Box::new(CheckMenuItem::with_id(
            id.clone(),
            label,
            true,
            *checked,
            None,
        ));
    }
    if let Some((agent, status)) = avatar {
        let rendered = icon::agent_avatar(*agent, *status).map(|pm| {
            let img = icon::to_rgba(&pm);
            tray_icon::menu::Icon::from_rgba(img.data, img.width, img.height)
        });
        if let Some(Ok(avatar_icon)) = rendered {
            return Box::new(IconMenuItem::with_id(
                id.clone(),
                label,
                true,
                Some(avatar_icon),
                None,
            ));
        }
    }
    Box::new(MenuItem::with_id(id.clone(), label, true, None))
}
