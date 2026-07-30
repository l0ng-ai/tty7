use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::host::local::LocalHost;

pub use tty7_core::host::{Host, HostId, SharedHost};

pub struct HostRegistry {
    hosts: HashMap<HostId, SharedHost>,
}

impl Global for HostRegistry {}

impl Default for HostRegistry {
    fn default() -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(HostId::LOCAL, LocalHost::shared());
        HostRegistry { hosts }
    }
}

impl HostRegistry {
    pub fn get(cx: &mut App, id: HostId) -> Option<SharedHost> {
        cx.default_global::<HostRegistry>().hosts.get(&id).cloned()
    }

    pub fn lookup(cx: &App, id: HostId) -> Option<SharedHost> {
        match cx.try_global::<HostRegistry>() {
            Some(reg) => reg.hosts.get(&id).cloned(),
            None => id.is_local().then(LocalHost::shared),
        }
    }

    pub fn local(cx: &mut App) -> SharedHost {
        HostRegistry::get(cx, HostId::LOCAL).expect("the local host is always registered")
    }

    pub fn insert(cx: &mut App, host: SharedHost) -> Option<SharedHost> {
        let id = host.id();
        cx.default_global::<HostRegistry>().hosts.insert(id, host)
    }

    pub fn remove(cx: &mut App, id: HostId) -> Option<SharedHost> {
        if id.is_local() {
            return None;
        }
        cx.default_global::<HostRegistry>().hosts.remove(&id)
    }

    pub fn ids(cx: &mut App) -> Vec<HostId> {
        let mut ids: Vec<HostId> = cx
            .default_global::<HostRegistry>()
            .hosts
            .keys()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    pub fn len(cx: &mut App) -> usize {
        cx.default_global::<HostRegistry>().hosts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_registry_already_holds_the_local_host() {
        let reg = HostRegistry::default();
        let local = reg.hosts.get(&HostId::LOCAL).expect("local is present");
        assert!(local.id().is_local());
        assert_eq!(reg.hosts.len(), 1);
    }

    #[test]
    fn remote_hosts_come_and_go_but_local_stays() {
        let mut reg = HostRegistry::default();
        let id = HostId::from_connection_key("ssh-direct:me@box:22");
        reg.hosts.insert(id, LocalHost::new());
        assert_eq!(reg.hosts.len(), 2);

        assert!(reg.hosts.remove(&id).is_some());
        assert!(reg.hosts.contains_key(&HostId::LOCAL));
        assert!(reg.hosts.get(&id).is_none());
    }
}
