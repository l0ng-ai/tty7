//! `tty7-server` — the tty7 session daemon with no GUI attached.
//!
//! This is the binary that runs on the machine a *remote* workspace lives on.
//! It runs the same
//! `daemon::server` the local GUI auto-spawns, plus the control listener that
//! backs a remote `Host`; the only difference from the GUI's daemon is that
//! nothing on this side ever opens a window, which is why the code it needs had
//! to leave the GUI crate first.
//!
//! | Command | Effect |
//! |---|---|
//! | `--daemon` | Serve panes *and* control connections in the foreground until killed |
//! | `--stdio` | Carry one control connection on this process's stdin/stdout |
//! | `agent-hook <agent> <event>` | Emit one agent sentinel event; the same code the GUI binary runs |
//!
//! # The two sockets
//!
//! `--daemon` listens twice, on purpose:
//!
//! | Dialect | Endpoint | Served by |
//! |---|---|---|
//! | Panes (`daemon::protocol`) | `<config-dir>/daemon.sock` | `daemon::server::run` |
//! | Control (`daemon::control`) | `$XDG_RUNTIME_DIR/tty7/daemon.sock` | `host::server` |
//!
//! They are separate because the roles are separate. A machine can back a remote
//! workspace's file tree without hosting a single pane, and a pane daemon that
//! predates the control dialect must keep working untouched. Folding control
//! into the pane listener would have made every existing client's version
//! negotiation answer for a feature it does not use.
//!
//! # `--stdio`
//!
//! Two jobs behind one flag, chosen by whether a control server is already
//! listening on this machine:
//!
//! | Situation | Mode | Why |
//! |---|---|---|
//! | A `--daemon` is up | **bridge** — copy bytes between stdio and its socket | One server per machine owns the state; a second one would fork it |
//! | Nothing is listening | **serve** — answer control requests here | A box with no daemon still has to be reachable |
//!
//! `--bridge` and `--serve` force one or the other. The auto choice is what
//! makes `ssh host tty7-server --stdio` work whether or not the remote already
//! had a daemon, which is the fallback path for
//! `AllowStreamLocalForwarding no`, the only path under WSL, and how the
//! end-to-end conformance test reaches a real server without an sshd.

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

    // The agent-hook emitter runs before anything else touches config, logging
    // or the crash handler: it is a fire-and-forget child of an agent's hook
    // runner that must stay silent and exit fast, and the same code the GUI
    // binary runs for `tty7 agent-hook`. Only the binary that carries it
    // changed — a remote machine has a `tty7-server` and no `tty7`.
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
    // Before the config dir, the crash handler and the logger, like `--version`:
    // a client asking what this binary speaks must not touch the machine's
    // state, and must answer even on a box where the config dir is unwritable.
    //
    // One line of JSON on stdout, because the reader is a client parsing SSH
    // output rather than a person (`install::RemoteProtocol::parse`).
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

    // Resolve the config-dir override before anything touches config, session,
    // or the socket path — they all resolve under it, so the order matters.
    // Same parsing as the GUI's `apply_config_dir_arg`.
    apply_config_dir_arg(&args);

    // Panics in the server are recorded to `crash.log` in the config dir, for
    // the same reason the GUI does it: on a headless box there is no console to
    // read a backtrace off, and the process that notices the crash is a client
    // on the other end of a socket.
    tty7_core::core::crash::install("server");
    // Same reasoning for the ordinary log records: a headless box has no
    // console, and the client that notices a problem is on the far end of a
    // socket. Off unless `TTY7_LOG` asks for it.
    tty7_core::core::logfile::install("server");

    if args.iter().any(|a| a == "--stdio") {
        return match run_stdio(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                // stderr, never stdout: stdout is the protocol.
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

/// Serve panes and control connections until killed.
///
/// The whole of it lives in [`tty7_core::daemon::server::run_daemon`], shared
/// verbatim with `tty7 --daemon`: local and remote machines run the identical
/// daemon, which is what makes "one machine = one daemon = one workspace tree"
/// a fact rather than a convention.
fn run_daemon() -> ExitCode {
    if let Err(e) = tty7_core::daemon::server::run_daemon() {
        eprintln!("tty7-server: daemon exited with error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Carry one control connection on this process's stdin/stdout.
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

        // Probe unless told which mode to use. Connecting is the only way to
        // tell a live server from a socket file a crash left behind, and it is
        // also exactly the connection the bridge would have made anyway.
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
                    // One control server per machine, started if nobody has —
                    // the same rule `bridge_panes` follows one dialect over,
                    // and for the same reason. Two `--stdio` sessions both
                    // falling through to serving in-process would each hold
                    // their own `MachineStore` over the one file, and
                    // `persist` writes the whole document: the second to save
                    // silently drops the first's changes. Their attachment
                    // registries would be separate too, which makes design
                    // the takeover a no-op between them — both clients would
                    // hold the same workspace and neither would be told.
                    //
                    // Not attempted when the caller named a socket: starting a
                    // daemon binds the machine's default endpoint, not theirs,
                    // so it would be a daemon nobody asked for and nobody uses.
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
                // Takes stdin/stdout away from the rest of the process before a
                // single frame is written — see `StdioDuplex::take`.
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

/// Carry one **pane** connection on stdin/stdout, bridged to this machine's pane
/// socket.
///
/// # Why panes need their own stdio mode
///
/// `--daemon` listens twice (see this module's header), and a routed connection
/// is for exactly one of the two dialects. The control half already had a way in
/// — plain `--stdio`. The pane half had none, which is why a remote workspace
/// could browse a file tree and could not open a single terminal.
///
/// # Why it always bridges and never serves
///
/// Panes are *state*. Serving them in this process would give every routed
/// connection its own registry, so a pane would die with the window that opened
/// it and `List` would never see anything anyone else spawned — the exact
/// failure `install::wsl::ensure_wsl_server`'s doc warns about, one layer down.
/// There is one pane daemon per machine and this connects to it, starting it
/// first if nobody has.
#[cfg(unix)]
fn bridge_panes() -> io::Result<()> {
    use tty7_core::daemon::{spawn, transport};

    let upstream = match transport::connect() {
        Ok(s) => s,
        Err(e) => {
            // `ensure_running` re-execs *this* binary with `--daemon`, which is
            // what starts both listeners. Normally the install path has already
            // done it and this never runs; it covers the daemon dying between
            // that check and this connection.
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

/// Copy bytes between this process's stdio and an already-running control
/// server, in both directions, until either side stops.
///
/// Deliberately dumb: it parses nothing. The version handshake this stream
/// carries is between the *client* and the server at the far end, and a bridge
/// that understood the frames would be a third opinion about
/// the protocol version, which is exactly the coupling the design forbids.
#[cfg(unix)]
fn bridge(upstream: std::os::unix::net::UnixStream) -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::net::Shutdown;

    let mut up_read = upstream.try_clone()?;
    let mut up_write = upstream.try_clone()?;

    // Upstream → stdout on this thread, stdin → upstream on another. Either
    // direction ending means the session is over, so whichever finishes first
    // shuts the socket down and the other returns immediately instead of
    // parking on a peer that will never speak again.
    //
    // **The feeder is never joined.** Shutting the socket down wakes a thread
    // blocked on *the socket*, but this one is blocked on `stdin`, and nothing
    // this process can do wakes that — the far end of the pipe is `ssh`, or a
    // parent that has no reason to close it. Joining it turns "the server hung
    // up" into a bridge that never exits and, worse, never closes its stdout, so
    // the client at the far end waits forever for an EOF that is sitting in this
    // process. Returning lets the process exit, which closes stdout, which is
    // the signal the client is actually waiting for. The takeover is
    // the case that made this visible: the server closes the displaced session's
    // link, and that has to reach the client through this bridge.
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
                // Flushed per read, not per buffer: a control reply is useless
                // sitting in a buffer waiting for the next one, and the peer is
                // blocked on it.
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

/// `--flag <value>` or `--flag=<value>`, first occurrence wins.
/// Whether a failed control probe may start the machine's daemon.
///
/// Only when the caller did not name a socket. `--control-sock` says "this
/// endpoint", and `spawn::ensure_running` binds the machine's default one — so
/// starting a daemon there would leave a process nobody asked for and nobody
/// reaches. It is also what keeps the test suite, and any `--config-dir`
/// isolation built on it, from spraying daemons across a developer's machine.
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

/// stderr only. In `--stdio` mode stdout belongs to the protocol, and the
/// `log` crate has no sink configured in this binary.
fn log_stderr(args: std::fmt::Arguments<'_>) {
    eprintln!("tty7-server: {args}");
}

/// Honour `--config-dir <dir>` / `--config-dir=<dir>`, first occurrence wins —
/// the same contract (and the same first-call-wins `set_config_dir`) as the GUI.
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

    /// A named socket suppresses the daemon start, in both spellings of the
    /// flag. Without this guard every `--stdio` in the test suite that points
    /// at a temp socket would start a real daemon on the developer's machine.
    #[test]
    fn a_named_control_socket_suppresses_starting_a_daemon() {
        assert!(may_start_daemon(&argv(&[])));
        assert!(may_start_daemon(&argv(&["--serve"])));
        assert!(!may_start_daemon(&argv(&["--control-sock", "/tmp/x.sock"])));
        assert!(!may_start_daemon(&argv(&["--control-sock=/tmp/x.sock"])));
    }
}
