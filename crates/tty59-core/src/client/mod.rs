mod control;
mod pane;

pub use control::{ControlClient, ControlEvents};
pub use pane::{PaneClient, PaneInput, PaneOutput, PaneSession};

pub use crate::daemon::control::{
    ControlEvent, ControlHello, ControlHelloOk, ControlRequest, ControlResponse, ReplyOk,
};
pub use crate::daemon::protocol::{
    DaemonMsg, DaemonVersion, PaneInfo, PaneProcs, ShellSpec, WinSize,
};
pub use crate::daemon::router::RouteTarget;

#[cfg(test)]
pub(crate) fn stream_pair() -> (
    crate::daemon::transport::Stream,
    crate::daemon::transport::Stream,
) {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::pair().expect("socketpair")
    }
    #[cfg(windows)]
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("bound addr");
        let connecting =
            std::thread::spawn(move || std::net::TcpStream::connect(addr).expect("connect back"));
        let (accepted, _) = listener.accept().expect("accept");
        (connecting.join().expect("connector thread"), accepted)
    }
}
