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
        use std::os::fd::FromRawFd as _;

        let stdin_fd = dup_fd(libc::STDIN_FILENO)?;
        let stdout_fd = dup_fd(libc::STDOUT_FILENO)?;
        redirect_to_null(libc::STDIN_FILENO)?;
        redirect_to_null(libc::STDOUT_FILENO)?;

        let read = unsafe { std::fs::File::from_raw_fd(stdin_fd) };
        let write = unsafe { std::fs::File::from_raw_fd(stdout_fd) };
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

#[cfg(unix)]
impl LinkShutdown for StdioWriter {
    fn shutdown_link(&self) -> io::Result<()> {
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if !taken {
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn dup_fd(fd: libc::c_int) -> io::Result<libc::c_int> {
    let new = unsafe { libc::dup(fd) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(new)
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
}
