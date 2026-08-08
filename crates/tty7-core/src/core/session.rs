use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon::protocol::NativeSshSpec;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SessionAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionPane {
    Leaf {
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        pane_id: Option<u64>,
        #[serde(default)]
        ssh_spec: Option<Box<NativeSshSpec>>,
        #[serde(default)]
        agent: Option<crate::core::cli_agent::CLIAgent>,
        #[serde(default)]
        agent_session_id: Option<String>,
        #[serde(default)]
        agent_launch_argv: Option<Vec<String>>,
    },
    Split {
        axis: SessionAxis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<SessionPane>,
        b: Box<SessionPane>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    #[serde(default)]
    pub name: Option<String>,
    pub pane: SessionPane,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_group: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub tree_id: Option<crate::core::machine::TabId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(uuid::Uuid);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn element_key(&self) -> u64 {
        self.0.as_u64_pair().0
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(WorkspaceId)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTarget {
    Profile {
        id: uuid::Uuid,
    },
    Alias {
        alias: String,
    },
    Direct {
        #[serde(default)]
        user: String,
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
    },
    Wsl {
        distro: String,
    },
    LocalStdio {
        program: String,
        args: Vec<String>,
    },
}

fn default_ssh_port() -> u16 {
    22
}

impl RemoteTarget {
    pub fn direct(user: impl Into<String>, host: impl Into<String>, port: u16) -> RemoteTarget {
        RemoteTarget::Direct {
            user: user.into(),
            host: host.into().to_ascii_lowercase(),
            port,
        }
    }

    pub fn parse_direct(input: &str) -> Option<RemoteTarget> {
        let q = crate::core::ssh_profile::parse_quick_connect(input)?;
        let port = q.port_or_default();
        Some(RemoteTarget::direct(
            q.user.unwrap_or_default(),
            q.host,
            port,
        ))
    }

    pub fn connection_key(&self) -> String {
        match self {
            RemoteTarget::Profile { id } => format!("ssh-profile:{id}"),
            RemoteTarget::Alias { alias } => format!("ssh-alias:{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                format!("ssh-direct:{user}@{}:{port}", host.to_ascii_lowercase())
            }
            RemoteTarget::Wsl { distro } => format!("wsl:{distro}"),
            RemoteTarget::LocalStdio { program, args } => {
                format!("local-stdio:{program} {}", args.join(" "))
            }
        }
    }

    pub fn is_ssh(&self) -> bool {
        match self {
            RemoteTarget::Profile { .. }
            | RemoteTarget::Alias { .. }
            | RemoteTarget::Direct { .. } => true,
            RemoteTarget::Wsl { .. } | RemoteTarget::LocalStdio { .. } => false,
        }
    }

    pub fn host_id(&self) -> crate::host::HostId {
        crate::host::HostId::from_connection_key(&self.connection_key())
    }
}

impl std::fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteTarget::Profile { id } => write!(f, "{id}"),
            RemoteTarget::Alias { alias } => write!(f, "{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                if !user.is_empty() {
                    write!(f, "{user}@")?;
                }
                write!(f, "{host}")?;
                if *port != 22 {
                    write!(f, ":{port}")?;
                }
                Ok(())
            }
            RemoteTarget::Wsl { distro } => write!(f, "wsl:{distro}"),
            RemoteTarget::LocalStdio { program, .. } => {
                let name = std::path::Path::new(program)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| program.clone());
                write!(f, "local:{name}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteRef {
    pub target: RemoteTarget,
    pub workspace: WorkspaceId,
}

impl RemoteRef {
    pub fn new(target: RemoteTarget, workspace: WorkspaceId) -> RemoteRef {
        RemoteRef { target, workspace }
    }

    pub fn host_id(&self) -> crate::host::HostId {
        self.target.host_id()
    }

    pub fn store_key(&self) -> String {
        self.workspace.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    #[serde(default)]
    pub id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::core::window_state::WindowState>,
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub last_active: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<RemoteRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl Default for WindowView {
    fn default() -> Self {
        Self {
            id: WorkspaceId::new(),
            window: None,
            open: true,
            last_active: now_secs(),
            host: None,
            label: None,
            subject: None,
        }
    }
}

impl WindowView {
    pub fn touch(&mut self) {
        self.last_active = now_secs();
    }

    pub fn on_remote(host: RemoteRef) -> WindowView {
        WindowView {
            host: Some(host),
            ..WindowView::default()
        }
    }

    pub fn is_remote(&self) -> bool {
        self.host.is_some()
    }

    pub fn host_id(&self) -> crate::host::HostId {
        match &self.host {
            Some(r) => r.host_id(),
            None => crate::host::HostId::LOCAL,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowViews {
    pub views: Vec<WindowView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<WorkspaceId>,
}

impl WindowViews {
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(crate::core::config::strip_bom(&text)) {
            Ok(loaded) => Some(loaded),
            Err(e) => {
                // The next `save` overwrites this file wholesale, so ignoring a
                // corrupt one quietly discards whatever it held. Keep a copy
                // aside first, the way `load_machine` does.
                log::warn!(
                    "failed to parse views at {}: {e}; quarantining it",
                    path.display()
                );
                crate::core::config::quarantine(&path);
                None
            }
        }
    }

    pub fn get(&self, id: WorkspaceId) -> Option<&WindowView> {
        self.views.iter().find(|w| w.id == id)
    }

    pub fn get_mut(&mut self, id: WorkspaceId) -> Option<&mut WindowView> {
        self.views.iter_mut().find(|w| w.id == id)
    }

    pub fn open_views(&self) -> impl Iterator<Item = &WindowView> {
        self.views.iter().filter(|w| w.open)
    }

    pub fn workspace_to_restore(&self) -> Option<WorkspaceId> {
        let focused = self
            .active
            .filter(|id| self.get(*id).is_some_and(|w| w.open));
        focused
            .or_else(|| {
                self.open_views()
                    .max_by_key(|w| w.last_active)
                    .map(|w| w.id)
            })
            .or_else(|| {
                self.views
                    .iter()
                    .max_by_key(|w| w.last_active)
                    .map(|w| w.id)
            })
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create views dir {}: {e}", parent.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize views: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write views to {}: {e}", path.display());
        }
    }

    fn path() -> Option<PathBuf> {
        crate::core::config::config_path("views.json")
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static SESSION_FILE: Mutex<()> = Mutex::new(());

    pub(crate) fn lock_session_file() -> MutexGuard<'static, ()> {
        SESSION_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn pin_config_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir.clone());
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{lock_session_file, pin_config_dir};
    use super::*;

    fn view() -> WindowView {
        WindowView::default()
    }

    fn remote_view(alias: &str) -> WindowView {
        WindowView::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: alias.into(),
            },
            WorkspaceId::new(),
        ))
    }

    #[test]
    fn views_round_trip_through_their_file() {
        let _file = lock_session_file();
        pin_config_dir();
        let mut entry = remote_view("build-box");
        entry.open = false;
        entry.last_active = 1_700_000_000;
        let id = entry.id;
        let host = entry.host.clone();
        WindowViews {
            active: Some(id),
            views: vec![entry],
        }
        .save();
        let loaded = WindowViews::load().expect("a saved views file should load back");
        let only = &loaded.views[0];
        assert_eq!(only.id, id, "identity must survive a restart");
        assert_eq!(
            only.host, host,
            "the remote pointer is the load-bearing half"
        );
        assert!(!only.open);
        assert_eq!(only.last_active, 1_700_000_000);
        assert_eq!(loaded.active, Some(id));
    }

    #[test]
    fn a_corrupt_views_file_is_kept_aside_before_being_ignored() {
        let _file = lock_session_file();
        let dir = pin_config_dir();
        let path = dir.join("views.json");
        let aside = dir.join("views.json.corrupt");
        std::fs::remove_file(&aside).ok();
        std::fs::write(&path, "{ not json").unwrap();

        assert!(
            WindowViews::load().is_none(),
            "a corrupt file yields nothing rather than a guess"
        );
        assert_eq!(
            std::fs::read_to_string(&aside).as_deref().ok(),
            Some("{ not json"),
            "the next save overwrites views.json wholesale, so the old contents \
             must already be parked beside it"
        );

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&aside).ok();
    }

    #[test]
    fn an_empty_or_partial_file_decodes_to_defaults() {
        let empty: WindowViews = serde_json::from_str("{}").unwrap();
        assert!(empty.views.is_empty());
        assert!(empty.active.is_none());
        let partial: WindowViews = serde_json::from_str(r#"{"views":[{}]}"#).unwrap();
        assert_eq!(partial.views.len(), 1);
        assert!(!partial.views[0].is_remote());
    }

    #[test]
    fn connection_keys_match_the_contract_table() {
        let uuid = uuid::Uuid::parse_str("6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01").unwrap();
        assert_eq!(
            RemoteTarget::Profile { id: uuid }.connection_key(),
            "ssh-profile:6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01"
        );
        assert_eq!(
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .connection_key(),
            "ssh-alias:devbox"
        );
        assert_eq!(
            RemoteTarget::direct("me", "box.local", 22).connection_key(),
            "ssh-direct:me@box.local:22"
        );
        assert_eq!(
            RemoteTarget::direct("me", "box.local", 2222).connection_key(),
            "ssh-direct:me@box.local:2222"
        );
        assert_eq!(
            RemoteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .connection_key(),
            "wsl:Ubuntu"
        );
    }

    #[test]
    fn only_ssh_machines_have_a_server_to_restart() {
        assert!(
            RemoteTarget::Profile {
                id: uuid::Uuid::nil()
            }
            .is_ssh()
        );
        assert!(
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .is_ssh()
        );
        assert!(RemoteTarget::direct("me", "box.local", 22).is_ssh());
        assert!(
            !RemoteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .is_ssh(),
            "a distribution's server is started by this client"
        );
        assert!(
            !RemoteTarget::LocalStdio {
                program: "tty7-server".into(),
                args: vec!["--stdio".into()],
            }
            .is_ssh(),
            "a stdio machine is a child process per connection"
        );
    }

    #[test]
    fn direct_targets_normalize_and_reuse_the_quick_connect_parser() {
        assert_eq!(
            RemoteTarget::parse_direct("ssh://me@Box.Local"),
            Some(RemoteTarget::direct("me", "box.local", 22))
        );
        assert_eq!(
            RemoteTarget::parse_direct("me@box.local:2222"),
            Some(RemoteTarget::direct("me", "box.local", 2222))
        );
        let shouty = RemoteTarget::Direct {
            user: "me".into(),
            host: "BOX.LOCAL".into(),
            port: 22,
        };
        assert_eq!(
            shouty.host_id(),
            RemoteTarget::direct("me", "box.local", 22).host_id()
        );
        assert_eq!(RemoteTarget::parse_direct(""), None);
        assert_eq!(RemoteTarget::parse_direct("me@box:0"), None);
        assert_ne!(
            RemoteTarget::Alias {
                alias: "Devbox".into()
            }
            .connection_key(),
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .connection_key()
        );
    }

    #[test]
    fn a_local_stdio_target_is_its_own_machine() {
        let a = RemoteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let b = RemoteTarget::LocalStdio {
            program: "/tmp/other-server".into(),
            args: vec!["--stdio".into()],
        };
        assert_eq!(a.connection_key(), "local-stdio:/opt/tty7-server --stdio");
        assert_ne!(a.host_id(), b.host_id());
        assert!(
            !a.host_id().is_local(),
            "a routed target is never the local host"
        );
        assert_eq!(a.to_string(), "local:tty7-server");
    }

    #[test]
    fn views_on_one_box_share_a_host_id() {
        let target = RemoteTarget::Alias {
            alias: "devbox".into(),
        };
        let a = WindowView::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        let b = WindowView::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        assert_ne!(
            a.host.as_ref().unwrap().workspace,
            b.host.as_ref().unwrap().workspace
        );
        assert_eq!(a.host_id(), b.host_id(), "same machine, one HostId");
        assert!(!a.host_id().is_local());

        let other = remote_view("other");
        assert_ne!(a.host_id(), other.host_id());

        assert_eq!(view().host_id(), crate::host::HostId::LOCAL);
        assert_eq!(
            a.host.as_ref().unwrap().store_key(),
            a.host.as_ref().unwrap().workspace.to_string()
        );
    }

    #[test]
    fn open_views_partition_by_flag() {
        let mut open_one = view();
        open_one.open = true;
        let mut closed = view();
        closed.open = false;
        let open_id = open_one.id;
        let all = WindowViews {
            active: None,
            views: vec![open_one, closed],
        };
        assert_eq!(
            all.open_views().map(|w| w.id).collect::<Vec<_>>(),
            vec![open_id]
        );
    }

    #[test]
    fn launch_restores_the_focused_workspace_not_the_most_recently_touched() {
        let mut focused = view();
        focused.open = true;
        focused.last_active = 100;
        let mut busier = view();
        busier.open = true;
        busier.last_active = 900;
        let (focused_id, busier_id) = (focused.id, busier.id);

        let all = WindowViews {
            active: Some(focused_id),
            views: vec![focused, busier],
        };
        assert_eq!(all.workspace_to_restore(), Some(focused_id));
        assert_eq!(
            all.open_views().count(),
            2,
            "the others stay open in the store — launch detaches them, this does not"
        );

        let all = WindowViews {
            active: None,
            ..all
        };
        assert_eq!(all.workspace_to_restore(), Some(busier_id));

        let mut closed = view();
        closed.open = false;
        let closed_id = closed.id;
        let mut open_one = view();
        open_one.open = true;
        let open_id = open_one.id;
        let all = WindowViews {
            active: Some(closed_id),
            views: vec![closed, open_one],
        };
        assert_eq!(all.workspace_to_restore(), Some(open_id));

        let mut first_closed = view();
        first_closed.open = false;
        first_closed.last_active = 100;
        let mut closed_last = view();
        closed_last.open = false;
        closed_last.last_active = 900;
        let closed_last_id = closed_last.id;
        let all = WindowViews {
            active: None,
            views: vec![first_closed, closed_last],
        };
        assert_eq!(all.workspace_to_restore(), Some(closed_last_id));

        let all = WindowViews {
            active: Some(WorkspaceId::new()),
            ..all
        };
        assert_eq!(all.workspace_to_restore(), Some(closed_last_id));

        assert_eq!(WindowViews::default().workspace_to_restore(), None);
    }

    #[test]
    fn an_open_workspace_outranks_a_more_recently_touched_detached_one() {
        let mut open_one = view();
        open_one.open = true;
        open_one.last_active = 100;
        let open_id = open_one.id;
        let mut detached = view();
        detached.open = false;
        detached.last_active = 900;

        let all = WindowViews {
            active: None,
            views: vec![open_one, detached],
        };
        assert_eq!(all.workspace_to_restore(), Some(open_id));
    }
}
