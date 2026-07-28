//! The command palette: a centered overlay (Cmd+P) that lists runnable
//! commands, fuzzy-filters them as you type, and runs the selected one.
//!
//! This module owns the palette's *data* — the command catalog and the
//! `ListDelegate` that filters it. The heavy lifting (search input, virtual
//! list, keyboard navigation, Enter/Esc handling) is supplied by
//! gpui-component's `list::ListState`, so we don't reimplement any of it.
//! [`PaletteView`] wraps that list with the overlay chrome (scrim + card) and
//! emits a [`PaletteEvent`] on confirm/dismiss; command *execution* lives in
//! `app.rs`, where it can touch `Tty7App`'s tab/pane operations.
//!
//! ## The naming grammar
//!
//! Every title in [`Command::base_commands`] follows one shape, because a
//! palette is read by scanning and a list written in three different styles
//! can't be scanned. The rules, in order of precedence:
//!
//! 1. **`Verb Object`** — "New Tab", "Split Right", "Clear Scrollback". The
//!    verb comes first because that is what the user is searching for.
//! 2. **`Namespace: Verb Object`** when the command belongs to an enumerable
//!    subsystem — `SSH:`, `Agent:`, `Right Panel:`. If one command in a group
//!    carries the prefix, *all* of them do; a single bare sibling (this list
//!    used to have "Reconnect SSH Session" sitting beside four `SSH:` rows) is
//!    what makes a namespace look accidental.
//! 3. **A trailing `…`** means "this asks for something else before it acts" —
//!    another list, a text field, a confirmation. Not "this opens a panel".
//! 4. **No `Toggle`.** A toggle names the mechanism; the user wants the result.
//!    Titles read "Hide Left Sidebar" or "Show Left Sidebar" depending on where
//!    the sidebar currently is, which also removes the guesswork about what a
//!    toggle would do from the current state.
//! 5. **Two commands that could be confused must not merely differ by a word.**
//!    "Toggle Tab Sidebar" and "Toggle Left Sidebar" were, respectively, moving
//!    the tab bar and collapsing the rail; they now read "Tab Bar: Move to Left
//!    Sidebar" and "Hide Left Sidebar".

use gpui::{
    App, Context, Entity, EventEmitter, MouseButton, MouseDownEvent, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, IndexPath, h_flex,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    v_flex,
};

use uuid::Uuid;

use crate::core::config::{Config, RightPanelTab, TabBarPosition};
use crate::core::ssh_profile::parse_quick_connect;

/// What a command actually does. Most variants map to an existing `Tty7App`
/// operation dispatched in `app.rs` (so it can touch tabs/panes); submenu
/// openers are handled inside [`PaletteView`] and never reach the host.
#[derive(Clone, PartialEq, Eq)]
pub enum CommandKind {
    NewTab,
    NewWorkspace,
    /// Submenu opener: swap the palette to the list of known workspaces.
    /// Handled inside `PaletteView`; never reaches the host.
    OpenWorkspacePicker,
    RenameWorkspace,
    /// Stop this window's workspace: kill its sessions and close the window,
    /// keeping the layout so it can be started again. The counterpart to
    /// closing a window, which only detaches.
    StopWorkspace,
    /// Stop it *and* discard the layout. The only irreversible one.
    DeleteWorkspace,
    SplitRight,
    SplitDown,
    ClosePane,
    // Tab operations that used to live only in the tab context menu, so the
    // palette could not reach what a right-click could.
    RenameTab,
    NewWorktreeTab,
    CloseOtherTabs,
    CloseTabsToTheRight,
    CopyWorkingDirectory,
    MarkTabUnread,
    /// Branch this tab's agent session into a second, independent one, opened
    /// in a new tab. The pane right-click menu offers the split placements;
    /// the palette's ask isn't a spatial one, so it takes the tab-level
    /// meaning.
    ForkAgentSession,
    CopyAgentSessionId,
    ResetFontSize,
    NextPane,
    PrevPane,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    SwapPaneNext,
    SwapPanePrev,
    NextTab,
    PrevTab,
    ToggleMaximizePane,
    ToggleFullscreen,
    ToggleTabSidebar,
    ToggleLeftPanel,
    ToggleRightPanel,
    /// Switch the right panel to a specific tab, opening it if it was closed —
    /// so the palette can land you on Changes without a toggle-then-click.
    ShowRightPanel(RightPanelTab),
    ClearTerminal,
    FindInTerminal,
    FindNext,
    FindPrevious,
    // The clipboard trio + Select All. Dispatched to the focused terminal, the
    // same actions the Edit menu and the right-click menu use.
    CopyText,
    CutText,
    PasteText,
    SelectAllText,
    ReopenClosedTab,
    OpenSettings,
    /// Settings, opened straight on its Keybindings section.
    ShowKeyboardShortcuts,
    /// Settings, opened straight on its About section.
    About,
    CheckForUpdates,
    OpenDocumentation,
    OpenDiscord,
    ReportIssue,
    Quit,
    RestartDaemon,
    /// Show the focused native-SSH pane's remote filesystem — the detail
    /// panel's Files tab, which browses over SFTP for a remote pane (WS5).
    ToggleSftp,
    /// Show the focused native-SSH pane's forwards in the detail panel's Info
    /// tab, add form open (WS4).
    ShowSshForwards,
    /// Toggle the code panel (file tree + editor overlay over the terminal).
    ToggleCodePanel,
    /// Reconnect a dead native-SSH pane in place (WS6, FR-E4).
    RestartSshSession,
    /// Send the focused pane's selection to a running CLI coding agent's pane
    /// as a prompt (build error → agent, the review-feed idea).
    SendSelectionToAgent,
    /// Send the repo's uncommitted `git diff` (from the focused pane's cwd) to
    /// a running CLI coding agent's pane as a review prompt.
    SendGitDiffToAgent,
    /// Opens the theme sub-list (a nested palette). Handled in `PaletteView`.
    OpenThemePicker,
    /// Opens a typed SSH connection sub-list. Handled in `PaletteView`.
    OpenSshConnectInput,
    /// Open a native SSH tab from a typed target/options line.
    OpenSshConnect(String),
    /// Apply the preset at this index in `presets::all()`. Emitted from the
    /// theme sub-list.
    SetTheme(usize),
    /// Switch to the tab at this index in `Tty7App::tabs`.
    ActivateTab(usize),
    /// Connect a saved SSH profile by id (over the native engine).
    ConnectSavedProfile(Uuid),
    /// Open the profile editor focused on this saved profile (⌘⏎ / → on a row).
    EditSavedProfile(Uuid),
    /// QuickConnect to a typed `user@host[:port]` target via the native path.
    QuickConnect(String),
    /// Open the profile editor pre-filled from a typed QuickConnect target
    /// ("save as profile" from a quick connect).
    SaveQuickConnect(String),
    /// Open the full-window SSH profile manager/editor page.
    OpenSshProfiles,
}

impl CommandKind {
    /// The "edit" counterpart of a connect-style command, for the ⌘⏎ / → gesture
    /// (PRD §6.2 ①). `None` for commands that have no editor.
    pub fn edit_variant(&self) -> Option<CommandKind> {
        match self {
            CommandKind::ConnectSavedProfile(id) => Some(CommandKind::EditSavedProfile(*id)),
            CommandKind::QuickConnect(s) => Some(CommandKind::SaveQuickConnect(s.clone())),
            _ => None,
        }
    }

    /// A stable key for [`Config::command_frecency`], or `None` for commands
    /// that aren't a repeatable "thing you run" — a specific tab index, a theme
    /// slot, a typed host. Recording those would fill the Recent group with
    /// entries that mean something different next launch.
    pub fn id(&self) -> Option<&'static str> {
        use CommandKind::*;
        Some(match self {
            NewTab => "new-tab",
            NewWorkspace => "new-workspace",
            OpenWorkspacePicker => "switch-workspace",
            RenameWorkspace => "rename-workspace",
            StopWorkspace => "stop-workspace",
            DeleteWorkspace => "delete-workspace",
            SplitRight => "split-right",
            SplitDown => "split-down",
            ClosePane => "close-pane",
            RenameTab => "rename-tab",
            NewWorktreeTab => "new-worktree-tab",
            CloseOtherTabs => "close-other-tabs",
            CloseTabsToTheRight => "close-tabs-right",
            CopyWorkingDirectory => "copy-cwd",
            MarkTabUnread => "mark-tab-unread",
            ForkAgentSession => "fork-agent-session",
            CopyAgentSessionId => "copy-agent-session-id",
            ResetFontSize => "reset-font-size",
            NextPane => "next-pane",
            PrevPane => "prev-pane",
            FocusPaneLeft => "focus-pane-left",
            FocusPaneRight => "focus-pane-right",
            FocusPaneUp => "focus-pane-up",
            FocusPaneDown => "focus-pane-down",
            ResizePaneLeft => "resize-pane-left",
            ResizePaneRight => "resize-pane-right",
            ResizePaneUp => "resize-pane-up",
            ResizePaneDown => "resize-pane-down",
            SwapPaneNext => "swap-pane-next",
            SwapPanePrev => "swap-pane-prev",
            NextTab => "next-tab",
            PrevTab => "prev-tab",
            ToggleMaximizePane => "zoom-pane",
            ToggleFullscreen => "full-screen",
            ToggleTabSidebar => "tab-bar-position",
            ToggleLeftPanel => "left-sidebar",
            ToggleRightPanel => "right-panel",
            ShowRightPanel(RightPanelTab::Info) => "right-panel-info",
            ShowRightPanel(RightPanelTab::Outline) => "right-panel-outline",
            ShowRightPanel(RightPanelTab::Changes) => "right-panel-changes",
            ShowRightPanel(RightPanelTab::Files) => "right-panel-files",
            ClearTerminal => "clear-scrollback",
            FindInTerminal => "find",
            FindNext => "find-next",
            FindPrevious => "find-previous",
            CopyText => "copy",
            CutText => "cut",
            PasteText => "paste",
            SelectAllText => "select-all",
            ReopenClosedTab => "reopen-closed-tab",
            OpenSettings => "settings",
            ShowKeyboardShortcuts => "keyboard-shortcuts",
            About => "about",
            CheckForUpdates => "check-for-updates",
            OpenDocumentation => "documentation",
            OpenDiscord => "discord",
            ReportIssue => "report-issue",
            Quit => "quit",
            RestartDaemon => "restart-daemon",
            ToggleSftp => "ssh-remote-files",
            ShowSshForwards => "ssh-port-forwarding",
            ToggleCodePanel => "code-panel",
            RestartSshSession => "ssh-reconnect",
            SendSelectionToAgent => "agent-send-selection",
            SendGitDiffToAgent => "agent-send-diff",
            OpenThemePicker => "change-theme",
            OpenSshConnectInput => "ssh-add-connection",
            OpenSshProfiles => "ssh-manage-profiles",
            // Instance-specific: a tab index, a theme slot, a profile id, a
            // typed host. Not stable across sessions, so not tracked.
            OpenSshConnect(_)
            | SetTheme(_)
            | ActivateTab(_)
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | QuickConnect(_)
            | SaveQuickConnect(_) => return None,
        })
    }

    /// The keystroke shown beside this command, as a config keyspec.
    ///
    /// Most commands resolve through the live keymap, so a user remap shows up
    /// here automatically. The clipboard trio and Select All are the exception:
    /// they're handled inline in `terminal::view::handle_cmd_shortcut` rather
    /// than as registered bindings (⌃C has to fall through to SIGINT with
    /// nothing selected, which a registered binding would swallow), so their
    /// chords are stated literally — the same thing the right-click menu does.
    fn key_spec(&self, cx: &App) -> Option<String> {
        use CommandKind::*;
        // Inline-handled chords, macOS-only: off macOS these live on Ctrl and
        // Ctrl+A / Ctrl+F keep their readline meaning, so advertising them
        // would be a lie.
        let inline =
            |spec: &str| -> Option<String> { cfg!(target_os = "macos").then(|| spec.to_string()) };
        match self {
            CopyText => return inline("secondary-c"),
            CutText => return inline("secondary-x"),
            PasteText => return inline("secondary-v"),
            SelectAllText => return inline("secondary-a"),
            _ => {}
        }
        let action = match self {
            NewTab => "NewTab",
            NewWorkspace => "NewWorkspace",
            RenameWorkspace => "RenameWorkspace",
            StopWorkspace => "StopWorkspace",
            DeleteWorkspace => "DeleteWorkspace",
            SplitRight => "SplitRight",
            SplitDown => "SplitDown",
            ClosePane => "CloseActiveTab",
            RenameTab => "RenameTab",
            NewWorktreeTab => "NewWorktreeTab",
            CloseOtherTabs => "CloseOtherTabs",
            CloseTabsToTheRight => "CloseTabsToTheRight",
            CopyWorkingDirectory => "CopyWorkingDirectory",
            MarkTabUnread => "MarkTabUnread",
            ForkAgentSession => "ForkAgentSession",
            CopyAgentSessionId => "CopyAgentSessionId",
            ResetFontSize => "ResetFontSize",
            NextPane => "FocusNextPane",
            PrevPane => "FocusPrevPane",
            FocusPaneLeft => "FocusPaneLeft",
            FocusPaneRight => "FocusPaneRight",
            FocusPaneUp => "FocusPaneUp",
            FocusPaneDown => "FocusPaneDown",
            ResizePaneLeft => "ResizePaneLeft",
            ResizePaneRight => "ResizePaneRight",
            ResizePaneUp => "ResizePaneUp",
            ResizePaneDown => "ResizePaneDown",
            SwapPaneNext => "SwapPaneNext",
            SwapPanePrev => "SwapPanePrev",
            NextTab => "NextTab",
            PrevTab => "PrevTab",
            ToggleMaximizePane => "ToggleMaximizePane",
            ToggleFullscreen => "ToggleFullscreen",
            ToggleTabSidebar => "ToggleTabSidebar",
            ToggleLeftPanel => "ToggleLeftPanel",
            ToggleRightPanel => "ToggleRightPanel",
            ShowRightPanel(tab) => match tab {
                RightPanelTab::Info => "ShowRightPanelInfo",
                RightPanelTab::Outline => "ShowRightPanelOutline",
                RightPanelTab::Changes => "ShowRightPanelChanges",
                RightPanelTab::Files => "ShowRightPanelFiles",
            },
            ClearTerminal => "ClearScrollback",
            // Previously grouped with the hint-less commands even though it has
            // shipped a default ⌘F for as long as the binding has existed — so
            // the one command whose shortcut users most want to learn was the
            // one the palette refused to teach.
            FindInTerminal => "FindInTerminal",
            FindNext => "FindNext",
            FindPrevious => "FindPrevious",
            ReopenClosedTab => "ReopenClosedTab",
            OpenSettings => "OpenSettings",
            ShowKeyboardShortcuts => "ShowKeyboardShortcuts",
            About => "About",
            CheckForUpdates => "CheckForUpdates",
            OpenDocumentation => "OpenDocumentation",
            OpenDiscord => "OpenDiscord",
            ReportIssue => "ReportIssue",
            Quit => "Quit",
            RestartDaemon => "RestartDaemon",
            ToggleSftp => "ToggleSftp",
            ShowSshForwards => "ShowSshForwards",
            ToggleCodePanel => "ToggleCodePanel",
            RestartSshSession => "RestartSshSession",
            OpenSshProfiles => "OpenSshProfiles",
            // No global binding, by design or by nature.
            CopyText
            | CutText
            | PasteText
            | SelectAllText
            | SendSelectionToAgent
            | SendGitDiffToAgent
            | OpenWorkspacePicker
            | OpenThemePicker
            | OpenSshConnectInput
            | OpenSshConnect(_)
            | SetTheme(_)
            | ActivateTab(_)
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | QuickConnect(_)
            | SaveQuickConnect(_) => return None,
        };
        crate::ui::keymap::effective_key(action, cx)
    }
}

/// The band a command is filed under in the unfiltered palette. Groups exist so
/// the resting list reads as a map of the app rather than 60 undifferentiated
/// rows; while a search is running they're dropped and the results rank purely
/// by match quality.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandGroup {
    TabsPanes,
    Workspaces,
    View,
    Terminal,
    Ssh,
    Agents,
    Application,
}

impl CommandGroup {
    /// Display order of the groups, which is roughly "how often you reach for
    /// this": the tab/pane verbs first, the app-level chores last.
    const ORDER: [CommandGroup; 7] = [
        CommandGroup::TabsPanes,
        CommandGroup::Workspaces,
        CommandGroup::View,
        CommandGroup::Terminal,
        CommandGroup::Ssh,
        CommandGroup::Agents,
        CommandGroup::Application,
    ];

    fn title(self) -> &'static str {
        match self {
            CommandGroup::TabsPanes => "Tabs & Panes",
            CommandGroup::Workspaces => "Workspaces",
            CommandGroup::View => "View",
            CommandGroup::Terminal => "Terminal",
            CommandGroup::Ssh => "SSH",
            CommandGroup::Agents => "Agents",
            CommandGroup::Application => "Application",
        }
    }
}

/// The chrome state the stateful titles ("Hide Left Sidebar", "Show Right
/// Panel") read, passed in by the window opening the palette.
///
/// Deliberately *not* read off `Config`: both of these are per-window state
/// living on `Tty7App`, and their config copies only record whichever window
/// toggled them last. Reading the config would label the row by another
/// window's rail — and clicking it would then do the opposite of what it said.
#[derive(Clone, Copy)]
pub struct ChromeState {
    /// This window's rail collapse flag (`Tty7App::sidebar_collapsed`), not the
    /// config's. Note this is the *toggle's* state, not whether the rail is on
    /// screen: on the home page there are no tabs to list, but the command
    /// still flips this flag, so the title has to describe that.
    pub rail_collapsed: bool,
    /// This window's `Tty7App::right_panel_visible`.
    pub right_panel_visible: bool,
}

/// A single palette entry: a label plus the action it triggers.
#[derive(Clone)]
pub struct Command {
    pub title: String,
    /// Optional dimmed secondary text on the right of the title (e.g. a saved
    /// profile's `user@host`, or `(~/.ssh/config)` for an alias).
    pub subtitle: Option<String>,
    pub kind: CommandKind,
    pub group: CommandGroup,
}

impl Command {
    pub fn new(title: impl Into<String>, kind: CommandKind) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            kind,
            // Overwritten by `base_commands`, which files every entry; the
            // default only matters for the sub-lists, which render ungrouped.
            group: CommandGroup::Application,
        }
    }

    /// Attach a dimmed subtitle rendered to the right of the title.
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// File this command under a group. The dynamic entries the host appends
    /// (saved SSH profiles, "Switch to Tab: …") have to say where they belong
    /// or they'd all land in the default band.
    pub fn in_group(mut self, group: CommandGroup) -> Self {
        self.group = group;
        self
    }

    /// The static commands available regardless of how many tabs exist. The
    /// caller appends the dynamic SSH-profile and "Switch to Tab: …" entries.
    ///
    /// Titles follow the grammar documented at the top of this module. Several
    /// are *stateful*: a command that flips something reads as the outcome it
    /// will produce right now ("Hide Left Sidebar" when the rail is out), which
    /// is why this needs `cx` and the calling window's [`ChromeState`].
    ///
    /// The held-key font zoom (⌘+/⌘−) is deliberately absent — stepping it needs
    /// a re-open per press, so it makes a poor palette citizen; only the
    /// one-shot Reset is worth a slot.
    pub fn base_commands(cx: &App, chrome: ChromeState) -> Vec<Command> {
        use CommandKind::*;
        let cfg = cx.global::<Config>();
        // The tab bar's side is a genuine app-wide setting, so it comes off the
        // config; the rail's collapse flag and the right panel's visibility do
        // not (see `ChromeState`).
        let tab_bar_left = cfg.tab_bar_position == TabBarPosition::Left;
        let sidebar_hidden = chrome.rail_collapsed || !tab_bar_left;
        let right_panel_open = chrome.right_panel_visible;

        let tabs = [
            Command::new("New Tab", NewTab),
            Command::new("New Worktree Tab", NewWorktreeTab)
                .with_subtitle("isolated checkout on a fresh branch"),
            Command::new("Rename Tab…", RenameTab),
            Command::new("Split Right", SplitRight),
            Command::new("Split Down", SplitDown),
            Command::new("Zoom Pane", ToggleMaximizePane),
            Command::new("Next Pane", NextPane),
            Command::new("Previous Pane", PrevPane),
            Command::new("Focus Pane Left", FocusPaneLeft),
            Command::new("Focus Pane Right", FocusPaneRight),
            Command::new("Focus Pane Up", FocusPaneUp),
            Command::new("Focus Pane Down", FocusPaneDown),
            Command::new("Resize Pane Left", ResizePaneLeft),
            Command::new("Resize Pane Right", ResizePaneRight),
            Command::new("Resize Pane Up", ResizePaneUp),
            Command::new("Resize Pane Down", ResizePaneDown),
            Command::new("Swap Pane Next", SwapPaneNext),
            Command::new("Swap Pane Previous", SwapPanePrev),
            Command::new("Next Tab", NextTab),
            Command::new("Previous Tab", PrevTab),
            Command::new("Copy Working Directory", CopyWorkingDirectory),
            Command::new("Copy Session ID", CopyAgentSessionId)
                .with_subtitle("the coding agent's own session id"),
            Command::new("Fork Session", ForkAgentSession)
                .with_subtitle("branch this agent session into a new tab"),
            Command::new("Mark Tab as Unread", MarkTabUnread),
            Command::new("Close Pane / Tab", ClosePane),
            Command::new("Close Other Tabs", CloseOtherTabs),
            Command::new("Close Tabs to the Right", CloseTabsToTheRight),
            Command::new("Reopen Closed Tab", ReopenClosedTab),
        ];

        let workspaces = [
            Command::new("New Workspace", NewWorkspace),
            Command::new("Switch Workspace…", OpenWorkspacePicker),
            Command::new("Rename Workspace…", RenameWorkspace),
            Command::new("Stop Workspace…", StopWorkspace)
                .with_subtitle("ends its sessions, keeps the layout"),
            Command::new("Delete Workspace…", DeleteWorkspace)
                .with_subtitle("ends its sessions and forgets the layout"),
        ];

        let view = [
            // Stateful titles: what the command will do from here, not the name
            // of the switch it throws.
            Command::new(
                if sidebar_hidden {
                    "Show Left Sidebar"
                } else {
                    "Hide Left Sidebar"
                },
                ToggleLeftPanel,
            ),
            Command::new(
                if right_panel_open {
                    "Hide Right Panel"
                } else {
                    "Show Right Panel"
                },
                ToggleRightPanel,
            ),
            Command::new("Show Code Panel", ToggleCodePanel),
            // Was "Toggle Tab Sidebar", one row away from "Toggle Left Sidebar"
            // and indistinguishable from it.
            Command::new(
                if tab_bar_left {
                    "Tab Bar: Move to Top"
                } else {
                    "Tab Bar: Move to Left Sidebar"
                },
                ToggleTabSidebar,
            ),
            Command::new("Right Panel: Info", ShowRightPanel(RightPanelTab::Info)),
            Command::new(
                "Right Panel: Outline",
                ShowRightPanel(RightPanelTab::Outline),
            ),
            Command::new(
                "Right Panel: Changes",
                ShowRightPanel(RightPanelTab::Changes),
            ),
            Command::new("Right Panel: Files", ShowRightPanel(RightPanelTab::Files)),
            Command::new("Change Theme…", OpenThemePicker),
            Command::new("Reset Font Size", ResetFontSize),
            Command::new("Enter Full Screen", ToggleFullscreen),
        ];

        let terminal = [
            // Was "Clear", which never said what it cleared.
            Command::new("Clear Scrollback", ClearTerminal),
            Command::new("Find in Terminal…", FindInTerminal),
            Command::new("Find Next", FindNext),
            Command::new("Find Previous", FindPrevious),
            Command::new("Copy", CopyText),
            Command::new("Cut", CutText),
            Command::new("Paste", PasteText),
            Command::new("Select All", SelectAllText),
        ];

        let ssh = [
            Command::new("SSH: Add Connection…", OpenSshConnectInput),
            Command::new("SSH: Manage Profiles…", OpenSshProfiles),
            // Was "Reconnect SSH Session" — the one bare sibling among five.
            Command::new("SSH: Reconnect Session", RestartSshSession),
            Command::new("SSH: Remote Files", ToggleSftp),
            Command::new("SSH: Port Forwarding", ShowSshForwards),
        ];

        let agents = [
            Command::new("Agent: Send Selection", SendSelectionToAgent)
                .with_subtitle("selection → running coding agent"),
            Command::new("Agent: Send Git Diff for Review", SendGitDiffToAgent)
                .with_subtitle("git diff → running coding agent"),
        ];

        let application = [
            // Was "Open Settings" while the menu bar, the tray and the home page
            // all said "Settings" — four names for one destination.
            Command::new("Settings…", OpenSettings),
            Command::new("Keyboard Shortcuts", ShowKeyboardShortcuts),
            Command::new("About tty7", About),
            Command::new("Check for Updates…", CheckForUpdates),
            Command::new("Documentation", OpenDocumentation),
            Command::new("Join the Discord", OpenDiscord),
            Command::new("Report an Issue…", ReportIssue),
            Command::new("Restart Daemon…", RestartDaemon)
                .with_subtitle("ends every running shell; layout is kept"),
            Command::new("Quit tty7", Quit).with_subtitle("sessions keep running"),
        ];

        let mut out = Vec::new();
        let mut push = |cmds: Vec<Command>, group: CommandGroup| {
            out.extend(cmds.into_iter().map(|c| c.in_group(group)));
        };
        push(tabs.into(), CommandGroup::TabsPanes);
        push(workspaces.into(), CommandGroup::Workspaces);
        push(view.into(), CommandGroup::View);
        push(terminal.into(), CommandGroup::Terminal);
        push(ssh.into(), CommandGroup::Ssh);
        push(agents.into(), CommandGroup::Agents);
        push(application.into(), CommandGroup::Application);
        out
    }

    /// The theme-picker sub-list: one entry per built-in preset, in the presets'
    /// display order. Confirming one emits `SetTheme(i)`, which applies that
    /// preset. The active theme is marked with a check so the list doubles as a
    /// "which theme am I on?" indicator.
    pub fn theme_commands(cx: &App) -> Vec<Command> {
        let active = crate::ui::theme::effective_preset_id(cx);
        crate::ui::presets::all(cx)
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let title = if p.id == active {
                    format!("{}  ✓", p.name)
                } else {
                    p.name.clone()
                };
                Command::new(title, CommandKind::SetTheme(i))
            })
            .collect()
    }

    fn ssh_connect_command(input: &str) -> Command {
        let title = if input.trim().is_empty() {
            "SSH: Add Connection…".to_string()
        } else {
            format!("SSH: Connect {}", input.trim())
        };
        Command::new(title, CommandKind::OpenSshConnect(input.to_string()))
    }
}

/// Score how well `query` matches `text`, or `None` when it doesn't match at
/// all. Higher is better; an empty query scores every candidate 0.
///
/// The rule is still "every character of the query appears in order", but the
/// old boolean version left results in catalog order, so typing `sr` put "New
/// Tab" (**s**plit… no — the first row whose letters happened to line up) above
/// "Split Right". Scoring adds what makes a palette feel like it read your
/// mind: matches on word boundaries and runs of adjacent characters count for
/// much more than letters scattered through a long title.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let needle: Vec<char> = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut qi = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    let mut prev_hit = false;
    for (i, ch) in hay.iter().enumerate() {
        if qi >= needle.len() {
            break;
        }
        if *ch != needle[qi] {
            prev_hit = false;
            run = 0;
            continue;
        }
        score += 1;
        // Start of the string, or of a word: "sr" → "**S**plit **R**ight" is
        // what the user meant, and it must outrank the same letters buried
        // mid-word somewhere else.
        let word_start = i == 0 || !hay[i - 1].is_alphanumeric();
        if word_start {
            score += 12;
        }
        if i == 0 {
            score += 10;
        }
        if prev_hit {
            run += 1;
            score += 6 + run.min(8);
        } else {
            run = 0;
        }
        prev_hit = true;
        qi += 1;
    }
    if qi < needle.len() {
        return None;
    }

    if hay == needle {
        score += 120;
    } else if hay.starts_with(&needle) {
        score += 50;
    }
    // Among equally good matches, prefer the shorter title: "Copy" should beat
    // "Copy Working Directory" for the query "copy".
    score -= (hay.len() as i32) / 6;
    Some(score)
}

/// The best score for a command against `query`: its title, or its subtitle at
/// a discount (a subtitle hit is a weaker signal of intent than a title hit,
/// but typing a hostname should still find the profile row it belongs to).
fn command_score(query: &str, cmd: &Command) -> Option<i32> {
    let title = fuzzy_score(query, &cmd.title);
    let subtitle = cmd
        .subtitle
        .as_deref()
        .and_then(|s| fuzzy_score(query, s))
        .map(|s| s / 2 - 25);
    match (title, subtitle) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// One rendered band of the list: an optional header plus its rows. A search
/// collapses everything into a single header-less section ranked by score.
#[derive(Clone)]
struct Section {
    title: Option<SharedString>,
    commands: Vec<Command>,
}

/// Feeds the command catalog to gpui-component's `ListState`. It keeps the full
/// catalog plus the sections matching the current query, re-filtering in
/// `perform_search` whenever the search input changes.
pub struct PaletteDelegate {
    /// The full catalog: static commands followed by per-tab switch entries.
    commands: Vec<Command>,
    /// Exactly what the list renders, in render order.
    sections: Vec<Section>,
    input: Option<PaletteInput>,
    /// Whether this is the root catalog: grouped when idle, and a query that
    /// parses as `user@host[:port]` injects live "Connect to …" / "Save … as
    /// profile" rows so QuickConnect shares the one entry box (PRD §6.2 ①).
    quick_connect_root: bool,
    /// Index of the highlighted row, mirrored from the list's own selection so
    /// `render_item` can mark it. `None` when nothing matches.
    selected: Option<IndexPath>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteInput {
    SshConnect,
}

impl PaletteDelegate {
    pub fn new(commands: Vec<Command>) -> Self {
        Self {
            sections: vec![Section {
                title: None,
                commands: commands.clone(),
            }],
            commands,
            input: None,
            quick_connect_root: false,
            selected: Some(IndexPath::default()),
        }
    }

    /// The root delegate: grouped headers while idle, QuickConnect rows on a
    /// host-like query.
    pub fn root(commands: Vec<Command>, cx: &App) -> Self {
        let mut this = Self {
            quick_connect_root: true,
            ..Self::new(commands)
        };
        this.sections = this.grouped_sections(cx);
        this
    }

    /// The idle (empty-query) layout: a Recent band built from
    /// [`Config::command_frecency`], then one band per [`CommandGroup`].
    ///
    /// Recent exists because the catalog's order is authored, not personal: the
    /// first screenful used to be whatever was typed first — four Focus Pane
    /// directions and four Resize Pane directions — while Change Theme and
    /// Settings sat below the fold.
    fn grouped_sections(&self, cx: &App) -> Vec<Section> {
        let cfg = cx.global::<Config>();
        let now = crate::core::config::unix_now();
        let mut sections = Vec::new();

        let mut recent: Vec<(f64, &Command)> = self
            .commands
            .iter()
            .filter_map(|c| {
                let id = c.kind.id()?;
                let usage = cfg.command_frecency.get(id)?;
                let score = usage.score(now);
                (score > 0.0).then_some((score, c))
            })
            .collect();
        recent.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        recent.truncate(RECENT_ROWS);
        if !recent.is_empty() {
            sections.push(Section {
                title: Some("Recent".into()),
                commands: recent.into_iter().map(|(_, c)| c.clone()).collect(),
            });
        }

        for group in CommandGroup::ORDER {
            let commands: Vec<Command> = self
                .commands
                .iter()
                .filter(|c| c.group == group)
                .cloned()
                .collect();
            if !commands.is_empty() {
                sections.push(Section {
                    title: Some(group.title().into()),
                    commands,
                });
            }
        }
        sections
    }

    /// The QuickConnect rows for a query at the root, if it parses as a target.
    ///
    /// Beyond parsing, the query must *look like* a connect target — contain
    /// `@`, `:` or `.` (`user@host`, `host:port`, an FQDN/IP; `ssh://` and
    /// bracketed IPv6 both carry a `:`). A bare word like "java" parses as a
    /// valid hostname too, but injecting these rows for every word would pin
    /// them above all command searches; bare short names keep the SSH Connect
    /// input as their path.
    fn quick_connect_commands(query: &str) -> Vec<Command> {
        if !query.contains(['@', ':', '.']) {
            return Vec::new();
        }
        match parse_quick_connect(query) {
            Some(_) => {
                let target = query.trim().to_string();
                vec![
                    Command::new(
                        format!("Connect to \"{target}\""),
                        CommandKind::QuickConnect(target.clone()),
                    ),
                    Command::new(
                        format!("Save \"{target}\" as profile…"),
                        CommandKind::SaveQuickConnect(target),
                    ),
                ]
            }
            None => Vec::new(),
        }
    }

    fn ssh_connect() -> Self {
        Self {
            commands: Vec::new(),
            sections: vec![Section {
                title: None,
                commands: vec![Command::ssh_connect_command("")],
            }],
            input: Some(PaletteInput::SshConnect),
            quick_connect_root: false,
            selected: Some(IndexPath::default()),
        }
    }

    /// The command kind at the given index path, if any. Called by `app.rs`
    /// when the list confirms a selection.
    pub fn command_at(&self, ix: IndexPath) -> Option<CommandKind> {
        self.sections
            .get(ix.section)?
            .commands
            .get(ix.row)
            .map(|c| c.kind.clone())
    }

    /// The currently highlighted command, if any (for the ⌘⏎ / → edit gesture).
    pub fn selected_command(&self) -> Option<CommandKind> {
        self.selected.and_then(|ix| self.command_at(ix))
    }

    /// The first selectable row, or `None` when nothing matched.
    fn first_row(&self) -> Option<IndexPath> {
        let section = self.sections.iter().position(|s| !s.commands.is_empty())?;
        Some(IndexPath::new(0).section(section))
    }
}

impl ListDelegate for PaletteDelegate {
    type Item = ListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        self.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.sections
            .get(section)
            .map(|s| s.commands.len())
            .unwrap_or(0)
    }

    /// Re-filter the catalog against the live query and reset the highlight to
    /// the first match.
    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        if let Some(PaletteInput::SshConnect) = self.input {
            self.sections = vec![Section {
                title: None,
                commands: vec![Command::ssh_connect_command(query)],
            }];
        } else if query.trim().is_empty() {
            // Idle: the grouped map of the app (root), or the sub-list as-is.
            self.sections = if self.quick_connect_root {
                self.grouped_sections(cx)
            } else {
                vec![Section {
                    title: None,
                    commands: self.commands.clone(),
                }]
            };
        } else {
            // Searching: one flat, header-less band ranked by match quality.
            // Headers would only get in the way of "type three letters, hit
            // Enter", and the ranking already puts the best row first.
            let mut scored: Vec<(i32, Command)> = self
                .commands
                .iter()
                .filter_map(|c| command_score(query, c).map(|s| (s, c.clone())))
                .collect();
            // Stable sort, so equal scores keep catalog order.
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            let mut commands: Vec<Command> = Vec::new();
            // At the root, a query that parses as a connect target leads with
            // QuickConnect rows (PRD §6.2 ①), above the ranked catalog.
            if self.quick_connect_root {
                commands.extend(Self::quick_connect_commands(query));
            }
            commands.extend(scored.into_iter().map(|(_, c)| c));
            self.sections = vec![Section {
                title: None,
                commands,
            }];
        }
        self.selected = self.first_row();
        Task::ready(())
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let title = self.sections.get(section)?.title.clone()?;
        Some(
            // Same fixed height as a row: the card's viewport is sized to a
            // whole number of `PALETTE_ROW_H` units, and a header of any other
            // height would leave the last visible row sliced by the card edge.
            h_flex()
                .h(px(PALETTE_ROW_H))
                .px(px(11.))
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        // A blank card reads as a hang. Name the miss and point at the one
        // thing this box does besides run commands.
        v_flex()
            .py_8()
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("No matching commands")
            .child(
                div()
                    .text_xs()
                    .child("Type user@host to connect over SSH instead."),
            )
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let cmd = self.sections.get(ix.section)?.commands.get(ix.row)?.clone();

        // Read the colours we need as Copy values, then release the theme borrow
        // so we can borrow `cx` again for the keybinding lookup below.
        let (kbd_bg, border, muted) = {
            let t = cx.theme();
            (t.secondary.opacity(0.6), t.border, t.muted_foreground)
        };

        // Shortcut hint: the effective keystroke for this command, rendered as
        // small keycaps on the right — the Raycast/VSCode convention that makes a
        // command palette feel professional and teaches the shortcut in passing.
        let keys = cmd
            .kind
            .key_spec(cx)
            .map(|spec| crate::ui::keymap::key_tokens(&spec));

        // Title, with an optional dimmed subtitle to its right (a profile's
        // `user@host`, or `(~/.ssh/config)` for an alias).
        let mut left = h_flex().items_center().gap_2().child(cmd.title.clone());
        if let Some(subtitle) = cmd.subtitle.clone() {
            left = left.child(div().text_xs().text_color(muted).child(subtitle));
        }

        let mut row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .child(left);
        // Editable rows (saved profiles, quick-connect) advertise the ⌘⏎ / →
        // edit gesture with a subtle trailing hint (PRD §6.2 ①).
        if cmd.kind.edit_variant().is_some() {
            row = row.child(div().text_xs().text_color(muted).child("→ edit"));
        }
        if let Some(tokens) = keys {
            row = row.child(h_flex().gap_1().children(tokens.into_iter().map(move |t| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(20.))
                    .h(px(20.))
                    .px_1()
                    .rounded_md()
                    .bg(kbd_bg)
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(t)
            })));
        }

        Some(
            // Keyed by section *and* row: with grouped sections a bare row index
            // repeats across bands, and duplicate element ids make the list
            // reuse the wrong row's state.
            ListItem::new(("palette-row", ix.section * 1000 + ix.row))
                .selected(Some(ix) == self.selected)
                // Fixed-height, dense rows (see `PALETTE_ROW_H`: the card's
                // list viewport is sized to a whole number of rows). The 5px
                // side margin + 6px radius turn the highlight into the same
                // inset pill the context menu / dropdown use.
                .h(px(PALETTE_ROW_H))
                .mx(px(5.))
                .rounded(px(6.))
                .text_sm()
                .child(row),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }
}

/// What the palette tells its host (`Tty7App`) when the user acts on it.
pub enum PaletteEvent {
    /// A command was chosen; the host should close the palette and run it.
    Confirm(CommandKind),
    /// The palette was dismissed (Esc or click outside) with nothing chosen.
    Dismiss,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteMenu {
    Root,
    Theme,
    SshConnect,
}

/// The command palette as a self-contained view. It owns the `ListState`
/// (search input, fuzzy filter, keyboard nav) and the scrim/card overlay
/// chrome, and emits a [`PaletteEvent`] when the user confirms or dismisses.
/// The host builds the root catalog and executes the chosen command, so this
/// view stays ignorant of what most commands do. Submenus are two-level flows
/// the palette drives internally: picking an opener swaps the list to that
/// catalog, Esc steps back to the root, and only the final command reaches the
/// host.
pub struct PaletteView {
    list: Entity<ListState<PaletteDelegate>>,
    /// The root catalog, kept so Esc inside a sub-list can restore it instead
    /// of dismissing the whole palette.
    root: Vec<Command>,
    /// Which catalog the palette is currently showing.
    menu: PaletteMenu,
    /// Keeps the *current* list's event subscription alive. Replaced on every
    /// [`show`](Self::show) (root ⇄ sub-list) so it always targets the live list.
    _sub: Subscription,
}

impl PaletteView {
    pub fn new(commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let list = Self::build_root_list(commands.clone(), window, cx);
        let _sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            root: commands,
            menu: PaletteMenu::Root,
            _sub,
        }
    }

    /// Build a fresh `ListState` for `commands` and focus its search input.
    /// gpui-component supplies the search box, fuzzy filtering, ↑/↓ navigation
    /// and Enter/Esc; focusing the input keeps keystrokes off the terminal PTY
    /// until the palette closes.
    fn build_list(
        commands: Vec<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        Self::build_list_with_delegate(PaletteDelegate::new(commands), window, cx)
    }

    /// The root list: grouped while idle, and its delegate injects live
    /// QuickConnect rows for a host-like query.
    fn build_root_list(
        commands: Vec<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        let delegate = PaletteDelegate::root(commands, cx);
        Self::build_list_with_delegate(delegate, window, cx)
    }

    fn build_list_with_delegate(
        delegate: PaletteDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
        list.update(cx, |state, cx| state.focus(window, cx));
        list
    }

    /// Swap the visible list to `commands` (root ⇄ sub-list). Recreating
    /// the `ListState` from scratch — rather than mutating the delegate in
    /// place — hands us a cleared search box, reset selection and fresh row
    /// cache for free, sidestepping the list's internal query/selection caching.
    fn show(&mut self, commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) {
        let list = Self::build_list(commands, window, cx);
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    fn show_ssh_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = Self::build_list_with_delegate(PaletteDelegate::ssh_connect(), window, cx);
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    fn search_placeholder(&self) -> &'static str {
        match self.menu {
            PaletteMenu::SshConnect => "user@host [-p 2222 -J jump]",
            PaletteMenu::Root => "Search or type user@host to connect…",
            PaletteMenu::Theme => "Search…",
        }
    }

    /// Read the currently highlighted command's "edit" variant, if any — the
    /// target of the ⌘⏎ / → gesture on a profile / quick-connect row.
    fn selected_edit_command(&self, cx: &App) -> Option<CommandKind> {
        self.list
            .read(cx)
            .delegate()
            .selected_command()
            .and_then(|k| k.edit_variant())
    }

    /// Translate the current list's confirm/cancel into either a host-facing
    /// event or an in-place transition into/out of a sub-list.
    fn on_list_event(
        &mut self,
        list: &Entity<ListState<PaletteDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ListEvent::Confirm(ix) => {
                let kind = list.read(cx).delegate().command_at(*ix);
                match kind {
                    // A submenu opener never reaches the host: it swaps this
                    // palette to another command catalog and stays open.
                    Some(CommandKind::OpenThemePicker) => {
                        self.menu = PaletteMenu::Theme;
                        let themes = Command::theme_commands(cx);
                        self.show(themes, window, cx);
                    }
                    Some(CommandKind::OpenSshConnectInput) => {
                        self.menu = PaletteMenu::SshConnect;
                        self.show_ssh_connect(window, cx);
                    }
                    // Unlike the other openers, this one *leaves*: switching
                    // workspace has its own surface now (`ui::switcher`), which
                    // groups by machine and carries per-row actions a palette
                    // list cannot. Emitting hands the host the job of closing
                    // this and opening that.
                    Some(kind @ CommandKind::OpenWorkspacePicker) => {
                        cx.emit(PaletteEvent::Confirm(kind))
                    }
                    Some(CommandKind::OpenSshConnect(input)) if input.trim().is_empty() => {}
                    Some(kind) => cx.emit(PaletteEvent::Confirm(kind)),
                    None => cx.emit(PaletteEvent::Dismiss),
                }
            }
            // Esc: from the sub-list, step back to the root catalog; from the
            // root, dismiss the palette.
            ListEvent::Cancel => {
                if self.menu != PaletteMenu::Root {
                    self.menu = PaletteMenu::Root;
                    let root = self.root.clone();
                    let list = Self::build_root_list(root, window, cx);
                    self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
                    self.list = list;
                    cx.notify();
                } else {
                    cx.emit(PaletteEvent::Dismiss);
                }
            }
            ListEvent::Select(_) => {}
        }
    }
}

impl EventEmitter<PaletteEvent> for PaletteView {}

/// Fixed command-row height (see `render_item`). The list viewport must hold a
/// whole number of rows, or the card's bottom edge cuts the last one mid-height.
/// Section headers are pinned to the same height for the same reason.
const PALETTE_ROW_H: f32 = 30.;
/// Rows visible before the list scrolls.
const PALETTE_VISIBLE_ROWS: f32 = 12.;
/// How many entries the idle "Recent" band shows. Small on purpose: it's a
/// shortcut to the two or three things you actually repeat, not a history log.
const RECENT_ROWS: usize = 5;

impl Render for PaletteView {
    /// The centered overlay: a dim full-window scrim plus the command card. The
    /// card just frames gpui-component's `List`, which renders its own search
    /// input and the filtered, scrollable, keyboard-driven rows.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, popover) = (theme.border, theme.popover);
        // The scrim is the shared one (see `presets::Surfaces::scrim`), the same
        // dim the workspace switcher opens over. What it replaces was a wash of
        // the window's *own* colour, which barely moved the window — so a card
        // lifted 5% off it landed at nearly the same value, and both overlays
        // read as a hole in the screen rather than a card over it.
        let scrim = crate::ui::presets::scrim_fill(cx);

        // The list viewport holds exactly `PALETTE_VISIBLE_ROWS` fixed-height
        // rows plus the list's own 4px top padding (`py_1` below, which scrolls
        // with the content) — any other height leaves the last visible row cut
        // mid-height at the card's bottom edge. The card wraps its content (no
        // max_h of its own) and adds `pb_1` so that row still clears the
        // rounded corners.
        let list_max_h = px(PALETTE_ROW_H * PALETTE_VISIBLE_ROWS + 4.);
        let card = v_flex()
            .w(px(560.))
            .bg(popover)
            .border_1()
            .border_color(border)
            // 10px radius + the floatier shadow match the context menu /
            // dropdown panel (see the fork's `PopupMenu`).
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .pb_1()
            // `py_1` (not `p_1`): the search input keeps its full-bleed width;
            // the rows inset themselves into rounded pills (see `render_item`),
            // Spotlight-style, matching the context menu's highlight language.
            .child(
                List::new(&self.list)
                    .search_placeholder(self.search_placeholder())
                    .py_1()
                    .max_h(list_max_h),
            );

        // Full-window scrim; clicking the empty area dismisses the palette (the
        // card itself is occluded so its clicks don't bubble here).
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(scrim)
            // ⌘⏎ or → on a highlighted profile / quick-connect row opens its
            // editor instead of connecting (PRD §6.2 ①). Captured on the scrim
            // (an ancestor of the focused search box) so it fires before the list
            // acts on a bare Enter. Plain Enter / navigation keys fall through.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                let is_edit_gesture = (ks.key == "enter" && ks.modifiers.platform)
                    || (ks.key == "right" && !ks.modifiers.platform);
                if is_edit_gesture {
                    if let Some(edit) = this.selected_edit_command(cx) {
                        cx.stop_propagation();
                        cx.emit(PaletteEvent::Confirm(edit));
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _window, cx| {
                    cx.emit(PaletteEvent::Dismiss);
                }),
            )
            .child(div().occlude().child(card))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Titles of the QuickConnect rows injected for a root-palette query.
    fn row_titles(query: &str) -> Vec<String> {
        PaletteDelegate::quick_connect_commands(query)
            .into_iter()
            .map(|c| c.title)
            .collect()
    }

    #[test]
    fn bare_word_gets_no_quick_connect_rows() {
        assert!(row_titles("java").is_empty());
        assert!(row_titles("split").is_empty());
        assert!(row_titles("").is_empty());
    }

    #[test]
    fn host_like_queries_get_connect_and_save_rows() {
        for q in [
            "deploy@10.0.0.5",
            "host.example.com",
            "java:2222",
            "ssh://java",
            "[::1]:2222",
        ] {
            let titles = row_titles(q);
            assert_eq!(
                titles,
                vec![
                    format!("Connect to \"{q}\""),
                    format!("Save \"{q}\" as profile…"),
                ],
                "query {q:?}"
            );
        }
    }

    #[test]
    fn host_like_but_unparsable_gets_no_rows() {
        // Contains ':' but the port segment is invalid → parse fails.
        assert!(row_titles("java:99999").is_empty());
        assert!(row_titles("@").is_empty());
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("zzz", "Split Right"), None);
        assert_eq!(fuzzy_score("thgir", "Split Right"), None);
    }

    /// The ranking's whole job: word-initials and prefixes beat letters
    /// scattered through a longer title.
    #[test]
    fn word_initials_outrank_scattered_letters() {
        let target = fuzzy_score("sr", "Split Right").expect("matches");
        // "Se...r" — an s and a later r, neither on a word boundary after the
        // first, in a longer title.
        let scattered = fuzzy_score("sr", "SSH: Manage Profiles…").expect("matches");
        assert!(
            target > scattered,
            "expected 'Split Right' ({target}) to outrank 'SSH: Manage Profiles…' ({scattered})"
        );
    }

    #[test]
    fn exact_and_prefix_beat_mid_string() {
        let exact = fuzzy_score("copy", "Copy").expect("matches");
        let longer = fuzzy_score("copy", "Copy Working Directory").expect("matches");
        assert!(
            exact > longer,
            "expected exact 'Copy' ({exact}) above 'Copy Working Directory' ({longer})"
        );
    }

    /// A subtitle hit still finds the row, but never outranks a title hit —
    /// typing a hostname should reach the profile whose subtitle carries it.
    #[test]
    fn subtitle_matches_are_found_but_discounted() {
        let cmd = Command::new("prod-web", CommandKind::NewTab)
            .with_subtitle("deploy@10.0.0.5".to_string());
        assert!(command_score("10.0.0", &cmd).is_some());
        let title_hit = command_score("prod", &cmd).expect("title matches");
        let subtitle_hit = command_score("deploy", &cmd).expect("subtitle matches");
        assert!(title_hit > subtitle_hit);
    }

    /// Every command that can be filed under Recent needs a stable id, and no
    /// two commands may share one — a collision would make the Recent band
    /// promote the wrong row.
    #[test]
    fn stable_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            CommandKind::NewTab,
            CommandKind::SplitRight,
            CommandKind::ClearTerminal,
            CommandKind::CopyText,
            CommandKind::CutText,
            CommandKind::PasteText,
            CommandKind::SelectAllText,
            CommandKind::FindInTerminal,
            CommandKind::FindNext,
            CommandKind::FindPrevious,
            CommandKind::OpenSettings,
            CommandKind::ShowKeyboardShortcuts,
            CommandKind::About,
            CommandKind::Quit,
            CommandKind::ShowRightPanel(RightPanelTab::Info),
            CommandKind::ShowRightPanel(RightPanelTab::Files),
        ] {
            let id = kind.id().expect("static command has an id");
            assert!(seen.insert(id), "duplicate command id {id:?}");
        }
    }

    /// Instance-specific commands must stay out of the frecency store.
    #[test]
    fn dynamic_commands_have_no_id() {
        assert!(CommandKind::ActivateTab(2).id().is_none());
        assert!(CommandKind::SetTheme(0).id().is_none());
        assert!(CommandKind::QuickConnect("a@b".into()).id().is_none());
    }
}
