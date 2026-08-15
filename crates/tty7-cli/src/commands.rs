use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tty7_core::core::agent_hooks::{HookAgent, HooksState};
use tty7_core::core::machine::{Axis, Machine, PaneSeed, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::core::tab_view::tab_views_of;
use tty7_core::daemon::control::{
    CONTROL_VERSION, ControlEvent, ControlRequest, ReplyOk, RouteInfo,
};
use tty7_core::daemon::protocol::PROTOCOL_VERSION;

use crate::address::{self, Context, WorkspaceAddress};
use crate::backend::{Backend, RunSpec};
use crate::cli::{
    CaptureArgs, Cli, Command, MachineCmd, PaneCmd, RunArgs, SendArgs, ServerCmd, SplitArgs,
    TabCmd, WaitArgs, WaitState, WsCmd,
};
use crate::output;
use crate::resolve;
use crate::screen;

#[derive(Debug)]
pub struct Report {
    pub human: String,
    pub json: Value,
}

#[derive(Debug)]
pub enum Outcome {
    Report(Report),
    /// A verb that stands in for a child process: the code is the CLI's own
    /// exit status. The report comes too, so `--json` still answers here.
    Exit(i32, Report),
}

pub const EXIT_CODE_UNKNOWN: &str = "the command exited but its real exit code could not be determined — exiting 1 as a \
     stand-in, not as the command's own code";

fn report(human: impl Into<String>, json: Value) -> Result<Outcome> {
    Ok(Outcome::Report(Report {
        human: human.into(),
        json,
    }))
}

pub fn execute(cli: Cli, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let json_mode = cli.json;
    let machine = cli.machine.clone();
    match cli.command {
        None => match cli.path {
            // clap has no subcommand for this word, so it landed in [PATH].
            // Treat a word that is not a path as the typo it almost certainly
            // is: launching the GUI for `tty7 statu` would hide the typo.
            Some(word) if !looks_like_a_path(&word) => bail!(
                "unknown subcommand '{}' — run `tty7 --help` for the list. \
                 (A path in this position would open the GUI there, but \
                 '{}' does not name one.)",
                word.display(),
                word.display(),
            ),
            path => launch_gui(path, machine.as_deref(), backend),
        },
        Some(Command::Ls) | Some(Command::Ws(WsCmd::Ls)) => ws_ls(backend),
        Some(Command::Ws(WsCmd::Tree { ws })) => ws_tree(ws.as_deref(), ctx, backend),
        Some(Command::Ws(WsCmd::New { name })) => ws_new(name, backend),
        Some(Command::Ws(WsCmd::Rename { ws, name })) => ws_rename(&ws, name, backend),
        Some(Command::Ws(WsCmd::Stop { .. })) => bail!(
            "`tty7 ws stop` is not implemented yet — the control dialect has no \
             workspace-stop request; it arrives with the multi-subscriber slice"
        ),
        Some(Command::Ws(WsCmd::Rm { ws })) => ws_rm(&ws, backend),
        Some(Command::Ws(WsCmd::Attach { ws })) => {
            ws_attach(address::parse_workspace(&ws), backend)
        }
        Some(Command::Ws(WsCmd::Detach { ws })) => ws_detach(&ws, backend),
        Some(Command::New { path, open }) => new_workspace(path, open, backend),
        Some(Command::Run(args)) => run(args, ctx, backend),
        Some(Command::Split(args)) | Some(Command::Pane(PaneCmd::Split(args))) => {
            pane_split(args, ctx, backend)
        }
        Some(Command::Send(args)) => send(args, ctx, backend),
        Some(Command::Capture(args)) => capture(args, ctx, backend),
        Some(Command::Procs { target }) => procs(target.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::Ls { ws })) => tab_ls(ws.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::New { ws, cwd })) => tab_new(ws.as_deref(), cwd, ctx, backend),
        Some(Command::Tab(TabCmd::Close { tab })) => tab_close(&tab, backend),
        Some(Command::Tab(TabCmd::Rename { tab, name })) => tab_rename(&tab, name, backend),
        Some(Command::Tab(TabCmd::Move { tab, index })) => tab_move(&tab, index, backend),
        Some(Command::Pane(PaneCmd::Ls { ws, all })) => pane_ls(ws.as_deref(), all, backend),
        Some(Command::Pane(PaneCmd::Close { targets, orphans })) => {
            pane_close(&targets, orphans, ctx, backend)
        }
        Some(Command::Events) => events(json_mode, backend),
        Some(Command::Agents) => agents(backend),
        Some(Command::Wait(args)) => wait(args, ctx, backend),
        Some(Command::Status) | Some(Command::Server(ServerCmd::Status)) => status(backend),
        Some(Command::Machine(MachineCmd::Ls)) => machine_ls(backend),
        Some(Command::Machine(MachineCmd::Connect { .. }))
        | Some(Command::Machine(MachineCmd::Disconnect { .. })) => bail!(
            "managing machine links from the CLI is not implemented yet — \
             use the GUI's connection manager for now"
        ),
        Some(Command::Server(ServerCmd::Start)) => {
            local_server(machine.as_deref(), "start", crate::server::start)
        }
        Some(Command::Server(ServerCmd::Stop)) => {
            local_server(machine.as_deref(), "stop", crate::server::stop)
        }
        Some(Command::Server(ServerCmd::Restart)) => {
            local_server(machine.as_deref(), "restart", crate::server::restart)
        }
        Some(Command::Server(ServerCmd::Logs)) => {
            local_server(machine.as_deref(), "logs", crate::server::logs)
        }
        Some(Command::Doctor) => doctor(ctx, backend),
    }
}

fn local_server(
    machine: Option<&str>,
    verb: &str,
    act: fn() -> Result<Outcome>,
) -> Result<Outcome> {
    if let Some(machine) = machine {
        bail!(
            "`tty7 server {verb}` manages only the server on THIS machine — with -m {machine} \
             it would still have acted on the LOCAL server, so it was refused; a remote \
             machine's server lifecycle is handled by the install/reconnect flows, not the CLI"
        );
    }
    act()
}

/// Whether a bare word in the `[PATH]` position was meant as a path.
///
/// Anything with a separator, a leading `.`/`~`, or that actually exists on
/// disk counts. A plain word like `tree` or `statu` does not — it is a
/// mistyped subcommand, and saying so beats offering to open the GUI there.
fn looks_like_a_path(path: &std::path::Path) -> bool {
    path.to_str().is_some_and(|s| {
        s.starts_with('/')
            || s.starts_with('.')
            || s.starts_with('~')
            || s.contains('/')
            || s.contains('\\')
    }) || path.exists()
}

fn launch_gui(
    path: Option<std::path::PathBuf>,
    machine: Option<&str>,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    if let Some(machine) = machine {
        bail!(
            "`tty7 [PATH]` controls the GUI on this machine and cannot be combined with -m {machine}"
        );
    }

    let path = path.map(resolve_gui_path).transpose()?;
    let wire_path = path.as_deref().and_then(gui_wire_path);
    // A live GUI receives the request through the daemon. If the daemon itself
    // is absent, the same fallback as "no GUI registered" starts the app, which
    // will start its daemon during normal initialization.
    let request_path = path
        .is_none()
        .then_some(None)
        .or_else(|| wire_path.clone().map(Some));
    let delivered = match request_path {
        Some(path) => match backend.control(ControlRequest::GuiOpen {
            path,
            workspace: None,
        }) {
            Ok(ReplyOk::Bool(delivered)) => delivered,
            Ok(other) => bail!("the server answered GuiOpen with {other:?}"),
            Err(_) => false,
        },
        // The JSON control protocol cannot preserve a native non-Unicode path.
        // Launching the app does: Command passes the Path as an OsStr, and the
        // app keeps it locally when it cannot forward it to another process.
        None => false,
    };

    if !delivered {
        crate::gui::launch(path.as_deref())?;
    }
    report(
        "",
        json!({
            "path": wire_path,
            "delivered": delivered,
            "launched": !delivered,
        }),
    )
}

fn gui_wire_path(path: &std::path::Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

fn resolve_gui_path(raw: std::path::PathBuf) -> Result<std::path::PathBuf> {
    let expanded = raw.to_str().and_then(expand_home).unwrap_or(raw);
    // Do not canonicalize here: preserving the caller's junction or symlink
    // spelling keeps shell cwd reporting and tab labels consistent.
    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .context("reading the current directory")?
            .join(expanded)
    };
    let metadata =
        std::fs::metadata(&path).with_context(|| format!("opening {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

fn expand_home(raw: &str) -> Option<std::path::PathBuf> {
    let rest = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"));
    if raw != "~" && rest.is_none() {
        return None;
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)?;
    Some(match rest {
        Some(rest) => home.join(rest),
        None => home,
    })
}

fn fetch_machine(backend: &mut dyn Backend) -> Result<Machine> {
    match backend.control(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(m) => Ok(*m),
        other => bail!("the server answered MachineGet with {other:?}"),
    }
}

fn workspace_summary(ws: &Workspace) -> Value {
    json!({
        "id": ws.id.to_string(),
        "name": ws.name,
        "tabs": ws.tabs.len(),
        "panes": ws.tabs.iter().map(|t| t.root.pane_ids().len()).sum::<usize>(),
        "attached": ws.attachment.as_ref().map(|a| a.hostname.clone()),
    })
}

fn ws_ls(backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let summaries: Vec<Value> = machine.workspaces.iter().map(workspace_summary).collect();
    report(
        output::workspace_table(&machine),
        json!({ "workspaces": summaries }),
    )
}

fn resolve_ws(explicit: Option<&str>, ctx: &Context, machine: &Machine) -> Result<WorkspaceId> {
    let addr = address::workspace_or_context(explicit, ctx)?;
    Ok(resolve::workspace(machine, &addr)?.id)
}

fn ws_tree(explicit: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    match backend.control(ControlRequest::WorkspaceTree { workspace: id })? {
        ReplyOk::WorkspaceTree(ws) => report(
            output::workspace_tree(&ws, &machine),
            serde_json::to_value(&*ws)?,
        ),
        other => bail!("the server answered WorkspaceTree with {other:?}"),
    }
}

fn ws_new(name: Option<String>, backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::WorkspaceCreate {
        name,
        workspace: None,
    })? {
        ReplyOk::WorkspaceTree(ws) => report(
            ws.id.to_string(),
            json!({ "id": ws.id.to_string(), "name": ws.name }),
        ),
        other => bail!("the server answered WorkspaceCreate with {other:?}"),
    }
}

fn ws_rename(ws: &str, name: String, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    backend.control(ControlRequest::WorkspaceRename {
        workspace: id,
        name: Some(name.clone()),
    })?;
    report("", json!({ "id": id.to_string(), "name": name }))
}

fn ws_rm(ws: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    let reply = backend.control(ControlRequest::WorkspaceRemove { workspace: id })?;
    hang_up_removed_panes("WorkspaceRemove", reply, backend)?;
    report("", json!({ "removed": id.to_string() }))
}

fn ws_attach(addr: WorkspaceAddress, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &addr)?.id;
    match backend.control(ControlRequest::WorkspaceAttach { id: id.to_string() })? {
        ReplyOk::Attached { took_over_from } => {
            let human = took_over_from
                .as_ref()
                .map(|host| format!("took over from {host}"))
                .unwrap_or_default();
            report(
                human,
                json!({ "attached": id.to_string(), "took_over_from": took_over_from }),
            )
        }
        other => bail!("the server answered WorkspaceAttach with {other:?}"),
    }
}

fn ws_detach(ws: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    backend.control(ControlRequest::WorkspaceDetach { id: id.to_string() })?;
    report("", json!({ "detached": id.to_string() }))
}

fn new_workspace(path: Option<String>, open: bool, backend: &mut dyn Backend) -> Result<Outcome> {
    // Same instruction-not-a-hint rule as `run --cwd`, and checked before the
    // workspace exists: a path that does not resolve left the pane in whatever
    // directory the CLI was started from, so `tty7 new ~/porj` answered with an
    // id and a workspace rooted somewhere the caller never named. Nothing runs
    // here, so this only misplaces a shell — but it misplaces it silently.
    if let Some(dir) = path.as_deref()
        && backend.is_this_machine()
    {
        let at = std::path::Path::new(dir);
        if !at.is_dir() {
            let why = match at.exists() {
                true => "not a directory",
                false => "no such directory",
            };
            bail!("{dir}: {why} on this machine");
        }
    }
    let ws = match backend.control(ControlRequest::WorkspaceCreate {
        name: None,
        workspace: None,
    })? {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => bail!("the server answered WorkspaceCreate with {other:?}"),
    };
    let pane = backend.spawn_shell(ws.id, path.clone())?;
    // The workspace goes too. It was made by this command and holds nothing,
    // so leaving it behind adds an empty row to `tty7 ls` that the caller
    // never asked for and would have to clear by hand.
    if let Err(e) = filing_or_hang_up(backend, pane, |b| {
        b.control(ControlRequest::TabCreate {
            workspace: ws.id,
            at: None,
            pane: PaneSeed {
                pane,
                cwd: path,
                ssh_spec: None,
                agent: None,
                shell: None,
            },
            tab: None,
        })
    }) {
        let _ = backend.control(ControlRequest::WorkspaceRemove { workspace: ws.id });
        return Err(e);
    }
    // Only when asked: a workspace made from a script has no business
    // stealing the screen, and the switcher lists it either way.
    let opened = match open {
        false => false,
        true => match backend.control(ControlRequest::GuiOpen {
            path: None,
            workspace: Some(ws.id),
        }) {
            Ok(ReplyOk::Bool(opened)) => {
                // The workspace exists by the time we ask, so an unreachable
                // GUI is worth a word and not an exit code: failing here would
                // read as "nothing was made".
                if !opened {
                    eprintln!(
                        "tty7: no GUI is running on this machine; \
                         the workspace was made all the same"
                    );
                }
                opened
            }
            Ok(other) => bail!("the server answered GuiOpen with {other:?}"),
            Err(error) => {
                eprintln!(
                    "tty7: could not ask the GUI to open it ({error:#}); \
                     the workspace was made all the same"
                );
                false
            }
        },
    };
    report(
        ws.id.to_string(),
        json!({ "id": ws.id.to_string(), "pane": pane, "opened": opened }),
    )
}

fn run(args: RunArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    // An explicit `--cwd` is an instruction, not a hint. The daemon falls back
    // to a directory that does resolve when the one it is handed does not —
    // right for a cwd inherited from a pane's OSC 7, which is only as good as
    // the shell that reported it, and wrong for one the user typed: a mistyped
    // `--cwd` otherwise ran the command in whatever directory the *CLI* was
    // started from and reported success, so `run --cwd ~/proj -- make` built
    // the wrong tree and said it worked.
    //
    // Only answerable for this machine; a routed `--cwd` names a directory on
    // the far side, as `agent_hooks_state` says of config for the same reason.
    if let Some(dir) = args.cwd.as_deref()
        && backend.is_this_machine()
    {
        let path = std::path::Path::new(dir);
        if !path.is_dir() {
            let why = match path.exists() {
                true => "not a directory",
                false => "no such directory",
            };
            bail!("--cwd {dir}: {why} on this machine");
        }
    }
    let workspace = match args.ws.as_deref() {
        Some(explicit) => {
            let machine = fetch_machine(backend)?;
            Some(resolve::workspace(&machine, &address::parse_workspace(explicit))?.id)
        }
        None => ctx
            .ws
            .as_deref()
            .and_then(|v| v.parse::<WorkspaceId>().ok()),
    };
    if args.keep && workspace.is_none() {
        bail!(
            "`run --keep` keeps the pane alive, so it must be filed into a workspace — \
             pass --ws, or run inside a tty7 shell where $TTY7_WS names one"
        );
    }
    // `--keep` files the pane into its workspace *after* the spawn, so a
    // `$TTY7_WS` that no longer names one here — a workspace since removed, or
    // a shell opened against another machine — started the pane and then failed
    // the filing, leaving it running with nothing holding it. Recoverable
    // (`pane ls --all`, then `pane close`), but nobody asked for the pane.
    //
    // The `--ws` arm above has always resolved before spawning. This is that
    // check for the inherited id, and only when `--keep` needs it: a plain
    // `run` uses the workspace as an ownership stamp, where a stale id costs
    // nothing and a round trip to find out would.
    if args.keep && args.ws.is_none() {
        let id = workspace.expect("checked above: --keep requires a workspace");
        let machine = fetch_machine(backend)?;
        resolve::workspace(&machine, &address::WorkspaceAddress::Id(id))?;
    }
    let pane = backend.run_spawn(RunSpec {
        workspace,
        cwd: args.cwd.clone(),
        command: args.cmd,
        keep: args.keep,
    })?;
    if args.keep {
        let workspace = workspace.expect("checked above: --keep requires a workspace");
        backend.control(ControlRequest::TabCreate {
            workspace,
            at: None,
            pane: PaneSeed {
                pane,
                cwd: args.cwd,
                ssh_spec: None,
                agent: None,
                shell: None,
            },
            tab: None,
        })?;
    }
    let (code, exact) = match backend.run_wait()? {
        Some(code) => (code, true),
        None => {
            eprintln!("tty7: {EXIT_CODE_UNKNOWN}");
            (1, false)
        }
    };
    // The command's own output already went to stdout as it streamed, so the
    // human report is empty — but --json still owes the caller a machine
    // readable answer, and `exit_code_known` is how it tells a real 1 from the
    // stand-in above.
    Ok(Outcome::Exit(
        code,
        Report {
            human: String::new(),
            json: json!({
                "pane": pane,
                "exit": code,
                "exit_code_known": exact,
                "kept": args.keep,
            }),
        },
    ))
}

/// Do the filing that follows a spawn, hanging the pane up if it fails.
///
/// Every verb that puts a new pane in the tree spawns it first and asks the
/// tree to hold it second, because the seed carries the daemon's own pane id.
/// That leaves a window: a refusal in between — a workspace removed since it
/// was resolved, a reply the client cannot read, a link that dropped — ends
/// the command with a shell running that nothing references. `pane ls --all`
/// shows it, the tree does not, and `pane close --orphans` is the only way to
/// find it. Nobody asked for that pane, so it goes back down with the error.
fn filing_or_hang_up<T>(
    backend: &mut dyn Backend,
    pane: u64,
    file: impl FnOnce(&mut dyn Backend) -> Result<T>,
) -> Result<T> {
    match file(backend) {
        Ok(v) => Ok(v),
        Err(e) => {
            let _ = backend.kill_pane(pane);
            Err(e)
        }
    }
}

fn pane_split(args: SplitArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let machine = fetch_machine(backend)?;
    let workspace = resolve::workspace_of_pane(&machine, pane)?.id;
    let cwd = machine
        .panes
        .iter()
        .find(|p| p.id == pane)
        .and_then(|p| p.cwd.clone());
    let axis = if args.horizontal {
        Axis::Horizontal
    } else {
        Axis::Vertical
    };
    // Before the spawn, because the pane exists before the tree hears about
    // it. `--ratio nan` cannot even be put on the wire — serialising it breaks
    // the control connection, so the daemon's own "a split ratio must be a
    // finite number" never arrives — and by then a shell is already running
    // that no tree holds. The daemon's clamp to a usable range is deliberate
    // and stays its business; this only refuses what it would refuse too.
    if !args.ratio.is_finite() {
        bail!("--ratio {} is not a finite number", args.ratio);
    }
    let new = backend.spawn_shell(workspace, cwd.clone())?;
    filing_or_hang_up(backend, new, |b| {
        b.control(ControlRequest::PaneSplit {
            workspace,
            pane,
            axis,
            ratio: args.ratio,
            new: PaneSeed {
                pane: new,
                cwd,
                ssh_spec: None,
                agent: None,
                shell: None,
            },
            first: false,
        })
    })?;
    report(format!("%{new}"), json!({ "pane": new }))
}

fn send(args: SendArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    const KEY_GAP: Duration = Duration::from_millis(200);

    // `--enter` is the same thing as `--key enter`, and predates it. Keeping it
    // as sugar rather than deprecating it: it reads better for the overwhelming
    // case, which is typing one command and running it. Going through the same
    // parser leaves one definition of what Enter puts on the wire — and the list
    // is built here, before the dispatch, because the dispatch has to count it:
    // `send %42 --enter` used to report "needs TEXT … or a --key to press" while
    // the docs called `--enter` shorthand for exactly such a key (#581).
    let mut pressed = args.keys.clone();
    if args.enter {
        pressed.push(crate::keys::parse("enter").expect("enter is in the vocabulary"));
    }

    // Three shapes reach here, and only the address is ever ambiguous:
    // `send %3 "text"`, `send "text"` (this pane), and — new with --key —
    // `send %3 --key C-c`, where there is no text at all and the lone
    // positional is therefore an address rather than the missing-text error it
    // has to stay in every other case. `send %3 --enter` is that third shape
    // too, with the one carve-out below: only a marked address is promoted by
    // `--enter` alone.
    //
    // The address-shaped-but-broken case must not fall through to "type it":
    // `send %3x --key C-c` used to type `%3x` into the *caller's own* pane and
    // then interrupt whatever was in front of them (#538). The guard is kept
    // narrow on purpose — `%` followed by a digit means "tried to write an
    // address", so the parse error propagates; `%` followed by anything else
    // (`%s/foo/bar/` driving vim's ex, `%!sort`) stays text, because refusing
    // it would break a use that was never ambiguous.
    let (target, text) = match (&args.first, &args.second) {
        (Some(first), Some(text)) => (Some(first.as_str()), Some(text.as_str())),
        (Some(first), None) => match address::parse_pane(first) {
            Ok(_) => {
                if pressed.is_empty() {
                    bail!(
                        "send needs TEXT after the pane address, or a --key to press \
                         — to type '{first}' literally, name the pane too: send %PANE {first}"
                    );
                }
                // A `--key` is always an explicit "press this", so it promotes
                // either spelling of the address. `--enter` is not, for an
                // *unmarked* id: `send 2 --enter` reads as "type 2 and run it"
                // far more often than "press Enter in pane 2", and #538 was
                // about never quietly retargeting a keystroke. The `%` is what
                // says which was meant, so it stays the loud error it is today.
                if args.keys.is_empty() && !first.starts_with('%') {
                    bail!(
                        "'{first}' is a bare pane id and --enter has nothing to type \
                         — to press Enter in pane {first}: send %{first} --enter; \
                         to type '{first}' and press Enter, name the pane too: \
                         send %PANE {first} --enter"
                    );
                }
                (Some(first.as_str()), None)
            }
            // `%` then a digit is someone writing an address, so the parse
            // error is the answer. Anything else is text and always was.
            Err(error) if tried_to_write_an_address(first) => return Err(error),
            Err(_) => (None, Some(first.as_str())),
        },
        (None, _) => {
            if pressed.is_empty() {
                bail!("send needs TEXT to type or a --key to press");
            }
            (None, None)
        }
    };

    let pane = address::pane_or_context(target, ctx)?;
    let mut already_wrote = false;
    if let Some(text) = text {
        let attempt = backend.send_input(pane, text.as_bytes().to_vec());
        or_no_such_pane(attempt, pane, backend)?;
        already_wrote = true;
    }
    for key in &pressed {
        // Raw-mode TUIs detect a fast stream as pasted input and intentionally
        // absorb Enter as a newline — and a menu being driven by arrow keys has
        // the same problem. Let each keystroke leave the burst window on its
        // own, which is what makes a sequence land as a sequence. Nothing
        // precedes the first write, though, so an interrupt stays immediate.
        if already_wrote {
            std::thread::sleep(KEY_GAP);
        }
        let attempt = backend.send_input(pane, key.bytes.clone());
        or_no_such_pane(attempt, pane, backend)?;
        already_wrote = true;
    }
    report(
        "",
        json!({
            "pane": pane,
            "sent": text.unwrap_or_default(),
            "enter": args.enter,
            "keys": pressed.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
        }),
    )
}

/// Whether a lone positional that failed to parse was reaching for an address
/// rather than being text. Only `%` followed by a digit qualifies: it is the
/// shape every pane address has, so `%3x` is a typo worth refusing, while
/// `%s/foo/bar/` and `%!sort` are the ex commands they look like. A bare `3x`
/// is not included — nothing marks it as an address, and it has always typed.
fn tried_to_write_an_address(s: &str) -> bool {
    s.strip_prefix('%')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

/// Says the pane is not there, when that is what went wrong.
///
/// The daemon's own refusal makes a fine diagnostic and a poor sentence: it
/// names the wire request rather than the verb that was typed, so `capture %99`
/// answered "observing pane %99: daemon refused Observe: no such pane 99" while
/// `procs %99` answered `no_such_pane` and `send %99` a third thing — three
/// readings of one condition, one of them naming a request no user has heard
/// of.
///
/// The registry is consulted only once something has already failed, so the
/// ordinary path still costs a single round trip, and a registry that cannot be
/// read leaves the original error alone: it is the better answer when the
/// daemon is the thing that is wrong.
fn or_no_such_pane<T>(result: Result<T>, pane: u64, backend: &mut dyn Backend) -> Result<T> {
    let Err(original) = result else {
        return result;
    };
    match backend.list_panes() {
        Ok(panes) if !panes.iter().any(|p| p.pane_id == pane) => {
            Err(not_running_or_absent(pane, backend))
        }
        _ => Err(original),
    }
}

/// Which of the two "nothing is running there" answers this pane has earned.
///
/// Absent from the running registry is not the same as absent from the machine:
/// a pane `run --keep` filed into a workspace stays in the tree, so `tab ls`
/// still names it. Only the tree can tell them apart, and it is fetched only
/// here — on a path that has already failed twice — so no ordinary call pays
/// for it. A tree that cannot be read falls back to the plainer answer.
fn not_running_or_absent(pane: u64, backend: &mut dyn Backend) -> anyhow::Error {
    match fetch_machine(backend)
        .ok()
        .and_then(|m| resolve::workspace_of_pane(&m, pane).ok().cloned())
    {
        Some(ws) => resolve::pane_not_running(pane, &ws),
        None => resolve::no_such_pane(pane),
    }
}

fn capture(args: CaptureArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let attempt = backend.capture(pane, args.scrollback);
    let segments = or_no_such_pane(attempt, pane, backend)?;
    // Raw is the default and stays byte-for-byte what the daemon stored, joined
    // in replay order; `--plain` hands the same bytes to a grid instead. Either
    // way `--json` carries whatever was printed, so a caller reads one field.
    let text = if args.plain {
        screen::render(&segments)
    } else {
        let bytes: Vec<u8> = segments.into_iter().flat_map(|s| s.bytes).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    report(text.clone(), json!({ "pane": pane, "text": text }))
}

fn procs(target: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(target, ctx)?;
    let procs = backend.procs(pane)?;
    // The daemon answers a pane it does not have exactly as it answers an idle
    // one: `registry.get` misses and the reply is an empty `PaneProcs`. Every
    // other verb taking a `%PANE` says when the pane is not there, and without
    // this an agent cannot tell "nothing is running in it" from "there is no
    // such pane" — both printed `nothing running in this pane` and exited 0.
    //
    // Asked only when the answer was empty, so a pane with anything in it
    // still costs one request. Checked against the registry rather than the
    // workspace tree, because a pane no workspace holds is still a pane whose
    // processes are worth reporting — that is exactly what `pane ls --all`
    // exists to surface.
    if procs.procs.is_empty()
        && procs.ports.is_empty()
        && !backend.list_panes()?.iter().any(|p| p.pane_id == pane)
    {
        return Err(resolve::no_such_pane(pane));
    }
    report(output::procs_tables(&procs), serde_json::to_value(&procs)?)
}

fn tab_ls(explicit: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    let ws = machine
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("resolve_ws returned an id straight out of this machine");
    let views = tab_views_of(ws, &machine.panes);
    let rows: Vec<Vec<String>> = ws
        .tabs
        .iter()
        .zip(&views)
        .map(|(tab, view)| {
            vec![
                format!("@{}", resolve::ordinal_of(&machine, tab.id).unwrap_or(0)),
                output::tab_label(view),
                // The GUI files tabs under a directory and shows its last
                // segment as the heading; the full path would be the widest
                // column in the table for no gain.
                tab.sidebar_group
                    .as_deref()
                    .map(|g| output::path_leaf(g).to_string())
                    .unwrap_or_else(|| "-".to_string()),
                tab.root.pane_ids().len().to_string(),
            ]
        })
        .collect();
    let tabs: Vec<Value> = ws
        .tabs
        .iter()
        .zip(&views)
        .map(|(tab, view)| {
            json!({
                "ordinal": resolve::ordinal_of(&machine, tab.id),
                "id": tab.id.to_string(),
                // `name` stays what someone actually named the tab — usually
                // nothing. `label` is what the table prints.
                "name": tab.name,
                "label": output::tab_label(view),
                "agent": view.agent.map(|a| a.display_name()),
                "group": tab.sidebar_group,
                "panes": tab.root.pane_ids(),
            })
        })
        .collect();
    report(
        output::table(&["TAB", "NAME", "GROUP", "PANES"], &rows),
        json!({ "workspace": id.to_string(), "tabs": tabs }),
    )
}

fn tab_new(
    explicit: Option<&str>,
    cwd: Option<String>,
    ctx: &Context,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    let pane = backend.spawn_shell(id, cwd.clone())?;
    let tab = filing_or_hang_up(backend, pane, |b| {
        match b.control(ControlRequest::TabCreate {
            workspace: id,
            at: None,
            pane: PaneSeed {
                pane,
                cwd,
                ssh_spec: None,
                agent: None,
                shell: None,
            },
            tab: None,
        })? {
            ReplyOk::TabTree(tab) => Ok(*tab),
            other => bail!("the server answered TabCreate with {other:?}"),
        }
    })?;
    report(
        format!("%{pane}"),
        json!({ "tab": tab.id.to_string(), "pane": pane }),
    )
}

fn tab_close(tab: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    let reply = backend.control(ControlRequest::TabClose { workspace, tab })?;
    hang_up_removed_panes("TabClose", reply, backend)?;
    report("", json!({ "closed": tab.to_string() }))
}

fn hang_up_removed_panes(request: &str, reply: ReplyOk, backend: &mut dyn Backend) -> Result<()> {
    let panes = match reply {
        ReplyOk::Panes(panes) => panes,
        other => bail!("the server answered {request} with {other:?}"),
    };
    let mut failures = Vec::new();
    for pane in panes {
        if let Err(error) = backend.kill_pane(pane) {
            failures.push(format!("%{pane}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!(
            "failed to hang up {} pane(s) removed by {request}: {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(())
}

fn tab_rename(tab: &str, name: String, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    backend.control(ControlRequest::TabRename {
        workspace,
        tab,
        name: Some(name.clone()),
    })?;
    report("", json!({ "tab": tab.to_string(), "name": name }))
}

fn tab_move(tab: &str, index: u64, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    backend.control(ControlRequest::TabMove {
        workspace,
        tab,
        to: index,
    })?;
    report("", json!({ "tab": tab.to_string(), "to": index }))
}

fn pane_ls(explicit: Option<&str>, all: bool, backend: &mut dyn Backend) -> Result<Outcome> {
    if all {
        return pane_ls_all(backend);
    }
    let machine = fetch_machine(backend)?;
    let only = match explicit {
        Some(s) => Some(resolve::workspace(&machine, &address::parse_workspace(s))?.id),
        None => None,
    };
    let mut panes = Vec::new();
    for ws in &machine.workspaces {
        if only.is_some_and(|id| id != ws.id) {
            continue;
        }
        for tab in &ws.tabs {
            for pane in tab.root.pane_ids() {
                let record = machine.panes.iter().find(|p| p.id == pane);
                panes.push(json!({
                    "pane": pane,
                    "workspace": ws.id.to_string(),
                    "tab": tab.id.to_string(),
                    "cwd": record.and_then(|r| r.cwd.clone()),
                    "live": record.map(|r| r.live),
                }));
            }
        }
    }
    report(
        output::pane_table(&machine, only),
        json!({ "panes": panes }),
    )
}

/// The registry's own list, annotated with the workspace holding each pane.
/// Panes with no holder are the ones every tree-walking listing misses: an
/// interrupted `tty7 run` leaves its pane running with nothing referencing it,
/// and until it shows up here there is no way to find or stop it.
fn pane_ls_all(backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let running = backend.list_panes()?;
    let held: Vec<(u64, WorkspaceId)> = machine
        .workspaces
        .iter()
        .flat_map(|ws| ws.tabs.iter().map(move |tab| (ws.id, tab)))
        .flat_map(|(id, tab)| tab.root.pane_ids().into_iter().map(move |pane| (pane, id)))
        .collect();
    let holder = |pane: u64| held.iter().find(|(p, _)| *p == pane).map(|(_, ws)| *ws);

    let panes: Vec<Value> = running
        .iter()
        .map(|info| {
            json!({
                "pane": info.pane_id,
                "workspace": holder(info.pane_id).map(|ws| ws.to_string()),
                "orphan": holder(info.pane_id).is_none(),
                "owner": info.owner,
                "title": info.title,
                "cwd": info.cwd,
                "live": info.alive,
            })
        })
        .collect();
    let orphans = running
        .iter()
        .filter(|info| holder(info.pane_id).is_none())
        .count();
    let mut human = output::registry_table(&running, &|pane| holder(pane).map(|ws| ws.to_string()));
    if orphans > 0 {
        human.push_str(&format!(
            "\n{orphans} pane(s) held by no workspace — `tty7 pane close %<id>` stops one, \
             `tty7 pane close --orphans` stops all of them\n"
        ));
    }
    report(human, json!({ "panes": panes, "orphans": orphans }))
}

fn pane_close(
    targets: &[String],
    orphans: bool,
    ctx: &Context,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    // One tree read for the whole batch: it resolves the orphan set and then
    // every pane's owning workspace.
    let machine = fetch_machine(backend)?;
    let panes = if orphans {
        let found = orphan_panes(&machine, backend)?;
        if found.is_empty() {
            return report("no orphan panes\n", json!({ "closed": [] }));
        }
        found
    } else if targets.is_empty() {
        vec![address::pane_or_context(None, ctx)?]
    } else {
        targets
            .iter()
            .map(|t| address::pane_or_context(Some(t), ctx))
            .collect::<Result<Vec<_>>>()?
    };

    // Every pane is attempted even if an earlier one fails — a reaper that
    // stops at the first error leaves the rest of the leak in place, which is
    // the state the caller was trying to fix.
    let mut closed = Vec::new();
    let mut failures = Vec::new();
    // The running-pane registry, read lazily on the first direct kill: the
    // direct path is fire-and-forget (the daemon never says whether it knew
    // the pane), so the registry is the only way `%99` can fail instead of
    // reporting `{"closed":[99]}` for a pane that never existed (#588).
    let mut running: Option<Vec<u64>> = None;
    for pane in panes {
        let outcome = match resolve::workspace_of_pane(&machine, pane) {
            Ok(ws) => {
                let workspace = ws.id;
                match backend.control(ControlRequest::PaneClose { workspace, pane }) {
                    Ok(reply) => hang_up_removed_panes("PaneClose", reply, backend),
                    Err(e) => Err(e),
                }
            }
            // No workspace holds it, so PaneClose has nothing to route through.
            // Hang it up directly instead of refusing — this is exactly the
            // orphan `pane ls --all` points the user at.
            Err(_) => {
                if running.is_none() {
                    running = Some(
                        backend
                            .list_panes()?
                            .iter()
                            .map(|info| info.pane_id)
                            .collect(),
                    );
                }
                if running.as_ref().is_some_and(|ids| ids.contains(&pane)) {
                    // A pane that exits between the listing and the kill is
                    // gone either way, which is what closing it wanted.
                    backend.kill_pane(pane)
                } else {
                    Err(anyhow::anyhow!("no such pane"))
                }
            }
        };
        match outcome {
            Ok(()) => closed.push(pane),
            Err(e) => failures.push(format!("%{pane}: {e:#}")),
        }
    }

    if !failures.is_empty() {
        // Structured even here, for the reason `wait` is: the caller was
        // cleaning up, and what they need next is which panes are still theirs
        // to deal with — an anyhow error would leave `--json` holding prose.
        // The complaint goes to stderr all the same, so `-q` still reports it
        // and the exit code is not the only thing that says so.
        eprintln!(
            "tty7: closed {} pane(s); {} could not be closed — {}",
            closed.len(),
            failures.len(),
            failures.join("; ")
        );
        return Ok(Outcome::Exit(
            1,
            Report {
                human: String::new(),
                json: json!({ "closed": closed, "failed": failures }),
            },
        ));
    }
    let human = match closed.as_slice() {
        // The single-pane case is the overwhelming one and has always been
        // silent on success; only a batch is worth narrating.
        [_] => String::new(),
        many => format!(
            "closed {} panes: {}\n",
            many.len(),
            many.iter()
                .map(|p| format!("%{p}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    report(human, json!({ "closed": closed }))
}

/// The panes the daemon is running that no workspace's tab tree references.
/// Every pane the server runs that no workspace tree holds.
///
/// This set is wider than "leftovers", and the difference matters because
/// `pane close --orphans` hangs up everything in it. A `run` that is executing
/// *right now* is in here too: `run --ws W` stamps the pane with `W` as its
/// owner and files it into no tab unless `--keep` is given, so for the length
/// of the command it is indistinguishable from the pane an interrupted `run`
/// left behind — same `orphan`, same `owner`, same everything `PaneInfo`
/// carries. Verified against a live server, and it is why the reaper cannot be
/// made safe from here.
///
/// The distinction that would work is whether a client is still attached: an
/// interrupted `run` has none, a running one does. The daemon knows, and
/// `PaneInfo` does not carry it. Adding the field is backward compatible —
/// every other field on that struct is already `#[serde(default)]`, so a peer
/// that predates it simply omits it — but it is a wire change, and until it
/// happens the honest thing is that the docs say plainly that `--orphans` will
/// kill a `run` started from another shell.
fn orphan_panes(machine: &Machine, backend: &mut dyn Backend) -> Result<Vec<u64>> {
    let held: Vec<u64> = machine
        .workspaces
        .iter()
        .flat_map(|ws| ws.tabs.iter())
        .flat_map(|tab| tab.root.pane_ids())
        .collect();
    Ok(backend
        .list_panes()?
        .iter()
        .map(|info| info.pane_id)
        .filter(|pane| !held.contains(pane))
        .collect())
}

fn events(json_mode: bool, backend: &mut dyn Backend) -> Result<Outcome> {
    backend.events(&mut |event| {
        if json_mode {
            crate::stdio::line(&serde_json::to_string(&event)?);
        } else {
            crate::stdio::line(&event_line(&event));
        }
        Ok(())
    })?;
    report("", Value::Null)
}

fn event_line(event: &ControlEvent) -> String {
    match event {
        ControlEvent::PaneExited { pane_id, code } => match code {
            Some(code) => format!("pane %{pane_id} exited with code {code}"),
            None => format!("pane %{pane_id} exited"),
        },
        ControlEvent::AgentStatus { pane_id, json } => {
            format!("pane %{pane_id} agent status: {json}")
        }
        ControlEvent::Preempted { workspace, by } => {
            format!("workspace {workspace} taken over by {by}")
        }
        ControlEvent::Layout { workspace, delta } => {
            format!("workspace {workspace} layout: {delta:?}")
        }
        ControlEvent::LayoutResync => "layout resync".to_string(),
        other => format!("{other:?}"),
    }
}

/// The one verb that *blocks*: poll until the watched pane reaches a requested
/// state, then report it. This is what turns the CLI into an orchestration tool
/// — "wake me when my peer agent needs input, or finishes its turn" — without
/// the screen-scraping a tmux-based agent team resorts to.
///
/// Two kinds of pane can be waited on, and they are watched differently. An
/// agent pane has a status the server keeps from hook events; a pane merely
/// running a command has none, and for it the question is whether the
/// foreground command has exited — `free`, read off the process tree. Keeping
/// both here rather than in two verbs means a caller that does not know which
/// kind it has can ask for `waiting,done,free,exit` and get an answer either
/// way.
///
/// A poll rather than an `events` subscription on purpose: a one-shot,
/// stateless question composes into scripts (`tty7 wait %3 && tty7 capture %3
/// --plain`), survives a server restart mid-wait, and needs no cursor
/// management. At the default 500ms interval an agent wait costs one aggregate
/// control request per tick — the same request `tty7 agents` makes once.
fn wait(args: WaitArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    use std::time::{Duration, Instant};
    use tty7_core::core::cli_agent::AgentStatus;

    /// How many polls may pass before liveness is re-checked. The agent
    /// snapshot carries no liveness of its own and the daemon keeps a dead
    /// pane in its registry until it is closed, so an agent that died
    /// mid-turn would otherwise report `working` until the timeout. Every
    /// few polls is cheap; the fast path — the first poll already answers —
    /// still costs exactly one request.
    const LIVENESS_EVERY: u32 = 4;

    /// Where the pane stood the moment we looked. `None` is "no agent state
    /// at all", which is itself a position: an agent appearing is a change.
    type Cursor = Option<(AgentStatus, u64)>;

    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    // `checked_add` rather than `+`: an absurd `--timeout` must not panic.
    let deadline = args
        .timeout
        .and_then(|t| Instant::now().checked_add(Duration::from_secs(t)));
    let interval = Duration::from_millis(args.interval);
    let watch_free = args.until.contains(&WaitState::Free);
    let mut baseline: Option<Cursor> = None;
    // Sticky: has the pane been seen running something since the wait began?
    // This is `--changed`'s edge for `free` — see the flag's own comment.
    let mut seen_busy = false;
    let mut polls: u32 = 0;
    loop {
        let states = match backend.control(ControlRequest::AgentStates)? {
            ReplyOk::AgentStates(states) => states,
            other => bail!("the server answered AgentStates with {other:?}"),
        };
        let entry = states.into_iter().find(|s| s.pane_id == pane);
        let mut current = match &entry {
            Some(e) => match e.state.status {
                AgentStatus::Idle => WaitState::Idle,
                AgentStatus::Working => WaitState::Working,
                AgentStatus::Waiting => WaitState::Waiting,
                AgentStatus::Done => WaitState::Done,
            },
            // No agent state for the pane: a live one is agentless — a plain
            // shell, or an agent whose hooks never got installed — and a dead
            // or vanished one has exited. Reporting `idle` here (as this once
            // did) made `--until idle` answer "finished" about a pane that was
            // midway through a build. The machine tree is only fetched on this
            // branch — while an agent is reporting, its state alone answers.
            None if pane_is_live(backend, pane)? => WaitState::NoAgent,
            None => WaitState::Exit,
        };

        // Status is a level, not an edge (see `--changed` in cli.rs): the
        // position we arrived at is last turn's answer until the agent moves.
        let cursor: Cursor = entry.as_ref().map(|e| (e.state.status, e.state.activity));
        let baseline = *baseline.get_or_insert(cursor);
        let mut changed = cursor != baseline;

        // `free` is a fact about the process tree, not the agent ladder, so it
        // is asked separately, only when requested, and only once the ladder
        // has failed to answer. A state the caller listed is their answer:
        // overwriting a real `waiting` with a process-tree fact would strand a
        // pane whose depth-0 process *is* the agent (see `pane_is_free`), where
        // the tree reads free for the whole turn.
        if watch_free && current != WaitState::Exit && !args.until.contains(&current) {
            if pane_is_free(backend, pane)? {
                current = WaitState::Free;
                changed = seen_busy;
            } else {
                seen_busy = true;
            }
        }

        let mut matched = args.until.contains(&current) && (changed || !args.changed);
        polls += 1;
        // A reporting agent can outlive its pane; re-check on a throttle so a
        // crashed worker ends the wait instead of spinning on a stale status.
        if !matched
            && entry.is_some()
            && polls.is_multiple_of(LIVENESS_EVERY)
            && !pane_is_live(backend, pane)?
        {
            current = WaitState::Exit;
            matched = args.until.contains(&current);
        }

        // Exit ends every wait, requested or not: whatever the caller was
        // waiting for can no longer happen, and reporting beats spinning
        // forever on a ghost. `--changed` does not veto it either — a pane
        // that was already dead is not going to move.
        if matched || current == WaitState::Exit {
            let session = entry.as_ref().map(|e| &e.state);
            let json = json!({
                "pane": pane,
                "status": current.name(),
                "matched": matched,
                // False means the pane moved into this state while we watched;
                // true means it was already there when the wait began, i.e.
                // the answer may belong to a previous turn.
                "stale": !changed,
                "activity": session.map(|s| s.activity),
                "message": session.and_then(|s| s.message.clone()),
                "session_id": session.and_then(|s| s.session_id.clone()),
            });
            if !matched {
                // Structured even here: a script has to tell "my peer died"
                // apart from "the daemon is unreachable", and an anyhow error
                // would leave --json with nothing to read.
                //
                // The headline goes to stderr all the same, so `-q` still
                // reports it — the discipline `pane close` set: a failure is
                // not "output on success", and an exit code alone says which
                // wait died nowhere (#590).
                eprintln!("tty7: pane %{pane} exited before reaching the awaited state");
                return Ok(Outcome::Exit(
                    1,
                    Report {
                        human: format!("pane %{pane} exited before reaching the awaited state"),
                        json,
                    },
                ));
            }
            let mut human = format!("pane %{pane}: {}", current.name());
            if let Some(msg) = session.and_then(|s| s.message.as_deref()) {
                human.push_str(&format!(" — {msg}"));
            }
            if !changed {
                human.push_str(match current {
                    WaitState::Free => " (already free — nothing ran while we watched)",
                    _ => " (unchanged since the wait began)",
                });
            }
            return report(human, json);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // 124 = the `timeout(1)` convention: "gave up", distinct from
            // both success and error, so orchestration scripts can branch.
            let mut human = format!("pane %{pane}: still {} — timed out", current.name());
            // A wait for agent states that never move is the shape of both
            // "there is no agent here" and "the agent's hooks are missing",
            // and neither is visible from a timeout alone. Say which door to
            // try rather than leaving the caller to poll harder.
            if current == WaitState::NoAgent {
                human.push_str(
                    "\nnothing is reporting agent status in this pane — for a plain command \
                     wait `--until free`, and for an agent check `tty7 agents` for a missing \
                     status hook",
                );
            }
            // `--changed` needs to have *seen* the pane busy, and a command
            // that starts and finishes inside one interval never is. That
            // looks exactly like "the command never ran", so say both, rather
            // than let a finished command read as a timeout.
            if current == WaitState::Free && !seen_busy {
                human.push_str(
                    "\nnothing was ever seen running here — either the command never started, \
                     or it finished inside one --interval. Poll faster (--interval 100) or drop \
                     --changed",
                );
            }
            // The headline goes to stderr all the same, so `-q` still
            // reports it — see the sibling exit above for why (#590).
            eprintln!("tty7: pane %{pane}: still {} — timed out", current.name());
            return Ok(Outcome::Exit(
                124,
                Report {
                    human,
                    // The same shape as a finished wait, plus the flag that
                    // says the deadline ended it: a consumer written against
                    // the success path must not find its fields missing on
                    // exactly the branch it wrote error handling for (#589).
                    json: {
                        let session = entry.as_ref().map(|e| &e.state);
                        json!({
                            "pane": pane,
                            "status": current.name(),
                            "matched": false,
                            "stale": !changed,
                            "timed_out": true,
                            "activity": session.map(|s| s.activity),
                            "message": session.and_then(|s| s.message.clone()),
                            "session_id": session.and_then(|s| s.session_id.clone()),
                        })
                    },
                },
            ));
        }
        // Never sleep past the deadline: a long `--interval` must not turn a
        // short `--timeout` into a long one.
        let nap = match deadline {
            Some(d) => interval.min(d.saturating_duration_since(Instant::now())),
            None => interval,
        };
        std::thread::sleep(nap);
    }
}

/// Whether the pane is back to its bare shell — nothing running in front of it.
///
/// Depth, not count: the pane's own shell sits at depth 0 and everything it
/// launched hangs below, so "nothing deeper than the shell" holds however many
/// shells the pane ended up with, and does not have to guess at process names.
/// It is also the portable question — Windows has no foreground process group
/// to ask about, so `ProcEntry::foreground` is never true there.
///
/// What it cannot see, both by construction: a pane whose depth-0 process *is*
/// the command — which is what `tty7 run` spawns — reads free for as long as it
/// runs, and a backgrounded job keeps a pane busy after the foreground command
/// is long gone. An empty tree is "we could not see in" rather than "free":
/// answering free there would be the same false success `no-agent` exists to
/// remove.
fn pane_is_free(backend: &mut dyn Backend, pane: u64) -> Result<bool> {
    let procs = backend.procs(pane)?.procs;
    Ok(!procs.is_empty() && procs.iter().all(|p| p.depth == 0))
}

/// Whether the daemon still has a live pane behind this id. Absent from the
/// tree counts as dead: a closed pane is as gone as an exited one.
fn pane_is_live(backend: &mut dyn Backend, pane: u64) -> Result<bool> {
    Ok(fetch_machine(backend)?
        .panes
        .iter()
        .any(|p| p.id == pane && p.live))
}

fn agents(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::AgentStates)? {
        ReplyOk::AgentStates(states) => {
            // AgentStates only contains panes that have already emitted a hook
            // event. The machine snapshot independently records the daemon's
            // foreground-process detection, including a supported agent that
            // has not been able to report yet.
            let diagnostics = fetch_machine(backend)
                .map(|machine| agent_hook_diagnostics(&states, &machine, backend))
                .unwrap_or_default();
            let mut json = json!({ "agents": serde_json::to_value(&states)? });
            if !diagnostics.is_empty() {
                json["diagnostics"] =
                    Value::Array(diagnostics.iter().map(AgentHookDiagnostic::json).collect());
            }
            report(agents_human(&states, &diagnostics), json)
        }
        other => bail!("the server answered AgentStates with {other:?}"),
    }
}

/// The two hook states worth reporting. Building one of these is the only way
/// to reach a diagnostic, so "installed hooks are not a diagnostic" is a shape
/// the type cannot hold rather than a branch that has to stay unreachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookGap {
    Missing,
    Outdated,
}

impl HookGap {
    fn of(state: HooksState) -> Option<HookGap> {
        match state {
            HooksState::NotInstalled => Some(HookGap::Missing),
            HooksState::Outdated => Some(HookGap::Outdated),
            HooksState::Installed => None,
        }
    }

    /// The verb of the Settings button that closes the gap.
    fn action(self) -> &'static str {
        match self {
            HookGap::Missing => "install",
            HookGap::Outdated => "update",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            HookGap::Missing => "not_installed",
            HookGap::Outdated => "outdated",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            HookGap::Missing => "not installed",
            HookGap::Outdated => "outdated",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AgentHookDiagnostic {
    agent: HookAgent,
    gap: HookGap,
}

impl AgentHookDiagnostic {
    fn json(&self) -> Value {
        json!({
            "kind": "agent_status_hooks_unavailable",
            "agent": self.agent.slug(),
            "hooks_state": self.gap.slug(),
            "action": self.gap.action(),
        })
    }
}

fn agent_hook_diagnostics(
    states: &[tty7_core::daemon::control::PaneAgentState],
    machine: &Machine,
    backend: &mut dyn Backend,
) -> Vec<AgentHookDiagnostic> {
    HookAgent::ALL
        .into_iter()
        .filter(|hook_agent| {
            machine.panes.iter().any(|pane| {
                pane.live
                    && !states.iter().any(|state| state.pane_id == pane.id)
                    && pane.agent.as_ref().is_some_and(|facts| {
                        HookAgent::of_detected(facts.agent) == Some(*hook_agent)
                    })
            })
        })
        .filter_map(|agent| {
            let gap = HookGap::of(backend.agent_hooks_state(agent)?)?;
            Some(AgentHookDiagnostic { agent, gap })
        })
        .collect()
}

fn agents_human(
    states: &[tty7_core::daemon::control::PaneAgentState],
    diagnostics: &[AgentHookDiagnostic],
) -> String {
    if diagnostics.is_empty() {
        return output::agents_table(states);
    }
    let mut human = if states.is_empty() {
        "no agents reporting status\n".to_string()
    } else {
        output::agents_table(states)
    };
    for diagnostic in diagnostics {
        let state = diagnostic.gap.describe();
        human.push_str(&format!(
            "{} is running, but its tty7 agent-status hooks are {state}. Open Settings → Agents to {} the hooks, then start a new {} session.\n",
            diagnostic.agent.display_name(),
            diagnostic.gap.action(),
            diagnostic.agent.display_name(),
        ));
    }
    human
}

fn status(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::Status)? {
        ReplyOk::Status(status) => report(
            output::status_lines(&status),
            serde_json::to_value(&status)?,
        ),
        other => bail!("the server answered Status with {other:?}"),
    }
}

fn machine_ls(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::Routes)? {
        ReplyOk::Routes(routes) => {
            // The local machine is not a route — a route is a link to some
            // *other* machine — but `machine ls` is "the local machine plus
            // every link", so it belongs in the answer. Assembled once here
            // and handed to both renderings: while the table synthesized this
            // row on its own, `--json` served the routes alone, and a machine
            // with no remotes answered the machine-readable half with an empty
            // list. An agent enumerating machines concluded there were none,
            // including the one it was running on.
            let machines: Vec<RouteInfo> = std::iter::once(RouteInfo {
                key: "local".to_string(),
                kind: "local".to_string(),
                connected: true,
            })
            .chain(routes)
            .collect();
            report(
                output::routes_table(&machines),
                json!({ "machines": serde_json::to_value(&machines)? }),
            )
        }
        other => bail!("the server answered Routes with {other:?}"),
    }
}

fn doctor(ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let mark = |v: &Option<String>| match v {
        Some(value) => format!("set ({value})"),
        None => "missing".to_string(),
    };
    let mut rows = vec![
        vec![address::ENV_CONFIG_DIR.to_string(), mark(&ctx.config_dir)],
        vec![address::ENV_WS.to_string(), mark(&ctx.ws)],
        vec![address::ENV_PANE.to_string(), mark(&ctx.pane)],
    ];
    let mut server = json!({ "reachable": false });
    let mut hooks: Vec<(HookAgent, HooksState)> = Vec::new();
    match backend.hello() {
        Ok(hello) => {
            let dialect_ok = hello.control_version == CONTROL_VERSION
                && hello.protocol_version == PROTOCOL_VERSION;
            let dialect = if dialect_ok {
                format!("ok (control v{CONTROL_VERSION}, protocol v{PROTOCOL_VERSION})")
            } else {
                format!(
                    "MISMATCH (server speaks control v{} protocol v{}, this build \
                     v{CONTROL_VERSION}/v{PROTOCOL_VERSION})",
                    hello.control_version, hello.protocol_version
                )
            };
            rows.push(vec![
                "server".to_string(),
                format!("ok (build {})", hello.build),
            ]);
            rows.push(vec!["dialect".to_string(), dialect]);
            let status = match backend.control(ControlRequest::Status)? {
                ReplyOk::Status(status) => status,
                other => bail!("the server answered Status with {other:?}"),
            };
            rows.push(vec![
                "status".to_string(),
                format!(
                    "pid {}, up {}s, {} pane{}",
                    status.pid,
                    status.uptime_secs,
                    status.panes,
                    if status.panes == 1 { "" } else { "s" }
                ),
            ]);
            let routes = match backend.control(ControlRequest::Routes)? {
                ReplyOk::Routes(routes) => routes,
                other => bail!("the server answered Routes with {other:?}"),
            };
            let connected = routes.iter().filter(|r| r.connected).count();
            rows.push(vec![
                "machine links".to_string(),
                format!("{} known, {connected} connected", routes.len()),
            ]);
            // Without hooks an agent reports no status, which means `tty7
            // agents` shows it standing still and `tty7 wait` never wakes. That
            // failure looks like a hang rather than a missing install, so the
            // check that explains it belongs in the verb people run when
            // something is not working.
            hooks = hook_survey(backend);
            rows.push(vec!["agent hooks".to_string(), hooks_summary(&hooks)]);
            server = json!({
                "reachable": true,
                "dialect_ok": dialect_ok,
                "build": hello.build,
                "status": serde_json::to_value(&status)?,
                "routes": serde_json::to_value(&routes)?,
            });
        }
        Err(e) => {
            rows.push(vec!["server".to_string(), format!("unreachable — {e:#}")]);
        }
    }
    let mut human = output::table(&["CHECK", "RESULT"], &rows);
    if ctx.config_dir.is_none() && ctx.pane.is_none() {
        human.push_str(
            "\nnot inside a tty7 shell — address commands need an explicit %pane/@tab/workspace\n",
        );
    }
    let report = Report {
        human,
        json: json!({
            "context": {
                "config_dir": ctx.config_dir.is_some(),
                "workspace": ctx.ws.is_some(),
                "pane": ctx.pane.is_some(),
            },
            "server": server,
            "hooks": hooks_json(&hooks),
        }),
    };
    if report.json["server"]["reachable"] == false {
        // doctor is the verb people run when something is not working, so an
        // unreachable server is *the* finding — not a row to exit 0 over:
        // `tty7 doctor || alert` has to fire (#592). The table and JSON go
        // out all the same, and stderr carries the headline under `-q`.
        eprintln!("tty7: doctor: the server is unreachable");
        return Ok(Outcome::Exit(1, report));
    }
    Ok(Outcome::Report(report))
}

/// Where every installable status hook stands on this machine.
///
/// Agents whose state cannot be read at all are left out rather than guessed
/// at: the backend answers `None` both for a `-m` run (hooks are a local
/// install, and this is a local check) and when the app itself cannot be found,
/// and neither is the same as "not installed".
fn hook_survey(backend: &mut dyn Backend) -> Vec<(HookAgent, HooksState)> {
    HookAgent::ALL
        .into_iter()
        .filter_map(|agent| Some((agent, backend.agent_hooks_state(agent)?)))
        .collect()
}

fn hooks_summary(hooks: &[(HookAgent, HooksState)]) -> String {
    if hooks.is_empty() {
        return "unknown — hooks are a local install, and this check could not read them".into();
    }
    let named = |want: HooksState| -> Vec<&'static str> {
        hooks
            .iter()
            .filter(|(_, state)| *state == want)
            .map(|(agent, _)| agent.display_name())
            .collect()
    };
    let installed = named(HooksState::Installed);
    let outdated = named(HooksState::Outdated);

    // The current ones are named because that is the answer to "can I delegate
    // to this agent"; the missing ones are a count, since listing every agent
    // tty7 knows about would bury it. "Up to date" rather than "installed":
    // an outdated hook *is* installed, and saying "none installed" next to six
    // outdated ones reads as a contradiction.
    let mut summary = if installed.is_empty() {
        "none up to date".to_string()
    } else {
        format!("{} up to date", installed.join(", "))
    };
    if !outdated.is_empty() {
        summary.push_str(&format!("; {} OUTDATED", outdated.join(", ")));
    }
    let missing = hooks.len() - installed.len() - outdated.len();
    if missing > 0 {
        summary.push_str(&format!("; {missing} not installed"));
    }
    if installed.is_empty() || !outdated.is_empty() {
        summary.push_str(" (Settings → Agents)");
    }
    summary
}

fn hooks_json(hooks: &[(HookAgent, HooksState)]) -> Value {
    let slugs = |want: HooksState| -> Vec<&'static str> {
        hooks
            .iter()
            .filter(|(_, state)| *state == want)
            .map(|(agent, _)| agent.slug())
            .collect()
    };
    json!({
        "installed": slugs(HooksState::Installed),
        "outdated": slugs(HooksState::Outdated),
        "not_installed": slugs(HooksState::NotInstalled),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::testbed::two_workspace_machine;
    use clap::Parser;
    use tty7_core::core::cli_agent::CLIAgent;
    use tty7_core::core::machine::Tab;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("test invocations use the documented grammar")
    }

    fn mock() -> MockBackend {
        MockBackend::with_machine(two_workspace_machine())
    }

    /// A replayed snapshot at a 20-column pane, which is narrow enough that the
    /// wrapping tests can wrap without pages of fixture.
    fn segment(bytes: &[u8]) -> crate::backend::CaptureSegment {
        crate::backend::CaptureSegment {
            size: tty7_core::daemon::protocol::WinSize {
                cols: 20,
                rows: 10,
                cell_w: 8,
                cell_h: 16,
            },
            bytes: bytes.to_vec(),
        }
    }

    fn run_cli(args: &[&str], ctx: &Context, backend: &mut MockBackend) -> Outcome {
        execute(cli(args), ctx, backend).expect("this command should succeed against the mock")
    }

    fn human(outcome: Outcome) -> String {
        match outcome {
            Outcome::Report(r) => r.human,
            Outcome::Exit(code, _) => panic!("expected a report, got exit {code}"),
        }
    }

    fn json_of(outcome: Outcome) -> Value {
        match outcome {
            Outcome::Report(r) => r.json,
            Outcome::Exit(code, _) => panic!("expected a report, got exit {code}"),
        }
    }

    fn pane_info(pane_id: u64, owner: Option<&str>) -> tty7_core::daemon::protocol::PaneInfo {
        tty7_core::daemon::protocol::PaneInfo {
            pane_id,
            cwd: None,
            title: "sh".into(),
            osc_title: None,
            alive: true,
            owner: owner.map(str::to_string),
        }
    }

    /// One condition, one sentence.
    ///
    /// `capture %999` used to answer with the wire request's name — "daemon
    /// refused Observe: no such pane 999" — `send %999` with the daemon's bare
    /// refusal, and `procs %999` with the line below. Three readings of one
    /// state of the world, one of them naming a request no user has heard of,
    /// on the surface agents read stderr from.
    #[test]
    fn a_pane_that_is_not_there_gets_the_same_answer_whatever_failed() {
        let mut backend = mock();
        backend.registry = vec![pane_info(1, None)];

        // Not in the registry: whatever the daemon said, the useful sentence is
        // that the pane does not exist.
        let swapped = or_no_such_pane::<()>(
            Err(anyhow::anyhow!("daemon refused Observe: no such pane 999")),
            999,
            &mut backend,
        )
        .expect_err("still a failure");
        assert_eq!(
            swapped.to_string(),
            "no pane %999 on this machine — `tty7 pane ls --all` lists them"
        );

        // Held by a workspace tree but not running: "no pane %3 on this
        // machine" would be false, and it would point at `pane ls --all`,
        // which lists what the server runs and so can never show this one.
        let parked = or_no_such_pane::<()>(
            Err(anyhow::anyhow!("daemon refused Observe")),
            3,
            &mut backend,
        )
        .expect_err("still a failure");
        let said = parked.to_string();
        assert!(
            said.starts_with("pane %3 is not running"),
            "a pane the tree still holds is not absent: {said}"
        );
        assert!(
            said.contains("api") && said.contains("tab ls"),
            "it has to name the holder and a command that can actually show it: {said}"
        );
        assert!(
            !said.contains("pane ls --all"),
            "that is the one command guaranteed not to list it: {said}"
        );

        // The pane is there, so the failure was about something else and its
        // own words are the better ones — this must not swallow real errors.
        let kept = or_no_such_pane::<()>(Err(anyhow::anyhow!("connection reset")), 1, &mut backend)
            .expect_err("still a failure");
        assert_eq!(kept.to_string(), "connection reset");

        assert!(
            or_no_such_pane(Ok(7), 999, &mut backend).is_ok(),
            "a success is never reconsidered, so the happy path costs no extra request"
        );
    }

    /// An empty answer from the daemon means one of two things and the caller
    /// has to be told which. `registry.get` misses for a pane that does not
    /// exist and the reply is an empty `PaneProcs` — the same reply an idle
    /// pane gives — so without this both printed `nothing running in this
    /// pane` and exited 0.
    #[test]
    fn procs_tells_an_idle_pane_apart_from_one_that_does_not_exist() {
        let mut backend = mock();
        backend.registry = vec![pane_info(1, None)];
        let out = run_cli(&["tty7", "procs", "%1"], &Context::default(), &mut backend);
        assert!(
            human(out).contains("nothing running"),
            "a pane that exists and is idle still reports as idle"
        );

        let mut backend = mock();
        backend.registry = vec![pane_info(1, None)];
        let error = execute(
            cli(&["tty7", "procs", "%999"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a pane the server does not have must not read as an idle one");
        assert!(error.to_string().contains("no pane %999"), "{error:#}");
    }

    /// A pane no workspace holds is still a pane, and its processes are the
    /// whole reason `pane ls --all` surfaces it — so `procs` must answer for
    /// one rather than deciding it does not exist.
    #[test]
    fn procs_answers_for_an_orphaned_pane() {
        let mut backend = mock();
        backend.registry = vec![pane_info(77, Some("tty7-cli"))];
        let out = run_cli(&["tty7", "procs", "%77"], &Context::default(), &mut backend);
        assert!(human(out).contains("nothing running"), "an orphan answers");
    }

    #[test]
    fn pane_ls_all_surfaces_the_panes_no_workspace_holds() {
        let mut backend = mock();
        // %1 and %3 are in the tree (see two_workspace_machine); %77 is what an
        // interrupted `tty7 run` leaves behind.
        backend.registry = vec![
            pane_info(1, None),
            pane_info(3, None),
            pane_info(77, Some("tty7-cli")),
        ];

        let out = run_cli(
            &["tty7", "pane", "ls", "--all"],
            &Context::default(),
            &mut backend,
        );
        let json = json_of(out);
        assert_eq!(json["orphans"], serde_json::json!(1), "{json}");

        let orphan = json["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane"] == serde_json::json!(77))
            .expect("the orphan is listed");
        assert_eq!(orphan["orphan"], serde_json::json!(true));
        assert!(
            orphan["workspace"].is_null(),
            "no workspace holds it: {orphan}"
        );
        assert_eq!(orphan["owner"], serde_json::json!("tty7-cli"), "{orphan}");

        let filed = json["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane"] == serde_json::json!(1))
            .expect("the filed pane is listed too");
        assert_eq!(filed["orphan"], serde_json::json!(false));
        assert!(!filed["workspace"].is_null(), "{filed}");
    }

    #[test]
    fn pane_ls_without_all_cannot_see_an_orphan() {
        let mut backend = mock();
        backend.registry = vec![pane_info(77, Some("tty7-cli"))];
        let json = json_of(run_cli(
            &["tty7", "pane", "ls"],
            &Context::default(),
            &mut backend,
        ));
        let listed: Vec<&Value> = json["panes"].as_array().unwrap().iter().collect();
        assert!(
            listed.iter().all(|p| p["pane"] != serde_json::json!(77)),
            "the tree-walking listing cannot reach the registry — that is why --all exists"
        );
    }

    #[test]
    fn closing_an_orphan_falls_back_to_hanging_the_pane_up() {
        let mut backend = mock();
        // The registry must know %77: a direct kill is fire-and-forget, so
        // close verifies existence against it first (#588).
        backend.registry = vec![pane_info(77, Some("tty7-cli"))];
        run_cli(
            &["tty7", "pane", "close", "%77"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.killed,
            vec![77],
            "a pane no workspace holds must still be stoppable"
        );
        assert_eq!(
            backend.control_calls,
            vec![ControlRequest::MachineGet],
            "PaneClose needs a workspace, so it must not be attempted for an orphan"
        );

        // A pane the tree does hold still goes through PaneClose.
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Panes(vec![1]));
        run_cli(
            &["tty7", "pane", "close", "%1"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.killed, vec![1], "the removed pane is hung up");
        assert!(
            backend
                .control_calls
                .iter()
                .any(|c| matches!(c, ControlRequest::PaneClose { pane: 1, .. })),
            "{:?}",
            backend.control_calls
        );
    }

    #[test]
    fn closing_a_pane_that_never_existed_is_a_failure_not_a_ghost_success() {
        // %99 is in no workspace and in no registry — exactly the typo a
        // reaper script makes. Closing it used to print {"closed":[99]} and
        // exit 0, telling the script the leak it was chasing was gone (#588).
        let mut backend = mock();
        backend.registry = vec![pane_info(77, Some("tty7-cli"))];
        let out = execute(
            cli(&["tty7", "pane", "close", "%99"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a ghost close is an exit code, not an error");
        let Outcome::Exit(1, r) = out else {
            panic!("closing a pane that does not exist has to fail: {out:?}");
        };
        assert_eq!(r.json["closed"], serde_json::json!([]));
        assert!(
            r.json["failed"]
                .as_array()
                .expect("the failures are a list")
                .iter()
                .any(|f| f.as_str().is_some_and(|f| f.contains("%99"))),
            "{}",
            r.json
        );
        assert!(
            backend.killed.is_empty(),
            "no kill may be sent for a pane the registry does not hold"
        );
    }

    #[test]
    fn ls_and_ws_ls_are_the_same_request() {
        let ctx = Context::default();
        let mut a = mock();
        run_cli(&["tty7", "ls"], &ctx, &mut a);
        let mut b = mock();
        run_cli(&["tty7", "ws", "ls"], &ctx, &mut b);
        assert_eq!(a.control_calls, vec![ControlRequest::MachineGet]);
        assert_eq!(a.control_calls, b.control_calls, "the alias must not drift");
    }

    #[test]
    fn ws_tree_asks_for_the_resolved_workspace() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].clone();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(api.clone())));
        run_cli(
            &["tty7", "ws", "tree", "api"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::WorkspaceTree { workspace: api.id },
            ]
        );
    }

    #[test]
    fn ws_new_carries_the_name() {
        let mut backend = mock();
        let created = Workspace::default();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(created.clone())));
        let out = run_cli(
            &["tty7", "ws", "new", "dev"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.control_calls,
            vec![ControlRequest::WorkspaceCreate {
                name: Some("dev".into()),
                workspace: None,
            }]
        );
        assert_eq!(
            human(out),
            created.id.to_string(),
            "the id is the printed result"
        );
    }

    #[test]
    fn new_spawns_first_and_seeds_the_tab_with_the_daemons_pane_id() {
        let mut backend = mock();
        let created = Workspace::default();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(created.clone())));
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(6))));
        let out = run_cli(
            &["tty7", "new", "C:\\newproj"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::WorkspaceCreate {
                    name: None,
                    workspace: None,
                },
                ControlRequest::TabCreate {
                    workspace: created.id,
                    at: None,
                    pane: PaneSeed {
                        pane: 6,
                        cwd: Some("C:\\newproj".into()),
                        ssh_spec: None,
                        agent: None,
                        shell: None,
                    },
                    tab: None,
                },
            ],
            "the daemon-assigned pane id (6) lands in the tree op, so the spawn came first"
        );
        assert_eq!(
            backend.spawned,
            vec![(created.id, Some("C:\\newproj".to_string()))],
            "the tree op alone leaves a dead pane — the shell must be spawned"
        );
        assert_eq!(human(out), created.id.to_string());
    }

    #[test]
    fn ws_rename_rm_attach_detach_build_their_requests() {
        let ctx = Context::default();
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let web = backend.machine.workspaces[1].id;

        run_cli(&["tty7", "ws", "rename", "api", "core"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceRename {
                workspace: api,
                name: Some("core".into()),
            }
        );

        backend.control_calls.clear();
        backend.replies.push_back(ReplyOk::Panes(vec![3, 4]));
        run_cli(&["tty7", "ws", "rm", "web"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceRemove { workspace: web }
        );
        assert_eq!(
            backend.killed,
            vec![3, 4],
            "removing a workspace must hang up the panes it held"
        );

        backend.control_calls.clear();
        backend.replies.push_back(ReplyOk::Attached {
            took_over_from: Some("laptop".into()),
        });
        let out = run_cli(&["tty7", "ws", "attach", "api"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceAttach {
                id: api.to_string(),
            }
        );
        assert_eq!(human(out), "took over from laptop");

        backend.control_calls.clear();
        run_cli(&["tty7", "ws", "detach", "api"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceDetach {
                id: api.to_string(),
            }
        );
    }

    #[test]
    fn tab_verbs_resolve_machine_wide_ordinals_to_real_ids() {
        let ctx = Context::default();
        let mut backend = mock();
        let web = backend.machine.workspaces[1].clone();

        backend.replies.push_back(ReplyOk::Panes(Vec::new()));
        run_cli(&["tty7", "tab", "close", "@3"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::TabClose {
                    workspace: web.id,
                    tab: web.tabs[0].id,
                },
            ]
        );

        backend.control_calls.clear();
        let api = backend.machine.workspaces[0].clone();
        run_cli(
            &["tty7", "tab", "rename", "@1", "build2"],
            &ctx,
            &mut backend,
        );
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabRename {
                workspace: api.id,
                tab: api.tabs[0].id,
                name: Some("build2".into()),
            }
        );

        backend.control_calls.clear();
        run_cli(&["tty7", "tab", "move", "@2", "0"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabMove {
                workspace: api.id,
                tab: api.tabs[1].id,
                to: 0,
            }
        );
    }

    #[test]
    fn tab_ls_names_an_unnamed_tab_and_shows_the_leaf_of_its_group() {
        let mut backend = mock();
        backend.machine.workspaces[0].tabs[1].sidebar_group = Some("C:\\proj\\sub".into());

        let out = run_cli(
            &["tty7", "tab", "ls", "api"],
            &Context::default(),
            &mut backend,
        );

        // @1 was named; @2 was not, so it borrows the leaf of its cwd. The
        // GROUP column is the heading's last segment, not the whole path.
        assert_eq!(
            human(out),
            "TAB  NAME   GROUP  PANES\n@1   build  -      1\n@2   proj   sub    2\n"
        );
    }

    #[test]
    fn tab_ls_json_keeps_the_literal_name_beside_the_label() {
        let mut backend = mock();
        backend.machine.workspaces[0].tabs[1].sidebar_group = Some("C:\\proj\\sub".into());

        let out = run_cli(
            &["tty7", "tab", "ls", "api"],
            &Context::default(),
            &mut backend,
        );

        let Outcome::Report(report) = out else {
            panic!("tab ls must report");
        };
        let tabs = report.json["tabs"].as_array().expect("tabs").clone();
        assert_eq!(tabs[1]["name"], Value::Null, "nobody named this tab");
        assert_eq!(tabs[1]["label"], "proj", "the table's stand-in travels too");
        assert_eq!(
            tabs[1]["group"], "C:\\proj\\sub",
            "the JSON keeps the whole heading the table abbreviates"
        );
    }

    #[test]
    fn tab_close_hangs_up_every_pane_the_server_removed() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Panes(vec![2, 3]));

        run_cli(
            &["tty7", "tab", "close", "@2"],
            &Context::default(),
            &mut backend,
        );

        assert_eq!(
            backend.killed,
            vec![2, 3],
            "every pane removed with the tab must be hung up"
        );
    }

    #[test]
    fn tab_close_attempts_every_hangup_before_reporting_failures() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Panes(vec![2, 3]));
        backend.kill_failures.push(2);

        let error = execute(
            cli(&["tty7", "tab", "close", "@2"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a failed pane hangup must fail tab close");

        assert_eq!(
            backend.killed,
            vec![2, 3],
            "a failed hangup must not skip the remaining panes"
        );
        assert!(error.to_string().contains("%2"), "{error:#}");
    }

    #[test]
    fn tab_close_rejects_an_unexpected_server_reply() {
        let mut backend = mock();
        let error = execute(
            cli(&["tty7", "tab", "close", "@2"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("TabClose must return the panes it removed");

        assert!(
            error
                .to_string()
                .contains("the server answered TabClose with Unit"),
            "{error:#}"
        );
    }

    #[test]
    fn tab_new_uses_the_workspace_from_the_environment() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].clone();
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(6))));
        let ctx = Context {
            ws: Some(api.id.to_string()),
            ..Context::default()
        };
        run_cli(
            &["tty7", "tab", "new", "--cwd", "C:\\elsewhere"],
            &ctx,
            &mut backend,
        );
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabCreate {
                workspace: api.id,
                at: None,
                pane: PaneSeed {
                    pane: 6,
                    cwd: Some("C:\\elsewhere".into()),
                    ssh_spec: None,
                    agent: None,
                    shell: None,
                },
                tab: None,
            }
        );
        assert_eq!(
            backend.spawned,
            vec![(api.id, Some("C:\\elsewhere".to_string()))]
        );
    }

    #[test]
    fn pane_split_builds_the_split_and_spawns_the_new_shell() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let out = run_cli(
            &["tty7", "pane", "split", "%2", "--v", "--ratio", "0.3"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::PaneSplit {
                    workspace: api,
                    pane: 2,
                    axis: Axis::Vertical,
                    ratio: 0.3,
                    new: PaneSeed {
                        pane: 6,
                        cwd: Some("C:\\proj".into()),
                        ssh_spec: None,
                        agent: None,
                        shell: None,
                    },
                    first: false,
                },
            ],
            "the new pane inherits the split pane's cwd"
        );
        assert_eq!(backend.spawned, vec![(api, Some("C:\\proj".to_string()))]);
        assert_eq!(
            human(out),
            "%6",
            "the new pane address is the printed result"
        );
    }

    #[test]
    fn split_without_an_address_uses_the_pane_from_the_environment() {
        let mut backend = mock();
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };
        run_cli(&["tty7", "split", "--h"], &ctx, &mut backend);
        let web = backend.machine.workspaces[1].id;
        match &backend.control_calls[1] {
            ControlRequest::PaneSplit {
                workspace,
                pane,
                axis,
                ..
            } => {
                assert_eq!(*workspace, web);
                assert_eq!(*pane, 5);
                assert_eq!(*axis, Axis::Horizontal);
            }
            other => panic!("expected PaneSplit, got {other:?}"),
        }
    }

    #[test]
    fn pane_close_traces_the_pane_to_its_workspace() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Panes(vec![5]));
        run_cli(
            &["tty7", "pane", "close", "%5"],
            &Context::default(),
            &mut backend,
        );
        let web = backend.machine.workspaces[1].id;
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::PaneClose {
                    workspace: web,
                    pane: 5,
                },
            ]
        );
        assert_eq!(
            backend.killed,
            vec![5],
            "a pane removed from its workspace must also be hung up"
        );
    }

    /// The CLI is what creates orphans, so it should be able to clear them.
    /// `--orphans` closes exactly the panes the registry holds and the tab
    /// trees do not — panes that *are* held must survive it untouched.
    #[test]
    fn pane_close_orphans_reaps_only_what_no_workspace_holds() {
        let mut backend = mock();
        // %1 and %3 live in the tree (see two_workspace_machine); %77 and %78
        // are what interrupted `run`s left behind.
        backend.registry = vec![
            pane_info(1, None),
            pane_info(3, None),
            pane_info(77, Some("tty7-cli")),
            pane_info(78, Some("tty7-cli")),
        ];

        let json = json_of(run_cli(
            &["tty7", "pane", "close", "--orphans"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["closed"], serde_json::json!([77, 78]));
        assert_eq!(backend.killed, vec![77, 78]);
        assert!(
            !backend
                .control_calls
                .iter()
                .any(|c| matches!(c, ControlRequest::PaneClose { .. })),
            "orphans have no workspace to route a PaneClose through"
        );

        // Nothing to reap is a success with an empty list, not an error: a
        // cleanup step that fails when the machine is already clean is one a
        // script has to guard, and every script would then guard it the same way.
        let mut backend = mock();
        backend.registry = vec![pane_info(1, None), pane_info(3, None)];
        let json = json_of(run_cli(
            &["tty7", "pane", "close", "--orphans"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["closed"], serde_json::json!([]));
        assert!(backend.killed.is_empty());
    }

    /// A batch keeps going after a failure. Stopping at the first one would
    /// leave the rest of the leak exactly where it was — while still reporting
    /// the failure, because a half-done cleanup that claims success is worse.
    ///
    /// Reported as an exit code carrying a report, not as an error: the caller
    /// was cleaning up, and the useful answer is which panes are still theirs
    /// to deal with. An anyhow error would leave `--json` with prose.
    #[test]
    fn pane_close_reports_failures_without_abandoning_the_batch() {
        let mut backend = mock();
        backend.registry = vec![
            pane_info(77, None),
            pane_info(78, None),
            pane_info(79, None),
        ];
        backend.kill_failures = vec![78];

        let out = execute(
            cli(&["tty7", "pane", "close", "--orphans"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a partial cleanup is an exit code, not an error");
        let Outcome::Exit(1, r) = out else {
            panic!("a pane that could not be closed has to be reported");
        };
        assert_eq!(
            r.json["closed"],
            serde_json::json!([77, 79]),
            "the survivors of the batch are what a retry needs: {}",
            r.json
        );
        assert!(
            r.json["failed"]
                .as_array()
                .expect("the failures are a list")
                .iter()
                .any(|f| f.as_str().is_some_and(|f| f.contains("%78"))),
            "{}",
            r.json
        );
        assert_eq!(
            backend.killed,
            vec![77, 78, 79],
            "the panes after the failure still had to be attempted"
        );
    }

    #[test]
    fn send_reaches_the_pane_socket_seam_not_the_control_socket() {
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "%1", "make -j8", "--enter"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.sent,
            vec![(1, b"make -j8".to_vec()), (1, b"\r".to_vec())]
        );
        assert!(backend.control_calls.is_empty());

        let ctx = Context {
            pane: Some("%3".into()),
            ..Context::default()
        };
        backend.sent.clear();
        run_cli(&["tty7", "send", "echo hi"], &ctx, &mut backend);
        assert_eq!(backend.sent, vec![(3, b"echo hi".to_vec())]);
    }

    /// The keystrokes text cannot express. Each goes out as its own write, in
    /// the order given, because a pane reads them as separate key events —
    /// which is what walking a menu and then confirming it requires.
    #[test]
    fn send_key_presses_keys_in_order() {
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "%1", "--key", "down", "--key", "enter"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.sent,
            vec![(1, b"\x1b[B".to_vec()), (1, b"\r".to_vec())]
        );

        // Text and keys compose: type the answer, then press the key that
        // submits it in whatever the pane is showing.
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "%1", "y", "--key", "enter"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.sent, vec![(1, b"y".to_vec()), (1, b"\r".to_vec())]);

        // Interrupting takes no text at all — the case that made TEXT optional.
        let mut backend = mock();
        let json = json_of(run_cli(
            &["tty7", "send", "%1", "--key", "C-c"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(backend.sent, vec![(1, vec![0x03])]);
        assert_eq!(json["keys"], serde_json::json!(["c-c"]));
        assert_eq!(json["sent"], "", "nothing was typed");
    }

    /// A lone address still has to be the missing-text error it always was —
    /// otherwise `tty7 send %42` would silently do nothing at all.
    #[test]
    fn send_still_refuses_a_bare_address_when_there_is_nothing_to_press() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "%1"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a bare address sends nothing and must say so");
        assert!(err.to_string().contains("needs TEXT"), "{err}");
        assert!(backend.sent.is_empty());

        // And outside a tty7 shell, with neither text nor keys, the complaint
        // is about the missing input rather than the missing pane.
        let mut backend = mock();
        let err = execute(cli(&["tty7", "send"]), &Context::default(), &mut backend)
            .expect_err("send with no arguments has nothing to do");
        assert!(err.to_string().contains("--key"), "{err}");
    }

    /// A mistyped address must not degrade into text aimed at the caller's
    /// own pane: `send %3x --key C-c` used to type `%3x` where you sat and
    /// then interrupt whatever was in front of you. The guard only fires when
    /// the `%` is followed by a digit — "tried to write an address" — so vim's
    /// `%s/…` and `%!sort` keep working as text. The `Context::default()`
    /// every other send test uses can never reach this branch: without
    /// $TTY7_PANE the fallback errors OUTSIDE_SHELL before the guard matters
    /// (#538).
    #[test]
    fn a_broken_address_errors_instead_of_typing_into_the_callers_pane() {
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };

        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "%3x", "--key", "C-c"]),
            &ctx,
            &mut backend,
        )
        .expect_err("a broken address is an error, not text for your own pane");
        assert!(err.to_string().contains("pane address"), "{err}");
        assert!(
            backend.sent.is_empty(),
            "nothing reached any pane — not the text, not the key"
        );

        // Any digit after the `%` is the same reach for an address.
        let mut backend = mock();
        let err = execute(cli(&["tty7", "send", "%42a"]), &ctx, &mut backend)
            .expect_err("still an address-shaped error, not text");
        assert!(err.to_string().contains("pane address"), "{err}");
        assert!(backend.sent.is_empty());
    }

    /// A lone bare id now reads as an address, so `send 83` no longer types
    /// "83" into the caller's pane — it says it has nothing to send. That is
    /// the one behaviour this change takes away, and it has to fail loudly
    /// rather than quietly press keys somewhere else: `--enter` presses a key
    /// now (#581), but it still does not turn an unmarked id into a target.
    #[test]
    fn a_lone_bare_id_refuses_loudly_rather_than_retargeting() {
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };
        let mut backend = mock();
        let err = execute(cli(&["tty7", "send", "83"]), &ctx, &mut backend)
            .expect_err("a bare id has nothing to send");
        assert!(err.to_string().contains("needs TEXT"), "{err}");
        // The escape hatch for typing it anyway is in the message.
        assert!(err.to_string().contains("send %PANE 83"), "{err}");
        assert!(
            backend.sent.is_empty(),
            "no keystroke reached pane 83 or pane 5"
        );

        // `--enter` counts as the keystroke it always was (#581) — but not
        // enough to promote an *unmarked* id, or `send 2 --enter` meaning "type
        // 2 and run it" would press Enter in pane 2 instead. Both ways out are
        // named, because either could have been meant.
        let mut backend = mock();
        let err = execute(cli(&["tty7", "send", "83", "--enter"]), &ctx, &mut backend)
            .expect_err("--enter alone does not make a bare id a target");
        assert!(err.to_string().contains("send %83 --enter"), "{err}");
        assert!(err.to_string().contains("send %PANE 83 --enter"), "{err}");
        assert!(
            backend.sent.is_empty(),
            "no keystroke reached pane 83 or pane 5"
        );
    }

    /// `--enter` is documented as shorthand for `--key enter`, so it has to
    /// give a lone address something to do exactly as `--key` does — it used to
    /// report "needs TEXT … or a --key to press" and press nothing (#581).
    #[test]
    fn enter_alone_presses_enter_at_the_address_it_was_given() {
        let mut backend = mock();
        let json = json_of(run_cli(
            &["tty7", "send", "%42", "--enter"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(backend.sent, vec![(42, b"\r".to_vec())]);
        assert_eq!(json["sent"], "", "nothing was typed");
        assert_eq!(json["keys"], serde_json::json!(["enter"]));
        assert_eq!(json["enter"], true);

        // With no address at all it is the caller's own pane, the same as
        // `send --key enter` already was.
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };
        let mut backend = mock();
        run_cli(&["tty7", "send", "--enter"], &ctx, &mut backend);
        assert_eq!(backend.sent, vec![(5, b"\r".to_vec())]);

        // The long way round stays open for a bare id, and means the same
        // thing: an explicit `--key` promotes either spelling.
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "83", "--key", "enter"],
            &ctx,
            &mut backend,
        );
        assert_eq!(backend.sent, vec![(83, b"\r".to_vec())]);
        let mut backend = mock();
        run_cli(&["tty7", "send", "%83", "--enter"], &ctx, &mut backend);
        assert_eq!(backend.sent, vec![(83, b"\r".to_vec())]);
    }

    /// The narrowing has to leave real text alone: `%` followed by a non-digit
    /// is nobody's idea of a pane address, and driving vim's ex commands is a
    /// documented use of `send`.
    #[test]
    fn percent_led_text_still_types_into_the_current_pane() {
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };
        let mut backend = mock();
        run_cli(&["tty7", "send", "%s/a/b/"], &ctx, &mut backend);
        assert_eq!(backend.sent, vec![(5, b"%s/a/b/".to_vec())]);

        let mut backend = mock();
        run_cli(&["tty7", "send", "%!sort", "--enter"], &ctx, &mut backend);
        assert_eq!(
            backend.sent,
            vec![(5, b"%!sort".to_vec()), (5, b"\r".to_vec())]
        );

        // Nothing marks these as addresses, so the narrowing must leave them
        // typing: `3x` has no `%`, and `+5` only looked numeric to
        // `u64::from_str`.
        for text in ["3x", "+5", "50%"] {
            let mut backend = mock();
            run_cli(&["tty7", "send", text], &ctx, &mut backend);
            assert_eq!(
                backend.sent,
                vec![(5, text.as_bytes().to_vec())],
                "'{text}' is text, not an address"
            );
        }
    }

    /// The explicit address slot takes the bare id `pane ls --json` prints,
    /// not only the `%`-marked spelling (#538).
    #[test]
    fn a_bare_id_works_as_an_explicit_address() {
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "83", "--key", "C-c"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.sent, vec![(83, vec![0x03])]);

        // A lone bare id with nothing to press is the missing-text error, same
        // as a lone `%83` — parseable address, nothing to do with it.
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "83"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a bare address sends nothing and must say so");
        assert!(err.to_string().contains("needs TEXT"), "{err}");
        assert!(backend.sent.is_empty());
    }

    #[test]
    fn send_outside_a_shell_without_an_address_names_the_fix() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "echo hi"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), address::OUTSIDE_SHELL);

        let err = execute(
            cli(&["tty7", "send", "%1"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("TEXT"),
            "an address with nothing to send is a mistake, not empty input: {err}"
        );
    }

    #[test]
    fn capture_and_procs_are_wired_through_the_backend() {
        let mut backend = mock();
        backend.capture_segments = vec![segment(b"$ make\r\nok\r\n")];
        let out = run_cli(
            &["tty7", "capture", "%2", "--scrollback"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.captured, vec![(2, true)]);
        assert_eq!(
            human(out),
            "$ make\r\nok\r\n",
            "without --plain the pane's bytes are passed through untouched"
        );

        // The registry has to name %1 as well as the tree: `procs` reads an
        // empty answer as "no such pane" unless the server is running one, and
        // a server running the pane its tree names is what the real pair look
        // like.
        backend.registry = vec![pane_info(1, None)];
        run_cli(&["tty7", "procs", "%1"], &Context::default(), &mut backend);
        assert_eq!(backend.procs_calls, vec![1]);
    }

    #[test]
    fn capture_plain_replays_the_bytes_through_a_grid() {
        let mut backend = mock();
        // Coloured, CR-overwritten, and wrapped past the 20-column pane: three
        // things the raw form shows verbatim and `--plain` has to resolve.
        backend.capture_segments = vec![segment(
            b"\x1b[32m$ make\x1b[0m\r\n10%\r100%\r\nabcdefghijklmnopqrstuvwxyz\r\n",
        )];
        let raw = human(run_cli(
            &["tty7", "capture", "%2"],
            &Context::default(),
            &mut backend,
        ));
        assert!(
            raw.contains("\x1b[32m"),
            "the default keeps escapes: {raw:?}"
        );

        let plain = human(run_cli(
            &["tty7", "capture", "%2", "--plain"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(plain, "$ make\n100%\nabcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn capture_json_carries_whichever_form_was_asked_for() {
        let mut backend = mock();
        backend.capture_segments = vec![segment(b"\x1b[31mred\x1b[0m\r\n")];
        let raw = json_of(run_cli(
            &["tty7", "capture", "%2"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(raw["text"], json!("\u{1b}[31mred\u{1b}[0m\r\n"));

        let plain = json_of(run_cli(
            &["tty7", "capture", "%2", "--plain"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(plain["text"], json!("red"));
        assert_eq!(plain["pane"], json!(2));
    }

    #[test]
    fn run_passes_the_command_and_its_exit_code_through() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let ctx = Context {
            ws: Some(api.to_string()),
            ..Context::default()
        };
        let out = execute(
            cli(&["tty7", "run", "--keep", "--", "cargo", "test"]),
            &ctx,
            &mut backend,
        )
        .unwrap();
        assert_eq!(
            backend.runs,
            vec![RunSpec {
                workspace: Some(api),
                cwd: None,
                command: vec!["cargo".into(), "test".into()],
                keep: true,
            }]
        );
        assert!(
            matches!(out, Outcome::Exit(0, _)),
            "run's outcome is the child's exit code"
        );
    }

    #[test]
    fn run_keep_spawns_first_then_files_the_pane_into_the_workspace() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let out = execute(
            cli(&[
                "tty7", "run", "--keep", "--ws", "api", "--cwd", "C:\\proj", "--", "cargo", "watch",
            ]),
            &Context::default(),
            &mut backend,
        )
        .unwrap();
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::TabCreate {
                    workspace: api,
                    at: None,
                    pane: PaneSeed {
                        pane: 6,
                        cwd: Some("C:\\proj".into()),
                        ssh_spec: None,
                        agent: None,
                        shell: None,
                    },
                    tab: None,
                },
            ],
            "the daemon-assigned pane id (6) lands in the tree op, so the spawn came first"
        );
        assert_eq!(
            backend.runs,
            vec![RunSpec {
                workspace: Some(api),
                cwd: Some("C:\\proj".into()),
                command: vec!["cargo".into(), "watch".into()],
                keep: true,
            }]
        );
        assert!(matches!(out, Outcome::Exit(0, _)));
    }

    #[test]
    fn run_without_keep_files_nothing() {
        let mut backend = mock();
        let out = execute(
            cli(&["tty7", "run", "--", "cargo", "test"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap();
        assert!(
            backend.control_calls.is_empty(),
            "a reaped pane must not be filed into the tree"
        );
        assert_eq!(backend.runs.len(), 1);
        assert!(matches!(out, Outcome::Exit(0, _)));
    }

    #[test]
    fn run_keep_with_a_tty7_ws_this_machine_lost_spawns_nothing() {
        // The shell that set `$TTY7_WS` may have outlived its workspace, or be
        // pointed at another machine. Filing happens after the spawn, so
        // checking only at that point started the pane and then failed — an
        // orphan nobody asked for, recoverable but only through
        // `pane ls --all`. The `--ws` arm resolves before spawning; this is
        // the same guarantee for the inherited id.
        let mut backend = mock();
        let ctx = Context {
            ws: Some("11111111-2222-3333-4444-555555555555".to_string()),
            ..Context::default()
        };
        let err = execute(
            cli(&["tty7", "run", "--keep", "--", "make"]),
            &ctx,
            &mut backend,
        )
        .expect_err("a workspace this machine does not have cannot hold a kept pane");
        assert!(err.to_string().contains("no workspace with id"), "{err}");
        assert!(backend.runs.is_empty(), "nothing must be spawned");
    }

    #[test]
    fn run_keep_without_a_workspace_names_the_fix() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "run", "--keep", "--", "make"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a kept pane with no workspace would be an unlisted orphan");
        assert!(err.to_string().contains("--ws"), "{err}");
        assert!(backend.runs.is_empty(), "nothing must be spawned");
    }

    /// A `--cwd` that does not name a directory must stop the run.
    ///
    /// The daemon falls back to a directory that resolves when the one it gets
    /// does not, which is right for a cwd inherited from a pane's OSC 7 and
    /// wrong for one typed on the command line: `run --cwd ~/porj -- make`
    /// used to build whatever tree the CLI happened to be started in and exit
    /// 0. Nothing may be spawned — a refusal after the spawn is not a refusal.
    #[test]
    fn a_cwd_that_is_not_a_directory_stops_the_run() {
        let mut backend = mock();
        backend.this_machine = true;
        let err = execute(
            cli(&["tty7", "run", "--cwd", "/nonexistent-dir-xyz", "--", "make"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a directory that is not there cannot be run in");
        assert!(err.to_string().contains("no such directory"), "{err}");
        assert!(backend.runs.is_empty(), "nothing must be spawned");

        // A path that exists but is a file is the other half, and reads
        // differently to whoever typed it.
        let mut backend = mock();
        backend.this_machine = true;
        let err = execute(
            cli(&["tty7", "run", "--cwd", "/etc/hosts", "--", "make"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a file is not a working directory");
        assert!(err.to_string().contains("not a directory"), "{err}");
        assert!(backend.runs.is_empty(), "nothing must be spawned");

        // Routed: the path belongs to the far side's filesystem, so this
        // machine has no standing to judge it and the run goes ahead.
        let mut backend = mock();
        backend.this_machine = false;
        execute(
            cli(&["tty7", "run", "--cwd", "/nonexistent-dir-xyz", "--", "make"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a routed cwd is the remote machine's to resolve");
        assert_eq!(backend.runs.len(), 1, "the routed run must still happen");
    }

    /// Filing a spawned pane can fail, and the pane must not survive it.
    ///
    /// Every verb that adds a pane spawns it first and files it second — the
    /// seed carries the daemon's own pane id, so there is no other order. A
    /// refusal in the gap used to end the command with a shell running that no
    /// tree referenced: invisible to `tty7 ls`, visible only to `pane ls
    /// --all`, and collectable only by `pane close --orphans`.
    #[test]
    fn a_pane_that_cannot_be_filed_does_not_outlive_the_command() {
        // tab new: spawn, then TabCreate refuses.
        let mut backend = mock();
        backend.fail_nth_control = Some(1); // 0 is the MachineGet that resolves the workspace
        let err = execute(
            cli(&["tty7", "tab", "new", "api"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("the tab was refused");
        assert_eq!(backend.spawned.len(), 1, "the pane was already spawned");
        assert_eq!(
            backend.killed.len(),
            1,
            "and has to be hung up again: {err}"
        );

        // new <path>: spawn, then TabCreate refuses — and the workspace this
        // command made along the way goes too, rather than staying empty.
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::default()));
        backend.fail_nth_control = Some(1);
        let err = execute(cli(&["tty7", "new"]), &Context::default(), &mut backend)
            .expect_err("the first tab was refused");
        assert_eq!(backend.spawned.len(), 1, "the pane was already spawned");
        assert_eq!(backend.killed.len(), 1, "and is hung up again: {err}");
        assert!(
            backend
                .control_calls
                .iter()
                .any(|c| matches!(c, ControlRequest::WorkspaceRemove { .. })),
            "the empty workspace is taken back too: {:?}",
            backend.control_calls
        );
    }

    /// A ratio the split cannot use is refused before a pane is spawned.
    ///
    /// `split` spawns the shell and only then asks the tree to hold it, so any
    /// refusal in between leaves a pane running that nothing references —
    /// visible to `pane ls --all`, absent from the tree, and removable only by
    /// `pane close --orphans`. `--ratio nan` did exactly that twice over,
    /// because a NaN cannot be serialised onto the control connection at all:
    /// the link dropped, so the daemon's own "must be a finite number" never
    /// came back, and two orphans were left behind.
    #[test]
    fn a_ratio_that_cannot_be_used_spawns_nothing() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "split", "%1", "--horizontal", "--ratio", "nan"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a split needs a real ratio");
        assert!(err.to_string().contains("finite"), "{err}");
        assert!(
            backend.spawned.is_empty(),
            "the refusal has to come before the spawn, or it leaves an orphan"
        );
    }

    /// `tty7 new <path>` refuses a path it cannot root the workspace at, and
    /// refuses it before creating anything.
    ///
    /// The pane otherwise landed in whatever directory the CLI was started
    /// from, so the caller got an id back and a workspace somewhere they never
    /// named. Checked before `WorkspaceCreate`, or the refusal leaves an empty
    /// workspace behind for the user to clean up.
    #[test]
    fn new_refuses_a_path_it_cannot_root_the_workspace_at() {
        let mut backend = mock();
        backend.this_machine = true;
        let err = execute(
            cli(&["tty7", "new", "/nonexistent-dir-xyz"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a workspace cannot be rooted at a directory that is not there");
        assert!(err.to_string().contains("no such directory"), "{err}");
        assert!(
            backend.control_calls.is_empty(),
            "nothing may be created before the path is judged: {:?}",
            backend.control_calls
        );
        assert!(backend.spawned.is_empty(), "and no shell spawned");
    }

    #[test]
    fn a_missing_exit_code_still_exits_nonzero_via_the_note_path() {
        let mut backend = mock();
        backend.run_exit = None;
        let out = execute(
            cli(&["tty7", "run", "--", "make"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Exit(1, _)),
            "an unknown exit code is still a failure"
        );
        assert!(EXIT_CODE_UNKNOWN.contains("could not be determined"));

        let Outcome::Exit(_, report) = out else {
            unreachable!("just matched")
        };
        assert_eq!(
            report.json["exit_code_known"],
            serde_json::json!(false),
            "--json has to distinguish a stand-in 1 from the command's own 1: {}",
            report.json
        );
    }

    #[test]
    fn run_answers_json_even_though_it_carries_an_exit_code() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let ctx = Context {
            ws: Some(api.to_string()),
            ..Context::default()
        };
        let out = execute(
            cli(&["tty7", "run", "--json", "--", "cargo", "test"]),
            &ctx,
            &mut backend,
        )
        .unwrap();
        let Outcome::Exit(code, report) = out else {
            panic!("run stands in for its child, so it exits with the child's code");
        };
        assert_eq!(code, 0);
        assert_eq!(report.json["exit"], serde_json::json!(0), "{}", report.json);
        assert_eq!(
            report.json["exit_code_known"],
            serde_json::json!(true),
            "{}",
            report.json
        );
        assert!(
            report.json["pane"].as_u64().is_some(),
            "the pane that ran it is part of the answer: {}",
            report.json
        );
        assert!(
            report.human.is_empty(),
            "the command's own output already streamed; the report must not repeat it"
        );
    }

    #[test]
    fn a_machine_flag_refuses_the_local_server_verbs() {
        for verb in ["start", "stop", "restart", "logs"] {
            let mut backend = mock();
            let err = execute(
                cli(&["tty7", "-m", "devbox", "server", verb]),
                &Context::default(),
                &mut backend,
            )
            .expect_err("server lifecycle verbs are local-only");
            let msg = err.to_string();
            assert!(msg.contains("LOCAL"), "{msg}");
            assert!(msg.contains(verb), "{msg}");
            assert!(
                backend.control_calls.is_empty(),
                "the refusal must come before any dial"
            );
        }
    }

    #[test]
    fn a_mistyped_subcommand_is_named_as_one_not_offered_to_the_gui() {
        for typo in ["tree", "statu", "pnae", "workspace"] {
            let err = execute(cli(&["tty7", typo]), &Context::default(), &mut mock())
                .expect_err("a bare word is not a path");
            let msg = err.to_string();
            assert!(msg.contains("unknown subcommand"), "{typo}: {msg}");
            assert!(msg.contains(typo), "{typo}: {msg}");
        }
    }

    #[test]
    fn a_path_asks_the_running_gui_to_open_an_absolute_directory() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Bool(true));
        let out = run_cli(&["tty7", "."], &Context::default(), &mut backend);
        let expected = std::env::current_dir()
            .unwrap()
            .join(".")
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            backend.control_calls,
            vec![ControlRequest::GuiOpen {
                path: Some(expected.clone()),
                workspace: None,
            }]
        );
        let Outcome::Report(report) = out else {
            panic!("GUI open is a regular report");
        };
        assert_eq!(report.json["path"], expected);
        assert_eq!(report.json["delivered"], true);
        assert_eq!(report.json["launched"], false);
    }

    #[test]
    fn an_invalid_gui_path_fails_before_touching_the_wire() {
        let mut backend = mock();
        let missing =
            std::env::temp_dir().join(format!("tty7-cli-missing-path-{}", std::process::id()));
        let arg = missing.to_str().unwrap().to_owned();
        let err = execute(cli(&["tty7", &arg]), &Context::default(), &mut backend)
            .expect_err("a missing directory must be rejected");
        assert!(err.to_string().contains("opening"), "{err:#}");
        assert!(backend.control_calls.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_gui_path_stays_off_the_string_protocol() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = std::env::temp_dir().join(OsString::from_vec(b"tty7-\xff".to_vec()));
        assert_eq!(gui_wire_path(&path), None);
    }

    #[test]
    fn the_gui_launcher_is_local_only() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "-m", "devbox", "."]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a local GUI request cannot be routed to another machine");
        assert!(err.to_string().contains("cannot be combined"), "{err:#}");
        assert!(backend.control_calls.is_empty());
    }

    #[test]
    fn the_still_missing_verbs_say_so_without_touching_the_wire() {
        for (args, needle) in [
            (vec!["tty7", "ws", "stop", "api"], "not implemented"),
            (
                vec!["tty7", "machine", "connect", "devbox"],
                "not implemented",
            ),
            (
                vec!["tty7", "machine", "disconnect", "devbox"],
                "not implemented",
            ),
        ] {
            let mut backend = mock();
            let err = execute(cli(&args), &Context::default(), &mut backend)
                .expect_err("stubbed verbs must fail loudly, not pretend");
            assert!(
                err.to_string().contains(needle),
                "{args:?} should mention '{needle}': {err}"
            );
            assert!(
                backend.control_calls.is_empty(),
                "{args:?} must not invent protocol traffic"
            );
        }
    }

    fn agent_state(
        pane_id: u64,
        status: tty7_core::core::cli_agent::AgentStatus,
    ) -> tty7_core::daemon::control::PaneAgentState {
        agent_state_at(pane_id, status, 0)
    }

    /// A pane sitting at its prompt: the shell, and nothing in front of it.
    fn idle_procs() -> tty7_core::daemon::protocol::PaneProcs {
        tty7_core::daemon::protocol::PaneProcs {
            procs: vec![proc_entry(100, "zsh", 0, true)],
            ports: Vec::new(),
        }
    }

    /// The same pane with a command running in it.
    fn busy_procs() -> tty7_core::daemon::protocol::PaneProcs {
        tty7_core::daemon::protocol::PaneProcs {
            procs: vec![
                proc_entry(100, "zsh", 0, false),
                proc_entry(101, "cargo", 1, true),
            ],
            ports: Vec::new(),
        }
    }

    fn proc_entry(
        pid: u32,
        name: &str,
        depth: u8,
        foreground: bool,
    ) -> tty7_core::daemon::protocol::ProcEntry {
        tty7_core::daemon::protocol::ProcEntry {
            pid,
            name: name.into(),
            depth,
            foreground,
        }
    }

    fn agent_state_at(
        pane_id: u64,
        status: tty7_core::core::cli_agent::AgentStatus,
        activity: u64,
    ) -> tty7_core::daemon::control::PaneAgentState {
        tty7_core::daemon::control::PaneAgentState {
            pane_id,
            agent: None,
            state: tty7_core::core::cli_agent::AgentSessionState {
                status,
                message: Some("needs permission".into()),
                session_id: Some("sess-9".into()),
                activity,
                ..Default::default()
            },
        }
    }

    fn detect_agent(backend: &mut MockBackend, pane_id: u64, agent: CLIAgent) {
        use tty7_core::core::machine::AgentFacts;

        let pane = backend
            .machine
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
            .expect("test pane exists");
        pane.live = true;
        pane.agent = Some(AgentFacts {
            agent,
            session_id: None,
            launch_argv: None,
            status: None,
        });
    }

    /// The happy path is one aggregate poll: a matching agent state answers
    /// immediately, carrying the event's message and native session id — the
    /// two things an orchestrator needs to act on the wake-up.
    #[test]
    fn wait_returns_the_moment_the_state_matches() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Waiting,
            )]));
        let out = run_cli(&["tty7", "wait", "%3"], &Context::default(), &mut backend);
        let json = json_of(out);
        assert_eq!(json["status"], "waiting");
        assert_eq!(json["matched"], true);
        assert_eq!(json["message"], "needs permission");
        assert_eq!(json["session_id"], "sess-9");
        // …and flagged as a state we merely walked in on, not one we watched
        // the pane move into: it may be answering a previous turn.
        assert_eq!(json["stale"], true);
        // The machine tree was never consulted — the agent state alone answered.
        assert_eq!(backend.control_calls, vec![ControlRequest::AgentStates]);
    }

    /// `--changed` is the fix for a level-triggered status: right after a
    /// `send`, the agent still reports last turn's state. The flag refuses the
    /// position the wait arrived at and wakes only once the pane moves.
    #[test]
    fn wait_changed_refuses_the_state_it_arrived_in() {
        use tty7_core::core::cli_agent::AgentStatus;

        // Already `waiting` when the wait began, and it never moves: timeout,
        // not a bogus wake-up carrying the previous turn's message.
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Waiting,
            )]));
        let out = execute(
            cli(&[
                "tty7",
                "wait",
                "%3",
                "--until",
                "waiting",
                "--changed",
                "--timeout",
                "0",
            ]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        assert!(
            matches!(out, Outcome::Exit(124, _)),
            "a state that was already standing must not satisfy --changed"
        );

        // The same state again, but this time the agent moved under it: the
        // activity counter ticks even when the status letter does not.
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state_at(
                3,
                AgentStatus::Waiting,
                0,
            )]));
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state_at(
                3,
                AgentStatus::Waiting,
                1,
            )]));
        let json = json_of(run_cli(
            &[
                "tty7",
                "wait",
                "%3",
                "--until",
                "waiting",
                "--changed",
                "--interval",
                "50",
            ],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "waiting");
        assert_eq!(json["matched"], true);
        assert_eq!(json["stale"], false, "this one we watched it move into");
        assert_eq!(json["activity"], 1);
    }

    /// An agent state outlives the pane's child — the daemon keeps a dead pane
    /// registered until it is closed. Without a liveness re-check a crashed
    /// worker would report `working` right up to the timeout.
    #[test]
    fn wait_ends_when_a_reporting_agents_pane_dies() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        for p in &mut backend.machine.panes {
            if p.id == 3 {
                p.live = false;
            }
        }
        for _ in 0..4 {
            backend
                .replies
                .push_back(ReplyOk::AgentStates(vec![agent_state(
                    3,
                    AgentStatus::Working,
                )]));
        }
        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--interval", "50"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "exit");
        assert_eq!(json["matched"], true, "the default until-set covers exit");
        assert!(
            backend.control_calls.contains(&ControlRequest::MachineGet),
            "liveness has to come from the tree; the agent snapshot has none"
        );
    }

    /// Panes without an agent state fall back to the machine tree: live means
    /// `no-agent`, dead-or-gone means exit — which ends every wait, but only
    /// counts as *matched* when the caller listed it.
    #[test]
    fn wait_reads_agentless_panes_from_the_tree() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(
            &["tty7", "wait", "%3", "--until", "no-agent"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(json_of(out)["status"], "no-agent");

        // Pane 9 exists nowhere: "exit", matched by the default until-set.
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(&["tty7", "wait", "%9"], &Context::default(), &mut backend);
        let json = json_of(out);
        assert_eq!(json["status"], "exit");
        assert_eq!(json["matched"], true);

        // Waiting for a state a dead pane can never reach is a failure, not a
        // silent success — but a *structured* one: a script has to tell "my
        // peer died" apart from "the daemon is unreachable", and an anyhow
        // error would leave --json with nothing to read.
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = execute(
            cli(&["tty7", "wait", "%9", "--until", "done"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a dead pane is an exit code with a report, not a bare error");
        let Outcome::Exit(1, r) = out else {
            panic!("a dead pane that cannot reach `done` must exit 1 with its report");
        };
        assert_eq!(r.json["status"], "exit");
        assert_eq!(r.json["matched"], false);
        assert!(r.human.contains("exited"), "{}", r.human);
    }

    /// The trap this state exists to close. A pane with nothing reporting used
    /// to answer `idle`, so `--until idle` returned success — instantly, with
    /// `matched: true` — about a shell that was midway through a build. The
    /// caller then read a half-finished screen and believed it.
    #[test]
    fn wait_does_not_call_a_busy_shell_idle() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        backend.procs_reply = busy_procs();

        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "idle", "--timeout", "0"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        let Outcome::Exit(124, r) = out else {
            panic!("a pane with no agent must not satisfy --until idle");
        };
        assert_eq!(r.json["status"], "no-agent");
        assert!(
            r.human.contains("--until free"),
            "the timeout should point at the flag that answers this question: {}",
            r.human
        );
    }

    /// `free` is the missing half of the verb: an agent pane has a status to
    /// wait on, a pane merely running a command has only its process tree.
    /// Nothing below the depth-0 shell means the foreground command exited.
    #[test]
    fn wait_free_ends_when_the_foreground_command_exits() {
        let mut backend = mock();
        for _ in 0..3 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        // Busy, busy, then back to the bare shell.
        backend.procs_replies.push_back(busy_procs());
        backend.procs_replies.push_back(busy_procs());
        backend.procs_replies.push_back(idle_procs());

        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--until", "free", "--interval", "50"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "free");
        assert_eq!(json["matched"], true);
        assert_eq!(json["stale"], false, "we watched the command finish");
        assert_eq!(
            backend.procs_calls.len(),
            3,
            "one process-tree read per poll, and only because `free` was asked for"
        );
    }

    /// The process tree is level-triggered like the agent ladder, but a shell
    /// that goes free → busy → free lands back where it started, so a baseline
    /// comparison would miss it. `--changed` therefore means "something ran
    /// while I watched" here — which is what a caller wants right after `send`.
    #[test]
    fn wait_changed_free_waits_for_something_to_actually_run() {
        // Already free and it stays that way: the command has not started yet,
        // so answering "free" would report the shell we sent the work *to*.
        let mut backend = mock();
        for _ in 0..2 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend.procs_reply = idle_procs();
        let out = execute(
            cli(&[
                "tty7",
                "wait",
                "%3",
                "--until",
                "free",
                "--changed",
                "--timeout",
                "0",
            ]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        let Outcome::Exit(124, r) = out else {
            panic!("a pane that was free all along has not run anything");
        };
        // A timeout answers in the success path's own shape, plus the flag —
        // a consumer's error branch must not meet missing fields (#589).
        assert_eq!(r.json["timed_out"], true);
        assert_eq!(r.json["matched"], false);
        assert_eq!(r.json["stale"], true, "nothing ran while we watched");
        assert!(r.json.get("session_id").is_some());

        // Free → busy → free is the real shape, and it must wake.
        let mut backend = mock();
        for _ in 0..3 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend.procs_replies.push_back(idle_procs());
        backend.procs_replies.push_back(busy_procs());
        backend.procs_replies.push_back(idle_procs());
        let json = json_of(run_cli(
            &[
                "tty7",
                "wait",
                "%3",
                "--until",
                "free",
                "--changed",
                "--interval",
                "50",
            ],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "free");
        assert_eq!(json["matched"], true);
        assert_eq!(json["stale"], false);
    }

    /// A command that starts and finishes between two polls is never *seen*
    /// busy, which is indistinguishable from one that never ran — so the
    /// timeout has to name both doors instead of letting a finished command
    /// read as "still going".
    #[test]
    fn wait_changed_free_says_why_it_saw_nothing_run() {
        let mut backend = mock();
        for _ in 0..2 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend.procs_reply = idle_procs();

        let out = execute(
            cli(&[
                "tty7",
                "wait",
                "%3",
                "--until",
                "free",
                "--changed",
                "--timeout",
                "0",
            ]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        let Outcome::Exit(124, r) = out else {
            panic!("a pane that was free all along has not run anything");
        };
        assert!(
            r.human.contains("--interval") && r.human.contains("--changed"),
            "the timeout should name the two ways out: {}",
            r.human
        );
    }

    /// `free` answers for a pane the agent ladder cannot, so it must not answer
    /// *over* it. A pane whose depth-0 process is the agent itself reads free
    /// for its whole turn; letting that outrank a `waiting` the caller asked
    /// for would strand exactly the delegation loop the verb exists for.
    #[test]
    fn wait_free_does_not_overrule_a_state_the_caller_asked_for() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Waiting,
            )]));
        // The agent is the pane's only process, so the tree reads "free".
        backend.procs_reply = idle_procs();

        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--until", "waiting,free"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "waiting", "the ladder answered first");
        assert_eq!(json["matched"], true);
        assert!(
            backend.procs_calls.is_empty(),
            "and the process tree was never asked"
        );
    }

    /// An unreadable process tree is not an idle one. Answering `free` on an
    /// empty reply would be the same false success `no-agent` was added to
    /// remove, one layer down.
    #[test]
    fn wait_free_does_not_read_an_empty_process_tree_as_finished() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        backend.procs_reply = tty7_core::daemon::protocol::PaneProcs::default();

        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "free", "--timeout", "0"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        assert!(
            matches!(out, Outcome::Exit(124, _)),
            "nothing was seen, so nothing can be claimed"
        );
    }

    /// Watching `free` must not cost anything for callers who did not ask:
    /// the process tree is a second round trip per poll on top of the agent
    /// snapshot, and the default wait is for agents.
    #[test]
    fn wait_only_reads_the_process_tree_when_free_is_asked_for() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Waiting,
            )]));
        run_cli(&["tty7", "wait", "%3"], &Context::default(), &mut backend);
        assert!(
            backend.procs_calls.is_empty(),
            "the default until-set names no pane-level state"
        );
    }

    /// A `--timeout` that runs out exits 124 — the `timeout(1)` convention —
    /// so scripts can branch on "not yet" separately from "broken".
    #[test]
    fn wait_timeout_exits_124() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Working,
            )]));
        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "done", "--timeout", "0"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        match out {
            Outcome::Exit(124, r) => assert_eq!(r.json["timed_out"], true),
            other => panic!("expected exit 124, got {other:?}"),
        }
    }

    #[test]
    fn agents_distinguishes_no_agent_from_a_healthy_agent() {
        use tty7_core::core::cli_agent::AgentStatus;

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(&["tty7", "agents"], &Context::default(), &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![ControlRequest::AgentStates, ControlRequest::MachineGet]
        );
        assert_eq!(human(out), "no agents running\n");

        let mut backend = mock();
        detect_agent(&mut backend, 1, CLIAgent::Codex);
        backend.agent_hooks_states = vec![(HookAgent::Codex, HooksState::NotInstalled)];
        let mut reporting = agent_state(1, AgentStatus::Working);
        reporting.agent = Some(CLIAgent::Codex);
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![reporting]));
        let out = run_cli(&["tty7", "agents"], &Context::default(), &mut backend);
        let rendered = human(out);
        assert!(rendered.contains("codex"), "{rendered}");
        assert!(
            !rendered.contains("hooks"),
            "an agent already reporting status is healthy: {rendered}"
        );

        let mut backend = mock();
        detect_agent(&mut backend, 1, CLIAgent::Codex);
        backend.agent_hooks_states = vec![(HookAgent::Codex, HooksState::Installed)];
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(&["tty7", "agents"], &Context::default(), &mut backend);
        assert_eq!(
            human(out),
            "no agents running\n",
            "installed hooks are not diagnosed during the gap before the first event"
        );
    }

    #[test]
    fn agents_reports_missing_hooks_once_per_agent_without_failing() {
        let mut backend = mock();
        detect_agent(&mut backend, 1, CLIAgent::Codex);
        detect_agent(&mut backend, 2, CLIAgent::Codex);
        backend.agent_hooks_states = vec![(HookAgent::Codex, HooksState::NotInstalled)];
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));

        let out = run_cli(&["tty7", "agents"], &Context::default(), &mut backend);
        let Outcome::Report(report) = out else {
            panic!("a successful diagnosis must not be a command failure");
        };
        assert_eq!(report.human.matches("Codex is running").count(), 1);
        assert!(report.human.contains("hooks are not installed"));
        assert!(report.human.contains("install the hooks"));
        assert_eq!(
            report.json,
            json!({
                "agents": [],
                "diagnostics": [{
                    "kind": "agent_status_hooks_unavailable",
                    "agent": "codex",
                    "hooks_state": "not_installed",
                    "action": "install",
                }],
            })
        );
    }

    #[test]
    fn agents_reports_outdated_hooks_with_the_update_action() {
        let mut backend = mock();
        detect_agent(&mut backend, 3, CLIAgent::Claude);
        backend.agent_hooks_states = vec![(HookAgent::Claude, HooksState::Outdated)];
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));

        let report = match run_cli(&["tty7", "agents"], &Context::default(), &mut backend) {
            Outcome::Report(report) => report,
            Outcome::Exit(code, _) => panic!("diagnosis unexpectedly exited {code}"),
        };
        assert!(report.human.contains("Claude Code is running"));
        assert!(report.human.contains("hooks are outdated"));
        assert!(report.human.contains("update the hooks"));
        assert_eq!(report.json["diagnostics"][0]["agent"], "claude");
        assert_eq!(report.json["diagnostics"][0]["hooks_state"], "outdated");
        assert_eq!(report.json["diagnostics"][0]["action"], "update");
    }

    #[test]
    fn agents_json_keeps_the_existing_agents_shape_when_there_is_no_diagnostic() {
        use tty7_core::core::cli_agent::AgentStatus;

        let mut backend = mock();
        let mut reporting = agent_state(1, AgentStatus::Waiting);
        reporting.agent = Some(CLIAgent::Codex);
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![reporting.clone()]));
        let json = json_of(run_cli(
            &["tty7", "--json", "agents"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json, json!({ "agents": [reporting] }));
        assert!(json.get("diagnostics").is_none());
    }

    #[test]
    fn status_and_machine_ls_are_single_aggregate_requests() {
        use tty7_core::daemon::control::ServerStatus;

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Status(ServerStatus {
            pid: 4242,
            uptime_secs: 61,
            panes: 3,
            control_version: CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION,
            build: "26.7.5".into(),
            socket: "127.0.0.1:5555".into(),
        }));
        let out = run_cli(&["tty7", "status"], &Context::default(), &mut backend);
        assert_eq!(backend.control_calls, vec![ControlRequest::Status]);
        let rendered = human(out);
        assert!(rendered.contains("4242"), "{rendered}");
        assert!(rendered.contains("61s"), "{rendered}");

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Routes(vec![RouteInfo {
            key: "me@build-box:22".into(),
            kind: "ssh".into(),
            connected: true,
        }]));
        let out = run_cli(
            &["tty7", "machine", "ls"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.control_calls, vec![ControlRequest::Routes]);
        let rendered = human(out);
        assert!(
            rendered.contains("local"),
            "machine 0 is always listed: {rendered}"
        );
        assert!(rendered.contains("me@build-box:22"), "{rendered}");
    }

    /// `machine ls` is documented as "the local machine plus every link", and
    /// the two halves of it have to say the same thing.
    ///
    /// They did not: the table synthesized the local row while `--json`
    /// serialized the server's routes alone, so a machine with no remotes —
    /// which is every machine until someone connects one — printed a row for
    /// itself and answered `--json` with `{"machines":[]}`. An agent
    /// enumerating machines read that as "there are none", including the one
    /// it was running on.
    #[test]
    fn machine_ls_lists_the_local_machine_in_json_as_well_as_the_table() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Routes(Vec::new()));
        let out = run_cli(
            &["tty7", "machine", "ls"],
            &Context::default(),
            &mut backend,
        );
        let (rendered, json) = match out {
            Outcome::Report(r) => (r.human, r.json),
            Outcome::Exit(code, _) => panic!("expected a report, got exit {code}"),
        };
        assert!(
            rendered.contains("local"),
            "the table lists the local machine: {rendered}"
        );
        assert_eq!(
            json,
            json!({ "machines": [{ "key": "local", "kind": "local", "connected": true }] }),
            "and so does the JSON, on a machine with no remotes"
        );

        // With a link, both carry both, local first.
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Routes(vec![RouteInfo {
            key: "me@build-box:22".into(),
            kind: "ssh".into(),
            connected: false,
        }]));
        let out = run_cli(
            &["tty7", "machine", "ls"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            json_of(out),
            json!({ "machines": [
                { "key": "local", "kind": "local", "connected": true },
                { "key": "me@build-box:22", "kind": "ssh", "connected": false },
            ] })
        );
    }

    fn doctor_backend() -> MockBackend {
        use tty7_core::daemon::control::ServerStatus;

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Status(ServerStatus {
            pid: 4242,
            uptime_secs: 61,
            panes: 3,
            control_version: CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION,
            build: "26.7.5".into(),
            socket: "127.0.0.1:5555".into(),
        }));
        backend.replies.push_back(ReplyOk::Routes(Vec::new()));
        backend
    }

    #[test]
    fn doctor_reports_the_injected_context_and_the_server_half() {
        let out = human(run_cli(
            &["tty7", "doctor"],
            &Context::default(),
            &mut doctor_backend(),
        ));
        assert!(out.contains("TTY7_CONFIG_DIR"), "{out}");
        assert!(out.contains("missing"), "{out}");
        assert!(out.contains("dialect"), "{out}");
        assert!(
            out.contains(&format!("control v{CONTROL_VERSION}")),
            "{out}"
        );
        assert!(out.contains("pid 4242"), "{out}");
        assert!(out.contains("0 known"), "{out}");

        let ctx = Context {
            pane: Some("7".into()),
            ws: None,
            config_dir: Some("/cfg/tty7".into()),
        };
        let out = human(run_cli(&["tty7", "doctor"], &ctx, &mut doctor_backend()));
        assert!(out.contains("set (/cfg/tty7)"), "{out}");
    }

    /// A server running one pane said "1 panes".
    ///
    /// The count comes straight off the status reply, so every fresh server
    /// says it — `doctor` is the first thing a new user runs, and the first
    /// line it shows them was ungrammatical.
    #[test]
    fn doctor_counts_one_pane_in_the_singular() {
        use tty7_core::daemon::control::ServerStatus;

        let status = |panes: u64| ServerStatus {
            pid: 4242,
            uptime_secs: 61,
            panes,
            control_version: CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION,
            build: "26.7.5".into(),
            socket: "127.0.0.1:5555".into(),
        };
        let line = |panes: u64| {
            let mut backend = mock();
            backend.replies.push_back(ReplyOk::Status(status(panes)));
            backend.replies.push_back(ReplyOk::Routes(Vec::new()));
            human(run_cli(
                &["tty7", "doctor"],
                &Context::default(),
                &mut backend,
            ))
        };

        assert!(
            line(1).contains("1 pane\n") || line(1).contains("1 pane "),
            "{}",
            line(1)
        );
        assert!(!line(1).contains("1 panes"), "{}", line(1));
        assert!(line(0).contains("0 panes"), "{}", line(0));
        assert!(line(2).contains("2 panes"), "{}", line(2));
    }

    /// Missing hooks are the reason a perfectly healthy-looking agent never
    /// reports and `tty7 wait` sits there until it times out. `doctor` is the
    /// verb people run when something is not working, so it is where that has
    /// to be visible — and it long claimed to check hooks without doing so.
    #[test]
    fn doctor_reports_where_the_agent_status_hooks_stand() {
        use tty7_core::core::agent_hooks::HookAgent;

        let mut backend = doctor_backend();
        // The real backend answers for every agent it knows how to install
        // hooks for, so the mock does too — the interesting part is that the
        // three states are told apart, not that a lookup can come back empty.
        backend.agent_hooks_states = HookAgent::ALL
            .into_iter()
            .map(|agent| match agent {
                HookAgent::Claude => (agent, HooksState::Installed),
                HookAgent::Codex => (agent, HooksState::Outdated),
                other => (other, HooksState::NotInstalled),
            })
            .collect();
        let out = run_cli(&["tty7", "doctor"], &Context::default(), &mut backend);
        let Outcome::Report(r) = out else {
            panic!("doctor reports");
        };
        assert!(r.human.contains("agent hooks"), "{}", r.human);
        assert!(
            r.human.contains("OUTDATED"),
            "an outdated hook is the quiet failure worth shouting about: {}",
            r.human
        );
        assert!(
            r.human.contains("Settings → Agents"),
            "say where the fix is: {}",
            r.human
        );
        assert_eq!(r.json["hooks"]["installed"], serde_json::json!(["claude"]));
        assert_eq!(r.json["hooks"]["outdated"], serde_json::json!(["codex"]));
        assert_eq!(
            r.json["hooks"]["not_installed"]
                .as_array()
                .expect("the rest are reported as a list, not omitted")
                .len(),
            HookAgent::ALL.len() - 2
        );

        // A backend that cannot read hook state at all — a `-m` run, where the
        // hooks live on the other machine — says so rather than reporting a
        // machine-wide gap that is not there.
        let out = human(run_cli(
            &["tty7", "doctor"],
            &Context::default(),
            &mut doctor_backend(),
        ));
        assert!(out.contains("unknown"), "{out}");
    }

    /// An unreachable server is *the* finding doctor exists for, so the verb
    /// exits non-zero over it — `tty7 doctor || alert` has to fire — while
    /// still printing the full report (#592).
    #[test]
    fn doctor_exits_nonzero_when_the_server_is_unreachable() {
        let mut backend = mock();
        backend.unreachable = true;
        let out = run_cli(&["tty7", "doctor"], &Context::default(), &mut backend);
        let Outcome::Exit(1, r) = out else {
            panic!("an unreachable server is an exit 1, not a plain report: {out:?}");
        };
        assert_eq!(r.json["server"]["reachable"], serde_json::json!(false));
        // The rest of the report still goes out — the context rows are the
        // other half of what doctor is for.
        assert!(r.human.contains("TTY7_CONFIG_DIR"), "{}", r.human);
        assert!(r.human.contains("unreachable"), "{}", r.human);
        // No Status/Routes round-trips happen once hello has failed.
        assert!(
            backend.control_calls.is_empty(),
            "{:?}",
            backend.control_calls
        );
    }
}
