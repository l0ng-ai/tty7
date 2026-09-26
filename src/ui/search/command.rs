//! What a search row runs, and the rows of the Actions tab.

use gpui::{App, SharedString};

use tty7_core::core::machine::TabId;
use tty7_core::core::session::WorkspaceId;
use uuid::Uuid;

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::config::{Config, RightPanelTab, TabBarPosition};
use crate::ui::i18n::{L10nKey, alias_translations, t, t_fmt};

/// Everything a row can do once it is picked. `Tty7App::run_command` is the
/// one place these are carried out, whichever tab the row came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandKind {
    NewTab,
    NewWorkspace,
    OpenWorkspacePicker,
    RenameWorkspace,
    StopWorkspace,
    DeleteWorkspace,
    NewWindow,
    CloseWindow,
    SplitRight,
    SplitDown,
    ClosePane,
    RenameTab,
    NewWorktreeTab,
    NewGroup,
    OpenFolderAsGroup,
    CloseOtherTabs,
    CloseTabsToTheRight,
    CopyWorkingDirectory,
    MarkTabUnread,
    HibernateTab,
    ForkAgentSession,
    CopyAgentSessionId,
    NewAgentTab,
    LaunchAgent(CLIAgent),
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
    SelectNextTab,
    SelectPrevTab,
    ToggleMaximizePane,
    ToggleFullscreen,
    ToggleTabSidebar,
    ToggleLeftPanel,
    ToggleRightPanel,
    ShowRightPanel(RightPanelTab),
    ClearTerminal,
    FindInTerminal,
    FindNext,
    FindPrevious,
    CopyText,
    CutText,
    PasteText,
    SelectAllText,
    ReopenClosedTab,
    OpenSettings,
    ShowKeyboardShortcuts,
    About,
    CheckForUpdates,
    OpenDocumentation,
    OpenDiscord,
    ReportIssue,
    Quit,
    RestartDaemon,
    UpdateLocalServer,
    UpdateRemoteServer,
    ToggleSftp,
    ShowSshForwards,
    ToggleCodePanel,
    ToggleDocumentFill,
    DocumentWidthThird,
    DocumentWidthHalf,
    DocumentWidthTwoThirds,
    ToggleDocumentPreview,
    ToggleDocumentWrap,
    RestartSshSession,
    ScmCommit,
    ScmStageAll,
    ScmUnstageAll,
    ScmDiscardAll,
    ScmPush,
    ScmPull,
    ScmFetch,
    ScmSync,
    ScmCreateBranch,
    OpenBranchPicker,
    ToggleDiffViewMode,
    SendSelectionToAgent,
    SendGitDiffToAgent,
    OpenThemePicker,
    /// Moves the search to its Hosts tab, where typing an address connects.
    SearchHosts,
    /// Connect with a typed `ssh` command line (`-p`, `-J`, an alias…).
    OpenSshConnect(String),
    SetTheme(usize),
    /// A tab of any workspace this process knows, wherever it lives.
    GoToTab {
        workspace: WorkspaceId,
        tab: TabId,
    },
    ConnectSavedProfile(Uuid),
    EditSavedProfile(Uuid),
    /// Open the shell the window's inventory lists under this label, as the
    /// New Tab menu's row for it would.
    OpenShell(String),
    SaveSshSessionAsHost,
    QuickConnect(String),
    SaveQuickConnect(String),
    OpenSshProfiles,
}

impl CommandKind {
    pub fn edit_variant(&self) -> Option<CommandKind> {
        match self {
            CommandKind::ConnectSavedProfile(id) => Some(CommandKind::EditSavedProfile(*id)),
            CommandKind::QuickConnect(s) => Some(CommandKind::SaveQuickConnect(s.clone())),
            _ => None,
        }
    }

    pub fn id(&self) -> Option<&'static str> {
        use CommandKind::*;
        Some(match self {
            NewTab => "new-tab",
            NewWorkspace => "new-workspace",
            OpenWorkspacePicker => "switch-workspace",
            RenameWorkspace => "rename-workspace",
            StopWorkspace => "stop-workspace",
            DeleteWorkspace => "delete-workspace",
            NewWindow => "new-window",
            CloseWindow => "close-window",
            SplitRight => "split-right",
            SplitDown => "split-down",
            ClosePane => "close-pane",
            RenameTab => "rename-tab",
            NewWorktreeTab => "new-worktree-tab",
            NewGroup => "new-sidebar-group",
            OpenFolderAsGroup => "open-folder-as-group",
            CloseOtherTabs => "close-other-tabs",
            CloseTabsToTheRight => "close-tabs-right",
            CopyWorkingDirectory => "copy-cwd",
            MarkTabUnread => "mark-tab-unread",
            HibernateTab => "hibernate-tab",
            ForkAgentSession => "fork-agent-session",
            CopyAgentSessionId => "copy-agent-session-id",
            NewAgentTab => "new-agent-tab",
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
            SelectNextTab => "next-tab",
            SelectPrevTab => "prev-tab",
            ToggleMaximizePane => "zoom-pane",
            ToggleFullscreen => "full-screen",
            ToggleTabSidebar => "tab-bar-position",
            ToggleLeftPanel => "left-sidebar",
            ToggleRightPanel => "right-panel",
            ShowRightPanel(RightPanelTab::Info) => "right-panel-info",
            // Frecency is keyed by this string, so it stays `right-panel-changes`
            // even though the panel is now called Source Control.
            ShowRightPanel(RightPanelTab::Scm) => "right-panel-changes",
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
            UpdateLocalServer => "update-local-server",
            UpdateRemoteServer => "update-remote-server",
            ToggleSftp => "ssh-remote-files",
            ShowSshForwards => "ssh-port-forwarding",
            ToggleCodePanel => "code-panel",
            ToggleDocumentFill => "document-fill",
            DocumentWidthThird => "document-width-third",
            DocumentWidthHalf => "document-width-half",
            DocumentWidthTwoThirds => "document-width-two-thirds",
            ToggleDocumentPreview => "document-preview",
            ToggleDocumentWrap => "document-wrap",
            RestartSshSession => "ssh-reconnect",
            ScmCommit => "git-commit",
            ScmStageAll => "git-stage-all",
            ScmUnstageAll => "git-unstage-all",
            ScmDiscardAll => "git-discard-all",
            ScmPush => "git-push",
            ScmPull => "git-pull",
            ScmFetch => "git-fetch",
            ScmSync => "git-sync",
            ScmCreateBranch => "git-create-branch",
            OpenBranchPicker => "git-checkout",
            ToggleDiffViewMode => "diff-view-mode",
            SendSelectionToAgent => "agent-send-selection",
            SendGitDiffToAgent => "agent-send-diff",
            OpenThemePicker => "change-theme",
            SearchHosts => "ssh-add-connection",
            OpenSshProfiles => "ssh-manage-profiles",
            SaveSshSessionAsHost => "ssh-save-connection",
            OpenSshConnect(_)
            | SetTheme(_)
            | GoToTab { .. }
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | OpenShell(_)
            | QuickConnect(_)
            | SaveQuickConnect(_)
            | LaunchAgent(_) => return None,
        })
    }

    pub(crate) fn key_spec(&self, cx: &App) -> Option<String> {
        use CommandKind::*;
        let inline =
            |spec: &str| -> Option<String> { cfg!(target_os = "macos").then(|| spec.to_string()) };
        match self {
            CopyText => return inline("secondary-c"),
            CutText => return inline("secondary-x"),
            PasteText => return inline("secondary-v"),
            SelectAllText => return inline("secondary-a"),
            LaunchAgent(agent) => {
                return crate::ui::keymap::effective_key(
                    crate::ui::agent_launch::launch_action_name(*agent),
                    cx,
                );
            }
            _ => {}
        }
        let action = match self {
            NewTab => "NewTab",
            NewWorkspace => "NewWorkspace",
            RenameWorkspace => "RenameWorkspace",
            StopWorkspace => "StopWorkspace",
            DeleteWorkspace => "DeleteWorkspace",
            NewWindow => "NewWindow",
            CloseWindow => "CloseWindow",
            SplitRight => "SplitRight",
            SplitDown => "SplitDown",
            ClosePane => "CloseActiveTab",
            RenameTab => "RenameTab",
            NewWorktreeTab => "NewWorktreeTab",
            CloseOtherTabs => "CloseOtherTabs",
            CloseTabsToTheRight => "CloseTabsToTheRight",
            CopyWorkingDirectory => "CopyWorkingDirectory",
            MarkTabUnread => "MarkTabUnread",
            HibernateTab => "HibernateTab",
            ForkAgentSession => "ForkAgentSession",
            CopyAgentSessionId => "CopyAgentSessionId",
            NewAgentTab => "NewAgentTab",
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
            // What the search runs is the plain next/previous step, not the
            // MRU switcher `NextTab` opens, so its chord hint is that one's.
            SelectNextTab => "SelectNextTab",
            SelectPrevTab => "SelectPrevTab",
            ToggleMaximizePane => "ToggleMaximizePane",
            ToggleFullscreen => "ToggleFullscreen",
            ToggleTabSidebar => "ToggleTabSidebar",
            ToggleLeftPanel => "ToggleLeftPanel",
            ToggleRightPanel => "ToggleRightPanel",
            ShowRightPanel(tab) => match tab {
                RightPanelTab::Info => "ShowRightPanelInfo",
                RightPanelTab::Scm => "ShowRightPanelChanges",
                RightPanelTab::Files => "ShowRightPanelFiles",
            },
            ClearTerminal => "ClearScrollback",
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
            ToggleDocumentFill => "ToggleDocumentFill",
            DocumentWidthThird => "DocumentWidthThird",
            DocumentWidthHalf => "DocumentWidthHalf",
            DocumentWidthTwoThirds => "DocumentWidthTwoThirds",
            ToggleDocumentPreview => "ToggleDocumentPreview",
            ToggleDocumentWrap => "ToggleDocumentWrap",
            RestartSshSession => "RestartSshSession",
            OpenSshProfiles => "OpenSshProfiles",
            ScmCommit => "ScmCommit",
            ScmStageAll => "ScmStageAll",
            ScmUnstageAll => "ScmUnstageAll",
            ScmDiscardAll => "ScmDiscardAll",
            ScmPush => "ScmPush",
            ScmPull => "ScmPull",
            ScmFetch => "ScmFetch",
            ScmSync => "ScmSync",
            ScmCreateBranch => "ScmCreateBranch",
            OpenBranchPicker => "ScmCheckoutBranch",
            ToggleDiffViewMode => "ToggleDiffViewMode",
            CopyText
            | CutText
            | PasteText
            | SelectAllText
            | SendSelectionToAgent
            | SendGitDiffToAgent
            | NewGroup
            | OpenFolderAsGroup
            | UpdateLocalServer
            | UpdateRemoteServer
            | OpenWorkspacePicker
            | OpenThemePicker
            | SearchHosts
            | OpenSshConnect(_)
            | SetTheme(_)
            | GoToTab { .. }
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | OpenShell(_)
            | SaveSshSessionAsHost
            | QuickConnect(_)
            | SaveQuickConnect(_)
            | LaunchAgent(_) => return None,
        };
        crate::ui::keymap::effective_key(action, cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandGroup {
    TabsPanes,
    Workspaces,
    View,
    Git,
    Terminal,
    Ssh,
    Agents,
    Application,
}

impl CommandGroup {
    pub(crate) const ORDER: [CommandGroup; 8] = [
        CommandGroup::TabsPanes,
        CommandGroup::Workspaces,
        CommandGroup::View,
        CommandGroup::Git,
        CommandGroup::Terminal,
        CommandGroup::Ssh,
        CommandGroup::Agents,
        CommandGroup::Application,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            CommandGroup::TabsPanes => t(L10nKey::CmdGroupTabsPanes),
            CommandGroup::Workspaces => t(L10nKey::CmdGroupWorkspaces),
            CommandGroup::View => t(L10nKey::CmdGroupView),
            CommandGroup::Git => t(L10nKey::CmdGroupGit),
            CommandGroup::Terminal => t(L10nKey::CmdGroupTerminal),
            CommandGroup::Ssh => t(L10nKey::CmdGroupSsh),
            CommandGroup::Agents => t(L10nKey::CmdGroupAgents),
            CommandGroup::Application => t(L10nKey::CmdGroupApplication),
        }
    }
}

#[derive(Clone)]
pub struct ChromeState {
    pub rail_collapsed: bool,
    pub right_panel_visible: bool,
    /// Whether the *active tab's* document is filling the window. Passed in
    /// rather than read off the config here: `document_layout` in the config is
    /// only what a tab that has never been told starts from, so a tab that was
    /// told would have had the row offer it the state it is already in.
    pub document_filled: bool,
    /// The machine this window's workspace lives on, named for the search, when
    /// it runs a server this build installs there. `None` for this computer and
    /// for a `--stdio` program, where there is nothing of ours to update.
    pub remote_server: Option<String>,
}

/// A terminal row's avatar: the same disc, brand and status dot the tab
/// strip draws, so a tab reads the same in both places.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Avatar {
    pub agent: Option<CLIAgent>,
    pub status: Option<AgentStatus>,
    pub unread: usize,
    pub ssh: Option<u32>,
}

/// One row of any tab.
#[derive(Clone)]
pub struct Item {
    pub title: String,
    pub subtitle: Option<String>,
    /// Muted text at the row's right edge — the workspace a terminal lives in.
    pub note: Option<String>,
    /// Text the query may match but the row never shows: the same label as
    /// every other locale words it, the stable command id, a terminal's
    /// workspace. Built once, with the entry — the filter runs on every
    /// keystroke.
    pub aliases: Vec<SharedString>,
    pub kind: CommandKind,
    /// Actions only: the group that orders the Actions tab.
    pub group: Option<CommandGroup>,
    /// The header this row sits under while nothing is typed.
    pub section: Option<SharedString>,
    pub avatar: Option<Avatar>,
}

impl Item {
    pub fn new(title: impl Into<String>, kind: CommandKind) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            note: None,
            // The id is English and hyphenated ("change-theme"), which is close
            // enough to what someone reaching past a translated label types.
            aliases: kind.id().into_iter().map(SharedString::from).collect(),
            kind,
            group: None,
            section: None,
            avatar: None,
        }
    }

    /// A command whose label comes out of the locale table, and which can
    /// therefore also be found by the wording any other locale would show.
    pub fn localized(key: L10nKey, kind: CommandKind) -> Self {
        Self::new(t(key), kind).with_aliases_of(key)
    }

    /// Also findable by the wording every other locale gives `key`.
    pub fn with_aliases_of(mut self, key: L10nKey) -> Self {
        self.aliases
            .extend(alias_translations(key).into_iter().map(SharedString::from));
        self
    }

    pub fn with_alias(mut self, alias: impl Into<SharedString>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn in_section(mut self, section: impl Into<SharedString>) -> Self {
        self.section = Some(section.into());
        self
    }

    pub fn with_avatar(mut self, avatar: Avatar) -> Self {
        self.avatar = Some(avatar);
        self
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn in_group(mut self, group: CommandGroup) -> Self {
        self.group = Some(group);
        self.section = Some(group.title().into());
        self
    }

    /// The actions every window offers, in group order. The window adds the
    /// ones only it can judge (`Tty7App::search_catalog`).
    pub fn actions(cx: &App, chrome: ChromeState) -> Vec<Item> {
        use CommandKind::*;
        let cfg = cx.global::<Config>();
        let tab_bar_left = cfg.tab_bar_position == TabBarPosition::Left;
        let sidebar_hidden = chrome.rail_collapsed || !tab_bar_left;
        let right_panel_open = chrome.right_panel_visible;
        let document_filled = chrome.document_filled;

        let tabs = [
            Item::localized(L10nKey::CmdNewTab, NewTab),
            Item::localized(L10nKey::CmdNewWorktreeTab, NewWorktreeTab)
                .with_subtitle(t(L10nKey::CmdNewWorktreeTabSubtitle)),
            Item::localized(L10nKey::CmdRenameTab, RenameTab),
            Item::localized(L10nKey::CmdSplitRight, SplitRight),
            Item::localized(L10nKey::CmdSplitDown, SplitDown),
            Item::localized(L10nKey::CmdZoomPane, ToggleMaximizePane),
            Item::localized(L10nKey::CmdNextPane, NextPane),
            Item::localized(L10nKey::CmdPreviousPane, PrevPane),
            Item::localized(L10nKey::CmdFocusPaneLeft, FocusPaneLeft),
            Item::localized(L10nKey::CmdFocusPaneRight, FocusPaneRight),
            Item::localized(L10nKey::CmdFocusPaneUp, FocusPaneUp),
            Item::localized(L10nKey::CmdFocusPaneDown, FocusPaneDown),
            Item::localized(L10nKey::CmdResizePaneLeft, ResizePaneLeft),
            Item::localized(L10nKey::CmdResizePaneRight, ResizePaneRight),
            Item::localized(L10nKey::CmdResizePaneUp, ResizePaneUp),
            Item::localized(L10nKey::CmdResizePaneDown, ResizePaneDown),
            Item::localized(L10nKey::CmdSwapPaneNext, SwapPaneNext),
            Item::localized(L10nKey::CmdSwapPanePrevious, SwapPanePrev),
            Item::localized(L10nKey::CmdNextTab, SelectNextTab),
            Item::localized(L10nKey::CmdPreviousTab, SelectPrevTab),
            Item::localized(L10nKey::CmdCopyWorkingDirectory, CopyWorkingDirectory),
            Item::localized(L10nKey::CmdCopySessionId, CopyAgentSessionId)
                .with_subtitle(t(L10nKey::CmdCopySessionIdSubtitle)),
            Item::localized(L10nKey::CmdForkSession, ForkAgentSession)
                .with_subtitle(t(L10nKey::CmdForkSessionSubtitle)),
            Item::localized(L10nKey::CmdMarkTabAsUnread, MarkTabUnread),
            Item::localized(L10nKey::CmdHibernateTab, HibernateTab)
                .with_subtitle(t(L10nKey::CmdHibernateTabSubtitle)),
            Item::localized(L10nKey::CmdClosePaneTab, ClosePane),
            Item::localized(L10nKey::CmdCloseOtherTabs, CloseOtherTabs),
            Item::localized(L10nKey::CmdCloseTabsToTheRight, CloseTabsToTheRight),
            Item::localized(L10nKey::CmdReopenClosedTab, ReopenClosedTab),
        ];

        let workspaces = [
            Item::localized(L10nKey::CmdNewWorkspace, NewWorkspace),
            Item::localized(L10nKey::CmdSwitchWorkspace, OpenWorkspacePicker),
            Item::localized(L10nKey::CmdRenameWorkspace, RenameWorkspace),
            Item::localized(L10nKey::CmdStopWorkspace, StopWorkspace)
                .with_subtitle(t(L10nKey::CmdStopWorkspaceSubtitle)),
            Item::localized(L10nKey::CmdDeleteWorkspace, DeleteWorkspace)
                .with_subtitle(t(L10nKey::CmdDeleteWorkspaceSubtitle)),
        ];

        let view = [
            Item::localized(
                if sidebar_hidden {
                    L10nKey::CmdShowLeftSidebar
                } else {
                    L10nKey::CmdHideLeftSidebar
                },
                ToggleLeftPanel,
            ),
            Item::localized(
                if right_panel_open {
                    L10nKey::CmdHideRightPanel
                } else {
                    L10nKey::CmdShowRightPanel
                },
                ToggleRightPanel,
            ),
            Item::localized(L10nKey::CmdShowCodePanel, ToggleCodePanel),
            Item::localized(
                if document_filled {
                    L10nKey::CmdDocumentDock
                } else {
                    L10nKey::CmdDocumentFill
                },
                ToggleDocumentFill,
            ),
            Item::localized(L10nKey::CmdDocumentWidthThird, DocumentWidthThird),
            Item::localized(L10nKey::CmdDocumentWidthHalf, DocumentWidthHalf),
            Item::localized(L10nKey::CmdDocumentWidthTwoThirds, DocumentWidthTwoThirds),
            Item::localized(L10nKey::CmdToggleDocumentPreview, ToggleDocumentPreview),
            Item::localized(L10nKey::CmdToggleDocumentWrap, ToggleDocumentWrap),
            Item::localized(
                if tab_bar_left {
                    L10nKey::CmdTabBarMoveToTop
                } else {
                    L10nKey::CmdTabBarMoveToLeftSidebar
                },
                ToggleTabSidebar,
            ),
            Item::localized(
                L10nKey::CmdRightPanelInfo,
                ShowRightPanel(RightPanelTab::Info),
            ),
            Item::localized(
                L10nKey::CmdRightPanelChanges,
                ShowRightPanel(RightPanelTab::Scm),
            ),
            Item::localized(
                L10nKey::CmdRightPanelFiles,
                ShowRightPanel(RightPanelTab::Files),
            ),
            Item::localized(L10nKey::CmdChangeTheme, OpenThemePicker),
            Item::localized(L10nKey::CmdResetFontSize, ResetFontSize),
            Item::localized(L10nKey::CmdEnterFullScreen, ToggleFullscreen),
            Item::localized(L10nKey::CmdToggleDiffViewMode, ToggleDiffViewMode),
        ];

        // Their own group rather than more entries under View: View is a list
        // of things to show and hide, and ten git verbs in it would drown that.
        let git = [
            Item::localized(L10nKey::CmdGitCommit, ScmCommit),
            Item::localized(L10nKey::CmdGitStageAll, ScmStageAll),
            Item::localized(L10nKey::CmdGitUnstageAll, ScmUnstageAll),
            Item::localized(L10nKey::CmdGitDiscardAll, ScmDiscardAll)
                .with_subtitle(t(L10nKey::CmdGitDiscardAllSubtitle)),
            Item::localized(L10nKey::CmdGitCheckoutTo, OpenBranchPicker),
            Item::localized(L10nKey::CmdGitCreateBranch, ScmCreateBranch),
            Item::localized(L10nKey::CmdGitSync, ScmSync)
                .with_subtitle(t(L10nKey::CmdGitSyncSubtitle)),
            Item::localized(L10nKey::CmdGitPush, ScmPush),
            Item::localized(L10nKey::CmdGitPull, ScmPull),
            Item::localized(L10nKey::CmdGitFetch, ScmFetch),
        ];

        let terminal = [
            Item::localized(L10nKey::CmdClearScrollback, ClearTerminal),
            Item::localized(L10nKey::CmdFindInTerminal, FindInTerminal),
            Item::localized(L10nKey::CmdFindNext, FindNext),
            Item::localized(L10nKey::CmdFindPrevious, FindPrevious),
            Item::localized(L10nKey::CmdCopy, CopyText),
            Item::localized(L10nKey::CmdCut, CutText),
            Item::localized(L10nKey::CmdPaste, PasteText),
            Item::localized(L10nKey::CmdSelectAll, SelectAllText),
        ];

        let ssh = [
            Item::localized(L10nKey::CmdSshAddConnection, SearchHosts),
            Item::localized(L10nKey::CmdSshManageProfiles, OpenSshProfiles),
            Item::localized(L10nKey::CmdSshReconnect, RestartSshSession),
            Item::localized(L10nKey::CmdSshRemoteFiles, ToggleSftp),
            Item::localized(L10nKey::CmdSshPortForwarding, ShowSshForwards),
        ];

        let agents = [
            Item::localized(L10nKey::CmdNewAgentTab, NewAgentTab)
                .with_subtitle(t(L10nKey::CmdNewAgentTabSubtitle)),
            Item::localized(L10nKey::CmdAgentSendSelection, SendSelectionToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendSelectionSubtitle)),
            Item::localized(L10nKey::CmdAgentSendGitDiffForReview, SendGitDiffToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendGitDiffSubtitle)),
        ];

        let application = [
            Item::localized(L10nKey::CmdNewWindow, NewWindow),
            Item::localized(L10nKey::CmdSettings, OpenSettings),
            Item::localized(L10nKey::CmdKeyboardShortcuts, ShowKeyboardShortcuts),
            Item::localized(L10nKey::CmdAboutTty7, About),
            Item::localized(L10nKey::CmdCheckForUpdates, CheckForUpdates),
            Item::localized(L10nKey::CmdDocumentation, OpenDocumentation),
            Item::localized(L10nKey::CmdJoinDiscord, OpenDiscord),
            Item::localized(L10nKey::CmdReportIssue, ReportIssue),
            Item::localized(L10nKey::CmdRestartServer, RestartDaemon)
                .with_subtitle(t(L10nKey::CmdRestartServerSubtitle)),
            Item::localized(L10nKey::CmdUpdateLocalServer, UpdateLocalServer)
                .with_subtitle(t(L10nKey::CmdUpdateLocalServerSubtitle)),
            // Beside Quit, because the pair is the whole point of the action:
            // both end the window you are looking at, and only one of them
            // takes your shells with it. Read together the subtitles say which.
            Item::localized(L10nKey::CmdCloseWindow, CloseWindow)
                .with_subtitle(t(L10nKey::CmdCloseWindowSubtitle)),
            Item::localized(L10nKey::CmdQuitTty7, Quit)
                .with_subtitle(t(L10nKey::CmdQuitTty7Subtitle)),
        ];

        let mut out = Vec::new();
        let mut push = |cmds: Vec<Item>, group: CommandGroup| {
            out.extend(cmds.into_iter().map(|c| c.in_group(group)));
        };
        push(tabs.into(), CommandGroup::TabsPanes);
        push(workspaces.into(), CommandGroup::Workspaces);
        push(view.into(), CommandGroup::View);
        push(git.into(), CommandGroup::Git);
        push(terminal.into(), CommandGroup::Terminal);
        push(ssh.into(), CommandGroup::Ssh);
        push(agents.into(), CommandGroup::Agents);
        push(application.into(), CommandGroup::Application);

        // Only where there is a server of ours over there to update. Named for
        // the machine, because the confirmation ends every session on it and
        // the row should already have said which one.
        if let Some(machine) = &chrome.remote_server {
            out.push(
                Item::new(
                    t_fmt(L10nKey::CmdUpdateRemoteServer, &[("machine", machine)]),
                    UpdateRemoteServer,
                )
                .with_aliases_of(L10nKey::CmdUpdateRemoteServer)
                .with_subtitle(t(L10nKey::CmdUpdateRemoteServerSubtitle))
                .in_group(CommandGroup::Application),
            );
        }
        out
    }

    /// The theme picker's rows, the one in use checked.
    pub fn themes(cx: &App) -> Vec<Item> {
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
                Item::new(title, CommandKind::SetTheme(i))
            })
            .collect()
    }

    /// Row of the preset already in use, so the theme picker can open on it
    /// instead of previewing something else the moment it is opened.
    pub fn active_theme_index(cx: &App) -> Option<usize> {
        let active = crate::ui::theme::effective_preset_id(cx);
        crate::ui::presets::all(cx)
            .iter()
            .position(|p| p.id == active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The picker previews whatever row is highlighted, so it has to open on
    /// the row of the theme already in use — the one carrying the check mark.
    #[gpui::test]
    fn the_theme_picker_opens_on_the_theme_in_use(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let themes = crate::ui::presets::all(cx);
            let last = themes.last().expect("built-in themes").id.clone();
            cx.set_global(Config(crate::core::config::CoreConfig {
                theme_follow_system: false,
                theme_preset: last,
                ..Default::default()
            }));

            let ix = Item::active_theme_index(cx).expect("the live preset is listed");
            assert_eq!(ix, themes.len() - 1);
            let rows = Item::themes(cx);
            assert!(
                rows[ix].title.ends_with('✓'),
                "row {ix} ({:?}) should be the checked one",
                rows[ix].title
            );
        });
    }

    #[test]
    fn dynamic_commands_have_no_id() {
        assert!(
            CommandKind::GoToTab {
                workspace: WorkspaceId::new(),
                tab: TabId::new(),
            }
            .id()
            .is_none()
        );
        assert!(CommandKind::SetTheme(0).id().is_none());
        assert!(CommandKind::QuickConnect("a@b".into()).id().is_none());
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn every_action_has_a_stable_id(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");
            let chrome = ChromeState {
                rail_collapsed: false,
                right_panel_visible: false,
                document_filled: false,
                remote_server: None,
            };
            let mut seen = std::collections::HashSet::new();
            for cmd in Item::actions(cx, chrome) {
                // Frecency is keyed by this string. A command without one is
                // never learned, so it never rises in the list no matter how
                // often it is run.
                let id = cmd
                    .kind
                    .id()
                    .unwrap_or_else(|| panic!("`{}` has no stable id", cmd.title));
                assert!(seen.insert(id), "two commands claim the id {id:?}");
            }
        });
    }

    #[gpui::test]
    fn the_git_group_is_its_own_section(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");
            let chrome = ChromeState {
                rail_collapsed: false,
                right_panel_visible: false,
                document_filled: false,
                remote_server: None,
            };
            let cmds = Item::actions(cx, chrome);
            let git = cmds
                .iter()
                .filter(|c| c.group == Some(CommandGroup::Git))
                .count();
            assert_eq!(git, 10, "the git section should hold ten verbs");
            // View stays a list of things to show and hide.
            assert!(!cmds.iter().any(|c| c.group == Some(CommandGroup::View)
                && c.kind.id().unwrap_or("").starts_with("git-")),);
        });
    }

    fn chrome(remote_server: Option<&str>) -> ChromeState {
        ChromeState {
            rail_collapsed: false,
            right_panel_visible: false,
            document_filled: false,
            remote_server: remote_server.map(str::to_string),
        }
    }

    /// Updating this computer's server is always on offer; updating a remote
    /// one only where the window's workspace has one of ours to update.
    #[gpui::test]
    fn the_remote_update_is_offered_only_with_a_remote_server(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");

            let local = Item::actions(cx, chrome(None));
            assert!(
                local
                    .iter()
                    .any(|c| c.kind == CommandKind::UpdateLocalServer)
            );
            assert!(
                !local
                    .iter()
                    .any(|c| c.kind == CommandKind::UpdateRemoteServer),
                "a local window has no remote server to update"
            );

            let remote = Item::actions(cx, chrome(Some("build-box")));
            let update = remote
                .iter()
                .find(|c| c.kind == CommandKind::UpdateRemoteServer)
                .expect("a remote window offers to update its server");
            assert!(
                update.title.contains("build-box"),
                "the row names the machine whose sessions it ends: {:?}",
                update.title
            );
            assert_eq!(update.kind.id(), Some("update-remote-server"));
            assert!(
                remote
                    .iter()
                    .any(|c| c.kind == CommandKind::UpdateLocalServer),
                "this computer's server can still be updated from a remote window"
            );
        });
    }
}
