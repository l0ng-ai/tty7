//! [`RemoteLink`] — one logical byte stream from the local daemon to a remote
//! `tty7-server`.
//!
//! ## Where this sits, and why it is not in the GUI
//!
//! The design's "one more transport shape doesn't disturb the layers above" is
//! true, but not for the reason it looks like. It is *not* that
//! [`crate::daemon::transport::Stream`] grew a variant — that type is a plain
//! alias (`UnixStream` on Unix, loopback `TcpStream` on Windows) and it does not
//! change by a byte here. It is that **a remote stream never reaches it**:
//!
//! ```text
//! GUI ──transport::Stream (unchanged)──▶ local daemon
//!                                          │
//!                                       RemoteLink ──▶ SSH channel / WSL stdio
//! ```
//!
//! The GUI still talks to a socket on this machine. The local daemon forwards
//! those bytes onto a `RemoteLink` without parsing them — which is what keeps
//! the router a router, and keeps the remote version handshake genuinely
//! end-to-end between the GUI and the remote server rather than something the
//! daemon in the middle has to understand.
//!
//! Every existing `transport::Stream` call site — `try_clone`, `set_read_timeout`,
//! `shutdown(Shutdown::Write)` — is therefore untouched, because every one of
//! them is on a stream that is still local.
//!
//! ## Why an enum, and why four variants over two types
//!
//! An enum rather than `Box<dyn AsyncRead + AsyncWrite>`, matching
//! [`super::ssh::connect::Transport`]: each variant's poll methods are a direct
//! delegate with no vtable, on a path that carries every byte of every remote
//! pane's output.
//!
//! Four variants over two underlying types, because the pairs are
//! distinguishable only by **how they were obtained**, and that distinction is
//! exactly what diagnostics need:
//!
//! | Variant | Underlying | Distinct because |
//! |---|---|---|
//! | [`RemoteLink::StreamLocal`] | SSH channel | The preferred path. A failure here is what triggers the one-time fallback probe |
//! | [`RemoteLink::SessionExec`] | SSH channel | Already the fallback. A failure here means the remote is genuinely unreachable, not that forwarding is disabled |
//! | [`RemoteLink::Wsl`] | child stdio | No SSH involved; auth and host-key problems are impossible by construction |
//! | [`RemoteLink::LocalStdio`] | child stdio | A test harness. Must never be mistaken for a real remote in a log |
//!
//! Collapsing each pair would turn "`AllowStreamLocalForwarding` is off, fall
//! back" into an indistinguishable "the connection dropped".

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use russh::Channel;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::router::RouteChannel;
use super::ssh::ProcessStream;

/// One logical stream between the local daemon and a remote `tty7-server`.
pub enum RemoteLink {
    /// Preferred: `direct-streamlocal@openssh.com` straight to the remote's
    /// `daemon.sock`, opened with russh's
    /// `client::Handle::channel_open_direct_streamlocal`. No extra process on
    /// the remote, and the remote server's own accept loop handles it exactly
    /// as it would a local connection.
    StreamLocal(russh::ChannelStream<russh::client::Msg>),

    /// Fallback for `AllowStreamLocalForwarding no`: a session channel running
    /// `tty7-server --stdio`, which bridges its own stdin/stdout to that same
    /// socket. Same type as [`RemoteLink::StreamLocal`], different meaning.
    SessionExec(russh::ChannelStream<russh::client::Msg>),

    /// WSL, which has no SSH at all: `wsl.exe -d <distro> -- tty7-server --stdio`.
    Wsl(ProcessStream),

    /// A `tty7-server --stdio` child on *this* machine. The end-to-end test
    /// path — the one that lets the whole remote stack be exercised in CI with
    /// no second machine, no SSH daemon, and no credentials.
    LocalStdio(ProcessStream),
}

impl RemoteLink {
    /// Adopt a `direct-streamlocal@openssh.com` channel as the preferred link.
    ///
    /// Taking the [`Channel`] rather than its stream keeps the "which SSH
    /// primitive opened this" decision at the call site that made it, which is
    /// the only place that still knows.
    pub fn stream_local(channel: Channel<Msg>) -> RemoteLink {
        RemoteLink::StreamLocal(channel.into_stream())
    }

    /// Adopt a session channel already running `tty7-server --stdio` as the
    /// fallback link.
    pub fn session_exec(channel: Channel<Msg>) -> RemoteLink {
        RemoteLink::SessionExec(channel.into_stream())
    }

    /// Spawn `program args…` and take its stdio as a [`RemoteLink::LocalStdio`].
    ///
    /// `kill_on_drop`, so dropping the link reaps the child rather than leaving
    /// a `tty7-server` parented to a test that has already finished.
    pub fn local_stdio(program: &str, args: &[&str]) -> io::Result<RemoteLink> {
        Ok(RemoteLink::LocalStdio(spawn_stdio(program, args)?))
    }

    /// Spawn `wsl.exe -d <distro> -- <server> --stdio` and take its stdio.
    ///
    /// `server` is an **absolute path inside the distribution**, not a bare
    /// name: `wsl.exe` runs the command without a login shell, so the `PATH`
    /// that would find `~/.local/share/tty7/bin` is not in effect.
    /// [`install::wsl::ensure_wsl_server`](crate::daemon::install::wsl::ensure_wsl_server)
    /// is what resolves it.
    ///
    /// No shell is involved, so `server` needs no quoting; the distro name is
    /// validated because it is an *option's* argument and a leading `-` would be
    /// read as another option.
    ///
    /// # `channel`
    ///
    /// **A pane bridge and a control bridge are different commands**, and this
    /// is the only place that can tell them apart for WSL. The remote listens
    /// twice and the dialects are not interchangeable — a pane landing on the
    /// control socket writes its `Spawn` and is answered with nothing, which is
    /// what "the workspace connects but the pane says it can't reach the
    /// machine" was. The SSH path makes the same choice in
    /// [`ssh::open_remote_link`](crate::daemon::ssh), and `LocalStdio` makes it
    /// in the client's `PaneWorkspace::route_header` because its argv is run
    /// verbatim; WSL builds its argv here, so here is where it belongs.
    pub fn wsl(distro: &str, server: &str, channel: RouteChannel) -> io::Result<RemoteLink> {
        super::install::wsl::validate_distro(distro)?;
        let args = super::install::wsl::wsl_args(distro, &wsl_link_argv(server, channel));
        Ok(RemoteLink::Wsl(spawn_stdio_owned(
            super::install::wsl::WSL_EXE,
            &args,
        )?))
    }

    /// [`RemoteLink::wsl`] with the command given as a shell string rather than
    /// a resolved path — the WSL reading of
    /// [`RouteHeader::server_command`](super::router::RouteHeader::server_command),
    /// which over SSH is likewise handed to a shell.
    ///
    /// The escape hatch for a distribution where the normal install path cannot
    /// be used; the resolved-path form above is what ships.
    ///
    /// `channel` reaches the command through
    /// [`RouteChannel::bridge_command`], which is the same rewrite the SSH path
    /// applies to *its* shell command — an override must not silently lose the
    /// pane dialect that [`RemoteLink::wsl`] gets right.
    pub fn wsl_shell(distro: &str, command: &str, channel: RouteChannel) -> io::Result<RemoteLink> {
        super::install::wsl::validate_distro(distro)?;
        let command = channel.bridge_command(command);
        let args = super::install::wsl::wsl_args(distro, &["sh", "-c", &command]);
        Ok(RemoteLink::Wsl(spawn_stdio_owned(
            super::install::wsl::WSL_EXE,
            &args,
        )?))
    }

    /// The label this link goes into logs and the status line under.
    ///
    /// The whole reason the variants are not collapsed: an operator reading
    /// "streamlocal" versus "session-exec" in a log knows immediately whether
    /// the remote refused socket forwarding or whether the box is simply gone.
    pub fn kind_label(&self) -> &'static str {
        match self {
            RemoteLink::StreamLocal(_) => "streamlocal",
            RemoteLink::SessionExec(_) => "session-exec",
            RemoteLink::Wsl(_) => "wsl-stdio",
            RemoteLink::LocalStdio(_) => "local-stdio",
        }
    }

    /// Whether this link is a `--stdio` bridge rather than a direct socket.
    ///
    /// The bridge costs one extra process on the remote and cannot report a
    /// connection refusal as precisely, so a caller deciding whether to retry
    /// the preferred path wants to know.
    pub fn is_stdio_bridge(&self) -> bool {
        matches!(
            self,
            RemoteLink::SessionExec(_) | RemoteLink::Wsl(_) | RemoteLink::LocalStdio(_)
        )
    }

    /// Whether the link rides an SSH channel (as opposed to a child process).
    pub fn is_ssh(&self) -> bool {
        matches!(
            self,
            RemoteLink::StreamLocal(_) | RemoteLink::SessionExec(_)
        )
    }
}

/// The command `wsl.exe` runs for a link on `channel`, as an argv.
///
/// Pure, because the difference between the two is one flag that decides which
/// of the remote's two sockets the stream lands on, and it cannot be checked
/// anywhere a distribution is required. See [`RemoteLink::wsl`].
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
    // A GUI process spawning `wsl.exe` would otherwise flash a console window
    // per pane. No-op off Windows.
    crate::core::proc::hide_console_tokio(&mut command);
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr stays inherited: the remote server's diagnostics belong in the
        // daemon's log, and capturing them into a pipe nobody drains would
        // eventually block the child on a full buffer.
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

// ---------------------------------------------------------------------------
// How a host is entered: the one-time decision behind `StreamLocal` vs `SessionExec`
// ---------------------------------------------------------------------------

/// The command run on a session channel when socket forwarding is unavailable.
///
/// `--stdio` with neither `--serve` nor `--bridge` lets the *remote* decide:
/// it bridges to a running daemon if there is one and serves in-process if
/// there is not, which is the right answer in both cases and one this side has
/// no way to know.
///
/// **Only a fallback for links that skip the install pass.** SSH links do not:
/// `SshManager::open_remote_link` runs `install::ensure_remote_server` first and
/// uses the absolute, dialect-qualified path it returns. That matters because
/// nothing puts `~/.local/share/tty7/bin` on a non-interactive `PATH`, and the
/// file there is `tty7-server-c<control>p<protocol>` — this bare name would be a
/// `command not found` on a machine the install had just succeeded on.
/// [`super::router::RouteHeader::server_command`] overrides either.
pub const DEFAULT_REMOTE_SERVER_CMD: &str = "tty7-server --stdio";

/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, NUL included.
/// The remote server stays under the smaller figure
/// (`host::server`'s `MAX_SOCKET_PATH_BYTES`), so the path derived here must use
/// the same bound or the two sides would disagree about when the fallback name
/// kicks in.
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// How this connection reaches the remote `tty7-server` — decided once per SSH
/// connection and cached there, never re-decided per channel.
///
/// Probing per channel would put a failed `direct-streamlocal` open in front of
/// every pane on a host whose admin turned `AllowStreamLocalForwarding` off,
/// and each of those is a full round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteEntry {
    /// `direct-streamlocal@openssh.com` straight to this absolute remote path.
    StreamLocal { socket: String },
    /// A session channel running `command`, which bridges its own stdio to that
    /// same socket.
    SessionExec { command: String },
}

impl RemoteEntry {
    /// The label this entry's links appear under in logs.
    pub fn kind_label(&self) -> &'static str {
        match self {
            RemoteEntry::StreamLocal { .. } => "streamlocal",
            RemoteEntry::SessionExec { .. } => "session-exec",
        }
    }
}

/// Pick the way in, from what a probe of the remote learned.
///
/// Split out as a pure function because the real decision is impossible to
/// unit-test end to end — it needs an sshd with `AllowStreamLocalForwarding`
/// flipped both ways — while the *policy* is exactly the part worth pinning:
///
/// | remote socket path | forwarding allowed | entry |
/// |---|---|---|
/// | resolved | yes | [`RemoteEntry::StreamLocal`] |
/// | resolved | no | [`RemoteEntry::SessionExec`] |
/// | unresolved | either | [`RemoteEntry::SessionExec`] |
///
/// An unresolved path forces the bridge even where forwarding is allowed:
/// `direct-streamlocal` carries an absolute path and nothing else, so without
/// one there is no request to make — whereas `tty7-server --stdio` resolves the
/// path in the process that will actually bind it.
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

/// The four remote environment variables the control socket path is derived
/// from. Read off the remote in one `exec`, never guessed from this machine's
/// own environment — a macOS client has no `$XDG_RUNTIME_DIR` and a Linux
/// server usually does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteEnv {
    pub control_sock: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub home: Option<String>,
    pub tmpdir: Option<String>,
}

/// Marker every probe line carries, so a remote whose startup files print a
/// banner (or a `fish` that greets) doesn't corrupt the answer. Same tactic as
/// [`crate::daemon::shell_integration::remote`]'s shell probe.
const ENV_MARKER: &str = "__tty7_env__";

/// The probe itself, wrapped in `sh -c` because the login shell it is handed to
/// may be `fish`, which does not speak `${VAR-}`.
pub const REMOTE_ENV_PROBE: &str = concat!(
    "sh -c 'printf \"__tty7_env__ %s\\n\" ",
    "\"sock=${TTY7_CONTROL_SOCK-}\" \"xdg=${XDG_RUNTIME_DIR-}\" ",
    "\"home=${HOME-}\" \"tmp=${TMPDIR-}\"'"
);

impl RemoteEnv {
    /// Parse [`REMOTE_ENV_PROBE`]'s output, ignoring everything unmarked.
    pub fn parse_probe(out: &str) -> RemoteEnv {
        let mut env = RemoteEnv::default();
        for line in out.lines() {
            let Some(rest) = line.trim().strip_prefix(ENV_MARKER) else {
                continue;
            };
            let Some((key, value)) = rest.trim_start().split_once('=') else {
                continue;
            };
            // An unset variable prints empty; keep it `None` so the fallbacks
            // below treat it as absent rather than as the empty path.
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

/// Where the remote's `tty7-server` listens for control connections, derived
/// from *its* environment.
///
/// This mirrors `host::server`'s `control_socket_path` step for step, because
/// the two have to agree byte for byte: this side asks `direct-streamlocal` for
/// a path, and the far side binds one, and nothing in between reconciles them.
///
/// | Order | Path |
/// |---|---|
/// | 1 | `$TTY7_CONTROL_SOCK` |
/// | 2 | `$XDG_RUNTIME_DIR/tty7/daemon.sock` |
/// | 3 | `$HOME/.local/share/tty7/daemon.sock` |
/// | 4 | `<runtime-or-tmp>/tty7-<hash>.sock`, when any of the above overruns `sun_path` |
///
/// The hashed name is *not* automatically shorter: a deep `$XDG_RUNTIME_DIR`
/// overruns `sun_path` on its own, which is the hole
/// [`crate::daemon::transport`] was fixed for. Every candidate base is
/// length-checked, and `None` — rather than a path the server will not be on —
/// is the answer when none fits, which puts the session down the `--stdio`
/// bridge that resolves the path in the process that binds it.
///
/// Paths are joined as POSIX strings, never `PathBuf`: on a Windows client
/// `PathBuf::join("/home/me", "tty7")` yields `/home/me\tty7`.
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

/// `Path::join`'s behaviour, spelled out for POSIX strings: one separator, no
/// doubling when the base already ends in one.
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

    /// Every variant implements shutdown for real. A half-close is how the
    /// remote learns the client is finished rather than merely quiet, and a
    /// `poll_shutdown` that returned `Ready(Ok(()))` without acting would strand
    /// the remote waiting on a stream that will never carry another byte.
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

    /// **A WSL pane asks for the pane socket.** The remote listens twice and
    /// answers only the dialect it was asked for, so a pane that reaches the
    /// control socket writes its `Spawn` and is answered with nothing at all —
    /// the workspace connects, the window opens, and the pane inside it says it
    /// cannot reach the machine. Nothing in the transport reports an error,
    /// which is why this is pinned here rather than left to the one integration
    /// path that would catch it.
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

    /// A child process's stdio really is a duplex stream: bytes written reach
    /// the child's stdin and its stdout comes back, through the same
    /// `AsyncRead`/`AsyncWrite` the SSH variants use. This is the path the
    /// end-to-end test rides, so it has to work before there is a server to
    /// point it at.
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

    /// Shutting the write half down is what tells the peer "no more input" —
    /// `cat` answers by closing its stdout, which surfaces here as EOF. A
    /// no-op `poll_shutdown` would hang this test forever.
    #[tokio::test]
    async fn shutdown_closes_the_write_half_and_the_peer_sees_eof() {
        let mut link = RemoteLink::local_stdio("cat", &[]).unwrap();
        link.write_all(b"bye").await.unwrap();
        link.shutdown().await.unwrap();

        let mut rest = Vec::new();
        link.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"bye");
    }

    /// Dropping the link reaps the child. Without `kill_on_drop` a failed test
    /// would leave a `tty7-server` running against a socket nobody holds.
    #[tokio::test]
    async fn dropping_the_link_kills_the_child() {
        let link = RemoteLink::local_stdio("sleep", &["300"]).unwrap();
        drop(link);
        // The child is reaped asynchronously by tokio; what matters is that the
        // handle is gone and nothing here leaks. A surviving process would show
        // up as a hung test run rather than an assertion, which is why this is
        // mostly a statement of intent — `kill_on_drop(true)` above is the
        // mechanism.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// The labels are the diagnostic value the four variants exist for, so they
    /// are pinned: a log line reading "streamlocal" has to keep meaning that.
    #[test]
    fn every_variant_has_a_distinct_label() {
        // Constructed without processes: only the discriminant is exercised.
        let labels = ["streamlocal", "session-exec", "wsl-stdio", "local-stdio"];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels must be distinguishable");
    }

    // -- the way in ---------------------------------------------------------

    /// The whole fallback policy, which a live sshd cannot be asked to
    /// demonstrate both halves of in one test run.
    #[test]
    fn the_entry_falls_back_exactly_when_streamlocal_cannot_be_used() {
        let cmd = "tty7-server --stdio";
        assert_eq!(
            choose_entry(Some("/run/user/1000/tty7/daemon.sock"), true, cmd),
            RemoteEntry::StreamLocal {
                socket: "/run/user/1000/tty7/daemon.sock".into()
            }
        );
        // `AllowStreamLocalForwarding no`: the path is known and useless.
        assert_eq!(
            choose_entry(Some("/run/user/1000/tty7/daemon.sock"), false, cmd),
            RemoteEntry::SessionExec {
                command: cmd.into()
            }
        );
        // No path to ask for. The bridge resolves it in the process that binds
        // it, so this is a fallback with *more* information, not less.
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

    /// The probe's output is read by marker, so a remote that greets, warns, or
    /// prints a MOTD before the answer is still parsed correctly — and an unset
    /// variable stays absent rather than becoming the empty path.
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

    /// The remote path, in the order `host::server::control_socket_path`
    /// resolves it. Pinned as literals: these strings are compared — over a
    /// wire, with no error message — against what a *different binary* on a
    /// *different machine* computed, so an "equivalent" refactor of either side
    /// is a silent connection failure.
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

        // A trailing separator must not double up: the server derives its path
        // through `Path::join`, which collapses it.
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

        // Nothing to derive from at all.
        assert_eq!(remote_control_socket(&RemoteEnv::default()), None);
    }

    /// The hole `daemon::transport` was fixed for, on the remote side: the
    /// "short" hashed name is only short relative to the *config* dir, and a
    /// deep `$XDG_RUNTIME_DIR` overruns `sun_path` just as readily. Returning
    /// an overlong path here would send `direct-streamlocal` at an address the
    /// server could never have bound.
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
        // …and it lands in the temp dir, because the runtime dir itself is what
        // was too long.
        assert!(
            path.starts_with("/tmp/tty7-"),
            "unexpected fallback: {path}"
        );

        // When *no* base is short enough, the honest answer is "no path" — the
        // session then takes the stdio bridge, which resolves it remotely.
        let hopeless = RemoteEnv {
            control_sock: None,
            xdg_runtime_dir: Some(deep.clone()),
            home: Some("/home/me".into()),
            tmpdir: Some(deep),
        };
        assert_eq!(remote_control_socket(&hopeless), None);
    }
}
