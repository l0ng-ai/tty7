//! Stable per-workspace agent sockets. Only the current control connection
//! owns a backend; reconnecting changes the backend, not the shell's path.

use std::collections::HashMap;
use std::io;
use std::os::unix::{
    fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    net::UnixListener,
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;

#[derive(Clone, PartialEq, Eq)]
struct Backend {
    owner: u64,
    socket: PathBuf,
}

struct Entry {
    path: PathBuf,
    backend: watch::Sender<Option<Backend>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.backend.send_replace(None);
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub struct WorkspaceAgents {
    entries: Mutex<HashMap<String, Entry>>,
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("tty7-agent-proxy")
            .build()
            .expect("build workspace agent runtime")
    })
}

impl WorkspaceAgents {
    pub fn global() -> &'static Self {
        static AGENTS: OnceLock<WorkspaceAgents> = OnceLock::new();
        AGENTS.get_or_init(Self::default)
    }

    pub fn bind(&self, workspace: &str, owner: u64, socket: &str) -> io::Result<()> {
        let dir = crate::host::server::control_socket_path()?.with_extension("agents");
        self.bind_in(&dir, workspace, owner, socket)
    }

    fn bind_in(&self, dir: &Path, workspace: &str, owner: u64, socket: &str) -> io::Result<()> {
        // Accept only a real same-user Unix socket, never an arbitrary file or
        // symlink supplied on a control connection.
        let meta = std::fs::symlink_metadata(socket)?;
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "agent backend is not a same-user Unix socket",
            ));
        }
        self.set_in(dir, workspace, Some((owner, socket)))
            .map(|_| ())
    }

    pub fn prepare(&self, workspace: &str) -> io::Result<PathBuf> {
        let dir = crate::host::server::control_socket_path()?.with_extension("agents");
        self.set_in(&dir, workspace, None)
    }

    fn set_in(
        &self,
        dir: &Path,
        workspace: &str,
        target: Option<(u64, &str)>,
    ) -> io::Result<PathBuf> {
        let id: crate::core::session::WorkspaceId = workspace
            .parse()
            .map_err(|_| io::Error::other("invalid workspace id for agent forwarding"))?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if !entries.contains_key(workspace) {
            use sha2::{Digest, Sha256};
            let hash = hex::encode(Sha256::digest(id.to_string().as_bytes()));
            // Scope to the entire control endpoint, including when two
            // daemons put differently named control sockets in one directory.
            let mut dir = dir.to_path_buf();
            if dir.as_os_str().len() + 25 > 103 {
                let scope = hex::encode(Sha256::digest(dir.as_os_str().as_encoded_bytes()));
                // macOS's per-user TMPDIR can itself exceed sun_path's limit.
                // Use a private, uid/config-scoped directory under /tmp.
                dir = Path::new("/tmp").join(format!(
                    "tty7-agent-{}-{}",
                    unsafe { libc::geteuid() },
                    &scope[..12]
                ));
            }
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            let meta = std::fs::symlink_metadata(&dir)?;
            if !meta.is_dir()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o077 != 0
            {
                return Err(io::Error::other(
                    "workspace agent directory is not user-private",
                ));
            }
            let path = dir.join(format!("a-{}.sock", &hash[..16]));
            if path.as_os_str().len() > 103 {
                return Err(io::Error::other("workspace agent socket path is too long"));
            }
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                    return Err(io::Error::other("unsafe workspace agent socket path"));
                }
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "workspace agent socket is already served",
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            let listener = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let (backend, receiver) = watch::channel(None);
            let _enter = runtime().enter();
            let listener = tokio::net::UnixListener::from_std(listener)?;
            let task = runtime().spawn(serve(listener, receiver));
            entries.insert(
                workspace.to_string(),
                Entry {
                    path,
                    backend,
                    task,
                },
            );
        }
        let entry = entries.get(workspace).unwrap();
        if let Some((owner, socket)) = target {
            let next = Some(Backend {
                owner,
                socket: PathBuf::from(socket),
            });
            entry.backend.send_if_modified(|old| {
                if *old == next {
                    false
                } else {
                    *old = next;
                    true
                }
            });
        }
        Ok(entry.path.clone())
    }

    pub fn socket(&self, workspace: &str) -> Option<PathBuf> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace)
            .map(|entry| entry.path.clone())
    }

    /// owner=None is an explicit takeover without forwarding. A stale owner
    /// cannot revoke the backend installed by a newer control connection.
    pub fn revoke(&self, workspace: &str, owner: Option<u64>) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace)
        {
            entry.backend.send_if_modified(|backend| {
                if backend
                    .as_ref()
                    .is_some_and(|b| owner.is_none_or(|o| o == b.owner))
                {
                    *backend = None;
                    true
                } else {
                    false
                }
            });
        }
    }

    pub fn remove(&self, workspace: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(workspace);
    }
}

async fn serve(listener: tokio::net::UnixListener, backend: watch::Receiver<Option<Backend>>) {
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            break;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let mut current = backend.clone();
        let Some(target) = current.borrow_and_update().clone() else {
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            // Rebind/detach cancels existing streams as well as new requests.
            tokio::select! {
                _ = current.changed() => {},
                _ = async {
                    if let Ok(Ok(mut upstream)) = tokio::time::timeout(Duration::from_secs(5), tokio::net::UnixStream::connect(target.socket)).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                } => {},
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn agent(dir: &Path, name: &str, tag: u8) -> PathBuf {
        let socket = dir.join(name);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut data = [0; 1];
                    while stream.read_exact(&mut data).await.is_ok() {
                        if stream.write_all(&[tag, data[0]]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        socket
    }

    async fn exchange(stream: &mut tokio::net::UnixStream, tag: u8) {
        stream.write_all(b"x").await.unwrap();
        let mut data = [0; 2];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut data))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data, [tag, b'x']);
    }

    #[tokio::test]
    async fn reconnect_and_takeover_keep_the_path_and_cancel_old_streams() {
        let dir = tempfile::tempdir().unwrap();
        let first = agent(dir.path(), "first", b'A').await;
        let second = agent(dir.path(), "second", b'B').await;
        let agents = WorkspaceAgents::default();
        let ws = crate::core::session::WorkspaceId::new().to_string();
        agents
            .bind_in(&dir.path().join("proxy"), &ws, 1, first.to_str().unwrap())
            .unwrap();
        let path = agents.socket(&ws).unwrap();
        let mut old = tokio::net::UnixStream::connect(&path).await.unwrap();
        exchange(&mut old, b'A').await;
        agents
            .bind_in(&dir.path().join("proxy"), &ws, 2, second.to_str().unwrap())
            .unwrap();
        assert_eq!(agents.socket(&ws).unwrap(), path);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), old.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        agents.revoke(&ws, Some(1)); // late disconnect from the displaced client
        let mut new = tokio::net::UnixStream::connect(&path).await.unwrap();
        exchange(&mut new, b'B').await;
        agents.revoke(&ws, Some(2));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), new.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut detached = tokio::net::UnixStream::connect(&path).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), detached.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        agents
            .bind_in(&dir.path().join("proxy"), &ws, 3, first.to_str().unwrap())
            .unwrap();
        let mut reconnected = tokio::net::UnixStream::connect(&path).await.unwrap();
        exchange(&mut reconnected, b'A').await;
        agents.remove(&ws);
        assert!(!path.exists());
        assert!(agents.socket(&ws).is_none());
    }

    #[tokio::test]
    async fn workspaces_are_isolated_and_socket_permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let a = agent(dir.path(), "a", b'A').await;
        let b = agent(dir.path(), "b", b'B').await;
        let agents = WorkspaceAgents::default();
        let ws_a = crate::core::session::WorkspaceId::new().to_string();
        let ws_b = crate::core::session::WorkspaceId::new().to_string();
        agents
            .bind_in(&dir.path().join("proxy"), &ws_a, 1, a.to_str().unwrap())
            .unwrap();
        agents
            .bind_in(&dir.path().join("proxy"), &ws_b, 2, b.to_str().unwrap())
            .unwrap();
        let path = agents.socket(&ws_a).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        agents.revoke(&ws_a, None);
        let mut stream = tokio::net::UnixStream::connect(agents.socket(&ws_b).unwrap())
            .await
            .unwrap();
        exchange(&mut stream, b'B').await;
    }

    #[test]
    fn non_socket_backends_are_refused_without_creating_a_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"not an agent").unwrap();
        let agents = WorkspaceAgents::default();
        let ws = crate::core::session::WorkspaceId::new().to_string();
        assert!(
            agents
                .bind_in(&dir.path().join("proxy"), &ws, 1, file.to_str().unwrap())
                .is_err()
        );
        assert!(agents.socket(&ws).is_none());
    }
}
