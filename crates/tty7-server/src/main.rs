use std::io;
use std::process::ExitCode;

const USAGE: &str = "\
tty7-server — the tty7 session daemon, headless

USAGE:
    tty7-server --daemon [--config-dir <dir>]
    tty7-server --stdio [--serve | --bridge] [--control-sock <path>]
    tty7-server --stdio --pane [--config-dir <dir>]
    tty7-server agent-hook <agent> <event>

OPTIONS:
    --daemon              Serve panes and control connections until killed
    --stdio               Carry one control connection on stdin/stdout
      --serve               Answer requests in this process (no socket)
      --bridge              Forward to the machine's control socket
      --pane                Forward to the machine's *pane* socket instead
      --control-sock <p>    Use <p> as the control socket instead of the default
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
    #[cfg(not(unix))]
    {
        let _ = args;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--stdio is a Unix path; a Windows server is reached over its own transport",
        ));
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

#[cfg(unix)]
fn bridge(upstream: std::os::unix::net::UnixStream) -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::net::Shutdown;

    let mut up_read = upstream.try_clone()?;
    let mut up_write = upstream.try_clone()?;

    let feeder_socket = upstream.try_clone()?;
    let feeder = std::thread::Builder::new()
        .name("tty7-stdio-bridge-in".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let _ = io::copy(&mut stdin, &mut up_write);
            let _ = feeder_socket.shutdown(Shutdown::Both);
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
                let _ = upstream.shutdown(Shutdown::Both);
                drop(feeder);
                return Err(e);
            }
        }
    }

    let _ = upstream.shutdown(Shutdown::Both);
    drop(feeder);
    Ok(())
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
