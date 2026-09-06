//! Absolute I/O deadlines for finite socket handshakes, not long-lived readers.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub trait Socket: Read + Write {
    fn read_timeout(&self) -> io::Result<Option<Duration>>;
    fn write_timeout(&self) -> io::Result<Option<Duration>>;
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

macro_rules! socket {
    ($ty:ty) => {
        impl Socket for $ty {
            fn read_timeout(&self) -> io::Result<Option<Duration>> {
                <$ty>::read_timeout(self)
            }
            fn write_timeout(&self) -> io::Result<Option<Duration>> {
                <$ty>::write_timeout(self)
            }
            fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                let result = <$ty>::set_read_timeout(self, timeout);
                #[cfg(unix)]
                let result = timeout_result(self, result, libc::POLLIN);
                result
            }
            fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                let result = <$ty>::set_write_timeout(self, timeout);
                #[cfg(unix)]
                let result = timeout_result(self, result, libc::POLLOUT);
                result
            }
        }
    };
}

socket!(std::net::TcpStream);
#[cfg(unix)]
socket!(std::os::unix::net::UnixStream);

#[cfg(unix)]
fn timeout_result(
    socket: &impl std::os::fd::AsRawFd,
    result: io::Result<()>,
    events: libc::c_short,
) -> io::Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    // macOS rejects setsockopt with EINVAL after shutdown, even when the peer's
    // final HelloOk/refusal is still buffered. Only tolerate that error after
    // confirming hangup in this I/O direction: buffered reads then end at EOF,
    // writes fail immediately, and neither can outlive the handshake deadline.
    // Readiness alone is insufficient; a live peer must still get its timeout.
    if error.raw_os_error() == Some(libc::EINVAL) {
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, backed by a borrowed live socket.
        // The zero timeout only inspects state and never waits or consumes data.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready == 1 && descriptor.revents & libc::POLLHUP != 0 {
            return Ok(());
        }
    }
    Err(error)
}

pub struct DeadlineIo<'a, S: Socket> {
    socket: &'a mut S,
    deadline: Instant,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl<'a, S: Socket> DeadlineIo<'a, S> {
    pub fn new(socket: &'a mut S, budget: Duration) -> io::Result<Self> {
        Ok(Self {
            read_timeout: socket.read_timeout()?,
            write_timeout: socket.write_timeout()?,
            socket,
            deadline: Instant::now() + budget,
        })
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "socket handshake timed out"))
    }
}

fn timeout_error(error: io::Error) -> io::Error {
    // Unix reports SO_RCVTIMEO as WouldBlock, Windows as TimedOut.
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        io::Error::new(io::ErrorKind::TimedOut, "socket handshake timed out")
    } else {
        error
    }
}

impl<S: Socket> Read for DeadlineIo<'_, S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes).map_err(timeout_error)
    }
}

impl<S: Socket> Write for DeadlineIo<'_, S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes).map_err(timeout_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.flush().map_err(timeout_error)
    }
}

impl<S: Socket> Drop for DeadlineIo<'_, S> {
    fn drop(&mut self) {
        // The steady-state control reader must not inherit a handshake timer.
        let _ = self.socket.set_read_timeout(self.read_timeout);
        let _ = self.socket.set_write_timeout(self.write_timeout);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon::control::{CONTROL_VERSION, ControlHelloOk, ControlServerMsg};
    use std::os::unix::net::UnixStream;

    fn closed_peer(bytes: &[u8]) -> UnixStream {
        let (mut peer, socket) = UnixStream::pair().unwrap();
        peer.write_all(bytes).unwrap();
        drop(peer);
        socket
    }

    #[test]
    fn a_closed_peer_preserves_its_final_reply_and_eof() {
        let mut bytes = Vec::new();
        ControlServerMsg::HelloOk(ControlHelloOk {
            control_version: CONTROL_VERSION + 1,
            protocol_version: 3,
            build: "other".into(),
            separator: '/',
            home: "/root".into(),
            features: vec![],
            instance: "other-instance".into(),
        })
        .encode(&mut bytes)
        .unwrap();
        // Close before constructing DeadlineIo, deterministically exercising the
        // macOS race both before flush and between the frame's partial reads.
        let mut socket = closed_peer(&bytes);
        let mut io = DeadlineIo::new(&mut socket, Duration::from_secs(1)).unwrap();
        io.flush().unwrap();
        let ControlServerMsg::HelloOk(reply) = ControlServerMsg::read(&mut io).unwrap() else {
            panic!("expected the peer's final HelloOk");
        };
        assert_eq!(reply.control_version, CONTROL_VERSION + 1);
        assert_eq!(io.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn a_closed_peer_with_a_partial_frame_reports_eof() {
        let mut socket = closed_peer(&[1]);
        let mut io = DeadlineIo::new(&mut socket, Duration::from_secs(1)).unwrap();
        let error = ControlServerMsg::read(&mut io).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_closed_peer_does_not_bypass_an_expired_deadline() {
        let mut socket = closed_peer(b"reply");
        let mut io = DeadlineIo::new(&mut socket, Duration::ZERO).unwrap();
        assert_eq!(
            io.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(io.flush().unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn only_confirmed_hangup_tolerates_an_invalid_timeout_option() {
        let socket = closed_peer(b"reply");
        for events in [libc::POLLIN, libc::POLLOUT] {
            timeout_result(
                &socket,
                Err(io::Error::from_raw_os_error(libc::EINVAL)),
                events,
            )
            .unwrap();
        }
    }

    #[test]
    fn a_live_readable_peer_does_not_hide_timeout_setup_errors() {
        let (mut peer, socket) = UnixStream::pair().unwrap();
        peer.write_all(b"partial reply").unwrap();
        for events in [libc::POLLIN, libc::POLLOUT] {
            let error = timeout_result(
                &socket,
                Err(io::Error::from_raw_os_error(libc::EINVAL)),
                events,
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        }
    }

    #[test]
    fn a_closed_peer_does_not_hide_unrelated_timeout_setup_errors() {
        let socket = closed_peer(b"reply");
        let error = timeout_result(
            &socket,
            Err(io::Error::from_raw_os_error(libc::EACCES)),
            libc::POLLIN,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    }

    #[test]
    fn a_live_silent_peer_still_times_out_and_restores_its_settings() {
        let (_peer, mut socket) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        {
            let mut io = DeadlineIo::new(&mut socket, Duration::from_millis(30)).unwrap();
            assert_eq!(
                io.read(&mut [0]).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert_eq!(socket.read_timeout().unwrap(), Some(Duration::from_secs(2)));
        assert_eq!(
            socket.write_timeout().unwrap(),
            Some(Duration::from_secs(3))
        );
    }
}
