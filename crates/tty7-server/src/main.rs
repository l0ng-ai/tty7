use std::io;
use std::process::ExitCode;

const USAGE: &str = "\
tty7-server — the tty7 session daemon, headless

USAGE:
    tty7-server --daemon [--config-dir <dir>]
    tty7-server --stdio [--serve | --bridge] [--control-sock <path>]
    tty7-server --stdio --pane [--config-dir <dir>]
    tty7-server --stop [--config-dir <dir>]
    tty7-server agent-hook <agent> <event>

OPTIONS:
    --daemon              Serve panes and control connections until killed
    --stdio               Carry one control connection on stdin/stdout
      --serve               Answer requests in this process (no socket)
      --bridge              Forward to the machine's control socket
      --pane                Forward to the machine's *pane* socket instead
      --control-sock <p>    Use <p> as the control socket instead of the default
    --stop                Ask the running daemon to shut down, and wait for it
    --config-dir <dir>    Use <dir> for the socket, config and session files
    --protocol            Print the dialects this binary speaks, as JSON
    -V, --version         Print the version and exit
    -h, --help            Print this help and exit
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("agent-hook") {
        if let (Some(agent), Some(event)) = (args.get(1), args.get(2)) {
            tty7_core::core::agent_hooks::run_agent_hook(agent, event);
        }
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("tty7-server {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args
        .iter()
        .any(|a| a == tty7_core::daemon::install::PROTOCOL_FLAG)
    {
        println!(
            "{}",
            tty7_core::daemon::install::RemoteProtocol::of_this_build().to_line()
        );
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    apply_config_dir_arg(&args);

    tty7_core::core::crash::install("server");
    tty7_core::core::logfile::install("server");

    // What a remote Windows host is restarted with. Unix hosts are sent a
    // SIGTERM, which Windows has no equivalent of; this is the graceful
    // shutdown a local tty7 uses there, with its reap as the fallback.
    if args.iter().any(|a| a == "--stop") {
        tty7_core::daemon::spawn::stop();
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--stdio") {
        return match run_stdio(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tty7-server: stdio session ended with error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|a| a == "--daemon") {
        return run_daemon();
    }

    eprint!("tty7-server: nothing to do without --daemon or --stdio\n\n{USAGE}");
    ExitCode::FAILURE
}

fn run_daemon() -> ExitCode {
    if let Err(e) = tty7_core::daemon::server::run_daemon() {
        eprintln!("tty7-server: daemon exited with error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_stdio(args: &[String]) -> io::Result<()> {
    #[cfg(windows)]
    {
        run_stdio_windows(args)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = args;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--stdio is not available on this platform",
        ))
    }

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        use tty7_core::daemon::duplex::StdioDuplex;
        use tty7_core::daemon::spawn;
        use tty7_core::host::local::LocalHost;
        use tty7_core::host::server;

        let force_serve = args.iter().any(|a| a == "--serve");
        let force_bridge = args.iter().any(|a| a == "--bridge");
        if force_serve && force_bridge {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--serve and --bridge ask for opposite things",
            ));
        }

        if args.iter().any(|a| a == "--pane") {
            if force_serve {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--pane is a bridge; there is nothing for --serve to answer in this process",
                ));
            }
            return bridge_panes();
        }

        let sock = match flag_value(args, "--control-sock") {
            Some(p) => std::path::PathBuf::from(p),
            None => server::control_socket_path()?,
        };

        let upstream = if force_serve {
            None
        } else {
            match UnixStream::connect(&sock) {
                Ok(s) => Some(s),
                Err(e) if force_bridge => return Err(e),
                Err(e) => {
                    log_stderr(format_args!(
                        "no control server at {} ({e})",
                        sock.display()
                    ));
                    if may_start_daemon(args) {
                        match spawn::ensure_running()
                            .map_err(io::Error::other)
                            .and_then(|()| UnixStream::connect(&sock))
                        {
                            Ok(s) => {
                                log_stderr(format_args!("started one; bridging to it"));
                                Some(s)
                            }
                            Err(e) => {
                                log_stderr(format_args!(
                                    "could not start one ({e}); serving in this process"
                                ));
                                None
                            }
                        }
                    } else {
                        log_stderr(format_args!("serving in this process"));
                        None
                    }
                }
            }
        };

        match upstream {
            Some(s) => bridge(s),
            None => {
                let link = StdioDuplex::take()?;
                server::serve_with(
                    link,
                    LocalHost::shared(),
                    tty7_core::daemon::server::control_services(),
                )
            }
        }
    }
}

#[cfg(unix)]
fn bridge_panes() -> io::Result<()> {
    use tty7_core::daemon::{spawn, transport};

    let upstream = match transport::connect() {
        Ok(s) => s,
        Err(e) => {
            log_stderr(format_args!(
                "no pane daemon at {} ({e}); starting one",
                transport::endpoint_display()
            ));
            spawn::ensure_running().map_err(io::Error::other)?;
            transport::connect()?
        }
    };
    bridge(upstream)
}

/// What [`bridge`] needs from the connection to the daemon: a unix socket on
/// Linux and macOS, a loopback TCP stream on Windows.
trait Upstream: io::Read + io::Write + Send + Sized + 'static {
    fn duplicate(&self) -> io::Result<Self>;
    fn shutdown_both(&self);
}

#[cfg(unix)]
impl Upstream for std::os::unix::net::UnixStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_both(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(windows)]
impl Upstream for std::net::TcpStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_both(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(any(unix, windows))]
fn bridge<S: Upstream>(upstream: S) -> io::Result<()> {
    use std::io::Write as _;

    let mut up_read = upstream.duplicate()?;
    let mut up_write = upstream.duplicate()?;

    let feeder_socket = upstream.duplicate()?;
    let feeder = std::thread::Builder::new()
        .name("tty7-stdio-bridge-in".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let _ = io::copy(&mut stdin, &mut up_write);
            feeder_socket.shutdown_both();
        })?;

    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match up_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                upstream.shutdown_both();
                drop(feeder);
                return Err(e);
            }
        }
    }

    upstream.shutdown_both();
    drop(feeder);
    Ok(())
}

/// `--stdio` on a Windows host: always a bridge, to the daemon's loopback
/// endpoints (a TCP port and a token, recorded in the config directory).
///
/// There is no `--serve` here. Serving in this process needs stdin and stdout
/// as a duplex the control server can own, which only exists on unix; and a
/// remote Windows host always has a daemon to bridge to, because the installer
/// starts one before any link is opened.
///
/// A daemon this starts itself — the fallback when none answers — lives inside
/// the SSH session's job object and ends with that connection. The installer
/// launches the long-lived one outside of it; see `install::windows_host`.
#[cfg(windows)]
fn run_stdio_windows(args: &[String]) -> io::Result<()> {
    use std::time::{Duration, Instant};
    use tty7_core::daemon::{spawn, transport};
    use tty7_core::host::server;

    let force_serve = args.iter().any(|a| a == "--serve");
    let force_bridge = args.iter().any(|a| a == "--bridge");
    if force_serve && force_bridge {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--serve and --bridge ask for opposite things",
        ));
    }
    if force_serve {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--serve is not available on Windows; without it, --stdio bridges to the daemon",
        ));
    }

    if args.iter().any(|a| a == "--pane") {
        let upstream = match transport::connect() {
            Ok(s) => s,
            Err(e) => {
                log_stderr(format_args!(
                    "no pane daemon at {} ({e}); starting one",
                    transport::endpoint_display()
                ));
                spawn::ensure_running().map_err(io::Error::other)?;
                transport::connect()?
            }
        };
        return bridge(upstream);
    }

    let explicit = flag_value(args, "--control-sock");
    let connect = || match &explicit {
        Some(path) => transport::connect_endpoint_at(std::path::Path::new(path)),
        None => server::connect_control(),
    };
    let upstream = match connect() {
        Ok(s) => s,
        Err(e) if force_bridge || !may_start_daemon(args) => return Err(e),
        Err(e) => {
            log_stderr(format_args!(
                "no control server answering ({e}); starting one"
            ));
            spawn::ensure_running().map_err(io::Error::other)?;
            // `ensure_running` returns once the pane endpoint answers; the
            // control listener opens a moment after it.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match connect() {
                    Ok(s) => break s,
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        }
    };
    bridge(upstream)
}
fn may_start_daemon(args: &[String]) -> bool {
    flag_value(args, "--control-sock").is_none()
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let with_eq = format!("{flag}=");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix(&with_eq) {
            return Some(v.to_string());
        }
        if arg == flag {
            return it.next().cloned();
        }
    }
    None
}

fn log_stderr(args: std::fmt::Arguments<'_>) {
    eprintln!("tty7-server: {args}");
}

fn apply_config_dir_arg(args: &[String]) {
    if let Some(path) = flag_value(args, "--config-dir") {
        tty7_core::core::config::set_config_dir(path.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn a_named_control_socket_suppresses_starting_a_daemon() {
        assert!(may_start_daemon(&argv(&[])));
        assert!(may_start_daemon(&argv(&["--serve"])));
        assert!(!may_start_daemon(&argv(&["--control-sock", "/tmp/x.sock"])));
        assert!(!may_start_daemon(&argv(&["--control-sock=/tmp/x.sock"])));
    }
}
