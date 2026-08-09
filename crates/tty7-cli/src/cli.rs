use clap::{ArgGroup, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "tty7",
    version,
    about = "A terminal workbench, from the command line",
    long_about = "One name, three artifacts: this CLI, the tty7 GUI (tty7-app), and\n\
                  tty7-server.\n\n\
                  The GUI and this CLI are both clients of the server on each machine —\n\
                  neither needs the other to be running. This CLI is built for coding\n\
                  agents: every verb is non-interactive, and --json makes the output\n\
                  machine-readable.\n\n\
                  An agent reaches the server through this binary, not through a wire\n\
                  protocol of its own. Inside a tty7 shell, $TTY7_CONFIG_DIR points at\n\
                  the server, and $TTY7_PANE / $TTY7_WS let the address-taking verbs run\n\
                  with no address given."
)]
pub struct Cli {
    #[arg(
        short = 'm',
        long,
        global = true,
        value_name = "MACHINE",
        help = "Route to that machine over the local server's existing link"
    )]
    pub machine: Option<String>,

    #[arg(
        long,
        global = true,
        help = "Machine-readable output, one JSON object per command"
    )]
    pub json: bool,

    #[arg(short, long, global = true, help = "Quiet mode: no output on success")]
    pub quiet: bool,

    #[arg(
        value_name = "PATH",
        help = "Launch or activate the GUI, opening a new tab at PATH if given"
    )]
    pub path: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "This machine on one screen (= tty7 ws ls)")]
    Ls,

    #[command(about = "Start a pane running a command, stream it, pass through the exit code")]
    Run(RunArgs),

    #[command(about = "Create a workspace and its first tab, print the id")]
    New {
        #[arg(value_name = "PATH")]
        path: Option<String>,
        #[arg(
            long,
            help = "Also open a window on it, if a GUI is running on this machine"
        )]
        open: bool,
    },

    #[command(about = "Split a pane (= tty7 pane split)")]
    Split(SplitArgs),

    #[command(about = "Type text into a pane")]
    Send(SendArgs),

    #[command(
        about = "Print a pane's output — as text with `--plain`, otherwise with its ANSI \
                 escapes intact, decoded as UTF-8 (invalid bytes become U+FFFD): the \
                 newest scrollback segment by default"
    )]
    Capture(CaptureArgs),

    #[command(about = "Processes and listening ports inside a pane")]
    Procs {
        #[arg(value_name = "%PANE")]
        target: Option<String>,
    },

    #[command(about = "Every agent on this machine: running, waiting for a reply, idle")]
    Agents,

    #[command(
        about = "Block until a pane's agent needs input, finishes its turn, or the pane \
                 exits — the orchestration primitive: `tty7 wait %3 && tty7 capture %3 --plain`"
    )]
    Wait(WaitArgs),

    #[command(about = "Stream server events, one per line (NDJSON with --json)")]
    Events,

    #[command(about = "Server health at a glance (= tty7 server status)")]
    Status,

    #[command(
        about = "Check this install: socket, dialect, config, versions, hooks, links, context"
    )]
    Doctor,

    #[command(
        subcommand,
        about = "Workspaces: the named trees of tabs and panes the server keeps alive"
    )]
    Ws(WsCmd),

    #[command(subcommand, about = "Tabs within a workspace (@N addresses)")]
    Tab(TabCmd),

    #[command(subcommand, about = "Panes: the terminals themselves (%N addresses)")]
    Pane(PaneCmd),

    #[command(
        subcommand,
        about = "Machines: this one plus every remote the server has a link to"
    )]
    Machine(MachineCmd),

    #[command(subcommand, about = "The tty7-server process on this machine")]
    Server(ServerCmd),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, help = "Keep the pane after the command exits")]
    pub keep: bool,

    #[arg(long, value_name = "DIR", help = "Working directory for the command")]
    pub cwd: Option<String>,

    #[arg(
        long,
        value_name = "WORKSPACE",
        help = "Sets the pane's TTY7_WS and, with --keep, the workspace the kept pane is \
                filed into; defaults to $TTY7_WS inside a tty7 shell"
    )]
    pub ws: Option<String>,

    #[arg(
        last = true,
        required = true,
        value_name = "CMD",
        help = "The command, after `--`: tty7 run -- cargo test"
    )]
    pub cmd: Vec<String>,
}

#[derive(Debug, Args)]
#[command(group = ArgGroup::new("axis").required(true))]
pub struct SplitArgs {
    #[arg(
        value_name = "%PANE",
        help = "Pane to split; defaults to $TTY7_PANE inside a tty7 shell"
    )]
    pub target: Option<String>,

    // Spelled out, with the one-letter forms kept as aliases: `--h` alone reads
    // like a typo for `-h`, and scripts written against it keep working.
    #[arg(
        long = "horizontal",
        visible_alias = "h",
        group = "axis",
        help = "Side by side: the new pane goes to the right"
    )]
    pub horizontal: bool,

    #[arg(
        long = "vertical",
        visible_alias = "v",
        group = "axis",
        help = "Stacked: the new pane goes below"
    )]
    pub vertical: bool,

    #[arg(
        long,
        default_value_t = 0.5,
        value_name = "RATIO",
        help = "Share kept by the existing pane"
    )]
    pub ratio: f32,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    #[arg(value_name = "%PANE|TEXT")]
    pub first: String,

    #[arg(value_name = "TEXT")]
    pub second: Option<String>,

    #[arg(long, help = "Press Enter after the text")]
    pub enter: bool,
}

/// One resting place a `wait` can end on. `Exit` is pane-level (the child
/// died or the pane is gone), the rest are the agent-status ladder the
/// server maintains from hook events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WaitState {
    Idle,
    Working,
    Waiting,
    Done,
    Exit,
}

impl WaitState {
    /// The wire spelling: what `--until` accepts and what `--json` reports.
    /// Written out rather than derived from the variant name so renaming a
    /// variant cannot silently rewrite the JSON contract.
    pub fn name(self) -> &'static str {
        match self {
            WaitState::Idle => "idle",
            WaitState::Working => "working",
            WaitState::Waiting => "waiting",
            WaitState::Done => "done",
            WaitState::Exit => "exit",
        }
    }
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    #[arg(
        value_name = "%PANE",
        help = "Pane to watch; defaults to $TTY7_PANE inside a tty7 shell"
    )]
    pub target: Option<String>,

    // The default is the two states worth waking for plus the one nobody can
    // wait past: "my peer needs input", "my peer finished", "my peer died".
    #[arg(
        long,
        value_name = "STATE,…",
        value_delimiter = ',',
        default_values = ["waiting", "done", "exit"],
        help = "States that end the wait"
    )]
    pub until: Vec<WaitState>,

    #[arg(
        long,
        value_name = "SECS",
        help = "Give up after this many seconds, with exit code 124 (the `timeout(1)` \
                convention, so scripts can tell \"not yet\" from \"broken\")"
    )]
    pub timeout: Option<u64>,

    // The agent status the server keeps is a level, not an edge: `done` stands
    // until the next turn starts, `waiting` until the agent moves again. So a
    // wait issued right after a `send` would answer with last turn's status
    // before the agent has even read the input. `--changed` refuses the state
    // the pane was already in, which is what a delegation loop wants on every
    // round after the first.
    #[arg(
        long,
        help = "Ignore the state the pane was already in — only wake on a state it \
                moved into after the wait began (use this after `send`)"
    )]
    pub changed: bool,

    #[arg(
        long,
        value_name = "MS",
        default_value_t = 500,
        value_parser = clap::value_parser!(u64).range(50..=3_600_000),
        help = "Poll every this many milliseconds"
    )]
    pub interval: u64,
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    #[arg(
        value_name = "%PANE",
        help = "Pane to read; defaults to $TTY7_PANE inside a tty7 shell"
    )]
    pub target: Option<String>,

    #[arg(
        long,
        help = "Print the whole scrollback ring; the ring splits into segments on resize, \
                and without this flag only the last segment is printed (for a \
                never-resized pane the two are identical)"
    )]
    pub scrollback: bool,

    #[arg(
        long,
        help = "Replay the output through a terminal and print the resulting text: no \
                escapes, wrapped lines rejoined, overwrites and cursor moves applied"
    )]
    pub plain: bool,
}

#[derive(Debug, Subcommand)]
pub enum WsCmd {
    #[command(about = "Every workspace on this machine")]
    Ls,

    #[command(about = "One workspace as a tree: tabs, splits, panes")]
    Tree {
        #[arg(value_name = "WORKSPACE")]
        ws: Option<String>,
    },

    #[command(about = "Create an empty workspace")]
    New {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },

    #[command(about = "Name or rename a workspace")]
    Rename {
        #[arg(value_name = "WORKSPACE")]
        ws: String,
        #[arg(value_name = "NAME")]
        name: String,
    },

    #[command(about = "End the shells, keep the layout (not implemented yet)")]
    Stop {
        #[arg(value_name = "WORKSPACE")]
        ws: String,
    },

    #[command(about = "Delete the workspace")]
    Rm {
        #[arg(value_name = "WORKSPACE")]
        ws: String,
    },

    #[command(about = "Become the workspace's controlling client")]
    Attach {
        #[arg(value_name = "WORKSPACE")]
        ws: String,
    },

    #[command(about = "Let go without interrupting anything")]
    Detach {
        #[arg(value_name = "WORKSPACE")]
        ws: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum TabCmd {
    #[command(about = "Tabs of a workspace")]
    Ls {
        #[arg(value_name = "WORKSPACE")]
        ws: Option<String>,
    },

    #[command(about = "Add a tab with a fresh shell")]
    New {
        #[arg(value_name = "WORKSPACE")]
        ws: Option<String>,
        #[arg(
            long,
            value_name = "DIR",
            help = "Working directory for the tab's shell"
        )]
        cwd: Option<String>,
    },

    #[command(about = "Close a tab and every pane in it")]
    Close {
        #[arg(value_name = "@TAB")]
        tab: String,
    },

    #[command(about = "Name or rename a tab")]
    Rename {
        #[arg(value_name = "@TAB")]
        tab: String,
        #[arg(value_name = "NAME")]
        name: String,
    },

    #[command(about = "Move a tab to another position in its workspace")]
    Move {
        #[arg(value_name = "@TAB")]
        tab: String,
        #[arg(value_name = "INDEX")]
        index: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum PaneCmd {
    #[command(about = "Panes of a workspace, or of the whole machine")]
    Ls {
        #[arg(value_name = "WORKSPACE")]
        ws: Option<String>,

        #[arg(
            long,
            conflicts_with = "ws",
            help = "Every pane the server runs, including ones no workspace holds \
                    (an interrupted `run` leaves those behind)"
        )]
        all: bool,
    },

    #[command(about = "Split a pane in two")]
    Split(SplitArgs),

    #[command(about = "Close a pane; its shell is hung up")]
    Close {
        #[arg(value_name = "%PANE")]
        target: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MachineCmd {
    #[command(about = "This machine and every linked remote")]
    Ls,

    #[command(about = "Open a link to a machine from an SSH profile (not implemented yet)")]
    Connect {
        #[arg(value_name = "PROFILE")]
        profile: String,
    },

    #[command(about = "Drop the link; the remote server keeps running (not implemented yet)")]
    Disconnect {
        #[arg(value_name = "MACHINE")]
        machine: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServerCmd {
    #[command(about = "Start the server on this machine")]
    Start,

    #[command(about = "Stop the server; sessions end")]
    Stop,

    #[command(about = "Stop, then start")]
    Restart,

    #[command(about = "Version, uptime, panes, links")]
    Status,

    #[command(about = "Tail the server log")]
    Logs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("this invocation is part of the documented grammar")
    }

    #[test]
    fn bare_tty7_is_the_gui_launcher() {
        let cli = parse(&["tty7"]);
        assert!(cli.command.is_none(), "no subcommand means launch the GUI");
        assert!(cli.path.is_none());
    }

    #[test]
    fn a_path_that_is_not_a_verb_still_launches_the_gui() {
        let cli = parse(&["tty7", "C:\\Users\\me\\proj"]);
        assert!(cli.command.is_none());
        assert_eq!(
            cli.path.as_deref(),
            Some(std::path::Path::new("C:\\Users\\me\\proj"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_gui_path_preserves_native_windows_arguments() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;

        let native_path =
            OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16]);
        let cli = Cli::try_parse_from([OsString::from("tty7"), native_path.clone()])
            .expect("PathBuf arguments accept native Windows strings");

        assert_eq!(cli.path, Some(std::path::PathBuf::from(native_path)));
    }

    #[test]
    fn every_top_level_verb_parses() {
        assert!(matches!(parse(&["tty7", "ls"]).command, Some(Command::Ls)));
        assert!(matches!(
            parse(&["tty7", "new"]).command,
            Some(Command::New { path: None, .. })
        ));
        assert!(matches!(
            parse(&["tty7", "new", "C:\\proj"]).command,
            Some(Command::New { path: Some(p), .. }) if p == "C:\\proj"
        ));
        assert!(matches!(
            parse(&["tty7", "agents"]).command,
            Some(Command::Agents)
        ));
        assert!(matches!(
            parse(&["tty7", "events"]).command,
            Some(Command::Events)
        ));
        assert!(matches!(
            parse(&["tty7", "status"]).command,
            Some(Command::Status)
        ));
        assert!(matches!(
            parse(&["tty7", "doctor"]).command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            parse(&["tty7", "procs", "%3"]).command,
            Some(Command::Procs { target: Some(t) }) if t == "%3"
        ));
    }

    #[test]
    fn run_takes_the_command_after_a_double_dash() {
        let cli = parse(&["tty7", "run", "--", "cargo", "test", "--keep"]);
        let Some(Command::Run(args)) = cli.command else {
            panic!("run did not parse");
        };
        assert_eq!(args.cmd, vec!["cargo", "test", "--keep"]);
        assert!(
            !args.keep,
            "--keep after -- belongs to the child, not to tty7"
        );

        let cli = parse(&["tty7", "run", "--keep", "--cwd", "C:\\proj", "--", "make"]);
        let Some(Command::Run(args)) = cli.command else {
            panic!("run did not parse");
        };
        assert!(args.keep);
        assert_eq!(args.cwd.as_deref(), Some("C:\\proj"));
        assert_eq!(args.cmd, vec!["make"]);
    }

    #[test]
    fn run_without_a_command_is_a_usage_error() {
        let err = Cli::try_parse_from(["tty7", "run"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn split_requires_exactly_one_axis() {
        let cli = parse(&["tty7", "split", "--v"]);
        let Some(Command::Split(args)) = cli.command else {
            panic!("split did not parse");
        };
        assert!(args.vertical && !args.horizontal);
        assert!(args.target.is_none(), "the pane comes from $TTY7_PANE");
        assert_eq!(args.ratio, 0.5);

        let cli = parse(&["tty7", "pane", "split", "%42", "--h", "--ratio", "0.3"]);
        let Some(Command::Pane(PaneCmd::Split(args))) = cli.command else {
            panic!("pane split did not parse");
        };
        assert!(args.horizontal);
        assert_eq!(args.target.as_deref(), Some("%42"));
        assert_eq!(args.ratio, 0.3);

        let err = Cli::try_parse_from(["tty7", "split", "%1"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::MissingRequiredArgument,
            "an axis must be chosen"
        );
        let err = Cli::try_parse_from(["tty7", "split", "--h", "--v"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::ArgumentConflict,
            "the two axes exclude each other"
        );
    }

    #[test]
    fn send_takes_an_optional_address_then_text() {
        let cli = parse(&["tty7", "send", "%42", "make -j8", "--enter"]);
        let Some(Command::Send(args)) = cli.command else {
            panic!("send did not parse");
        };
        assert_eq!(args.first, "%42");
        assert_eq!(args.second.as_deref(), Some("make -j8"));
        assert!(args.enter);

        let cli = parse(&["tty7", "send", "make -j8"]);
        let Some(Command::Send(args)) = cli.command else {
            panic!("send did not parse");
        };
        assert_eq!(args.first, "make -j8");
        assert!(args.second.is_none());
    }

    #[test]
    fn every_ws_verb_parses() {
        assert!(matches!(
            parse(&["tty7", "ws", "ls"]).command,
            Some(Command::Ws(WsCmd::Ls))
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "tree", "api"]).command,
            Some(Command::Ws(WsCmd::Tree { ws: Some(w) })) if w == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "new", "api"]).command,
            Some(Command::Ws(WsCmd::New { name: Some(n) })) if n == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "rename", "api", "web"]).command,
            Some(Command::Ws(WsCmd::Rename { ws, name })) if ws == "api" && name == "web"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "stop", "api"]).command,
            Some(Command::Ws(WsCmd::Stop { ws })) if ws == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "rm", "api"]).command,
            Some(Command::Ws(WsCmd::Rm { ws })) if ws == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "attach", "api"]).command,
            Some(Command::Ws(WsCmd::Attach { ws })) if ws == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "ws", "detach", "api"]).command,
            Some(Command::Ws(WsCmd::Detach { ws })) if ws == "api"
        ));
    }

    #[test]
    fn every_tab_verb_parses() {
        assert!(matches!(
            parse(&["tty7", "tab", "ls", "api"]).command,
            Some(Command::Tab(TabCmd::Ls { ws: Some(w) })) if w == "api"
        ));
        assert!(matches!(
            parse(&["tty7", "tab", "new", "api", "--cwd", "C:\\proj"]).command,
            Some(Command::Tab(TabCmd::New { ws: Some(w), cwd: Some(c) })) if w == "api" && c == "C:\\proj"
        ));
        assert!(matches!(
            parse(&["tty7", "tab", "close", "@7"]).command,
            Some(Command::Tab(TabCmd::Close { tab })) if tab == "@7"
        ));
        assert!(matches!(
            parse(&["tty7", "tab", "rename", "@7", "build"]).command,
            Some(Command::Tab(TabCmd::Rename { tab, name })) if tab == "@7" && name == "build"
        ));
        assert!(matches!(
            parse(&["tty7", "tab", "move", "@7", "0"]).command,
            Some(Command::Tab(TabCmd::Move { tab, index: 0 })) if tab == "@7"
        ));
    }

    #[test]
    fn every_pane_verb_parses() {
        assert!(matches!(
            parse(&["tty7", "pane", "ls"]).command,
            Some(Command::Pane(PaneCmd::Ls {
                ws: None,
                all: false
            }))
        ));
        assert!(matches!(
            parse(&["tty7", "pane", "close", "%9"]).command,
            Some(Command::Pane(PaneCmd::Close { target: Some(t) })) if t == "%9"
        ));
    }

    #[test]
    fn every_machine_and_server_verb_parses() {
        assert!(matches!(
            parse(&["tty7", "machine", "ls"]).command,
            Some(Command::Machine(MachineCmd::Ls))
        ));
        assert!(matches!(
            parse(&["tty7", "machine", "connect", "devbox"]).command,
            Some(Command::Machine(MachineCmd::Connect { profile })) if profile == "devbox"
        ));
        assert!(matches!(
            parse(&["tty7", "machine", "disconnect", "devbox"]).command,
            Some(Command::Machine(MachineCmd::Disconnect { machine })) if machine == "devbox"
        ));
        for (verb, want) in [
            ("start", ServerCmd::Start),
            ("stop", ServerCmd::Stop),
            ("restart", ServerCmd::Restart),
            ("status", ServerCmd::Status),
            ("logs", ServerCmd::Logs),
        ] {
            let cli = parse(&["tty7", "server", verb]);
            let Some(Command::Server(got)) = cli.command else {
                panic!("server {verb} did not parse");
            };
            assert_eq!(
                std::mem::discriminant(&got),
                std::mem::discriminant(&want),
                "server {verb} parsed as the wrong verb"
            );
        }
    }

    #[test]
    fn global_flags_are_accepted_anywhere() {
        let cli = parse(&["tty7", "-m", "devbox", "ls", "--json"]);
        assert_eq!(cli.machine.as_deref(), Some("devbox"));
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Command::Ls)));

        let cli = parse(&["tty7", "ws", "ls", "-q", "-m", "devbox"]);
        assert!(cli.quiet);
        assert_eq!(cli.machine.as_deref(), Some("devbox"));

        let cli = parse(&["tty7", "capture", "%3", "--scrollback", "--json"]);
        assert!(cli.json);
        let Some(Command::Capture(args)) = cli.command else {
            panic!("capture did not parse");
        };
        assert!(args.scrollback);
        assert!(!args.plain, "raw bytes stay the default");
        assert_eq!(args.target.as_deref(), Some("%3"));
    }

    #[test]
    fn capture_plain_is_its_own_flag_and_composes_with_scrollback() {
        // The two answer different questions — how much to replay, and whether
        // to replay it through a grid — so neither implies or excludes the other.
        let Some(Command::Capture(args)) = parse(&["tty7", "capture", "--plain"]).command else {
            panic!("capture did not parse");
        };
        assert!(args.plain && !args.scrollback);
        assert!(args.target.is_none(), "the pane comes from $TTY7_PANE");

        let Some(Command::Capture(args)) =
            parse(&["tty7", "capture", "%3", "--plain", "--scrollback"]).command
        else {
            panic!("capture did not parse");
        };
        assert!(args.plain && args.scrollback);
    }

    #[test]
    fn usage_errors_exit_with_code_2() {
        let err = Cli::try_parse_from(["tty7", "ws", "frobnicate"]).unwrap_err();
        assert_eq!(
            err.exit_code(),
            2,
            "clap's usage-error exit code is the CLI contract"
        );
        let err = Cli::try_parse_from(["tty7", "tab", "move", "@7", "not-a-number"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
