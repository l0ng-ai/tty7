//! Endpoint-scoped maintenance. Never scan processes, send OS signals, start
//! a daemon implicitly, or interpret EOF as permission to terminate one.

use super::deadline::DeadlineIo;
use super::protocol::{
    ClientMsg, DaemonMsg, DaemonVersion, FEATURE_IDLE_SHUTDOWN, PROTOCOL_VERSION, PaneAccess,
};
use super::transport::{self, Stream};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::{Duration, Instant};

pub const PREPARE_FLAG: &str = "--prepare-idle-restart";
pub const HEALTH_FLAG: &str = "--check-running";
pub const SERVING_FLAG: &str = "--check-serving";
const IO_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Stopped,
    Healthy {
        control: u32,
        protocol: u32,
        build: String,
        instance: String,
    },
}

impl Reply {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("maintenance reply contains only JSON values")
    }
    pub fn parse(output: &str) -> Option<Self> {
        serde_json::from_str(output.lines().rev().find(|line| !line.trim().is_empty())?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_server_is_refused_before_any_mutating_frame() {
        use std::io::Read;
        let (mut client, mut server) = crate::client::stream_pair();
        let mut old = DaemonVersion::current();
        old.features
            .retain(|feature| feature != FEATURE_IDLE_SHUTDOWN);
        assert_eq!(
            request_idle_stop(&mut client, &old).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        drop(client);
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn idle_shutdown_requires_the_matching_ack_not_eof_or_another_instance() {
        for reply in [
            None,
            Some(DaemonMsg::ShutdownAck {
                instance: "another".into(),
            }),
            Some(DaemonMsg::Error("busy".into())),
        ] {
            let (mut client, mut server) = crate::client::stream_pair();
            let peer = std::thread::spawn(move || {
                assert!(matches!(
                    ClientMsg::read(&mut server).unwrap(),
                    ClientMsg::Access(PaneAccess::Manage)
                ));
                assert!(matches!(
                    ClientMsg::read(&mut server).unwrap(),
                    ClientMsg::ShutdownIfIdle { .. }
                ));
                if let Some(reply) = reply {
                    reply.encode(&mut server).unwrap();
                }
            });
            assert!(request_idle_stop(&mut client, &DaemonVersion::current()).is_err());
            peer.join().unwrap();
        }
    }

    #[test]
    fn maintenance_output_requires_structured_confirmation() {
        assert_eq!(
            Reply::parse("login banner\n{\"status\":\"stopped\"}\n"),
            Some(Reply::Stopped)
        );
        for invalid in [
            "",
            "success",
            "{}",
            "{\"status\":\"stopped\"}\nunrelated output",
        ] {
            assert!(Reply::parse(invalid).is_none());
        }
        assert_eq!(
            super::super::control::server_instance(),
            super::super::protocol::process_instance()
        );
    }
}

fn exchange(stream: &mut Stream, message: ClientMsg) -> io::Result<DaemonMsg> {
    let mut io = DeadlineIo::new(stream, IO_WAIT)?;
    message.encode(&mut io)?;
    DaemonMsg::read(&mut io)
}

fn version() -> io::Result<Option<DaemonVersion>> {
    let mut stream = match transport::connect() {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    match exchange(&mut stream, ClientMsg::Version)? {
        DaemonMsg::Version(version) if !version.instance.is_empty() => Ok(Some(version)),
        _ => Err(io::Error::other(
            "the pane endpoint did not identify a daemon instance",
        )),
    }
}

fn request_idle_stop(stream: &mut Stream, version: &DaemonVersion) -> io::Result<()> {
    if !version.has_feature(FEATURE_IDLE_SHUTDOWN) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the running server cannot safely check idle shutdown; keep it running and defer the update until its sessions have been closed explicitly",
        ));
    }
    let mut io = DeadlineIo::new(stream, IO_WAIT)?;
    ClientMsg::Access(PaneAccess::Manage).encode(&mut io)?;
    ClientMsg::ShutdownIfIdle {
        expected_instance: version.instance.clone(),
    }
    .encode(&mut io)?;
    match DaemonMsg::read(&mut io)? {
        DaemonMsg::ShutdownAck { instance } if instance == version.instance => Ok(()),
        DaemonMsg::Error(message) => Err(io::Error::other(message)),
        _ => Err(io::Error::other(
            "idle shutdown was not acknowledged by the expected daemon",
        )),
    }
}

pub fn prepare_idle_restart(wait: Duration) -> io::Result<Reply> {
    let Some(before) = version()? else {
        return Ok(Reply::Stopped);
    };
    // Check before opening a second connection, and again in the exchange.
    if !before.has_feature(FEATURE_IDLE_SHUTDOWN) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the running server does not support safe idle restart; no processes were stopped; close its sessions and stop that server explicitly before updating",
        ));
    }
    request_idle_stop(&mut transport::connect()?, &before)?;
    let deadline = Instant::now() + wait;
    loop {
        match version()? {
            None => return Ok(Reply::Stopped),
            Some(after) if after.instance != before.instance => {
                return Err(io::Error::other(
                    "another daemon replaced the acknowledged instance; reconnect before retrying maintenance",
                ));
            }
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the acknowledged daemon has not exited; no forced shutdown was attempted",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn check_running() -> io::Result<Reply> {
    check_health(true)
}

/// Normal connections accept a protocol-compatible build; maintenance verifies
/// the exact candidate build. Both require real replies from the same instance.
pub fn check_serving() -> io::Result<Reply> {
    check_health(false)
}

fn check_health(exact_build: bool) -> io::Result<Reply> {
    use crate::daemon::control::{CONTROL_VERSION, ControlHello, ControlRequest, ReplyOk};
    let before = version()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "no pane server is answering")
    })?;
    if before.protocol != PROTOCOL_VERSION
        || (exact_build && before.build != env!("CARGO_PKG_VERSION"))
    {
        return Err(io::Error::other(
            "the running pane server does not match this candidate build/dialect",
        ));
    }
    let control = crate::client::ControlClient::connect(&ControlHello::host_rpc(
        "maintenance-probe",
        "maintenance-probe",
    ))?;
    if control.hello().instance != before.instance
        || control.hello().protocol_version != before.protocol
    {
        return Err(io::Error::other(
            "control and pane endpoints do not identify the same daemon",
        ));
    }
    if !matches!(control.request(ControlRequest::Ping)?, ReplyOk::Pong) {
        return Err(io::Error::other("the control endpoint did not answer Ping"));
    }
    let after = version()?
        .ok_or_else(|| io::Error::other("the daemon disappeared during its health check"))?;
    if after.instance != before.instance {
        return Err(io::Error::other(
            "the daemon changed during its health check",
        ));
    }
    Ok(Reply::Healthy {
        control: CONTROL_VERSION,
        protocol: before.protocol,
        build: before.build,
        instance: before.instance,
    })
}
