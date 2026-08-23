use std::io;
use std::sync::{Arc, Mutex};

use crate::daemon::control::LinkShutdown;

pub struct Halves<R, W> {
    pub read: R,
    pub write: W,
    pub shutdown: Arc<dyn LinkShutdown>,
}

pub trait Duplex: Send + 'static {
    type Read: io::Read + Send + 'static;
    type Write: io::Write + Send + 'static;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>>;

    fn kind_label(&self) -> &'static str;
}

#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    type Read = std::os::unix::net::UnixStream;
    type Write = std::os::unix::net::UnixStream;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let read = self.try_clone()?;
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.try_clone()?);
        Ok(Halves {
            read,
            write: self,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "unix"
    }
}

impl Duplex for std::net::TcpStream {
    type Read = std::net::TcpStream;
    type Write = std::net::TcpStream;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let read = self.try_clone()?;
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.try_clone()?);
        Ok(Halves {
            read,
            write: self,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "tcp"
    }
}

#[cfg(unix)]
pub struct StdioDuplex {
    read: std::fs::File,
    write: StdioWriter,
}

#[cfg(unix)]
impl StdioDuplex {
    pub fn take() -> io::Result<StdioDuplex> {
        // Each descriptor is owned from the moment it exists, so the four `?`s
        // below cannot strand one: a `redirect_to_null` that fails after both
        // `dup`s used to return without closing either.
        let stdin_fd = dup_fd(libc::STDIN_FILENO)?;
        let stdout_fd = dup_fd(libc::STDOUT_FILENO)?;
        redirect_to_null(libc::STDIN_FILENO)?;
        redirect_to_null(libc::STDOUT_FILENO)?;

        let read = std::fs::File::from(stdin_fd);
        let write = std::fs::File::from(stdout_fd);
        Ok(StdioDuplex {
            read,
            write: StdioWriter {
                inner: Arc::new(Mutex::new(Some(write))),
            },
        })
    }
}

#[cfg(unix)]
impl Duplex for StdioDuplex {
    type Read = std::fs::File;
    type Write = StdioWriter;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.write.clone());
        Ok(Halves {
            read: self.read,
            write: self.write,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "stdio"
    }
}

#[cfg(unix)]
#[derive(Clone)]
pub struct StdioWriter {
    inner: Arc<Mutex<Option<std::fs::File>>>,
}

#[cfg(unix)]
impl io::Write for StdioWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(f) => f.write(buf),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdio link was shut down",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Closing stdio means closing our end and letting the peer close its own.
///
/// The other implementors of [`LinkShutdown`] are sockets, where
/// `shutdown(Both)` reaches into a reader already parked inside `read`. A pipe
/// has no such call, so `StdioDuplex::split` hands the *writer* back as the
/// closer: dropping it shuts the outbound pipe, the peer reads EOF, and the
/// peer closing its own end is what finally releases our reader.
///
/// So this transport satisfies the trait by way of the far end rather than on
/// its own, and a pipe carries no read timeout to fall back on. A peer that
/// never reacts leaves the reader parked.
/// `a_stdio_shutdown_closes_our_end_and_waits_on_the_peer_for_the_rest` holds
/// both halves of that.
#[cfg(unix)]
impl LinkShutdown for StdioWriter {
    fn shutdown_link(&self) -> io::Result<()> {
        // Dropping the file is the close; taking it out of the slot is also
        // what makes every later write a `BrokenPipe` rather than a write to
        // a descriptor the OS may have handed to somebody else by then.
        drop(self.inner.lock().unwrap_or_else(|e| e.into_inner()).take());
        Ok(())
    }
}

/// Hands back an *owned* descriptor rather than a raw one, so that a caller
/// which gives up between one `dup` and the next drops it instead of leaking
/// it. `dup` returns a fresh descriptor that nothing else holds, which is
/// exactly the contract `OwnedFd` wants.
#[cfg(unix)]
fn dup_fd(fd: libc::c_int) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;
    let new = unsafe { libc::dup(fd) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(new) })
}

#[cfg(unix)]
fn redirect_to_null(fd: libc::c_int) -> io::Result<()> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    use std::os::fd::AsRawFd as _;
    let rc = unsafe { libc::dup2(null.as_raw_fd(), fd) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn a_unix_stream_splits_into_working_halves() {
        use std::os::unix::net::UnixStream;
        let (a, b) = UnixStream::pair().unwrap();
        let Halves {
            mut read,
            mut write,
            shutdown,
        } = a.split().unwrap();

        let mut peer = b;
        write.write_all(b"ping").unwrap();
        write.flush().unwrap();
        let mut got = [0u8; 4];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        peer.write_all(b"pong").unwrap();
        let mut got = [0u8; 4];
        read.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"pong");

        let reader = std::thread::spawn(move || {
            let mut sink = Vec::new();
            read.read_to_end(&mut sink).map(|_| ())
        });
        shutdown.shutdown_link().unwrap();
        let _ = reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_shut_stdio_writer_refuses_further_writes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let file = std::fs::File::create(tmp.path()).unwrap();
        let mut w = StdioWriter {
            inner: Arc::new(Mutex::new(Some(file))),
        };
        w.write_all(b"before").unwrap();
        w.shutdown_link().unwrap();
        assert_eq!(
            w.write(b"after").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        w.shutdown_link().unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), b"before");
    }

    /// What the stdio link's shutdown does, and what it leaves to the peer.
    ///
    /// `LinkShutdown` asks for a way to "force the read half to return … while
    /// the reader is blocked inside `read`", and for a socket that is exactly
    /// what `shutdown(Both)` gives — `a_unix_stream_splits_into_working_halves`
    /// shows it. Stdio has no such call. `StdioDuplex::split` hands the
    /// *writer* back as the closer, so shutting the link down closes our end
    /// of the outbound pipe and nothing else; the reader parked on the inbound
    /// one is released when the peer notices that EOF and closes its own end.
    ///
    /// That is the whole mechanism, and it is worth having written down: it
    /// depends on the peer, and a pipe has no read timeout to fall back on, so
    /// a far end that never reacts leaves the reader parked. The socket
    /// transports do not have that dependency.
    #[cfg(unix)]
    #[test]
    fn a_stdio_shutdown_closes_our_end_and_waits_on_the_peer_for_the_rest() {
        use std::os::fd::FromRawFd as _;

        // Two pipes, standing in for the stdin and stdout a `StdioDuplex`
        // dups: `inbound` is what we read, `outbound` is what we write.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "inbound pipe");
        let (inbound_read, inbound_write) = (fds[0], fds[1]);
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "outbound pipe");
        let (outbound_read, outbound_write) = (fds[0], fds[1]);

        let mut read = unsafe { std::fs::File::from_raw_fd(inbound_read) };
        let peer_write = unsafe { std::fs::File::from_raw_fd(inbound_write) };
        let mut peer_read = unsafe { std::fs::File::from_raw_fd(outbound_read) };
        let writer = StdioWriter {
            inner: Arc::new(Mutex::new(Some(unsafe {
                std::fs::File::from_raw_fd(outbound_write)
            }))),
        };

        let (done_tx, done) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut sink = Vec::new();
            let out = read.read_to_end(&mut sink);
            let _ = done_tx.send(());
            out.map(|_| ())
        });

        // Shutting the link down closes the outbound half — the peer sees EOF
        // on what it reads — and refuses any further write.
        let mut writer_handle = writer.clone();
        writer.shutdown_link().unwrap();
        assert_eq!(
            writer_handle.write(b"after").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe,
            "the outbound half stayed open"
        );
        let mut sink = Vec::new();
        peer_read.read_to_end(&mut sink).unwrap();
        assert!(sink.is_empty(), "nothing was written before the shutdown");

        // And our reader is still parked, because nothing has closed the
        // inbound half. This is the part a socket would not need.
        assert!(
            done.recv_timeout(Duration::from_millis(200)).is_err(),
            "the reader came back on its own, so this transport does force a \
             parked read to return and the doc above is out of date"
        );

        // The peer reacting to that EOF is what releases it.
        drop(peer_write);
        done.recv_timeout(Duration::from_secs(5))
            .expect("the reader never came back after the peer closed");
        reader.join().unwrap().expect("the read ended cleanly");
    }
}
