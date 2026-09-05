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
                <$ty>::set_read_timeout(self, timeout)
            }
            fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                <$ty>::set_write_timeout(self, timeout)
            }
        }
    };
}

socket!(std::net::TcpStream);
#[cfg(unix)]
socket!(std::os::unix::net::UnixStream);

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
