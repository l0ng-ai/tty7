use std::io::{self, Read as _};
use std::path::PathBuf;
use std::time::Duration;

use crate::daemon::protocol::{
    ClientMsg, DaemonMsg, DaemonVersion, PaneInfo, PaneProcs, ShellSpec, WinSize, is_error_kind,
    peek_frame_kind, take_frame,
};
use crate::daemon::router::{RouteAction, RouteChannel, RouteHeader, RouteTarget, negotiate};
use crate::daemon::transport;

const OPEN_REPLY_WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Default)]
enum PaneEndpoint {
    #[default]
    Local,
    At(PathBuf),
    Routed(RouteTarget),
}

#[derive(Clone, Debug, Default)]
pub struct PaneClient {
    endpoint: PaneEndpoint,
}

impl PaneClient {
    pub fn local() -> PaneClient {
        PaneClient {
            endpoint: PaneEndpoint::Local,
        }
    }

    pub fn at(endpoint: impl Into<PathBuf>) -> PaneClient {
        PaneClient {
            endpoint: PaneEndpoint::At(endpoint.into()),
        }
    }

    pub fn routed(target: RouteTarget) -> PaneClient {
        PaneClient {
            endpoint: PaneEndpoint::Routed(target),
        }
    }

    fn open(&self) -> io::Result<transport::Stream> {
        match &self.endpoint {
            PaneEndpoint::Local => transport::connect(),
            PaneEndpoint::At(path) => transport::connect_endpoint_at(path),
            PaneEndpoint::Routed(target) => {
                let mut stream = transport::connect()?;
                let header = RouteHeader {
                    target: target.clone(),
                    server_command: None,
                    channel: RouteChannel::Pane,
                    action: RouteAction::Forward,
                };
                negotiate(&mut stream, &header)?;
                Ok(stream)
            }
        }
    }

    pub fn list(&self) -> io::Result<Vec<PaneInfo>> {
        let mut stream = self.open()?;
        ClientMsg::List.encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::PaneList(panes) => Ok(panes),
            DaemonMsg::Error(message) => Err(io::Error::other(message)),
            other => Err(unexpected_reply("List", &other)),
        }
    }

    pub fn version(&self) -> io::Result<DaemonVersion> {
        let mut stream = self.open()?;
        ClientMsg::Version.encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Version(version) => Ok(version),
            DaemonMsg::Error(message) => Err(io::Error::other(message)),
            other => Err(unexpected_reply("Version", &other)),
        }
    }

    pub fn kill(&self, pane_id: u64) -> io::Result<()> {
        let mut stream = self.open()?;
        ClientMsg::Kill { pane_id }.encode(&mut stream)
    }

    /// Ask the daemon to become `exe` without stopping, keeping every pane.
    ///
    /// Success is the connection ending: the process that answers this socket
    /// is replaced mid-call, and its replacement has never heard of the socket.
    /// Anything actually written back is the reason it did not happen, and in
    /// that case the daemon is still running, still serving, still on its old
    /// binary.
    ///
    /// Returning does not mean the new image is listening yet — rebinding the
    /// endpoint takes it a moment. Callers that need to talk to it again should
    /// wait for its version to answer.
    pub fn hand_off(&self, exe: &std::path::Path) -> io::Result<()> {
        use std::io::Write as _;

        let mut stream = self.open()?;
        ClientMsg::Handoff {
            exe: exe.to_path_buf(),
        }
        .encode(&mut stream)?;
        stream.flush()?;
        let _ = stream.set_read_timeout(Some(OPEN_REPLY_WAIT));
        match DaemonMsg::read(&mut stream) {
            Err(_) => Ok(()),
            Ok(DaemonMsg::Error(message)) => Err(io::Error::other(message)),
            Ok(other) => Err(unexpected_reply("Handoff", &other)),
        }
    }

    pub fn send_input(&self, pane_id: u64, bytes: &[u8]) -> io::Result<()> {
        let mut stream = self.open()?;
        ClientMsg::SendInput {
            pane_id,
            bytes: bytes.to_vec(),
        }
        .encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::InputAck { .. } => Ok(()),
            DaemonMsg::Error(message) => Err(io::Error::other(message)),
            other => Err(unexpected_reply("SendInput", &other)),
        }
    }

    pub fn procs(&self, pane_id: u64) -> io::Result<PaneProcs> {
        let mut stream = self.open()?;
        ClientMsg::QueryProcs { pane_id }.encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Procs(procs) => Ok(procs),
            DaemonMsg::Error(message) => Err(io::Error::other(message)),
            other => Err(unexpected_reply("QueryProcs", &other)),
        }
    }

    pub fn spawn(
        &self,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        workspace: Option<String>,
    ) -> io::Result<PaneSession> {
        PaneSession::spawn_over(
            self.open()?,
            cwd,
            size,
            shell,
            owner,
            workspace,
            OPEN_REPLY_WAIT,
        )
    }

    pub fn attach(&self, pane_id: u64, size: WinSize) -> io::Result<PaneSession> {
        PaneSession::attach_over(self.open()?, pane_id, size, OPEN_REPLY_WAIT)
    }

    pub fn observe(&self, pane_id: u64, size: WinSize) -> io::Result<PaneSession> {
        PaneSession::observe_over(self.open()?, pane_id, size, OPEN_REPLY_WAIT)
    }
}

#[derive(Debug)]
pub struct PaneSession {
    input: PaneInput,
    output: PaneOutput,
}

impl PaneSession {
    pub(crate) fn spawn_over(
        mut stream: transport::Stream,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        workspace: Option<String>,
        reply_wait: Duration,
    ) -> io::Result<PaneSession> {
        ClientMsg::Spawn {
            cwd,
            size,
            shell,
            owner,
            workspace,
            // Restoring a dead pane's screen is a window concern: it exists to
            // put a workspace back the way its user left it. Nothing that
            // scripts panes through this library has a screen to put back.
            restore: None,
        }
        .encode(&mut stream)?;
        let mut session = PaneSession::over(stream, 0)?;
        // Best effort, deliberately not `?`: see `checked` — a daemon that has
        // already refused and hung up makes setsockopt fail, and that must not
        // become the error the caller sees instead of the refusal itself.
        let _ = session.set_recv_timeout(Some(reply_wait));
        let first = session.recv();
        let _ = session.set_recv_timeout(None);
        match first {
            Ok(DaemonMsg::Spawned { pane_id }) => {
                session.input.pane_id = pane_id;
                Ok(session)
            }
            Ok(DaemonMsg::Error(message)) => {
                Err(io::Error::other(format!("daemon refused Spawn: {message}")))
            }
            Ok(other) => Err(unexpected_reply("Spawn", &other)),
            Err(e) if would_block(&e) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer to Spawn within {reply_wait:?}"),
            )),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn attach_over(
        mut stream: transport::Stream,
        pane_id: u64,
        size: WinSize,
        reply_wait: Duration,
    ) -> io::Result<PaneSession> {
        ClientMsg::Attach { pane_id, size }.encode(&mut stream)?;
        PaneSession::checked(stream, "Attach", pane_id, reply_wait)
    }

    pub(crate) fn observe_over(
        mut stream: transport::Stream,
        pane_id: u64,
        size: WinSize,
        reply_wait: Duration,
    ) -> io::Result<PaneSession> {
        ClientMsg::Observe { pane_id, size }.encode(&mut stream)?;
        PaneSession::checked(stream, "Observe", pane_id, reply_wait)
    }

    fn checked(
        stream: transport::Stream,
        request: &str,
        pane_id: u64,
        reply_wait: Duration,
    ) -> io::Result<PaneSession> {
        let mut session = PaneSession::over(stream, pane_id)?;
        // Best effort, deliberately not `?`. The daemon answers a bad request by
        // writing one Error frame and closing immediately; on macOS setsockopt
        // against a socket whose peer is already gone fails with EINVAL. Letting
        // that propagate replaces "no such pane 42" — which is sitting in our
        // buffer right now — with "Invalid argument", the least useful thing we
        // could tell the caller. If the timeout does not take, the read below
        // still cannot hang: a closed peer returns EOF at once, and a peer
        // that is alive is exactly the case where setsockopt succeeds.
        let _ = session.set_recv_timeout(Some(reply_wait));
        let verdict = session.output.refusal_check(request, pane_id);
        let _ = session.set_recv_timeout(None);
        verdict?;
        Ok(session)
    }

    fn over(stream: transport::Stream, pane_id: u64) -> io::Result<PaneSession> {
        let reader = stream.try_clone()?;
        Ok(PaneSession {
            input: PaneInput {
                writer: stream,
                pane_id,
            },
            output: PaneOutput {
                reader,
                buffered: Vec::new(),
            },
        })
    }

    pub fn pane_id(&self) -> u64 {
        self.input.pane_id()
    }

    pub fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.input(bytes)
    }

    pub fn resize(&mut self, size: WinSize) -> io::Result<()> {
        self.input.resize(size)
    }

    pub fn detach(self) -> io::Result<()> {
        self.input.detach()
    }

    pub fn kill(self) -> io::Result<()> {
        self.input.kill()
    }

    pub fn recv(&mut self) -> io::Result<DaemonMsg> {
        self.output.recv()
    }

    pub fn set_recv_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.output.set_recv_timeout(wait)
    }

    pub fn split(self) -> (PaneInput, PaneOutput) {
        (self.input, self.output)
    }
}

#[derive(Debug)]
pub struct PaneInput {
    writer: transport::Stream,
    pane_id: u64,
}

impl PaneInput {
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    pub fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        ClientMsg::Input(bytes.to_vec()).encode(&mut self.writer)
    }

    pub fn resize(&mut self, size: WinSize) -> io::Result<()> {
        ClientMsg::Resize(size).encode(&mut self.writer)
    }

    pub fn detach(mut self) -> io::Result<()> {
        ClientMsg::Detach.encode(&mut self.writer)
    }

    pub fn kill(mut self) -> io::Result<()> {
        ClientMsg::Kill {
            pane_id: self.pane_id,
        }
        .encode(&mut self.writer)
    }
}

#[derive(Debug)]
pub struct PaneOutput {
    reader: transport::Stream,
    buffered: Vec<u8>,
}

impl PaneOutput {
    pub fn recv(&mut self) -> io::Result<DaemonMsg> {
        loop {
            if let Some((kind, payload)) = take_frame(&mut self.buffered)? {
                return DaemonMsg::from_frame(kind, payload);
            }
            let mut scratch = [0u8; 16 * 1024];
            match self.reader.read(&mut scratch) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the daemon closed the pane connection",
                    ));
                }
                Ok(n) => self.buffered.extend_from_slice(&scratch[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn set_recv_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.reader.set_read_timeout(wait)
    }

    fn refusal_check(&mut self, request: &str, pane_id: u64) -> io::Result<()> {
        let mut scratch = [0u8; 4096];
        loop {
            if let Some(kind) = peek_frame_kind(&self.buffered) {
                if !is_error_kind(kind) {
                    return Ok(());
                }
                return Err(self.refusal(request, pane_id));
            }
            match self.reader.read(&mut scratch) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "the daemon closed the connection without answering \
                             {request} for pane {pane_id}"
                        ),
                    ));
                }
                Ok(n) => self.buffered.extend_from_slice(&scratch[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if would_block(&e) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn refusal(&mut self, request: &str, pane_id: u64) -> io::Error {
        match self.recv() {
            Ok(DaemonMsg::Error(message)) => {
                io::Error::other(format!("daemon refused {request}: {message}"))
            }
            Ok(other) => unexpected_reply(request, &other),
            Err(_) => io::Error::other(format!("daemon refused {request} for pane {pane_id}")),
        }
    }
}

fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn unexpected_reply(request: &str, got: &DaemonMsg) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected daemon reply to {request}: {got:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::stream_pair;

    const REPLY_WAIT: Duration = Duration::from_secs(10);

    fn size() -> WinSize {
        WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        }
    }

    #[test]
    fn a_session_speaks_the_pane_protocol_end_to_end() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;

            match ClientMsg::read(&mut r).expect("read Spawn") {
                ClientMsg::Spawn {
                    owner: Some(owner), ..
                } => assert_eq!(owner, "unit"),
                other => panic!("expected an owned Spawn, got {other:?}"),
            }
            DaemonMsg::Spawned { pane_id: 7 }
                .encode(&mut w)
                .expect("confirm the spawn");
            DaemonMsg::Output(b"hello from the pane".to_vec())
                .encode(&mut w)
                .expect("stream output");

            match ClientMsg::read(&mut r).expect("read Input") {
                ClientMsg::Input(bytes) => assert_eq!(bytes, b"ls\r"),
                other => panic!("expected Input, got {other:?}"),
            }
            match ClientMsg::read(&mut r).expect("read Resize") {
                ClientMsg::Resize(new_size) => assert_eq!(new_size.cols, 120),
                other => panic!("expected Resize, got {other:?}"),
            }
            DaemonMsg::Exited { code: Some(0) }
                .encode(&mut w)
                .expect("report the exit");
            match ClientMsg::read(&mut r).expect("read Detach") {
                ClientMsg::Detach => {}
                other => panic!("expected Detach, got {other:?}"),
            }
        });

        let mut session = PaneSession::spawn_over(
            client_end,
            None,
            size(),
            None,
            Some("unit".into()),
            None,
            REPLY_WAIT,
        )
        .expect("spawn");
        assert_eq!(session.pane_id(), 7);

        match session.recv().expect("first stream message") {
            DaemonMsg::Output(bytes) => assert_eq!(bytes, b"hello from the pane"),
            other => panic!("expected Output, got {other:?}"),
        }

        session.input(b"ls\r").expect("send input");
        session
            .resize(WinSize {
                cols: 120,
                ..size()
            })
            .expect("send resize");
        match session.recv().expect("exit message") {
            DaemonMsg::Exited { code: Some(0) } => {}
            other => panic!("expected Exited, got {other:?}"),
        }
        session.detach().expect("detach");
        server.join().expect("server thread");
    }

    #[test]
    fn an_attach_refusal_carries_the_daemons_message() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            match ClientMsg::read(&mut r).expect("read Attach") {
                ClientMsg::Attach { pane_id, .. } => assert_eq!(pane_id, 42),
                other => panic!("expected Attach, got {other:?}"),
            }
            DaemonMsg::Error("no such pane 42".into())
                .encode(&mut w)
                .expect("refuse the attach");
        });

        let err = PaneSession::attach_over(client_end, 42, size(), REPLY_WAIT)
            .expect_err("attaching to a missing pane must fail");
        assert!(
            err.to_string().contains("no such pane 42"),
            "the daemon's reason was lost: {err}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn observe_opens_read_only_and_keeps_the_replay() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            match ClientMsg::read(&mut r).expect("read Observe") {
                ClientMsg::Observe { pane_id, .. } => assert_eq!(pane_id, 9),
                other => panic!("expected Observe, got {other:?}"),
            }
            DaemonMsg::Size(size()).encode(&mut w).expect("replay size");
            DaemonMsg::Snapshot(b"ring contents".to_vec())
                .encode(&mut w)
                .expect("replay snapshot");
        });

        let mut session =
            PaneSession::observe_over(client_end, 9, size(), REPLY_WAIT).expect("observe");
        match session.recv().expect("replayed size") {
            DaemonMsg::Size(_) => {}
            other => panic!("expected Size, got {other:?}"),
        }
        match session.recv().expect("replayed snapshot") {
            DaemonMsg::Snapshot(bytes) => assert_eq!(bytes, b"ring contents"),
            other => panic!("expected Snapshot, got {other:?}"),
        }
        server.join().expect("server thread");
    }

    #[test]
    fn an_observe_refusal_carries_the_daemons_message() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            match ClientMsg::read(&mut r).expect("read Observe") {
                ClientMsg::Observe { pane_id, .. } => assert_eq!(pane_id, 42),
                other => panic!("expected Observe, got {other:?}"),
            }
            DaemonMsg::Error("no such pane 42".into())
                .encode(&mut w)
                .expect("refuse the observe");
        });

        let err = PaneSession::observe_over(client_end, 42, size(), REPLY_WAIT)
            .expect_err("observing a missing pane must fail");
        assert!(
            err.to_string().contains("no such pane 42"),
            "the daemon's reason was lost: {err}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn an_accepted_attach_keeps_the_replay_it_peeked_at() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            match ClientMsg::read(&mut r).expect("read Attach") {
                ClientMsg::Attach { pane_id, .. } => assert_eq!(pane_id, 9),
                other => panic!("expected Attach, got {other:?}"),
            }
            DaemonMsg::Size(size()).encode(&mut w).expect("replay size");
            DaemonMsg::Snapshot(b"screen contents".to_vec())
                .encode(&mut w)
                .expect("replay snapshot");
        });

        let mut session =
            PaneSession::attach_over(client_end, 9, size(), REPLY_WAIT).expect("attach");
        match session.recv().expect("replayed size") {
            DaemonMsg::Size(_) => {}
            other => panic!("expected Size, got {other:?}"),
        }
        match session.recv().expect("replayed snapshot") {
            DaemonMsg::Snapshot(bytes) => assert_eq!(bytes, b"screen contents"),
            other => panic!("expected Snapshot, got {other:?}"),
        }
        server.join().expect("server thread");
    }
}
