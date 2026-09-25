use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tty7_core::core::agent_hooks::{HookAgent, HooksState};
use tty7_core::core::machine::{Axis, Machine, PaneSeed, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::core::tab_view::tab_views_of;
use tty7_core::daemon::control::{CONTROL_VERSION, ControlEvent, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::PROTOCOL_VERSION;

use crate::address::{self, Context, WorkspaceAddress};
use crate::backend::{Backend, RunSpec};
use crate::cli::{
    CaptureArgs, Cli, Command, ExecArgs, MachineCmd, PaneCmd, RunArgs, SendArgs, ServerCmd,
    SplitArgs, TabCmd, WaitArgs, WaitState, WsCmd,
};
use crate::exec::ExecEnd;
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
        Some(Command::Exec(args)) => exec(args, ctx, backend),
        Some(Command::Capture(args)) => capture(args, ctx, backend),
        Some(Command::Procs { target }) => procs(target.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::Ls { ws })) => tab_ls(ws.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::New { ws, cwd, pane })) => {
            tab_new(ws.as_deref(), cwd, pane.as_deref(), ctx, backend)
        }
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
        Some(Command::Server(ServerCmd::Restart { hard })) => {
            local_server(machine.as_deref(), "restart", || {
                crate::server::restart(hard)
            })
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
    act: impl FnOnce() -> Result<Outcome>,
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
            shell: None,
        },
        tab: None,
    })?;
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
            shell: None,
        },
        first: false,
    })?;
    report(format!("%{new}"), json!({ "pane": new }))
}

fn send(args: SendArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
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

    // Text from stdin or a file has no positional of its own, so the one
    // positional there is can only be an address — and a word that is not one
    // is a mistake, not text to type into the caller's own pane.
    if args.stdin || args.from_file.is_some() {
        let source = if args.stdin { "--stdin" } else { "--from-file" };
        if let Some(first) = &args.first {
            address::parse_pane(first).with_context(|| {
                format!(
                    "with {source} the text comes from there, so '{first}' has to be the \
                     pane to send it to"
                )
            })?;
        }
        let pane = address::pane_or_context(args.first.as_deref(), ctx)?;
        let payload = match &args.from_file {
            Some(path) => {
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?
            }
            None => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
                    .context("reading stdin")?;
                bytes
            }
        };
        return deliver(
            pane,
            Payload::Piped {
                source: if args.stdin { "stdin" } else { "file" },
                bytes: payload,
            },
            &args,
            pressed,
            backend,
        );
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
    let payload = match text {
        Some(text) => Payload::Typed(text.to_string()),
        None => Payload::None,
    };
    deliver(pane, payload, &args, pressed, backend)
}

/// What `send` has to put in front of the keys.
enum Payload {
    None,
    /// From the command line — echoed back in `--json`, as it always was.
    Typed(String),
    /// From stdin or a file. Never echoed: the reason to use these is to keep
    /// the bytes out of places other people can read, and a JSON line in a
    /// log is one of them.
    Piped {
        source: &'static str,
        bytes: Vec<u8>,
    },
}

fn deliver(
    pane: u64,
    payload: Payload,
    args: &SendArgs,
    pressed: Vec<crate::keys::Key>,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    const KEY_GAP: Duration = Duration::from_millis(200);

    let (source, bytes) = match &payload {
        Payload::None => ("none", &[][..]),
        Payload::Typed(text) => ("argument", text.as_bytes()),
        Payload::Piped { source, bytes } => (*source, &bytes[..]),
    };
    if args.paste && bytes.is_empty() {
        bail!("--paste needs text to paste — TEXT, --stdin or --from-file");
    }
    // `--paste` frames the way the GUI's paste does, which is to ask the pane
    // first: brackets for a pane that switched mode 2004 on, and for one that
    // did not, the line breaks a keyboard would send. Forcing the brackets on
    // a pane that never asked for them would put `ESC[200~` in front of a
    // program that reads it as keystrokes. `None` is a server that cannot say.
    let mode = if args.paste {
        backend.procs(pane)?.context.and_then(|c| c.bracketed_paste)
    } else {
        None
    };
    let bracketed = mode == Some(true);
    if args.paste && !bracketed {
        eprintln!(
            "tty7: pane %{pane} {} bracketed paste, so the text went unframed — each line \
             break was sent as Enter",
            if mode.is_none() {
                "is on a server too old to say whether it has"
            } else {
                "has not switched on"
            }
        );
    }
    let wire = if args.paste {
        tty7_core::core::paste::paste_bytes(bytes, bracketed)
    } else {
        bytes.to_vec()
    };

    let mut already_wrote = false;
    if !wire.is_empty() {
        backend.send_input(pane, wire)?;
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
        backend.send_input(pane, key.bytes.clone())?;
        already_wrote = true;
    }
    report(
        "",
        json!({
            "pane": pane,
            "sent": match &payload {
                Payload::Typed(text) => Some(text.as_str()),
                Payload::None => Some(""),
                Payload::Piped { .. } => None,
            },
            "source": source,
            "bytes": bytes.len(),
            "paste": args.paste,
            // Whether the paste went framed, and what the pane said about the
            // mode — `null` when nothing was asked or the server cannot say.
            "bracketed": bracketed,
            "bracketed_paste_mode": mode,
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

fn exec(args: ExecArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let line = args.cmd.join(" ");
    if line.trim().is_empty() {
        bail!("exec needs a command after `--`");
    }
    // The line is typed, so every byte of it goes through the shell's line
    // editor: a newline would submit half of it and a Tab would complete. One
    // line of printable text is the only thing that runs as written — the same
    // test the GUI applies before it types a line rather than pasting it.
    if line.chars().any(char::is_control) {
        bail!(
            "exec types one line at the shell's prompt, and this one holds a control \
             character (a newline, a tab, an escape) the line editor would act on — join \
             the commands with `;` or `&&`, or put them in a script and exec that"
        );
    }
    let mut wire = line.clone().into_bytes();
    wire.push(b'\r');
    let started = std::time::Instant::now();
    let run = backend.exec(pane, wire, args.timeout.map(Duration::from_secs))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // The caller's own pane is the one pane that can never be at a prompt
    // while this runs: its shell is busy running `tty7 exec`.
    let own = ctx
        .pane
        .as_deref()
        .and_then(|p| address::parse_pane(p).ok())
        == Some(pane);
    match run.end {
        ExecEnd::NoIntegration => bail!(
            "pane %{pane} has sent no shell-integration prompt marks, so nothing would say \
             when a command there ends — exec needs tty7's shell integration loaded in the \
             pane's shell (`tty7 doctor`); without it, `send … --enter` and \
             `wait --until free` are the way"
        ),
        ExecEnd::Busy if own => bail!(
            "pane %{pane} is this shell, and it is busy running this very command — exec \
             into another pane"
        ),
        ExecEnd::Busy => bail!(
            "pane %{pane} is not at a shell prompt — something is already running there. \
             `tty7 wait %{pane} --until free` waits for it"
        ),
        _ => {}
    }

    let text = if args.raw {
        String::from_utf8_lossy(&run.output).into_owned()
    } else {
        screen::render(&[crate::backend::CaptureSegment {
            size: run.size,
            bytes: run.output.clone(),
        }])
    };
    if run.dropped > 0 {
        eprintln!(
            "tty7: exec %{pane}: the first {} bytes of output were dropped to keep the rest \
             in memory — the pane's own scrollback may still hold them",
            run.dropped
        );
    }
    let (code, known, timed_out) = match run.end {
        ExecEnd::Finished {
            exit: Some(code),
            ran: true,
        } => (code, true, false),
        ExecEnd::Finished {
            exit: None,
            ran: true,
        } => {
            eprintln!("tty7: {EXIT_CODE_UNKNOWN}");
            (1, false, false)
        }
        ExecEnd::Finished { ran: false, .. } => {
            eprintln!(
                "tty7: pane %{pane}: the shell came back to its prompt without running the \
                 line — a syntax error, most likely; what it printed is the output"
            );
            (1, false, false)
        }
        ExecEnd::TimedOut { started: true } => {
            eprintln!(
                "tty7: pane %{pane}: still running after {}s — timed out, and left running \
                 (`tty7 send %{pane} --key C-c` interrupts it)",
                args.timeout.unwrap_or_default()
            );
            (124, false, true)
        }
        ExecEnd::TimedOut { started: false } => {
            eprintln!(
                "tty7: pane %{pane}: timed out after {}s, and the shell never started the \
                 line — it is most likely waiting on a continuation prompt (an unclosed \
                 quote or bracket); `tty7 send %{pane} --key C-c` abandons it",
                args.timeout.unwrap_or_default()
            );
            (124, false, true)
        }
        ExecEnd::PaneExited(_) => {
            eprintln!("tty7: pane %{pane} exited before its shell came back to a prompt");
            (1, false, false)
        }
        ExecEnd::NoIntegration | ExecEnd::Busy => unreachable!("refused above"),
    };
    // `-q` keeps the exit code and drops the output, which is what the global
    // flag already does to any report.
    Ok(Outcome::Exit(
        code,
        Report {
            human: text.clone(),
            json: json!({
                "pane": pane,
                "command": line,
                "exit": code,
                "exit_code_known": known,
                "timed_out": timed_out,
                "pane_exited": matches!(run.end, ExecEnd::PaneExited(_)),
                "output": text,
                "bytes": run.output.len(),
                "elapsed_ms": elapsed_ms,
            }),
        },
    ))
}

fn capture(args: CaptureArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let segments = backend.capture(pane, args.scrollback)?;
    // How much the replay actually carried, counted before anything renders or
    // trims it. An empty answer used to be one fact — "nothing came back" — and
    // a caller could not tell a blank pane from a capture that lost its bytes
    // on the way (#841). With this beside it the two read differently: zero
    // bytes is a pane that printed nothing, and bytes with no text is a screen
    // whose content did not survive the grid.
    let replayed: usize = segments.iter().map(|segment| segment.bytes.len()).sum();
    // Raw is the default and stays byte-for-byte what the daemon stored, joined
    // in replay order; `--plain` hands the same bytes to a grid instead. Either
    // way `--json` carries whatever was printed, so a caller reads one field.
    let rendered = if args.plain {
        screen::render(&segments)
    } else {
        let bytes: Vec<u8> = segments.into_iter().flat_map(|s| s.bytes).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    // Said once, on stderr, where it cannot corrupt stdout or the JSON line —
    // and reported as the observation it is, not as a diagnosis. Only `--plain`
    // can reach it: lossy UTF-8 never turns bytes into an empty string, so the
    // raw form's text is empty exactly when the replay was.
    if rendered.is_empty() && replayed > 0 {
        eprintln!(
            "tty7: capture %{pane}: {replayed} bytes replayed and the grid they \
             drive is blank — this empty result is the pane's screen, not a \
             capture that came back short"
        );
    }
    // After the note, not before: a tail is a view of the answer, and whether
    // the answer itself was blank is a fact about the pane either way.
    let text = match args.tail {
        Some(keep) => last_lines(&rendered, keep as usize),
        None => rendered,
    };
    report(
        text.clone(),
        json!({ "pane": pane, "text": text, "bytes": replayed }),
    )
}

/// The last `keep` lines of `text`, counted the way `tail -n` counts them.
///
/// A trailing newline terminates the last line rather than opening an empty
/// one, so `tail -n 1` of `"a\nb\n"` is `"b\n"` and not `""`. Splitting on
/// `\n` alone leaves the `\r` of a CRLF attached to the line it ended, which
/// is what the raw form is supposed to hand back byte-for-byte.
fn last_lines(text: &str, keep: usize) -> String {
    let (body, trailer) = match text.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (text, ""),
    };
    let start = body
        .rmatch_indices('\n')
        .nth(keep.saturating_sub(1))
        .map(|(at, _)| at + 1)
        .unwrap_or(0);
    format!("{}{trailer}", &body[start..])
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
                "hibernated": tab.hibernated,
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
    adopt: Option<&str>,
    ctx: &Context,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let (id, pane, cwd) = match adopt {
        Some(spec) => adopt_pane(spec, explicit, cwd, ctx, &machine, backend)?,
        None => {
            let id = resolve_ws(explicit, ctx, &machine)?;
            let pane = backend.spawn_shell(id, cwd.clone())?;
            (id, pane, cwd)
        }
    };
    let tab = match backend.control(ControlRequest::TabCreate {
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
        ReplyOk::TabTree(tab) => *tab,
        other => bail!("the server answered TabCreate with {other:?}"),
    };
    report(
        format!("%{pane}"),
        json!({ "tab": tab.id.to_string(), "pane": pane }),
    )
}

/// Works out which workspace a pane that is already running goes into, and what
/// to seed the tab around it with.
///
/// The seed is rebuilt from the **live pane registry**, not from the tree, and
/// that is the whole design of this verb. `tab_close` retains the panes it
/// orphaned out of `m.panes` at the same moment it drops the tab, so by the
/// time anyone wants a pane back the tree has forgotten its record — its cwd,
/// its title, the shell it was started with. The registry still has the pane,
/// because the pane is still running; it is the only place left that knows
/// anything about it.
///
/// What the registry does not carry is `ssh_spec`, `agent` or `shell`, so a
/// re-homed pane is seeded without them. That costs nothing while the shell
/// lives — the tab is a view onto a pty that is already there — and only shows
/// up if the pane later dies and something tries to restore it from the seed.
/// Reconstructing those from a running pty is a separate problem; a tab you can
/// see and close beats a shell nobody can reach.
fn adopt_pane(
    spec: &str,
    explicit: Option<&str>,
    cwd: Option<String>,
    ctx: &Context,
    machine: &Machine,
    backend: &mut dyn Backend,
) -> Result<(WorkspaceId, u64, Option<String>)> {
    let pane = address::parse_pane(spec)?;
    let running = backend.list_panes()?;
    let info = running
        .iter()
        .find(|info| info.pane_id == pane)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no pane %{pane} is running on this machine — \
                 `tty7 pane ls --all` lists every pane the server holds"
            )
        })?;
    if let Ok(holder) = resolve::workspace_of_pane(machine, pane) {
        bail!(
            "%{pane} is already in a tab of workspace {} — `tty7 pane split` adds \
             to that tab, and `tty7 tab new --pane` is for panes no tab holds",
            resolve::short_id(&holder.id)
        );
    }
    // A pane the tree still knows nothing about, addressed with no workspace,
    // goes back to the one it was spawned for: that is what `pane ls --all`
    // prints as its owner, and the shell being recovered from is by definition
    // not inside tty7, so `$TTY7_WS` is not going to answer here.
    let id = match (explicit, ctx.ws.as_deref()) {
        (None, None) => owner_of(info, machine).ok_or_else(|| {
            anyhow::anyhow!(
                "%{pane} does not name a workspace that still exists — \
                 say which one to re-home it into: `tty7 tab new <workspace> --pane %{pane}`"
            )
        })?,
        _ => resolve_ws(explicit, ctx, machine)?,
    };
    // The recorded cwd is a courtesy for a later restore, not something the
    // running shell is moved to; an explicit `--cwd` overrides it.
    let cwd = cwd.or_else(|| {
        info.cwd
            .as_ref()
            .map(|dir| dir.display().to_string())
            .filter(|dir| !dir.is_empty())
    });
    Ok((id, pane, cwd))
}

/// The workspace a pane was spawned for, if it is still on this machine.
fn owner_of(
    info: &tty7_core::daemon::protocol::PaneInfo,
    machine: &Machine,
) -> Option<WorkspaceId> {
    let owner = info.owner.as_deref()?;
    machine
        .workspaces
        .iter()
        .find(|ws| ws.id.to_string() == owner)
        .map(|ws| ws.id)
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
                    "hibernated": tab.hibernated,
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
            "\n{orphans} pane(s) held by no workspace — `tty7 tab new --pane %<id>` puts one \
             back in a tab, `tty7 pane close %<id>` stops one, `tty7 pane close --orphans` \
             stops all of them\n"
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

    /// How many polls in a row must fail to determine freeness before the wait
    /// calls it structural and stops.
    ///
    /// One is not enough: a pane whose `ssh` handshake is still in flight has
    /// no far-side prompt mark yet and no local tree worth reading, and that
    /// window is shorter than a poll. Two consecutive misses is one `--interval`
    /// of grace — long enough for the transient, short enough that a wait with
    /// no `--timeout` at all still ends rather than hanging on a question this
    /// machine cannot answer.
    const UNKNOWN_POLLS: u32 = 2;

    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    // `checked_add` rather than `+`: an absurd `--timeout` must not panic.
    let deadline = args
        .timeout
        .and_then(|t| Instant::now().checked_add(Duration::from_secs(t)));
    let interval = Duration::from_millis(args.interval);
    let watch_free = args.until.contains(&WaitState::Free);
    // `free` is the only state this wait computes rather than reads. When it is
    // also the only thing that could still answer, an undeterminable freeness
    // ends the wait instead of riding out the deadline — `exit` does not count,
    // because it arrives on its own whether it was asked for or not.
    let free_is_the_only_hope = watch_free
        && args
            .until
            .iter()
            .all(|s| matches!(s, WaitState::Free | WaitState::Exit));
    let mut baseline: Option<Cursor> = None;
    // Sticky: has the pane been seen running something since the wait began?
    // This is `--changed`'s edge for `free` — see the flag's own comment.
    let mut seen_busy = false;
    // Why freeness could not be read, and for how many polls running. Cleared
    // the moment a verdict does arrive: a wait that timed out on a pane it
    // could read perfectly well by then must not blame the handshake it
    // watched go by.
    let mut unknown: Option<String> = None;
    let mut unknown_polls: u32 = 0;
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

        // `free` is a fact about the pane, not the agent ladder, so it is asked
        // separately, only when requested, and only once the ladder has failed
        // to answer. A state the caller listed is their answer: overwriting a
        // real `waiting` with a process-tree fact would strand a pane whose
        // depth-0 process *is* the agent (see `pane_freeness`), where the tree
        // reads free for the whole turn.
        if watch_free && current != WaitState::Exit && !args.until.contains(&current) {
            match pane_freeness(backend, pane)? {
                Freeness::Free => {
                    current = WaitState::Free;
                    changed = seen_busy;
                    (unknown, unknown_polls) = (None, 0);
                }
                Freeness::Busy => {
                    seen_busy = true;
                    (unknown, unknown_polls) = (None, 0);
                }
                // Not `seen_busy`: "we could not look" is not "something was
                // running", and letting it set that flag was what suppressed
                // the one hint that would have pointed at the gap (#840).
                Freeness::Unknown(why) => {
                    unknown_polls += 1;
                    unknown = Some(why);
                }
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

        // Nothing here can answer, and `free` was the only thing left that
        // could. Say so now: polling harder cannot turn "we cannot see the far
        // side" into a verdict, and a wait with no `--timeout` would otherwise
        // sit on this question until the caller killed it.
        if free_is_the_only_hope && unknown_polls >= UNKNOWN_POLLS {
            let why = unknown.as_deref().unwrap_or("no reason recorded");
            let human = format!("pane %{pane}: cannot determine whether it is free — {why}");
            eprintln!("tty7: pane %{pane}: cannot determine whether it is free");
            let session = entry.as_ref().map(|e| &e.state);
            return Ok(Outcome::Exit(
                1,
                Report {
                    human,
                    // The success path's shape, so a consumer written against
                    // it does not find its fields missing on the one branch it
                    // wrote error handling for (#589) — plus the reason, which
                    // is the only thing this branch actually knows.
                    json: json!({
                        "pane": pane,
                        "status": WaitState::Unknown.name(),
                        "matched": false,
                        "stale": !changed,
                        "free_unknown": why,
                        "activity": session.map(|s| s.activity),
                        "message": session.and_then(|s| s.message.clone()),
                        "session_id": session.and_then(|s| s.session_id.clone()),
                    }),
                },
            ));
        }

        if deadline.is_some_and(|d| Instant::now() >= d) {
            // 124 = the `timeout(1)` convention: "gave up", distinct from
            // both success and error, so orchestration scripts can branch.
            let mut human = format!("pane %{pane}: still {} — timed out", current.name());
            // A wait for agent states that never move is the shape of both
            // "there is no agent here" and "the agent's hooks are missing",
            // and neither is visible from a timeout alone. Say which door to
            // try rather than leaving the caller to poll harder.
            //
            // Only for a caller who has not already tried that door. Sending
            // someone back to `--until free` when `--until free` is what just
            // timed out is the part of this message that cost an agent a
            // session (#840); when they did ask, the hint below carries the
            // reason freeness never answered instead.
            if current == WaitState::NoAgent && !watch_free {
                human.push_str(
                    "\nnothing is reporting agent status in this pane — for a plain command \
                     wait `--until free`, and for an agent check `tty7 agents` for a missing \
                     status hook",
                );
            }
            // Freeness was asked for and never came back with a verdict. This
            // is the whole answer to "why did nothing happen for --timeout
            // seconds", so it goes first among the `free` hints.
            if let Some(why) = &unknown {
                human.push_str(&format!(
                    "\ncould not determine whether this pane is free — {why}"
                ));
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
                            // Present only when `free` was asked for and never
                            // resolved. A script that reads it knows the
                            // timeout says nothing about the pane.
                            "free_unknown": unknown,
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
/// Three answers, not two. `Busy` and `Unknown` used to be the same `false`,
/// and that is the whole of #840: on a pane where the question is structurally
/// unanswerable the wait read "still busy" on every poll, rode the full
/// `--timeout`, and then recommended the flag that had just failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Freeness {
    Free,
    Busy,
    /// Nothing here can answer. The string is the reason, written for the
    /// caller: it is what the wait prints instead of a timeout.
    Unknown(String),
}

/// Read the pane's freeness.
///
/// Two sources, and which one leads depends on where the pane's session is.
///
/// **The process tree**, for a pane whose pty is on this machine. Depth, not
/// count: the pane's own shell sits at depth 0 and everything it launched hangs
/// below, so "nothing deeper than the shell" holds however many shells the pane
/// ended up with, and does not have to guess at process names. It is also the
/// portable question — Windows has no foreground process group to ask about, so
/// `ProcEntry::foreground` is never true there. What it cannot see, both by
/// construction: a pane whose depth-0 process *is* the command — which is what
/// `tty7 run` spawns — reads free for as long as it runs, and a backgrounded
/// job keeps a pane busy after the foreground command is long gone.
///
/// **The shell's own OSC 133 marks**, which are the only thing that can speak
/// for a pane that is the near end of a connection. There the local tree
/// describes the tunnel: a native-SSH pane has no local tree at all, and a pane
/// running `ssh` has one whose depth-1 process is the `ssh` itself, busy for as
/// long as you are logged in. A prompt mark on such a pane can only have come
/// from the far shell — the near one cannot be at a prompt while the connection
/// owns its pty — so it is both trustworthy and the only evidence available.
///
/// Where they disagree on a local pane, a prompt mark outranks a deeper
/// process, because the process drawing that prompt is a shell: a `sudo -i`, a
/// nested `bash`, an `ssh` on a platform where the daemon cannot name it as
/// remote. It does not work the other way round — the absence of a mark proves
/// nothing, and the tree keeps the verdict there.
fn pane_freeness(backend: &mut dyn Backend, pane: u64) -> Result<Freeness> {
    let answer = backend.procs(pane)?;
    let bare_tree = !answer.procs.is_empty() && answer.procs.iter().all(|p| p.depth == 0);
    let Some(ctx) = &answer.context else {
        // A daemon from before the field existed. It can only be answering for
        // a pty of its own, so the tree is all there was and all there is.
        return Ok(match (bare_tree, answer.procs.is_empty()) {
            (true, _) => Freeness::Free,
            (false, false) => Freeness::Busy,
            (false, true) => Freeness::Unknown(
                "this server is too old to say where the pane's session lives, and its \
                 process tree came back empty"
                    .into(),
            ),
        });
    };

    let remote = ctx.remote.as_ref();
    if remote.is_some() || !ctx.local_pty {
        let whereabouts = match remote {
            Some(r) => format!("connected to {}", r.target),
            // A pane with no pty here and no context to name: the daemon knows
            // the session is elsewhere without knowing where.
            None => "not backed by a pty on this machine".to_string(),
        };
        return Ok(match ctx.at_prompt {
            // Only the far shell can be at a prompt while the near end is busy
            // holding the connection open.
            Some(true) => Freeness::Free,
            // The connection is over and the near shell is bare again — the
            // remote context just has not been re-probed yet.
            _ if ctx.local_pty && bare_tree => Freeness::Free,
            // Once the far side has proved it reports, its silence means work.
            Some(false) if ctx.remote_prompt_seen => Freeness::Busy,
            _ => Freeness::Unknown(format!(
                "this pane is {whereabouts}, so its local process tree describes this end of \
                 the connection, not what is running on the far one — and the far shell has \
                 sent no prompt mark, so nothing here can tell an idle remote prompt from a \
                 running remote command. Install tty7's shell integration on the remote host, \
                 or wait on something observable from here (`--until exit`, or an agent status)"
            )),
        });
    }

    // A local pane. The tree leads; a prompt mark only ever adds to it.
    if bare_tree || ctx.at_prompt == Some(true) {
        return Ok(Freeness::Free);
    }
    if !answer.procs.is_empty() || ctx.at_prompt == Some(false) {
        return Ok(Freeness::Busy);
    }
    Ok(Freeness::Unknown(
        "the pane's process tree came back empty and its shell has sent no prompt marks, so \
         there is nothing here to read freeness off — check `tty7 procs` on this pane"
            .into(),
    ))
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

    /// The recovery verb from #716. The pane is running and no tab holds it;
    /// the tab is built around it and no new shell is started.
    #[test]
    fn tab_new_with_a_pane_re_homes_an_orphan_instead_of_spawning() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].clone();
        let mut orphan = pane_info(37, Some(&api.id.to_string()));
        orphan.cwd = Some("C:\\work".into());
        backend.registry = vec![orphan];
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(37))));

        let out = run_cli(
            &["tty7", "tab", "new", "--pane", "%37"],
            &Context::default(),
            &mut backend,
        );

        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabCreate {
                workspace: api.id,
                at: None,
                pane: PaneSeed {
                    pane: 37,
                    // Rebuilt from the registry: the tree dropped this pane's
                    // record when the tab holding it closed.
                    cwd: Some("C:\\work".into()),
                    ssh_spec: None,
                    agent: None,
                    shell: None,
                },
                tab: None,
            },
            "with no workspace named, the pane goes back to the one it was spawned for"
        );
        assert!(
            backend.spawned.is_empty(),
            "re-homing must not start a second shell — the point is the one still running"
        );
        assert_eq!(human(out), "%37");
    }

    #[test]
    fn tab_new_with_a_pane_takes_an_explicit_workspace_and_cwd() {
        let mut backend = mock();
        let web = backend.machine.workspaces[1].id;
        backend.registry = vec![pane_info(37, None)];
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(37))));

        run_cli(
            &[
                "tty7", "tab", "new", "web", "--pane", "%37", "--cwd", "C:\\else",
            ],
            &Context::default(),
            &mut backend,
        );

        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabCreate {
                workspace: web,
                at: None,
                pane: PaneSeed {
                    pane: 37,
                    cwd: Some("C:\\else".into()),
                    ssh_spec: None,
                    agent: None,
                    shell: None,
                },
                tab: None,
            },
            "a pane with no owner still re-homes wherever it is told to"
        );
    }

    #[test]
    fn tab_new_refuses_a_pane_that_is_not_running() {
        let mut backend = mock();
        let error = execute(
            cli(&["tty7", "tab", "new", "api", "--pane", "%99"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("a pane the server does not hold cannot be re-homed");
        assert!(
            error.to_string().contains("no pane %99 is running"),
            "{error:#}"
        );
        assert!(
            !backend
                .control_calls
                .iter()
                .any(|call| matches!(call, ControlRequest::TabCreate { .. })),
            "and nothing is written to the tree"
        );
    }

    /// A pane a tab already holds is not an orphan, and putting it in a second
    /// tab would leave the tree with one pane in two places.
    #[test]
    fn tab_new_refuses_a_pane_a_tab_already_holds() {
        let mut backend = mock();
        backend.registry = vec![pane_info(2, None)];
        let error = execute(
            cli(&["tty7", "tab", "new", "api", "--pane", "%2"]),
            &Context::default(),
            &mut backend,
        )
        .expect_err("%2 is in a tab of api");
        assert!(error.to_string().contains("already in a tab"), "{error:#}");
    }

    /// The listing is where an orphan is found, so it is where the way out of
    /// being one has to be written down.
    #[test]
    fn pane_ls_all_points_at_the_way_back_as_well_as_the_way_out() {
        let mut backend = mock();
        backend.registry = vec![pane_info(37, None)];
        let out = human(run_cli(
            &["tty7", "pane", "ls", "--all"],
            &Context::default(),
            &mut backend,
        ));
        assert!(out.contains("tty7 tab new --pane %<id>"), "{out}");
        assert!(out.contains("tty7 pane close --orphans"), "{out}");
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
    fn capture_tail_keeps_the_last_lines_of_either_form() {
        let mut backend = mock();
        let replay = b"one\r\ntwo\r\nthree\r\nfour\r\n";
        backend.capture_segments = vec![segment(replay)];

        let plain = human(run_cli(
            &["tty7", "capture", "%2", "--plain", "--tail", "2"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(plain, "three\nfour");

        let raw = human(run_cli(
            &["tty7", "capture", "%2", "--tail", "2"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(
            raw, "three\r\nfour\r\n",
            "the raw form still hands back the pane's own bytes, CR included"
        );

        // More lines asked for than exist is the whole answer, not an error —
        // `tail -n 99` of a three-line file is the file.
        let all = human(run_cli(
            &["tty7", "capture", "%2", "--plain", "--tail", "99"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(all, "one\ntwo\nthree\nfour");

        // And `--json` reports the tail it printed, over the byte count of the
        // whole replay: the two together are what say a tail was taken.
        let tailed = json_of(run_cli(
            &["tty7", "capture", "%2", "--plain", "--tail", "1"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(tailed["text"], json!("four"));
        assert_eq!(tailed["bytes"], json!(replay.len()));
    }

    #[test]
    fn a_tail_counts_lines_the_way_tail_does() {
        // A trailing newline ends the last line rather than opening an empty
        // one, which is the difference between `tail -n 1` answering "b" and
        // answering nothing at all.
        assert_eq!(last_lines("a\nb\n", 1), "b\n");
        assert_eq!(last_lines("a\nb", 1), "b");
        assert_eq!(last_lines("a\nb\n", 2), "a\nb\n");
        assert_eq!(last_lines("a\nb\n", 9), "a\nb\n");
        assert_eq!(last_lines("", 3), "");
        assert_eq!(last_lines("\n", 1), "\n");
        assert_eq!(
            last_lines("keep\n\n\n", 2),
            "\n\n",
            "blank lines are lines; a tail is not a filter"
        );
    }

    #[test]
    fn capture_json_counts_the_bytes_the_replay_carried() {
        // The whole point of the field: `text` is empty in both of these, and
        // only `bytes` says which of the two happened. The first is a pane that
        // printed nothing; the second printed a screenful and then cleared it,
        // so the bytes are real and the grid they drive is blank.
        let mut backend = mock();
        let blank = json_of(run_cli(
            &["tty7", "capture", "%2", "--plain"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(blank["text"], json!(""));
        assert_eq!(blank["bytes"], json!(0));

        let cleared = b"\x1b[2J\x1b[3J\x1b[H";
        backend.capture_segments = vec![segment(cleared)];
        let wiped = json_of(run_cli(
            &["tty7", "capture", "%2", "--plain"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(wiped["text"], json!(""));
        assert_eq!(
            wiped["bytes"],
            json!(cleared.len()),
            "an empty render has to be told apart from an empty replay"
        );

        // And the count is of the replay, not of what came out of the grid:
        // escapes are bytes the pane produced even though no text survives them.
        let coloured_bytes = b"\x1b[31mred\x1b[0m\r\n";
        backend.capture_segments = vec![segment(coloured_bytes)];
        let coloured = json_of(run_cli(
            &["tty7", "capture", "%2", "--plain"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(coloured["text"], json!("red"));
        assert_eq!(coloured["bytes"], json!(coloured_bytes.len()));
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
            probe: Default::default(),
            context: Some(local_context(None)),
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
            probe: Default::default(),
            context: Some(local_context(None)),
        }
    }

    /// A pane whose pty is on this machine, optionally with a shell that is
    /// reporting prompt marks.
    fn local_context(at_prompt: Option<bool>) -> tty7_core::daemon::protocol::PaneContext {
        tty7_core::daemon::protocol::PaneContext {
            remote: None,
            local_pty: true,
            at_prompt,
            remote_prompt_seen: false,
            bracketed_paste: None,
        }
    }

    /// A pane whose shell is running `ssh`: the local tree has the connection
    /// in it, and everything that matters is on the far end.
    fn ssh_procs(
        at_prompt: Option<bool>,
        remote_prompt_seen: bool,
    ) -> tty7_core::daemon::protocol::PaneProcs {
        use tty7_core::daemon::protocol::{RemoteContext, RemoteKind};
        tty7_core::daemon::protocol::PaneProcs {
            procs: vec![
                proc_entry(100, "zsh", 0, false),
                proc_entry(101, "ssh", 1, true),
            ],
            ports: Vec::new(),
            probe: Default::default(),
            context: Some(tty7_core::daemon::protocol::PaneContext {
                remote: Some(RemoteContext {
                    kind: RemoteKind::Ssh,
                    argv: vec!["ssh".into(), "build-box".into()],
                    target: "build-box".into(),
                }),
                local_pty: true,
                at_prompt,
                remote_prompt_seen,
                bracketed_paste: None,
            }),
        }
    }

    /// A pane routed to a remote tty7 daemon: no pty on this machine at all,
    /// so the process list is empty by construction rather than by failure.
    fn routed_procs(at_prompt: Option<bool>) -> tty7_core::daemon::protocol::PaneProcs {
        use tty7_core::daemon::protocol::{RemoteContext, RemoteKind};
        tty7_core::daemon::protocol::PaneProcs {
            procs: Vec::new(),
            ports: Vec::new(),
            probe: Default::default(),
            context: Some(tty7_core::daemon::protocol::PaneContext {
                remote: Some(RemoteContext {
                    kind: RemoteKind::NativeSsh,
                    argv: Vec::new(),
                    target: "me@build-box".into(),
                }),
                local_pty: false,
                at_prompt,
                remote_prompt_seen: at_prompt == Some(true),
                bracketed_paste: None,
            }),
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
    ///
    /// A bare `PaneProcs` is also what a server from before `context` existed
    /// sends, so this pins the fallback: no context means the tree was all
    /// there ever was, and an empty one is still "we could not look".
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

        // Without a deadline to hide behind it says so rather than spinning.
        let mut backend = mock();
        for _ in 0..4 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend.procs_reply = tty7_core::daemon::protocol::PaneProcs::default();
        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "free", "--interval", "50"]),
            &Context::default(),
            &mut backend,
        )
        .expect("an undeterminable pane is an exit code, not an error");
        let Outcome::Exit(1, r) = out else {
            panic!("expected exit 1 with a reason, got {out:?}");
        };
        assert_eq!(r.json["status"], "unknown");
        assert!(
            r.json["free_unknown"]
                .as_str()
                .is_some_and(|w| w.contains("too old")),
            "an old server's silence is named as such: {:?}",
            r.json["free_unknown"]
        );
    }

    /// The heart of #840. A pane sitting at an idle prompt over `ssh` has a
    /// local process tree that is busy for as long as you are logged in — the
    /// `ssh` itself — so the tree can never report the pane free. The far
    /// shell's own prompt mark can, and it is trustworthy precisely because
    /// the near shell cannot be at a prompt while the connection holds its pty.
    #[test]
    fn wait_free_answers_from_the_far_shells_prompt_on_an_ssh_pane() {
        let mut backend = mock();
        for _ in 0..3 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        // The tree is identical on all three polls — `ssh` at depth 1 — and
        // only the marks move.
        backend
            .procs_replies
            .push_back(ssh_procs(Some(false), true));
        backend
            .procs_replies
            .push_back(ssh_procs(Some(false), true));
        backend.procs_replies.push_back(ssh_procs(Some(true), true));

        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--until", "free", "--interval", "50"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "free");
        assert_eq!(json["matched"], true);
        assert_eq!(json["stale"], false, "we watched the remote command finish");
    }

    /// A pane routed to a remote daemon has no local tree at all. Its far
    /// shell's prompt mark is the entire answer, and an empty `procs` beside
    /// it must not read as either "free" or "busy".
    #[test]
    fn wait_free_reads_a_routed_panes_prompt_mark() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        backend.procs_reply = routed_procs(Some(true));

        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--until", "free"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "free");
        assert_eq!(json["matched"], true);
    }

    /// The other half of #840: when the far shell sends no marks there is
    /// nothing on this machine that can tell an idle remote prompt from a
    /// running remote command. Saying so — and saying it without a `--timeout`
    /// to hide behind — beats polling until the caller gives up.
    #[test]
    fn wait_free_refuses_to_guess_on_a_remote_pane_with_no_integration() {
        let mut backend = mock();
        for _ in 0..4 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend.procs_reply = ssh_procs(Some(false), false);

        // Deliberately no `--timeout`: the old code would have polled here
        // until something killed it.
        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "free", "--interval", "50"]),
            &Context::default(),
            &mut backend,
        )
        .expect("an undeterminable pane is an exit code, not an error");
        let Outcome::Exit(1, r) = out else {
            panic!("expected exit 1 with a reason, got {out:?}");
        };
        assert_eq!(r.json["status"], "unknown");
        assert_eq!(r.json["matched"], false);
        let why = r.json["free_unknown"].as_str().expect("a recorded reason");
        assert!(
            why.contains("build-box") && why.contains("shell integration"),
            "the reason names the host and the thing that is missing: {why}"
        );
        assert_eq!(
            backend.procs_calls.len(),
            2,
            "one poll of grace for a handshake in flight, then it stops"
        );
    }

    /// A structurally undeterminable `free` must not cancel a wait that has
    /// another state still able to answer — the agent ladder is read from a
    /// different source and knows nothing about the far shell.
    #[test]
    fn wait_unknown_free_does_not_cancel_a_wait_on_an_agent_state() {
        use tty7_core::core::cli_agent::AgentStatus;
        let mut backend = mock();
        for _ in 0..3 {
            backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        }
        backend
            .replies
            .push_back(ReplyOk::AgentStates(vec![agent_state(
                3,
                AgentStatus::Done,
            )]));
        backend.procs_reply = routed_procs(None);

        let json = json_of(run_cli(
            &[
                "tty7",
                "wait",
                "%3",
                "--until",
                "done,free",
                "--interval",
                "50",
            ],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(
            json["status"], "done",
            "the ladder answered while `free` could not"
        );
    }

    /// The insult on top of the injury: the timeout hint used to send a caller
    /// to `--until free` when `--until free` was what had just timed out.
    #[test]
    fn wait_timeout_does_not_recommend_the_flag_that_just_failed() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        backend.procs_reply = ssh_procs(Some(false), false);

        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "free", "--timeout", "0"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        let Outcome::Exit(124, r) = out else {
            panic!("expected exit 124, got {out:?}");
        };
        assert!(
            !r.human.contains("--until free"),
            "it must not recommend the flag it was given: {}",
            r.human
        );
        assert!(
            r.human
                .contains("could not determine whether this pane is free"),
            "it must say what it could not determine: {}",
            r.human
        );
        assert_eq!(
            r.json["free_unknown"]
                .as_str()
                .map(|s| s.contains("build-box")),
            Some(true),
            "the JSON carries the reason too"
        );
    }

    /// `no-agent` still points at `--until free` for the caller who has not
    /// tried it — that hint is the right one, it was only wrong when repeated
    /// back at someone who already used it.
    #[test]
    fn wait_timeout_still_points_an_agentless_wait_at_free() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));

        let out = execute(
            cli(&["tty7", "wait", "%3", "--until", "done", "--timeout", "0"]),
            &Context::default(),
            &mut backend,
        )
        .expect("a timeout is an exit code, not an error");
        let Outcome::Exit(124, r) = out else {
            panic!("expected exit 124, got {out:?}");
        };
        assert!(r.human.contains("--until free"), "{}", r.human);
    }

    /// A prompt mark outranks a deeper process on a local pane too: whatever
    /// drew that prompt is a shell, not work. This is the shape a plain `ssh`
    /// takes on a platform where the daemon cannot name the pane as remote —
    /// Windows has no foreground process group to read the invocation from.
    #[test]
    fn wait_free_lets_a_prompt_mark_outrank_a_deeper_process() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let mut procs = busy_procs();
        procs.context = Some(local_context(Some(true)));
        backend.procs_reply = procs;

        let json = json_of(run_cli(
            &["tty7", "wait", "%3", "--until", "free"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(json["status"], "free");
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

    #[test]
    fn tab_ls_and_pane_ls_say_which_tabs_are_asleep() {
        let mut backend = mock();
        backend.machine.workspaces[0].tabs[1].hibernated = true;

        let Outcome::Report(tabs) = run_cli(
            &["tty7", "tab", "ls", "api"],
            &Context::default(),
            &mut backend,
        ) else {
            panic!("tab ls must report");
        };
        let tabs = tabs.json["tabs"].as_array().expect("tabs").clone();
        assert_eq!(tabs[0]["hibernated"], false);
        assert_eq!(tabs[1]["hibernated"], true);

        let Outcome::Report(panes) = run_cli(
            &["tty7", "pane", "ls", "api"],
            &Context::default(),
            &mut backend,
        ) else {
            panic!("pane ls must report");
        };
        let panes = panes.json["panes"].as_array().expect("panes").clone();
        let asleep: Vec<u64> = panes
            .iter()
            .filter(|p| p["hibernated"] == true)
            .map(|p| p["pane"].as_u64().unwrap())
            .collect();
        assert_eq!(asleep, vec![2, 3]);
    }

    fn exec_run(end: ExecEnd, output: &[u8]) -> crate::exec::ExecRun {
        crate::exec::ExecRun {
            end,
            output: output.to_vec(),
            dropped: 0,
            size: segment(b"").size,
        }
    }

    fn exec_outcome(args: &[&str], ctx: &Context, backend: &mut MockBackend) -> (i32, Report) {
        match execute(cli(args), ctx, backend) {
            Ok(Outcome::Exit(code, report)) => (code, report),
            other => panic!("exec stands in for the command, so it exits: {other:?}"),
        }
    }

    #[test]
    fn exec_types_one_line_and_passes_the_exit_code_through() {
        let mut backend = mock();
        backend.exec_run = exec_run(
            ExecEnd::Finished {
                exit: Some(3),
                ran: true,
            },
            b"built\r\n",
        );
        let (code, report) = exec_outcome(
            &[
                "tty7",
                "exec",
                "%1",
                "--raw",
                "--timeout",
                "30",
                "--",
                "make",
                "&&",
                "ls",
            ],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(code, 3, "the command's own code, not ours");
        assert_eq!(
            backend.execs,
            vec![(1, b"make && ls\r".to_vec(), Some(Duration::from_secs(30)))],
            "the words are one shell line, typed and submitted"
        );
        assert_eq!(report.human, "built\r\n");
        assert_eq!(report.json["exit"], 3);
        assert_eq!(report.json["exit_code_known"], true);
        assert_eq!(report.json["command"], "make && ls");
        assert_eq!(report.json["output"], "built\r\n");
        assert_eq!(report.json["timed_out"], false);
        // Nothing was sent anywhere but through the exec seam.
        assert!(backend.sent.is_empty());
    }

    #[test]
    fn exec_renders_the_output_the_way_capture_plain_does() {
        let mut backend = mock();
        backend.exec_run = exec_run(
            ExecEnd::Finished {
                exit: Some(0),
                ran: true,
            },
            b"\x1b[31mred\x1b[0m\r\nprogress 10%\rprogress 100%\r\n",
        );
        let (code, report) = exec_outcome(
            &["tty7", "exec", "%1", "--", "x"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(code, 0);
        assert!(!report.human.contains('\u{1b}'), "{:?}", report.human);
        assert!(report.human.contains("red"), "{:?}", report.human);
        assert!(report.human.contains("progress 100%"), "{:?}", report.human);
        assert!(
            !report.human.contains("progress 10%\n"),
            "{:?}",
            report.human
        );
    }

    #[test]
    fn exec_refuses_a_pane_it_cannot_follow_before_typing_anything() {
        // The backend sends nothing on these; the verb has to turn them into
        // an error that says what to do instead of an exit code that says
        // nothing.
        let mut backend = mock();
        backend.exec_run = exec_run(ExecEnd::NoIntegration, b"");
        let err = execute(
            cli(&["tty7", "exec", "%1", "--", "ls"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("shell-integration"), "{err}");
        assert!(err.contains("wait --until free"), "{err}");

        let mut backend = mock();
        backend.exec_run = exec_run(ExecEnd::Busy, b"");
        let err = execute(
            cli(&["tty7", "exec", "%1", "--", "ls"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not at a shell prompt"), "{err}");

        // The caller's own pane can only ever be busy — running this.
        let ctx = Context {
            pane: Some("%1".into()),
            ..Context::default()
        };
        let err = execute(cli(&["tty7", "exec", "--", "ls"]), &ctx, &mut backend)
            .unwrap_err()
            .to_string();
        assert!(err.contains("this shell"), "{err}");
    }

    #[test]
    fn exec_refuses_a_line_the_editor_would_act_on() {
        for line in ["make\nrm -rf build", "ls\tx", "a\u{1b}b"] {
            let mut backend = mock();
            let err = execute(
                cli(&["tty7", "exec", "%1", "--", line]),
                &Context::default(),
                &mut backend,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("control"), "{line:?}: {err}");
            assert!(backend.execs.is_empty(), "nothing may be typed: {line:?}");
        }
    }

    #[test]
    fn exec_reports_every_way_it_can_end_without_an_exit_code() {
        let cases = [
            (ExecEnd::TimedOut { started: true }, 124, true, false),
            (ExecEnd::TimedOut { started: false }, 124, true, false),
            (ExecEnd::PaneExited(Some(0)), 1, false, true),
            (
                ExecEnd::Finished {
                    exit: None,
                    ran: true,
                },
                1,
                false,
                false,
            ),
            (
                ExecEnd::Finished {
                    exit: None,
                    ran: false,
                },
                1,
                false,
                false,
            ),
        ];
        for (end, want, timed_out, pane_exited) in cases {
            let mut backend = mock();
            backend.exec_run = exec_run(end.clone(), b"partial\r\n");
            let (code, report) = exec_outcome(
                &["tty7", "exec", "%1", "--raw", "--timeout", "1", "--", "x"],
                &Context::default(),
                &mut backend,
            );
            assert_eq!(code, want, "{end:?}");
            assert_eq!(report.json["exit_code_known"], false, "{end:?}");
            assert_eq!(report.json["timed_out"], timed_out, "{end:?}");
            assert_eq!(report.json["pane_exited"], pane_exited, "{end:?}");
            assert_eq!(
                report.human, "partial\r\n",
                "what did arrive is still the answer: {end:?}"
            );
        }
    }

    fn procs_with_paste_mode(mode: Option<bool>) -> tty7_core::daemon::protocol::PaneProcs {
        tty7_core::daemon::protocol::PaneProcs {
            context: Some(tty7_core::daemon::protocol::PaneContext {
                local_pty: true,
                bracketed_paste: mode,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn send_from_file_sends_the_bytes_verbatim_and_never_echoes_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, b"s3cr\\et\n\xff").unwrap();
        let mut backend = mock();
        let json = json_of(run_cli(
            &["tty7", "send", "%1", "--from-file", path.to_str().unwrap()],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(backend.sent, vec![(1, b"s3cr\\et\n\xff".to_vec())]);
        assert!(
            json["sent"].is_null(),
            "the secret must not be echoed: {json}"
        );
        assert!(!json.to_string().contains("s3cr"), "{json}");
        assert_eq!(json["source"], "file");
        assert_eq!(json["bytes"], 9);
        assert!(
            backend.procs_calls.is_empty(),
            "without --paste the pane is not asked anything"
        );

        // Keys still follow the payload, the way they follow typed text.
        let mut backend = mock();
        run_cli(
            &[
                "tty7",
                "send",
                "%1",
                "--from-file",
                path.to_str().unwrap(),
                "--enter",
            ],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.sent,
            vec![(1, b"s3cr\\et\n\xff".to_vec()), (1, b"\r".to_vec())]
        );
    }

    #[test]
    fn send_from_file_takes_its_positional_as_an_address_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload");
        std::fs::write(&path, b"x").unwrap();
        let mut backend = mock();
        let err = execute(
            cli(&[
                "tty7",
                "send",
                "echo",
                "--from-file",
                path.to_str().unwrap(),
            ]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("has to be the pane"), "{err}");
        assert!(backend.sent.is_empty(), "nothing may be typed");

        // With no address it is this pane, as it is for text.
        let ctx = Context {
            pane: Some("%3".into()),
            ..Context::default()
        };
        run_cli(
            &["tty7", "send", "--from-file", path.to_str().unwrap()],
            &ctx,
            &mut backend,
        );
        assert_eq!(backend.sent, vec![(3, b"x".to_vec())]);
    }

    #[test]
    fn send_paste_frames_only_for_a_pane_that_switched_bracketed_paste_on() {
        let mut backend = mock();
        backend.procs_reply = procs_with_paste_mode(Some(true));
        let json = json_of(run_cli(
            &[
                "tty7",
                "send",
                "%1",
                "a\r\nb\x1b[201~c",
                "--paste",
                "--enter",
            ],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(
            backend.sent,
            vec![
                (1, b"\x1b[200~a\nb[201~c\x1b[201~".to_vec()),
                (1, b"\r".to_vec())
            ],
            "the GUI's framing: CRLF folded, ESC stripped so the text cannot close the paste"
        );
        assert_eq!(json["bracketed"], true);
        assert_eq!(json["bracketed_paste_mode"], true);

        // Off: brackets would reach the program as keystrokes, so the text goes
        // the way a paste into such a pane always has — a CR per line break.
        let mut backend = mock();
        backend.procs_reply = procs_with_paste_mode(Some(false));
        let json = json_of(run_cli(
            &["tty7", "send", "%1", "a\nb", "--paste"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(backend.sent, vec![(1, b"a\rb".to_vec())]);
        assert_eq!(json["bracketed"], false);
        assert_eq!(json["bracketed_paste_mode"], false);

        // A server that cannot say is not taken as a yes.
        let mut backend = mock();
        backend.procs_reply = procs_with_paste_mode(None);
        let json = json_of(run_cli(
            &["tty7", "send", "%1", "a\nb", "--paste"],
            &Context::default(),
            &mut backend,
        ));
        assert_eq!(backend.sent, vec![(1, b"a\rb".to_vec())]);
        assert!(json["bracketed_paste_mode"].is_null(), "{json}");
    }

    #[test]
    fn send_paste_needs_something_to_paste() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "%1", "--paste", "--enter"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--paste needs text"), "{err}");
        assert!(backend.sent.is_empty());
    }
}
