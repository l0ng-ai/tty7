use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::daemon::protocol::{
    LoopbackForward, LoopbackForwardId, LoopbackForwardInfo, RemoteContext, RemoteKind,
};

const FORWARD_IDLE_TTL: Duration = Duration::from_secs(60 * 60);
const FORWARD_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const FORWARD_STARTUP_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ForwardKey {
    pane_id: u64,
    target: String,
    remote_host: String,
    remote_port: u16,
}

struct ForwardEntry {
    local_port: u16,
    control_path: PathBuf,
    created_at: Instant,
    last_used: Instant,
}

#[derive(Default)]
pub(crate) struct ForwardManager {
    entries: Mutex<HashMap<ForwardKey, ForwardEntry>>,
}

impl ForwardManager {
    pub(crate) fn global() -> &'static Self {
        static MANAGER: OnceLock<ForwardManager> = OnceLock::new();
        MANAGER.get_or_init(ForwardManager::default)
    }

    pub(crate) fn ensure(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        remote_host: &str,
        remote_port: u16,
    ) -> anyhow::Result<LoopbackForward> {
        if remote.kind != RemoteKind::Ssh {
            anyhow::bail!("foreground remote context is not ssh");
        }
        if !is_loopback_forward_host(remote_host) {
            anyhow::bail!("only loopback hosts can be forwarded");
        }
        let control_path = remote
            .control_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pane is not a tty7-managed ssh session"))?;

        let key = ForwardKey {
            pane_id,
            target: remote.target.clone(),
            remote_host: remote_host.to_string(),
            remote_port,
        };

        let mut entries = self.entries.lock().unwrap();
        prune_dead_or_idle(&mut entries);
        if let Some(entry) = entries.get_mut(&key) {
            entry.last_used = Instant::now();
            return Ok(LoopbackForward {
                local_port: entry.local_port,
            });
        }

        let local_port = reserve_local_port()?;
        let mut cmd = build_ssh_control_command(
            control_path,
            "forward",
            &remote.target,
            "127.0.0.1",
            local_port,
            remote_host,
            remote_port,
        );
        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("ssh master failed to open local port {local_port}: {status}");
        }
        if let Err(err) = wait_for_forward_listener(local_port) {
            cancel_forward(
                &LoopbackForwardId {
                    pane_id,
                    target: remote.target.clone(),
                    remote_host: remote_host.to_string(),
                    remote_port,
                },
                local_port,
                control_path,
            );
            return Err(err);
        }
        entries.insert(
            key,
            ForwardEntry {
                local_port,
                control_path: control_path.clone(),
                created_at: Instant::now(),
                last_used: Instant::now(),
            },
        );
        Ok(LoopbackForward { local_port })
    }

    pub(crate) fn list(&self) -> Vec<LoopbackForwardInfo> {
        let mut entries = self.entries.lock().unwrap();
        prune_dead_or_idle(&mut entries);
        let mut list: Vec<_> = entries
            .iter()
            .map(|(key, entry)| LoopbackForwardInfo {
                id: key.to_id(),
                local_port: entry.local_port,
                age_secs: entry.created_at.elapsed().as_secs(),
                idle_secs: entry.last_used.elapsed().as_secs(),
            })
            .collect();
        list.sort_by(|a, b| {
            a.id.target
                .cmp(&b.id.target)
                .then_with(|| a.id.remote_host.cmp(&b.id.remote_host))
                .then_with(|| a.id.remote_port.cmp(&b.id.remote_port))
                .then_with(|| a.local_port.cmp(&b.local_port))
        });
        list
    }

    pub(crate) fn close(&self, id: &LoopbackForwardId) -> bool {
        let mut entries = self.entries.lock().unwrap();
        prune_dead_or_idle(&mut entries);
        let Some((_, entry)) = entries.remove_entry(&ForwardKey::from_id(id)) else {
            return false;
        };
        cancel_forward(id, entry.local_port, &entry.control_path);
        true
    }
}

impl ForwardKey {
    fn to_id(&self) -> LoopbackForwardId {
        LoopbackForwardId {
            pane_id: self.pane_id,
            target: self.target.clone(),
            remote_host: self.remote_host.clone(),
            remote_port: self.remote_port,
        }
    }

    fn from_id(id: &LoopbackForwardId) -> Self {
        Self {
            pane_id: id.pane_id,
            target: id.target.clone(),
            remote_host: id.remote_host.clone(),
            remote_port: id.remote_port,
        }
    }
}

fn prune_dead_or_idle(entries: &mut HashMap<ForwardKey, ForwardEntry>) {
    let dead_or_idle: Vec<_> = entries
        .iter_mut()
        .filter_map(|(key, entry)| {
            let idle = entry.last_used.elapsed() > FORWARD_IDLE_TTL;
            let master_alive = control_master_alive(&entry.control_path, &key.target);
            (idle || !master_alive).then(|| key.clone())
        })
        .collect();
    for key in dead_or_idle {
        if let Some(entry) = entries.remove(&key) {
            cancel_forward(&key.to_id(), entry.local_port, &entry.control_path);
        }
    }
}

fn reserve_local_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_forward_listener(local_port: u16) -> anyhow::Result<()> {
    let deadline = Instant::now() + FORWARD_STARTUP_TIMEOUT;
    loop {
        if TcpStream::connect(("127.0.0.1", local_port)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "ssh master did not open local port {local_port} within {:?}",
                FORWARD_STARTUP_TIMEOUT
            );
        }
        std::thread::sleep(FORWARD_STARTUP_POLL);
    }
}

fn build_ssh_control_command(
    control_path: &PathBuf,
    operation: &str,
    target: &str,
    local_host: &str,
    local_port: u16,
    remote_host: &str,
    remote_port: u16,
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg(operation)
        .arg("-L")
        .arg(format!(
            "{local_host}:{local_port}:{remote_host}:{remote_port}"
        ));
    cmd.arg(target);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn control_master_alive(control_path: &PathBuf, target: &str) -> bool {
    Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("check")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn cancel_forward(id: &LoopbackForwardId, local_port: u16, control_path: &PathBuf) {
    let _ = build_ssh_control_command(
        control_path,
        "cancel",
        &id.target,
        "127.0.0.1",
        local_port,
        &id.remote_host,
        id.remote_port,
    )
    .status();
}

fn is_loopback_forward_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_forward_command_against_control_master() {
        let control_path = PathBuf::from("/tmp/tty7-ssh.sock");
        let cmd = build_ssh_control_command(
            &control_path,
            "forward",
            "dev",
            "127.0.0.1",
            49152,
            "127.0.0.1",
            3000,
        );
        let argv: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "-S",
                "/tmp/tty7-ssh.sock",
                "-O",
                "forward",
                "-L",
                "127.0.0.1:49152:127.0.0.1:3000",
                "dev",
            ]
        );
    }

    #[test]
    fn loopback_host_gate_is_narrow() {
        assert!(is_loopback_forward_host("localhost"));
        assert!(is_loopback_forward_host("127.0.0.1"));
        assert!(is_loopback_forward_host("::1"));
        assert!(!is_loopback_forward_host("0.0.0.0"));
        assert!(!is_loopback_forward_host("example.com"));
    }
}
