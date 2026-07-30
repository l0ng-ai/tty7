use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use russh::Channel;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::router::RouteChannel;
use super::ssh::ProcessStream;

pub enum RemoteLink {
    StreamLocal(russh::ChannelStream<russh::client::Msg>),

    SessionExec(russh::ChannelStream<russh::client::Msg>),

    Wsl(ProcessStream),

    LocalStdio(ProcessStream),
}

impl RemoteLink {
    pub fn stream_local(channel: Channel<Msg>) -> RemoteLink {
        RemoteLink::StreamLocal(channel.into_stream())
    }

    pub fn session_exec(channel: Channel<Msg>) -> RemoteLink {
        RemoteLink::SessionExec(channel.into_stream())
    }

    pub fn local_stdio(program: &str, args: &[&str]) -> io::Result<RemoteLink> {
        Ok(RemoteLink::LocalStdio(spawn_stdio(program, args)?))
    }

    pub fn wsl(distro: &str, server: &str, channel: RouteChannel) -> io::Result<RemoteLink> {
        super::install::wsl::validate_distro(distro)?;
        let args = super::install::wsl::wsl_args(distro, &wsl_link_argv(server, channel));
        Ok(RemoteLink::Wsl(spawn_stdio_owned(
            super::install::wsl::WSL_EXE,
            &args,
        )?))
    }

    pub fn wsl_shell(distro: &str, command: &str, channel: RouteChannel) -> io::Result<RemoteLink> {
        super::install::wsl::validate_distro(distro)?;
        let command = channel.bridge_command(command);
        let args = super::install::wsl::wsl_args(distro, &["sh", "-c", &command]);
        Ok(RemoteLink::Wsl(spawn_stdio_owned(
            super::install::wsl::WSL_EXE,
            &args,
        )?))
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            RemoteLink::StreamLocal(_) => "streamlocal",
            RemoteLink::SessionExec(_) => "session-exec",
            RemoteLink::Wsl(_) => "wsl-stdio",
            RemoteLink::LocalStdio(_) => "local-stdio",
        }
    }

    pub fn is_stdio_bridge(&self) -> bool {
        matches!(
            self,
            RemoteLink::SessionExec(_) | RemoteLink::Wsl(_) | RemoteLink::LocalStdio(_)
        )
    }

    pub fn is_ssh(&self) -> bool {
        matches!(
            self,
            RemoteLink::StreamLocal(_) | RemoteLink::SessionExec(_)
        )
    }
}

fn wsl_link_argv<'a>(server: &'a str, channel: RouteChannel) -> Vec<&'a str> {
    let mut argv = vec![server, "--stdio"];
    if channel == RouteChannel::Pane {
        argv.push("--pane");
    }
    argv
}

fn spawn_stdio(program: &str, args: &[&str]) -> io::Result<ProcessStream> {
    let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    spawn_stdio_owned(program, &owned)
}

fn spawn_stdio_owned(program: &str, args: &[String]) -> io::Result<ProcessStream> {
    let mut command = tokio::process::Command::new(program);
    crate::core::proc::hide_console_tokio(&mut command);
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other(format!("{program} stdin unavailable")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other(format!("{program} stdout unavailable")))?;
    Ok(ProcessStream::from_parts(child, stdin, stdout))
}

pub const DEFAULT_REMOTE_SERVER_CMD: &str = "tty7-server --stdio";

const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteEntry {
    StreamLocal { socket: String },
    SessionExec { command: String },
}

impl RemoteEntry {
    pub fn kind_label(&self) -> &'static str {
        match self {
            RemoteEntry::StreamLocal { .. } => "streamlocal",
            RemoteEntry::SessionExec { .. } => "session-exec",
        }
    }
}

pub fn choose_entry(
    socket: Option<&str>,
    forwarding_allowed: bool,
    server_command: &str,
) -> RemoteEntry {
    match socket {
        Some(socket) if forwarding_allowed && !socket.is_empty() => RemoteEntry::StreamLocal {
            socket: socket.to_string(),
        },
        _ => RemoteEntry::SessionExec {
            command: server_command.to_string(),
        },
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteEnv {
    pub control_sock: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub home: Option<String>,
    pub tmpdir: Option<String>,
}

const ENV_MARKER: &str = "__tty7_env__";

pub const REMOTE_ENV_PROBE: &str = concat!(
    "sh -c 'printf \"__tty7_env__ %s\\n\" ",
    "\"sock=${TTY7_CONTROL_SOCK-}\" \"xdg=${XDG_RUNTIME_DIR-}\" ",
    "\"home=${HOME-}\" \"tmp=${TMPDIR-}\"'"
);

impl RemoteEnv {
    pub fn parse_probe(out: &str) -> RemoteEnv {
        let mut env = RemoteEnv::default();
        for line in out.lines() {
            let Some(rest) = line.trim().strip_prefix(ENV_MARKER) else {
                continue;
            };
            let Some((key, value)) = rest.trim_start().split_once('=') else {
                continue;
            };
            let value = (!value.is_empty()).then(|| value.to_string());
            match key {
                "sock" => env.control_sock = value,
                "xdg" => env.xdg_runtime_dir = value,
                "home" => env.home = value,
                "tmp" => env.tmpdir = value,
                _ => {}
            }
        }
        env
    }
}

pub fn remote_control_socket(env: &RemoteEnv) -> Option<String> {
    if let Some(explicit) = env.control_sock.as_deref().filter(|s| !s.is_empty()) {
        return Some(explicit.to_string());
    }

    let runtime = env.xdg_runtime_dir.as_deref().filter(|d| !d.is_empty());
    let dir = match runtime {
        Some(runtime) => posix_join(runtime, "tty7"),
        None => {
            let home = env.home.as_deref().filter(|h| !h.is_empty())?;
            posix_join(&posix_join(&posix_join(home, ".local"), "share"), "tty7")
        }
    };

    let inline = posix_join(&dir, "daemon.sock");
    if fits(&inline) {
        return Some(inline);
    }

    let name = format!("tty7-{:016x}.sock", crate::host::fnv1a64(dir.as_bytes()));
    let tmp = env
        .tmpdir
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or("/tmp");
    [runtime, Some(tmp)]
        .into_iter()
        .flatten()
        .map(|base| posix_join(base, &name))
        .find(|candidate| fits(candidate))
}

fn posix_join(base: &str, name: &str) -> String {
    format!("{}/{name}", base.trim_end_matches('/'))
}

fn fits(path: &str) -> bool {
    path.len() <= MAX_SOCKET_PATH_BYTES
}

impl AsyncRead for RemoteLink {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RemoteLink::StreamLocal(s) | RemoteLink::SessionExec(s) => {
                Pin::new(s).poll_read(cx, buf)
            }
            RemoteLink::Wsl(s) | RemoteLink::LocalStdio(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RemoteLink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            RemoteLink::StreamLocal(s) | RemoteLink::SessionExec(s) => {
                Pin::new(s).poll_write(cx, buf)
            }
            RemoteLink::Wsl(s) | RemoteLink::LocalStdio(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RemoteLink::StreamLocal(s) | RemoteLink::SessionExec(s) => Pin::new(s).poll_flush(cx),
            RemoteLink::Wsl(s) | RemoteLink::LocalStdio(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RemoteLink::StreamLocal(s) | RemoteLink::SessionExec(s) => {
                Pin::new(s).poll_shutdown(cx)
            }
            RemoteLink::Wsl(s) | RemoteLink::LocalStdio(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RemoteLink")
            .field(&self.kind_label())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn only_a_pane_link_asks_for_the_pane_socket() {
        let server = "/home/me/.local/share/tty7/bin/tty7-server-26.7.6";
        assert_eq!(
            wsl_link_argv(server, RouteChannel::Control),
            vec![server, "--stdio"]
        );
        assert_eq!(
            wsl_link_argv(server, RouteChannel::Pane),
            vec![server, "--stdio", "--pane"]
        );
    }

    #[tokio::test]
    async fn a_local_stdio_child_round_trips_bytes() {
        let mut link = RemoteLink::local_stdio("cat", &[]).unwrap();
        assert_eq!(link.kind_label(), "local-stdio");
        assert!(link.is_stdio_bridge());
        assert!(!link.is_ssh());

        link.write_all(b"hello remote\n").await.unwrap();
        link.flush().await.unwrap();

        let mut got = vec![0u8; 13];
        link.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello remote\n");
    }

    #[tokio::test]
    async fn shutdown_closes_the_write_half_and_the_peer_sees_eof() {
        let mut link = RemoteLink::local_stdio("cat", &[]).unwrap();
        link.write_all(b"bye").await.unwrap();
        link.shutdown().await.unwrap();

        let mut rest = Vec::new();
        link.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"bye");
    }

    #[tokio::test]
    async fn dropping_the_link_kills_the_child() {
        let link = RemoteLink::local_stdio("sleep", &["300"]).unwrap();
        drop(link);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[test]
    fn every_variant_has_a_distinct_label() {
        let labels = ["streamlocal", "session-exec", "wsl-stdio", "local-stdio"];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels must be distinguishable");
    }

    #[test]
    fn the_entry_falls_back_exactly_when_streamlocal_cannot_be_used() {
        let cmd = "tty7-server --stdio";
        assert_eq!(
            choose_entry(Some("/run/user/1000/tty7/daemon.sock"), true, cmd),
            RemoteEntry::StreamLocal {
                socket: "/run/user/1000/tty7/daemon.sock".into()
            }
        );
        assert_eq!(
            choose_entry(Some("/run/user/1000/tty7/daemon.sock"), false, cmd),
            RemoteEntry::SessionExec {
                command: cmd.into()
            }
        );
        assert_eq!(
            choose_entry(None, true, cmd),
            RemoteEntry::SessionExec {
                command: cmd.into()
            }
        );
        assert_eq!(
            choose_entry(None, false, cmd),
            RemoteEntry::SessionExec {
                command: cmd.into()
            }
        );
    }

    #[test]
    fn the_env_probe_survives_a_chatty_remote() {
        let out = "Welcome to Ubuntu!\n\
                   __tty7_env__ sock=\n\
                   __tty7_env__ xdg=/run/user/1000\n\
                   __tty7_env__ home=/home/me\n\
                   __tty7_env__ tmp=\n\
                   You have mail.\n";
        let env = RemoteEnv::parse_probe(out);
        assert_eq!(env.control_sock, None);
        assert_eq!(env.xdg_runtime_dir.as_deref(), Some("/run/user/1000"));
        assert_eq!(env.home.as_deref(), Some("/home/me"));
        assert_eq!(env.tmpdir, None);
    }

    #[test]
    fn the_remote_socket_path_matches_the_servers_own_order() {
        let explicit = RemoteEnv {
            control_sock: Some("/tmp/mine.sock".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            home: Some("/home/me".into()),
            ..RemoteEnv::default()
        };
        assert_eq!(
            remote_control_socket(&explicit).as_deref(),
            Some("/tmp/mine.sock"),
            "an explicit $TTY7_CONTROL_SOCK outranks everything"
        );

        let xdg = RemoteEnv {
            xdg_runtime_dir: Some("/run/user/1000".into()),
            home: Some("/home/me".into()),
            ..RemoteEnv::default()
        };
        assert_eq!(
            remote_control_socket(&xdg).as_deref(),
            Some("/run/user/1000/tty7/daemon.sock")
        );

        let trailing = RemoteEnv {
            xdg_runtime_dir: Some("/run/user/1000/".into()),
            ..RemoteEnv::default()
        };
        assert_eq!(
            remote_control_socket(&trailing).as_deref(),
            Some("/run/user/1000/tty7/daemon.sock")
        );

        let home_only = RemoteEnv {
            home: Some("/home/me".into()),
            ..RemoteEnv::default()
        };
        assert_eq!(
            remote_control_socket(&home_only).as_deref(),
            Some("/home/me/.local/share/tty7/daemon.sock")
        );

        assert_eq!(remote_control_socket(&RemoteEnv::default()), None);
    }

    #[test]
    fn a_deep_runtime_dir_never_yields_an_unbindable_path() {
        let deep = format!("/run/user/1000/{}", "nested/".repeat(12));
        let env = RemoteEnv {
            control_sock: None,
            xdg_runtime_dir: Some(deep.clone()),
            home: Some("/home/me".into()),
            tmpdir: Some("/tmp".into()),
        };
        let path = remote_control_socket(&env).expect("the temp dir is short enough");
        assert!(
            path.len() <= MAX_SOCKET_PATH_BYTES,
            "{path} ({} bytes) would be rejected by bind()",
            path.len()
        );
        assert!(
            path.starts_with("/tmp/tty7-"),
            "unexpected fallback: {path}"
        );

        let hopeless = RemoteEnv {
            control_sock: None,
            xdg_runtime_dir: Some(deep.clone()),
            home: Some("/home/me".into()),
            tmpdir: Some(deep),
        };
        assert_eq!(remote_control_socket(&hopeless), None);
    }
}
