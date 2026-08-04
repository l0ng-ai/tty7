use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tty7_core::core::agent_hooks::{HookAgent, HooksState};
use tty7_core::core::machine::{Axis, Machine, PaneSeed, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{CONTROL_VERSION, ControlEvent, ControlRequest, ReplyOk};
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
        Some(Command::New { path }) => new_workspace(path, backend),
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
        Some(Command::Pane(PaneCmd::Close { target })) => {
            pane_close(target.as_deref(), ctx, backend)
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
        Some(path) => match backend.control(ControlRequest::GuiOpen { path }) {
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

fn new_workspace(path: Option<String>, backend: &mut dyn Backend) -> Result<Outcome> {
    let ws = match backend.control(ControlRequest::WorkspaceCreate {
        name: None,
        workspace: None,
    })? {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => bail!("the server answered WorkspaceCreate with {other:?}"),
    };
    let pane = backend.spawn_shell(ws.id, path.clone())?;
    backend.control(ControlRequest::TabCreate {
        workspace: ws.id,
        at: None,
        pane: PaneSeed {
            pane,
            cwd: path,
            ssh_spec: None,
            agent: None,
        },
        tab: None,
    })?;
    report(
        ws.id.to_string(),
        json!({ "id": ws.id.to_string(), "pane": pane }),
    )
}

fn run(args: RunArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
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
    let new = backend.spawn_shell(workspace, cwd.clone())?;
    backend.control(ControlRequest::PaneSplit {
        workspace,
        pane,
        axis,
        ratio: args.ratio,
        new: PaneSeed {
            pane: new,
            cwd,
            ssh_spec: None,
            agent: None,
        },
        first: false,
    })?;
    report(format!("%{new}"), json!({ "pane": new }))
}

fn send(args: SendArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    const ENTER_GAP: Duration = Duration::from_millis(200);

    let (target, text) = match &args.second {
        Some(text) => (Some(args.first.as_str()), text.as_str()),
        None => {
            if args.first.starts_with('%') && address::parse_pane(&args.first).is_ok() {
                bail!("send needs TEXT after the pane address");
            }
            (None, args.first.as_str())
        }
    };
    let pane = address::pane_or_context(target, ctx)?;
    backend.send_input(pane, text.as_bytes().to_vec())?;
    if args.enter {
        // Raw-mode TUIs detect a fast stream as pasted input and intentionally
        // absorb Enter as a newline. Keep the public one-shot command, but let
        // the text leave that burst window before delivering the key itself.
        std::thread::sleep(ENTER_GAP);
        backend.send_input(pane, vec![b'\r'])?;
    }
    report(
        "",
        json!({ "pane": pane, "sent": text, "enter": args.enter }),
    )
}

fn capture(args: CaptureArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let segments = backend.capture(pane, args.scrollback)?;
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
    let rows: Vec<Vec<String>> = ws
        .tabs
        .iter()
        .map(|tab| {
            vec![
                format!("@{}", resolve::ordinal_of(&machine, tab.id).unwrap_or(0)),
                tab.name.clone().unwrap_or_else(|| "-".to_string()),
                tab.root.pane_ids().len().to_string(),
            ]
        })
        .collect();
    let tabs: Vec<Value> = ws
        .tabs
        .iter()
        .map(|tab| {
            json!({
                "ordinal": resolve::ordinal_of(&machine, tab.id),
                "id": tab.id.to_string(),
                "name": tab.name,
                "panes": tab.root.pane_ids(),
            })
        })
        .collect();
    report(
        output::table(&["TAB", "NAME", "PANES"], &rows),
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
    let tab = match backend.control(ControlRequest::TabCreate {
        workspace: id,
        at: None,
        pane: PaneSeed {
            pane,
            cwd,
            ssh_spec: None,
            agent: None,
        },
        tab: None,
    })? {
        ReplyOk::TabTree(tab) => *tab,
        other => bail!("the server answered TabCreate with {other:?}"),
    };
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
            "\n{orphans} pane(s) held by no workspace — `tty7 pane close %<id>` stops one\n"
        ));
    }
    report(human, json!({ "panes": panes, "orphans": orphans }))
}

fn pane_close(target: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(target, ctx)?;
    let machine = fetch_machine(backend)?;
    match resolve::workspace_of_pane(&machine, pane) {
        Ok(ws) => {
            let workspace = ws.id;
            let reply = backend.control(ControlRequest::PaneClose { workspace, pane })?;
            hang_up_removed_panes("PaneClose", reply, backend)?;
        }
        // No workspace holds it, so PaneClose has nothing to route through.
        // Hang it up directly instead of refusing — this is exactly the orphan
        // `pane ls --all` just pointed the user at.
        Err(_) => backend.kill_pane(pane)?,
    }
    report("", json!({ "closed": pane }))
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

/// The one verb that *blocks*: poll until the watched pane's agent reaches a
/// requested state, then report it. This is what turns the CLI into an
/// orchestration tool — "wake me when my peer agent needs input, or finishes
/// its turn" — without the screen-scraping a tmux-based agent team resorts to.
///
/// A poll of `AgentStates` rather than an `events` subscription on purpose: a
/// one-shot, stateless question composes into scripts (`tty7 wait %3 &&
/// tty7 capture %3 --plain`), survives a server restart mid-wait, and needs no
/// cursor management. At the default 500ms interval the cost is one aggregate
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
    let mut baseline: Option<Cursor> = None;
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
            // No agent state for the pane: an agentless-but-live pane reads
            // as idle; a dead or vanished one as exit. The machine tree is
            // only fetched on this branch — while an agent is reporting, its
            // state alone answers the question.
            None if pane_is_live(backend, pane)? => WaitState::Idle,
            None => WaitState::Exit,
        };

        // Status is a level, not an edge (see `--changed` in cli.rs): the
        // position we arrived at is last turn's answer until the agent moves.
        let cursor: Cursor = entry.as_ref().map(|e| (e.state.status, e.state.activity));
        let baseline = *baseline.get_or_insert(cursor);
        let changed = cursor != baseline;

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
                human.push_str(" (unchanged since the wait began)");
            }
            return report(human, json);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // 124 = the `timeout(1)` convention: "gave up", distinct from
            // both success and error, so orchestration scripts can branch.
            return Ok(Outcome::Exit(
                124,
                Report {
                    human: format!("pane %{pane}: still {} — timed out", current.name()),
                    json: json!({ "pane": pane, "status": current.name(), "timed_out": true }),
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
        ReplyOk::Routes(routes) => report(
            output::routes_table(&routes),
            json!({ "machines": serde_json::to_value(&routes)? }),
        ),
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
                    "pid {}, up {}s, {} panes",
                    status.pid, status.uptime_secs, status.panes
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
    report(
        human,
        json!({
            "context": {
                "config_dir": ctx.config_dir.is_some(),
                "workspace": ctx.ws.is_some(),
                "pane": ctx.pane.is_some(),
            },
            "server": server,
        }),
    )
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
            alive: true,
            owner: owner.map(str::to_string),
        }
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
                path: Some(expected.clone())
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
    /// idle, dead-or-gone means exit — which ends every wait, but only counts
    /// as *matched* when the caller listed it.
    #[test]
    fn wait_reads_agentless_panes_from_the_tree() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(
            &["tty7", "wait", "%3", "--until", "idle"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(json_of(out)["status"], "idle");

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
        use tty7_core::daemon::control::{RouteInfo, ServerStatus};

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
}
