use super::L10nKey;

pub fn translate_en(key: L10nKey) -> &'static str {
    match key {
        L10nKey::SearchTabs => "Search tabs…",
        L10nKey::SearchFiles => "Search files…",
        L10nKey::SearchThemes => "Search themes…",
        L10nKey::SearchSettings => "Search settings…",
        L10nKey::FilterHosts => "Filter hosts…",
        L10nKey::SearchCommandsOrHost => "Search or type user@host to connect…",
        L10nKey::SearchTheme => "Search…",
        L10nKey::Search => "Search",
        L10nKey::SearchWorkspacesAndMachines => "Search workspaces, tabs and machines",
        L10nKey::SearchFonts => "Search fonts…",
        L10nKey::NewFolderName => "New folder name",
        L10nKey::NewFileName => "New file name",
        L10nKey::HomeNewTab => "New Tab",
        L10nKey::HomeReopenClosedTab => "Reopen Closed Tab",
        L10nKey::HomeSwitchWorkspace => "Switch Workspace",
        L10nKey::HomeCommandPalette => "Command Palette",
        L10nKey::HomeSplitRight => "Split Right",
        L10nKey::HomeSplitDown => "Split Down",
        L10nKey::HomeSettings => "Settings…",
        L10nKey::TrayQuitStopServer => "Quit and Stop Server…",
        L10nKey::Reconnect => "Reconnect",
        L10nKey::None => "None.",
        L10nKey::TryAgain => "Try Again",
        L10nKey::Refreshing => "refreshing…",
        L10nKey::Binary => "binary",
        L10nKey::Delete => "Delete",
        L10nKey::NoMatchingCommands => "No matching commands",
        L10nKey::ConnectSshHint => "Type user@host to connect over SSH instead.",
        L10nKey::EditHint => "→ edit",
        L10nKey::OpenFileFromTree => "Open a file from the file tree",
        L10nKey::FileChangedOnDisk => "File changed on disk",
        L10nKey::Reload => "Reload",
        L10nKey::KeepMine => "Keep mine",
        L10nKey::Dismiss => "Dismiss",
        L10nKey::StoredPasswordRejected => "The stored password was rejected. Enter a new one.",
        L10nKey::Trust => "Trust",
        L10nKey::Abort => "Abort",
        L10nKey::HostKeyOverrideMessage => {
            "Type \"yes\" to override and trust the new key, or Esc to abort."
        }
        L10nKey::Override => "Override",
        L10nKey::RememberKeychain => "Remember (keychain)",
        L10nKey::Cancel => "Cancel",
        L10nKey::Close => "Close",
        L10nKey::QuitStopServerTitle => "Quit and Stop Server?",
        L10nKey::QuitStopServerBody => {
            "This quits tty7 and stops the background server — anything still running \
             in your shells is terminated. Your tabs and layout are kept and reopen with \
             fresh shells next launch. (Plain Quit keeps shells running.)"
        }
        L10nKey::QuitAndStop => "Quit and Stop",
        L10nKey::CloseSshConnectionTitle => "Close this SSH connection?",
        L10nKey::CloseSshConnectionBody => "The connection is live. Closing will end it.",
        L10nKey::Keep => "Keep",
        L10nKey::SettingsNavAppearance => "Appearance",
        L10nKey::SettingsNavTerminal => "Terminal",
        L10nKey::SettingsNavInput => "Input",
        L10nKey::SettingsNavSsh => "SSH",
        L10nKey::SettingsNavAgents => "Agents",
        L10nKey::SettingsNavWindowTabs => "Window & Tabs",
        L10nKey::SettingsNavKeybindings => "Keybindings",
        L10nKey::SettingsNavAbout => "About",
        L10nKey::SettingsHeader => "SETTINGS",
        L10nKey::Reset => "Reset",
        L10nKey::Save => "Save",
        L10nKey::Connect => "Connect",
        L10nKey::Download => "Download",
        L10nKey::Link => "Link",
        L10nKey::SettingsThemeIntroTitle => "Theme",
        L10nKey::SettingsThemeIntroDesc => {
            "Pick a color theme. Each one sets its own light or dark look."
        }
        L10nKey::SettingsTypography => "Typography",
        L10nKey::SettingsFontSize => "Font size",
        L10nKey::SettingsFontSizeDesc => "Terminal text size in pixels.",
        L10nKey::SettingsLineHeight => "Line height",
        L10nKey::SettingsLineHeightDesc => "Row spacing as a multiple of the font size.",
        L10nKey::SettingsFontFamily => "Font family",
        L10nKey::SettingsFontFamilyDesc => "Pick from fonts installed on your system.",
        L10nKey::SettingsBoldFont => "Bold font",
        L10nKey::SettingsBoldFontDesc => {
            "Face for bold text; Default synthesizes it from the primary."
        }
        L10nKey::SettingsItalicFont => "Italic font",
        L10nKey::SettingsItalicFontDesc => {
            "Face for italic text; Default synthesizes it from the primary."
        }
        L10nKey::SettingsFontLigatures => "Font ligatures",
        L10nKey::SettingsFontLigaturesDesc => {
            "Enable common programming ligature features for terminal text."
        }
        L10nKey::SettingsCursor => "Cursor",
        L10nKey::SettingsCursorShape => "Cursor shape",
        L10nKey::SettingsCursorShapeDesc => "How the terminal cursor is drawn.",
        L10nKey::SettingsCursorBlink => "Cursor blink",
        L10nKey::SettingsCursorBlinkDesc => "Pulse the cursor while the terminal is focused.",
        L10nKey::SettingsLanguage => "Language",
        L10nKey::SettingsLanguageDesc => "Choose the language used for the tty7 interface.",
        L10nKey::SettingsLanguageEnglish => "English",
        L10nKey::SettingsLanguageChinese => "简体中文",
        L10nKey::SettingsLanguageJapanese => "日本語",
        L10nKey::SettingsSearchLanguageKeywords => "language, locale, english, chinese",
        L10nKey::SettingsTransparency => "Transparency",
        L10nKey::SettingsOpacity => "Opacity",
        L10nKey::SettingsOpacityDesc => {
            "How opaque the window background is, for every theme. Below 100% the desktop shows through."
        }
        L10nKey::SettingsBlur => "Blur",
        L10nKey::SettingsBlurDesc => "Blur whatever is behind a translucent window (macOS).",
        L10nKey::FollowTheme => "Follow theme",
        L10nKey::SettingsDimInactivePanes => "Dim inactive panes",
        L10nKey::SettingsDimInactivePanesDesc => {
            "Fade unfocused panes in a split so the active one stands out."
        }
        L10nKey::SettingsOpenThemesFolder => "Open themes folder",
        L10nKey::SettingsChangeThemeImage => "Change…",
        L10nKey::SettingsChooseThemeImage => "Choose…",
        L10nKey::SettingsRemoveThemeImage => "Remove",
        L10nKey::SettingsImageOpacity => "Image opacity",
        L10nKey::SettingsImageOpacityDesc => {
            "How strongly the image shows over the background color."
        }
        L10nKey::SettingsEditTheme => "Edit theme",
        L10nKey::SettingsEditThemeIntro => {
            "You're editing a copy. Changes save to its file in the themes folder and apply live."
        }
        L10nKey::SettingsBackgroundImage => "Background image",
        L10nKey::SettingsBackgroundImageDesc => {
            "Composited over the background color, under the text."
        }
        L10nKey::SettingsAnsiColors => "ANSI colors",
        L10nKey::SettingsCustomThemes => "Custom themes",
        L10nKey::SettingsCustomThemesIntro => {
            "Duplicate a theme to edit its colors here, or drop your own in the themes folder: a tty7 YAML theme or an iTerm2 .itermcolors scheme."
        }
        L10nKey::SettingsDuplicateToEdit => "Duplicate to edit",
        L10nKey::SettingsHosts => "Hosts",
        L10nKey::SettingsDefaults => "Defaults",
        L10nKey::SettingsInheritedByEveryHost => "Inherited by every host",
        L10nKey::SettingsNoSavedHosts => "No saved hosts yet.",
        L10nKey::SettingsNothingMatches => "Nothing matches {query}.",
        L10nKey::SettingsInTty7 => "In tty7",
        L10nKey::SettingsImportFromSshConfig => "Import from ~/.ssh/config",
        L10nKey::SettingsExpandAllGroups => "Expand all groups",
        L10nKey::SettingsNoHostsYet => "No hosts yet",
        L10nKey::SettingsNothingSelected => "Nothing selected",
        L10nKey::SettingsTypeAddressToConnect => {
            "Type an address to connect now — tty7 offers to save it afterwards."
        }
        L10nKey::SettingsMoreInSshConfig => "{count} more in ~/.ssh/config",
        L10nKey::SettingsAliasesLinked => "{count} aliases linked.",
        L10nKey::SettingsImportAliases => "Import aliases",
        L10nKey::SettingsImportAliasesDesc => {
            "Re-reads the file and adds anything new. Edits you make here are stored by tty7 — the file itself is never written."
        }
        L10nKey::SettingsImportNow => "Import now",
        L10nKey::SettingsDefaultsIntro => {
            "Every host starts from these. Any host can override one under its own Advanced."
        }
        L10nKey::SettingsCopyAddress => "Copy address",
        L10nKey::SettingsDuplicate => "Duplicate",
        L10nKey::SettingsForgetPassword => "Forget password",
        L10nKey::SettingsForgotPasswordFor => "Forgot saved password for {endpoint}",
        L10nKey::SettingsCouldntForgetPassword => {
            "Couldn't forget password for {endpoint}: {error}"
        }
        L10nKey::SettingsSecurity => "Security",
        L10nKey::SettingsSecurityIntro => {
            "A host can override either of these under its own Advanced."
        }
        L10nKey::SettingsVerifyHostKeys => "Verify host keys",
        L10nKey::SettingsVerifyHostKeysDesc => {
            "Check each server's key against known_hosts and confirm unknown or changed keys before connecting. Off connects without checking, so a spoofed server would go unnoticed."
        }
        L10nKey::WarnBeforeClosing => "Warn before closing",
        L10nKey::SettingsWarnBeforeClosingDesc => {
            "Ask for confirmation before closing a tab or pane with a live SSH session."
        }
        L10nKey::SettingsNewHost => "New host",
        L10nKey::SettingsName => "Name",
        L10nKey::SettingsNameDesc => "A label for this connection.",
        L10nKey::SettingsHost => "Host",
        L10nKey::SettingsHostDesc => "Hostname or IP address.",
        L10nKey::SettingsUser => "User",
        L10nKey::SettingsUserDesc => "Login user (blank = resolve at connect).",
        L10nKey::SettingsAuth => "Auth",
        L10nKey::SettingsAuthDesc => "Authentication method. Auto tries every applicable method.",
        L10nKey::SettingsAuthModeAuto => "Auto",
        L10nKey::SettingsAuthModePassword => "Password",
        L10nKey::SettingsAuthModeKey => "Key",
        L10nKey::SettingsAuthModeAgent => "Agent",
        L10nKey::SettingsAuthMode2Fa => "2FA",
        L10nKey::SettingsJumpHost => "Jump host",
        L10nKey::SettingsJumpHostDesc => {
            "Name of another profile to tunnel through (blank = direct)."
        }
        L10nKey::SettingsNoneSummary => "(none)",
        L10nKey::SettingsNoneLower => "none",
        L10nKey::SettingsPortForwarding => "Port forwarding",
        L10nKey::SettingsRulesOpenedWithConnection => "1 rule, opened with the connection",
        L10nKey::SettingsAddRule => "+ Add rule",
        L10nKey::SettingsFwdLegendLocal => "L — a local port reaches the remote side",
        L10nKey::SettingsFwdLegendRemote => "R — a remote port reaches this machine",
        L10nKey::SettingsFwdLegendDynamic => "D — dynamic SOCKS proxy",
        L10nKey::SettingsFwdNeedsBoth => {
            "Needs a listen port and a target host:port — won't be saved."
        }
        L10nKey::SettingsFwdNeedsListen => "Needs a listen port — won't be saved.",
        L10nKey::SettingsAdvanced => "Advanced",
        L10nKey::SettingsAdvancedSummary => {
            "algorithms / keepalive / proxies / X11 / login scripts"
        }
        L10nKey::SettingsIdentityFiles => "Identity files",
        L10nKey::SettingsIdentityFilesDesc => "Private-key paths, one per line (%h/%r expand).",
        L10nKey::SettingsAgentForwarding => "Agent forwarding",
        L10nKey::SettingsAgentForwardingDesc => "Forward the local ssh-agent to the connection.",
        L10nKey::SettingsProxyCommand => "ProxyCommand",
        L10nKey::SettingsProxyCommandDesc => "Transport command (%h/%p/%r substituted).",
        L10nKey::SettingsSocks5Proxy => "SOCKS5 proxy",
        L10nKey::SettingsSocks5ProxyDesc => "host:port (blank = none).",
        L10nKey::SettingsHttpProxy => "HTTP proxy",
        L10nKey::SettingsHttpProxyDesc => "host:port (blank = none).",
        L10nKey::SettingsKexAlgorithms => "KEX algorithms",
        L10nKey::SettingsKexAlgorithmsDesc => "Comma-separated (blank = library default).",
        L10nKey::SettingsCiphers => "Ciphers",
        L10nKey::SettingsCiphersDesc => "Comma-separated (blank = default).",
        L10nKey::SettingsMacs => "MACs",
        L10nKey::SettingsMacsDesc => "Comma-separated (blank = default).",
        L10nKey::SettingsHostKeyAlgorithms => "Host-key algorithms",
        L10nKey::SettingsHostKeyAlgorithmsDesc => "Comma-separated (blank = default).",
        L10nKey::SettingsCompression => "Compression",
        L10nKey::SettingsJumpHostVia => "via {jump_name}",
        L10nKey::SettingsConnected => "connected",
        L10nKey::SettingsProfileCopied => "{name} (copy)",
        L10nKey::SettingsCompressionDesc => "Comma-separated (blank = default).",
        L10nKey::SettingsKeepaliveInterval => "Keepalive interval (s)",
        L10nKey::SettingsKeepaliveIntervalDesc => "Blank = library default.",
        L10nKey::SettingsKeepaliveCountMax => "Keepalive count max",
        L10nKey::SettingsKeepaliveCountMaxDesc => "Missed keepalives before dead.",
        L10nKey::SettingsConnectTimeout => "Connect timeout (s)",
        L10nKey::SettingsConnectTimeoutDesc => "Blank = library default.",
        L10nKey::SettingsX11Forwarding => "X11 forwarding",
        L10nKey::SettingsX11ForwardingDesc => "Request X11 forwarding (needs XQuartz on macOS).",
        L10nKey::SettingsShellIntegration => "Shell integration",
        L10nKey::SettingsShellIntegrationDesc => {
            "Let the remote shell report prompts, exit codes and directory."
        }
        L10nKey::SettingsLoginScripts => "Login scripts",
        L10nKey::SettingsLoginScriptsDesc => "Commands sent after the shell opens, one per line.",
        L10nKey::SettingsSkipBanner => "Skip banner",
        L10nKey::SettingsSkipBannerDesc => "Suppress the server login banner.",
        L10nKey::SettingsDefaultFollowsDefaults => "Default follows Defaults, which is {value}.",
        L10nKey::SettingsValueOn => "on",
        L10nKey::SettingsValueOff => "off",
        L10nKey::SettingsDefault => "Default",
        L10nKey::SettingsOn => "On",
        L10nKey::SettingsOff => "Off",
        L10nKey::SettingsShell => "Shell",
        L10nKey::SettingsShellIntro => {
            "The program each new terminal launches. Leave Program empty to use the platform default ({default})."
        }
        L10nKey::SettingsProgram => "Program",
        L10nKey::SettingsProgramDesc => {
            "Executable name on PATH or an absolute path. e.g. zsh, fish, pwsh."
        }
        L10nKey::SettingsArguments => "Arguments",
        L10nKey::SettingsArgumentsDesc => {
            "Space-separated launch flags. e.g. -l for a login shell."
        }
        L10nKey::SettingsStartIn => "Start in",
        L10nKey::SettingsStartInDesc => {
            "What a fresh shell starts in: tty7's launch directory, your home folder, or a fixed path."
        }
        L10nKey::SettingsCustomPath => "Custom path",
        L10nKey::SettingsCustomPathDesc => "The directory new shells start in.",
        L10nKey::SettingsWdInherit => "Inherit",
        L10nKey::SettingsWdHome => "Home",
        L10nKey::SettingsWdCustom => "Custom",
        L10nKey::SettingsShellFooter => {
            "Applies to shells with nothing to inherit — like the first tab of a window. New tabs and splits keep inheriting the active pane's directory, and shells already open keep running."
        }
        L10nKey::SettingsScrolling => "Scrolling",
        L10nKey::SettingsScrollback => "Scrollback",
        L10nKey::SettingsScrollbackDesc => "Lines of history kept per pane. Applies to new panes.",
        L10nKey::SettingsScrollSpeed => "Scroll speed",
        L10nKey::SettingsScrollSpeedDesc => "Multiplier applied to mouse-wheel scrolling.",
        L10nKey::SettingsMouse => "Mouse",
        L10nKey::SettingsFocusFollowsMouse => "Focus follows mouse",
        L10nKey::SettingsFocusFollowsMouseDesc => "Hovering a pane focuses it without a click.",
        L10nKey::SettingsHideMouseWhileTyping => "Hide mouse while typing",
        L10nKey::SettingsHideMouseWhileTypingDesc => {
            "Hide the pointer as you type; it returns on the next move."
        }
        L10nKey::SettingsReportMouseToApps => "Report mouse to apps",
        L10nKey::SettingsReportMouseToAppsDesc => {
            "Let full-screen apps (vim, tmux) handle clicks and scrolling; hold Shift to keep a gesture local."
        }
        L10nKey::SettingsBell => "Bell",
        L10nKey::SettingsTerminalBell => "Terminal bell",
        L10nKey::SettingsTerminalBellDesc => {
            "How a bell (^G) is signalled: silenced, a brief flash, the system sound, or both."
        }
        L10nKey::SettingsLinks => "Links",
        L10nKey::DetectUrls => "Detect URLs",
        L10nKey::SettingsDetectUrlsDesc => {
            "Underline links on hover and open them on {modifier}-click."
        }
        L10nKey::ForwardSshLoopbackLinks => "Forward SSH loopback links",
        L10nKey::SettingsForwardSshLoopbackLinksDesc => {
            "When a pane is in SSH, open localhost links through a temporary port forward."
        }
        L10nKey::OpenFilesWith => "Open files with",
        L10nKey::SettingsOpenFilesWithDesc => {
            "Command run when {modifier}-clicking a file link, instead of the default app. Use {path}, {line}, {column}; a flag whose value is absent is dropped (e.g. herdr edit {path} --line={line}). Empty uses the default app."
        }
        L10nKey::SettingsBellModeOff => "Off",
        L10nKey::SettingsBellModeVisual => "Visual",
        L10nKey::SettingsBellModeAudible => "Audible",
        L10nKey::SettingsBellModeBoth => "Both",
        L10nKey::SettingsPrompt => "Prompt",
        L10nKey::SettingsPromptIntro => {
            "tty7's own menus at the shell prompt. Turn one off to hand the key back to the shell."
        }
        L10nKey::SettingsTabCompletion => "Tab completion",
        L10nKey::SettingsTabCompletionDesc => {
            "Tab at the prompt opens tty7's completion menu. When off, Tab goes to the shell's own completion instead."
        }
        L10nKey::SettingsHistorySearch => "History search",
        L10nKey::SettingsHistorySearchDesc => {
            "⌃R at the prompt opens tty7's fuzzy history menu. When off, ⌃R goes to the shell instead — its own reverse-i-search, or whatever you've bound there (fzf, percol)."
        }
        L10nKey::SettingsSelectionClipboard => "Selection & clipboard",
        L10nKey::SettingsSmartSelection => "Smart selection",
        L10nKey::SettingsSmartSelectionDesc => {
            "Double-click selects the whole URL, file path, email, or bracket pair under the cursor."
        }
        L10nKey::SettingsCopyOnSelect => "Copy on select",
        L10nKey::SettingsCopyOnSelectDesc => {
            "Selecting text with the mouse copies it to the clipboard right away, no ⌘C needed."
        }
        L10nKey::SettingsTrimTrailingSpaces => "Trim trailing spaces on copy",
        L10nKey::SettingsTrimTrailingSpacesDesc => {
            "Strip trailing whitespace from each copied line."
        }
        L10nKey::SettingsKeyboard => "Keyboard",
        L10nKey::SettingsOptionAsMeta => "Option (⌥) acts as Meta",
        L10nKey::SettingsOptionAsMetaDesc => {
            "⌥+key sends the escape chord shells expect (⌥B = back one word) instead of typing a special character (∫)."
        }
        L10nKey::SettingsAgentsIntro => "Agents",
        L10nKey::SettingsAgentsIntroDesc => {
            "Hook integrations give panes running these agents live session status (working / waiting / done) in the tab bar. Only active inside tty7."
        }
        L10nKey::SettingsReadingAgentConfig => "Reading this machine's agent config…",
        L10nKey::SettingsStatusNotInstalled => "Not installed",
        L10nKey::SettingsStatusInstalled => "Installed",
        L10nKey::SettingsStatusOutdated => "Outdated",
        L10nKey::SettingsInstall => "Install",
        L10nKey::SettingsReinstall => "Reinstall",
        L10nKey::SettingsUpdate => "Update",
        L10nKey::SettingsUninstall => "Uninstall",
        L10nKey::SettingsOfflineMachines => {
            "{count} more saved machines are not connected — open a workspace on one to install its hooks there."
        }
        L10nKey::SettingsSyncWithSystem => "Sync with system",
        L10nKey::SettingsSyncWithSystemDesc => {
            "Follow the OS appearance with separate light and dark themes."
        }
        L10nKey::SettingsChangeTheme => "Change theme",
        L10nKey::SettingsThemes => "Themes",
        L10nKey::SettingsThemePanelManual => "Change your current theme.",
        L10nKey::SettingsThemePanelLight => "Choose the theme for light mode.",
        L10nKey::SettingsThemePanelDark => "Choose the theme for dark mode.",
        L10nKey::SettingsCustom => "Custom",
        L10nKey::SettingsBuiltIn => "Built-in",
        L10nKey::SettingsDark => "Dark",
        L10nKey::SettingsLight => "Light",
        L10nKey::SettingsLightMode => "Light mode",
        L10nKey::SettingsDarkMode => "Dark mode",
        L10nKey::SettingsActive => "Active",
        L10nKey::SettingsStartupWindow => "Startup window",
        L10nKey::SettingsStartupWindowDesc => "Window state when tty7 launches.",
        L10nKey::SettingsRememberWindowSize => "Remember window size & position",
        L10nKey::SettingsRememberWindowSizeDesc => {
            "Reopen at the size and position the window had when tty7 last quit. Off opens centered at the default size."
        }
        L10nKey::SettingsRestoreLastLayout => "Restore last layout",
        L10nKey::SettingsRestoreLastLayoutDesc => {
            "Reopen the last window's tabs, splits, and directories on launch. Off starts with a single fresh terminal."
        }
        L10nKey::SettingsShowTrayIcon => "Show tray icon",
        L10nKey::SettingsShowTrayIconDesc => {
            "Keep a status item in the system tray / menu bar: it signals when a coding agent needs your input, and its menu jumps to agent panes."
        }
        L10nKey::SettingsTabs => "Tabs",
        L10nKey::SettingsNewTabPosition => "New tab position",
        L10nKey::SettingsNewTabPositionDesc => "Where a freshly opened tab is inserted.",
        L10nKey::SettingsTabBarPosition => "Tab bar position",
        L10nKey::SettingsTabBarPositionDesc => {
            "Show tabs as a horizontal strip on top or a vertical sidebar on the left."
        }
        L10nKey::SettingsSidebarGrouping => "Sidebar grouping",
        L10nKey::SettingsSidebarGroupingDesc => {
            "Group sidebar tabs under a header per git repository, with non-repo tabs in a Scratch section. Only applies to the left sidebar."
        }
        L10nKey::SettingsDiffPreviewFromCounts => "Open diff preview from sidebar counts",
        L10nKey::SettingsDiffPreviewFromCountsDesc => {
            "Click a row's +N −N to open the working-tree diff in an overlay. Off keeps the branch and the counts on the row and just stops them being clickable."
        }
        L10nKey::SettingsNotifications => "Notifications",
        L10nKey::SettingsNotifyOnCommandFinish => "Notify on command finish",
        L10nKey::SettingsNotifyOnCommandFinishDesc => {
            "Desktop alert after a long foreground command completes."
        }
        L10nKey::SettingsNotifyThreshold => "Notify threshold",
        L10nKey::SettingsNotifyThresholdDesc => {
            "How long a command must run to qualify as \"long\"."
        }
        L10nKey::SettingsWindow => "Window",
        L10nKey::NotifyModeNever => "Never",
        L10nKey::NotifyModeUnfocused => "When Unfocused",
        L10nKey::NotifyModeAlways => "Always",
        L10nKey::SettingsStartupNormal => "Normal",
        L10nKey::SettingsStartupMaximized => "Maximized",
        L10nKey::SettingsStartupFullscreen => "Fullscreen",
        L10nKey::SettingsAfterCurrent => "After current",
        L10nKey::SettingsAtEnd => "At end",
        L10nKey::SettingsTop => "Top",
        L10nKey::SettingsLeft => "Left",
        L10nKey::SettingsByRepo => "By repo",
        L10nKey::SettingsFlat => "Flat",
        L10nKey::SettingsPreset => "Preset",
        L10nKey::SettingsPresetDesc => {
            "tmux remaps pane/tab actions onto prefix sequences (e.g. Ctrl-B then C)."
        }
        L10nKey::SettingsPrefix => "Prefix",
        L10nKey::SettingsPressKeys => "Press keys…",
        L10nKey::SettingsPauseToSaveEsc => "pause to save · Esc",
        L10nKey::SettingsKeybindingsIntroDesc => {
            "Click a shortcut, then press the new keys — it saves after a brief pause. Chain keys for a sequence like Ctrl-B then X. Esc cancels; Backspace removes the last key, or resets the shortcut to default when pressed first."
        }
        L10nKey::SettingsPrefixNote => {
            "With a prefix active, a bare prefix key reaches the shell after a ~1s pause, and prefix + an unbound key is sent through to the terminal."
        }
        L10nKey::SettingsRestoreAllDefaults => "Restore all defaults",
        L10nKey::SettingsAboutDesc1 => {
            "A terminal workbench: persistent sessions, remote work, agents."
        }
        L10nKey::SettingsVersion => "Version",
        L10nKey::SettingsUpdates => "Updates",
        L10nKey::SettingsUpdateAndRelaunch => "Update and Relaunch",
        L10nKey::SettingsUpdateViewRelease => "View Release",
        L10nKey::SettingsUpdateChecking => "Checking for updates…",
        L10nKey::SettingsUpdateUpToDate => "You're running the latest version.",
        L10nKey::SettingsUpdateDownloading => "Downloading and verifying the update…",
        L10nKey::SettingsUpdateInstalling => "Relaunching with the update…",
        L10nKey::SettingsUpdateCheckNow => "Check Now",
        L10nKey::SettingsUpdateCheckFailed => "Could not check for updates: {error}",
        L10nKey::SettingsUpdatePrepareFailed => "Update failed: {error}",
        L10nKey::SettingsUpdateLaunchFailed => "Could not start the installer: {error}",
        L10nKey::SettingsUpdateUnsupportedMacos => {
            "This copy is not running from a writable tty7.app bundle, so replacing it would be unsafe. Move tty7 to Applications or another writable folder, or open the release page to install the update."
        }
        L10nKey::SettingsUpdateUnsupportedLinux => {
            "The first in-app updater supports packaged macOS app bundles. Use the release page or your package manager to update this Linux installation."
        }
        L10nKey::SettingsUpdateUnsupportedWindows => {
            "Automatic Windows updates are available for recognized Inno Setup and portable ZIP installations. This copy is missing a valid installation marker, updater, or writable portable directory, so open the release page to update it manually."
        }
        L10nKey::SettingsUpdateWindowsAllUsers => {
            "tty7 is installed for all users, which needs administrator rights to replace. tty7 will not raise an elevation prompt on its own behalf, so open the release page and run the installer yourself to update it."
        }
        L10nKey::SettingsUpdateUnsupportedPlatform => {
            "Automatic installation is not available on this platform. Open the release page."
        }
        L10nKey::SettingsUpdateMissingPackage => {
            "The release has no {name} package for this installation. Open the release page to choose another package."
        }
        L10nKey::SettingsUpdateMissingChecksums => {
            "The release has no checksums.txt, so tty7 refuses to install it automatically."
        }
        L10nKey::SettingsVersionAvailable => "Version {version} is available.",
        L10nKey::SettingsCheckUpdatesDesc => {
            "Installations that cannot update in place open the release page instead."
        }
        L10nKey::SettingsCheckUpdatesOnLaunch => "Check for updates on launch",
        L10nKey::SettingsCommandLine => "Command line",
        L10nKey::SettingsCommandLineDesc => {
            "Put the bundled `tty7` command on your PATH at launch, so scripts and coding agents can drive tty7 from any terminal. Inside a tty7 pane it works either way. Turn this off if you keep your own `tty7` — one you built or installed yourself — and do not want it shadowed. Takes effect at next launch."
        }
        L10nKey::SettingsInstallCliOnPath => "Install the `tty7` command on PATH",
        L10nKey::SettingsServer => "Server",
        L10nKey::SettingsServerDesc => {
            "Restarts the background server that keeps your shells running. This ends every shell on this computer; your tabs and layout reopen with fresh ones."
        }
        L10nKey::SettingsRestartServer => "Restart server…",
        L10nKey::SettingsAppHttpProxy => "Proxy for updates",
        L10nKey::SettingsAppHttpProxyDesc => {
            "Used only for tty7's update checks and downloads, not for programs in your panes. Empty follows the system proxy."
        }
        L10nKey::SettingsAppHttpProxyInvalid => {
            "Not a valid proxy address — this value was not saved."
        }
        L10nKey::SettingsAgentClaudeCode => "Claude Code",
        L10nKey::SettingsAgentCodex => "Codex",
        L10nKey::SettingsAgentCopilotCli => "Copilot CLI",
        L10nKey::SettingsAgentOpencode => "OpenCode",
        L10nKey::SettingsAgentPi => "Pi",
        L10nKey::SettingsAgentGrokBuild => "Grok Build",
        L10nKey::SettingsSearchAboutKeywords => "version license credits build update check github",
        L10nKey::SettingsSearchAppHttpProxyKeywords => {
            "proxy http https socks socks5 clash v2ray network download update"
        }
        L10nKey::SettingsSearchAnsiColorsKeywords => "palette 16 terminal colours theme",
        L10nKey::SettingsSearchArgumentsKeywords => "shell flags login args",
        L10nKey::SettingsSearchBlurKeywords => {
            "transparency translucent frosted vibrancy window background"
        }
        L10nKey::SettingsSearchBoldFontKeywords => "typeface weight",
        L10nKey::SettingsSearchClaudeCodeKeywords => {
            "agent integration hooks install uninstall status rich session working waiting tab bar sidebar badge claude"
        }
        L10nKey::SettingsSearchCodexKeywords => "agent integration hooks install openai codex",
        L10nKey::SettingsSearchCommandLineToolKeywords => {
            "cli tty7 path shell command install symlink terminal iterm agent script"
        }
        L10nKey::SettingsSearchCommandLineToolTitle => "Command line tool",
        L10nKey::SettingsSearchCopilotCliKeywords => {
            "agent integration hooks install github copilot"
        }
        L10nKey::SettingsSearchCopyOnSelectKeywords => "clipboard selection yank mouse",
        L10nKey::SettingsSearchCursorBlinkKeywords => "caret blinking flash",
        L10nKey::SettingsSearchCursorShapeKeywords => "caret block bar underline beam",
        L10nKey::SettingsSearchCustomThemesKeywords => {
            "theme duplicate edit colors folder yaml import"
        }
        L10nKey::SettingsSearchDetectUrlsKeywords => "links hyperlink clickable open",
        L10nKey::SettingsSearchDiffPreviewFromCountsKeywords => {
            "diff overlay preview sidebar counts git changes click branch lines"
        }
        L10nKey::SettingsSearchDimInactivePanesKeywords => {
            "fade unfocused inactive split pane focus opacity highlight active dimming"
        }
        L10nKey::SettingsSearchFocusFollowsMouseKeywords => "pane hover activate",
        L10nKey::SettingsSearchFontFamilyKeywords => "typeface monospace typography",
        L10nKey::SettingsSearchFontLigaturesKeywords => "typography glyph fira",
        L10nKey::SettingsSearchFontSizeKeywords => "typography text bigger smaller zoom",
        L10nKey::SettingsSearchForwardSshLoopbackLinksKeywords => {
            "ssh remote port tunnel localhost forward links"
        }
        L10nKey::SettingsSearchGrokBuildKeywords => {
            "agent integration hooks install xai grok build"
        }
        L10nKey::SettingsSearchHideMouseWhileTypingKeywords => "cursor pointer autohide",
        L10nKey::SettingsSearchHistorySearchKeywords => {
            "ctrl-r reverse search fuzzy history recall fzf prompt"
        }
        L10nKey::SettingsSearchHostsKeywords => {
            "ssh host connection saved profile import ssh_config manage add edit quick connect"
        }
        L10nKey::SettingsSearchHowShellsWorkKeywords => {
            "shell session daemon server detach persist background close quit stop delete workspace layout survive reboot tmux"
        }
        L10nKey::SettingsSearchHowShellsWorkTitle => "How shells work",
        L10nKey::SettingsSearchItalicFontKeywords => "typeface oblique",
        L10nKey::SettingsSearchKeybindingsKeywords => {
            "shortcut hotkey keyboard binding chord tmux preset rebind prefix"
        }
        L10nKey::SettingsSearchKeybindingsTitle => "Keybindings",
        L10nKey::SettingsSearchLineHeightKeywords => "typography leading spacing",
        L10nKey::SettingsSearchNewTabPositionKeywords => "tabs order end after current",
        L10nKey::SettingsSearchNotifyOnCommandFinishKeywords => {
            "notification alert done osc desktop banner long command"
        }
        L10nKey::SettingsSearchNotifyThresholdKeywords => {
            "notification alert seconds duration long command delay"
        }
        L10nKey::SettingsSearchOpacityKeywords => {
            "transparency translucent see through window alpha"
        }
        L10nKey::SettingsSearchOpenFilesWithKeywords => {
            "links file editor command external app path line column"
        }
        L10nKey::SettingsSearchOpencodeKeywords => "agent integration plugin install opencode",
        L10nKey::SettingsSearchOptionAsMetaKeywords => {
            "alt keyboard modifier escape macos option meta option acts as meta"
        }
        L10nKey::SettingsSearchPiKeywords => "agent integration extension install pi",
        L10nKey::SettingsSearchPortForwardingKeywords => {
            "ssh tunnel local remote dynamic socks forward rule"
        }
        L10nKey::SettingsSearchProgramKeywords => {
            "shell binary zsh bash fish nu nushell pwsh powershell executable launch"
        }
        L10nKey::SettingsSearchRememberWindowSizeKeywords => {
            "window size position bounds geometry launch startup remember"
        }
        L10nKey::SettingsSearchReportMouseToAppsKeywords => {
            "mouse reporting vim tmux click scroll shift passthrough"
        }
        L10nKey::SettingsSearchRestoreLastLayoutKeywords => {
            "restore session previous tabs splits reopen launch startup layout"
        }
        L10nKey::SettingsSearchScrollSpeedKeywords => "mouse wheel multiplier scrolling",
        L10nKey::SettingsSearchScrollbackKeywords => "history buffer lines scroll",
        L10nKey::SettingsSearchShowTrayIconKeywords => {
            "tray menu bar status item agent attention system icon"
        }
        L10nKey::SettingsSearchSidebarGroupingKeywords => {
            "tabs group repo repository git scratch header sidebar flat"
        }
        L10nKey::SettingsSearchSmartSelectionKeywords => {
            "double click word url path select semantic bracket email"
        }
        L10nKey::SettingsSearchStartInKeywords => {
            "cwd working directory start folder path home inherit custom"
        }
        L10nKey::SettingsSearchSyncWithSystemKeywords => {
            "theme dark light auto follow os appearance mode"
        }
        L10nKey::SettingsSearchTabBarPositionKeywords => {
            "tabs vertical sidebar left top layout rail"
        }
        L10nKey::SettingsSearchTabCompletionKeywords => {
            "complete completion menu suggestions tab prompt"
        }
        L10nKey::SettingsSearchTerminalBellKeywords => {
            "bell audible visual flash sound silence beep both ^g"
        }
        L10nKey::SettingsSearchThemeKeywords => {
            "appearance color colours scheme dark light palette background foreground accent sync system os auto follow"
        }
        L10nKey::SettingsSearchTrimTrailingSpacesKeywords => "clipboard whitespace copy",
        L10nKey::SettingsSearchVerifyHostKeysKeywords => {
            "ssh security known_hosts fingerprint mitm host key verification"
        }
        L10nKey::SettingsSearchWarnBeforeClosingKeywords => {
            "ssh confirm close tab pane live session security"
        }
        L10nKey::SettingsSearchStartupWindowKeywords => "launch open maximized fullscreen normal",
        L10nKey::SwitcherNoMatch => "No workspace or machine matches.",
        L10nKey::AddSshHost => "Add SSH Host…",
        L10nKey::ClickForNewWindow => "click for a new window",
        L10nKey::RestartServer => "Restart Server",
        L10nKey::OtherMachines => "Other Machines",
        L10nKey::Ok => "OK",
        L10nKey::SftpNoTransfers => "No transfers yet.",
        L10nKey::SftpPanelTitleFiles => "Files",
        L10nKey::SftpTooltipRefresh => "Refresh",
        L10nKey::SftpTooltipMore => "More",
        L10nKey::SftpMenuNewFolder => "New folder",
        L10nKey::SftpMenuNewFile => "New file",
        L10nKey::SftpMenuUpload => "Upload…",
        L10nKey::SftpMenuGotoShellCwd => "Go to shell directory",
        L10nKey::SftpMenuHideTransferHistory => "Hide transfer history",
        L10nKey::SftpMenuTransferHistory => "Transfer history",
        L10nKey::SftpEditNewFolder => "New folder",
        L10nKey::SftpEditNewFile => "New file",
        L10nKey::SftpEditRename => "Rename",
        L10nKey::SftpEditPermissions => "Permissions · {mode}",
        L10nKey::SftpLoading => "Loading…",
        L10nKey::SftpEmptyDirectory => "Empty directory.",
        L10nKey::SftpContextOpen => "Open",
        L10nKey::SftpContextFollowSymlink => "Follow symlink",
        L10nKey::SftpContextRename => "Rename",
        L10nKey::SftpContextChmod => "chmod…",
        L10nKey::SftpTransferSummaryRunning => "{count} transferring · {pct}%",
        L10nKey::SftpTransferSummaryFailed => "{count} failed",
        L10nKey::SftpTransferSummaryIdle => "Transfers",
        L10nKey::SftpTransferProgress => "{done} / {total} ({pct}%)",
        L10nKey::SftpTransferDone => "done",
        L10nKey::SftpTransferCancelled => "cancelled",
        L10nKey::SftpTransferError => "error",
        L10nKey::SftpImagePasteUploadFailed => {
            "Could not upload the pasted image to {host}: {error}"
        }
        L10nKey::ForwardPanelTitle => "Forwards",
        L10nKey::ForwardDisconnected => "Disconnected",
        L10nKey::ForwardDisconnectedFrom => "Disconnected from {host}",
        L10nKey::ForwardTooltipAdd => "Add forward",
        L10nKey::ForwardTooltipRemove => "Remove",
        L10nKey::ForwardLocal => "Local",
        L10nKey::ForwardRemote => "Remote",
        L10nKey::ForwardDynamic => "Dynamic",
        L10nKey::ForwardBindLabel => "bind",
        L10nKey::ForwardToLabel => "to",
        L10nKey::ForwardSocksLabel => "SOCKS",
        L10nKey::ForwardAdd => "Add",
        L10nKey::FileTreePlaceholderFileName => "file name",
        L10nKey::FileTreePlaceholderFolderName => "folder name",
        L10nKey::FileTreePlaceholderNewName => "new name",
        L10nKey::FileTreeDeleteTitle => "Delete \"{name}\"?",
        L10nKey::FileTreeDeleteFolderBody => "The folder and everything inside it will be deleted.",
        L10nKey::FileTreeDeleteFileBody => "The file will be deleted.",
        L10nKey::FileTreeDeleteFailed => "Delete failed",
        L10nKey::FileTreeContextOpen => "Open",
        L10nKey::FileTreeContextCdHere => "cd Here",
        L10nKey::FileTreeContextInsertPath => "Insert Path in Terminal",
        L10nKey::FileTreeContextAttachAgent => "Attach to Agent",
        L10nKey::FileTreeContextNewFile => "New File",
        L10nKey::FileTreeContextNewFolder => "New Folder",
        L10nKey::FileTreeContextRename => "Rename",
        L10nKey::FileTreeContextCopyPath => "Copy Path",
        L10nKey::FileTreeContextHideDotfiles => "Hide Dotfiles",
        L10nKey::FileTreeContextShowDotfiles => "Show Dotfiles",
        L10nKey::SshPromptNewKey => "new {fingerprint}",
        L10nKey::SshPromptOldKey => "old {old_fingerprint}",
        L10nKey::EditorCantOpen => "Can't open {path}: {e}",
        L10nKey::EditorCantRead => "Can't read {path}: {e}",
        L10nKey::EditorNotUtf8 => "\"{path}\" is not valid UTF-8",
        L10nKey::EditorSaveFailed => "Save failed",
        L10nKey::EditorUnsavedChanges => "\"{name}\" has unsaved changes",
        L10nKey::EditorDiscard => "Discard",
        L10nKey::EditorNoFileOpen => "No file open",
        L10nKey::EditorBackToTerminal => "Back to Terminal (Esc)",
        L10nKey::EditorLnCol => "Ln {line}, Col {column}",
        L10nKey::EditorEdit => "Edit",
        L10nKey::EditorPreview => "Preview",
        L10nKey::EditorWrapOn => "Wrap: on",
        L10nKey::EditorWrapOff => "Wrap: off",
        L10nKey::EditorFileTooLarge => "\"{path}\" is too large for the editor ({size} MB)",
        L10nKey::EditorBinaryFile => "\"{path}\" looks like a binary file",
        L10nKey::PanelInfoTitle => "Info",
        L10nKey::PanelChangesTitle => "Changes",
        L10nKey::PanelFilesTitle => "Files",
        L10nKey::PanelNoSession => "No active session.",
        L10nKey::PanelNoSessionHint => {
            "Open a tab to see its shell, directory, and processes here."
        }
        L10nKey::PanelNoWorkingDirectory => "No working directory.",
        L10nKey::PanelNoWorkingDirectoryHint => "This pane has not reported one yet.",
        L10nKey::PanelLoading => "Loading…",
        L10nKey::PanelNotAGitRepo => "Not a git repository.",
        L10nKey::PanelNotAGitRepoHint => "cd into one and this tab lists its uncommitted changes.",
        L10nKey::PanelNoChanges => "No uncommitted changes.",
        L10nKey::PanelNoChangesHint => "The working tree is clean.",
        L10nKey::PanelSessionSubtitle => "Session",
        L10nKey::PanelProcessesSubtitle => "Processes",
        L10nKey::PanelPortsSubtitle => "Ports",
        L10nKey::PanelCwd => "cwd",
        L10nKey::PanelShell => "shell",
        L10nKey::PanelSsh => "ssh",
        L10nKey::PanelBranch => "branch",
        L10nKey::PanelChangesRow => "changes",
        L10nKey::PanelAgent => "agent",
        L10nKey::PanelAgentIdle => "idle",
        L10nKey::PanelAgentWorking => "working",
        L10nKey::PanelAgentWaiting => "waiting",
        L10nKey::PanelAgentDone => "done",
        L10nKey::PanelRevealInFinder => "Reveal in Finder",
        L10nKey::PanelOpenFolder => "Open Folder",
        L10nKey::WindowStop => "Stop",
        L10nKey::WindowDelete => "Delete",
        L10nKey::WindowThisWorkspace => "this workspace",
        L10nKey::WindowConfirmTitle => "{verb} Workspace \"{name}\"?",
        L10nKey::WindowStopUnreachable => {
            "Its machine could not be reached. Any shells still running there will be ended."
        }
        L10nKey::WindowDeleteUnreachable => {
            "Its machine could not be reached. Any shells still running there will be ended, and the layout forgotten."
        }
        L10nKey::WindowStopShells => "{count} running shells will be ended.",
        L10nKey::WindowDeleteShells => {
            "{count} running shells will be ended and the layout forgotten."
        }
        L10nKey::DiffReading => "Reading diff…",
        L10nKey::DiffNotARepo => "Not a git repository",
        L10nKey::DiffReadFailed => {
            "Couldn't read the working-tree diff — retrying on the next refresh."
        }
        L10nKey::DiffWorkingTreeClean => "Working tree clean",
        L10nKey::DiffCloseTooltip => "Close Diff (Esc)",
        L10nKey::DiffChangedFiles => "{count} changed files",
        L10nKey::DiffUntrackedCount => " · {count} untracked",
        L10nKey::DiffMoreFiles => {
            "… and {count} more changed files — run `git diff` in the terminal to see them."
        }
        L10nKey::DiffOversizedNotice => {
            "This working tree is too large to render efficiently ({summary}). Every file is collapsed — expand individual files, or run `git diff` in the terminal."
        }
        L10nKey::DiffTruncatedPerFile => {
            "Diff truncated at {limit} lines — run `git diff` in the terminal for the rest."
        }
        L10nKey::DiffTruncatedBudget => {
            "Body not loaded — this working tree is past tty7's diff budget. Run `git diff` in the terminal for this file."
        }
        L10nKey::DiffUntrackedHeader => "Untracked files ({count})",
        L10nKey::DiffMoreUntracked => {
            "… and {count} more — run `git status` in the terminal to see them."
        }
        L10nKey::DiffLines => "{count} diff lines",
        L10nKey::DiffChangedLines => {
            "{total} changed lines, {loaded} diff rows loaded before {cap} cut the rest"
        }
        L10nKey::DiffBudgetAndCap => "tty7's budget and the per-file cap",
        L10nKey::DiffBudget => "tty7's budget",
        L10nKey::DiffPerFileCap => "the per-file cap",
        L10nKey::DiffUntrackedSummary => "{count} untracked",
        L10nKey::PendingConnecting => "Connecting to {machine}…",
        L10nKey::PendingUnreachable => "Couldn't reach {machine}",
        L10nKey::WorktreePromptNeedsName => "The worktree needs a name",
        L10nKey::WorktreePromptTitle => "New Worktree Tab",
        L10nKey::WorktreePromptName => "Worktree Name",
        L10nKey::WorktreePromptBranch => "New Branch",
        L10nKey::WorktreePromptBase => "Start From",
        L10nKey::WorktreePromptCreating => "Creating…",
        L10nKey::WorktreePromptCreate => "Create",
        L10nKey::AppNewWorktreeFailed => "New worktree failed: {error}",
        L10nKey::HomeTimeJustNow => "just now",
        L10nKey::HomeTimeMinutesAgo => "{count} min ago",
        L10nKey::HomeTimeHourAgo => "1 hour ago",
        L10nKey::HomeTimeHoursAgo => "{count} hours ago",
        L10nKey::HomeTimeYesterday => "yesterday",
        L10nKey::HomeTimeDaysAgo => "{count} days ago",
        L10nKey::HomeTimeOverWeekAgo => "over a week ago",
        L10nKey::HomeReopenNamed => "Reopen \"{name}\"",
        L10nKey::RemoteStripDisconnected => "Not connected to {machine}",
        L10nKey::RemoteStripConnecting => "Connecting to {machine}…",
        L10nKey::RemoteStripReconnecting => "Reconnecting to {machine}…",
        L10nKey::RemoteStripReconnectingAttempt => "Reconnecting to {machine}… (attempt {count})",
        L10nKey::RemoteStripPreempted => "This workspace was opened on {by}",
        L10nKey::RemoteStripFailed => "Not connected to {machine} — {error}",
        L10nKey::RemoteNoticePreempted => "Opened elsewhere — typing has no effect",
        L10nKey::RemoteNoticeDisconnected => "Not connected — typing has no effect",
        L10nKey::RemoteActionRetryNow => "Retry Now",
        L10nKey::RemoteActionTakeBack => "Take Back",
        L10nKey::RemoteActionConnect => "Connect",
        L10nKey::RemoteActionRetry => "Retry",
        L10nKey::RemoteNoConnectionDetails => {
            "This window is a workspace on {machine}, but tty7 has no connection \
             details for it any more — check that its SSH profile or ~/.ssh/config \
             entry still exists."
        }
        L10nKey::RemoteThisComputer => "this computer",
        L10nKey::RemoteRestartTitle => "Restart tty7's server on \"{machine}\"?",
        L10nKey::RemoteRestartBody => {
            "This stops every shell on {machine} — anything still running in them \
             will be terminated, including shells this window is not showing. \
             Workspaces and layouts are kept and come back with fresh shells."
        }
        L10nKey::RemoteReplaceBody => {
            "tty7 will install a matching server on {machine} and start it.\n\
             \n\
             Every session running on {machine} ends, including any this window is not \
             connected to."
        }
        L10nKey::RemoteRestartFailedTitle => "tty7's server on \"{machine}\" was not restarted",
        L10nKey::RemoteRestartFailedBody => {
            "{error}\n\
             \n\
             Sessions still running there are on the older build. If they are \
             gone, reconnecting starts this build's server."
        }
        L10nKey::RemoteHostUnreachable => "could not reach {machine}: {error}",
        L10nKey::RemoteInstallTitle => "Install tty7's server on \"{machine}\"?",
        L10nKey::RemoteInstallDetail => {
            "tty7 will write its server binary to {machine} so this machine can host \
             workspaces there. Nothing else on {machine} is touched, and no sudo is used.\n\
             \n\
             {path_label}\u{2003}{path}\n\
             {version_label}\u{2003}{version}\n\
             {size_label}\u{2003}{size}\n\
             {from_label}\u{2003}{from}\n\
             {sha_label}\u{2003}{sha256}\n\
             \n\
             {silent_upgrades}"
        }
        L10nKey::RemoteInstallPathLabel => "Path",
        L10nKey::RemoteInstallVersionLabel => "Version",
        L10nKey::RemoteInstallSizeLabel => "Size",
        L10nKey::RemoteInstallFromLabel => "From",
        L10nKey::RemoteInstallShaLabel => "SHA-256",
        L10nKey::RemoteInstallSilentUpgrades => "Later upgrades on this machine install silently.",
        L10nKey::RemoteInstallBytes => "bytes",
        L10nKey::RemoteMismatchTitle => "Update tty7's server on \"{machine}\"?",
        L10nKey::RemoteMismatchDetail => {
            "{machine} is serving tty7 sessions from {running}, which speaks a protocol \
             this client ({wanted}) cannot. tty7 has installed a matching server there, \
             but the one already running is the one your sessions are on.\n\
             \n\
             {replace_server}\u{2003}replaces it with {wanted} and ends every session it is hosting.\n\
             {cancel}\u{2003}leaves {machine} exactly as it is. This window will not connect."
        }
        L10nKey::RemoteMismatchReplaceServer => "Update Server",
        L10nKey::RemoteMismatchUnknownBuild => "an unknown build",
        L10nKey::RemoteMismatchUnknownBuildFromExe => "an unknown build (from {exe})",
        L10nKey::RemoteServerOutdated => {
            "{machine} is running an old tty7 server ({build}) that this copy of tty7 \
             cannot talk to. Update it to connect."
        }
        L10nKey::RemoteServerTooNew => {
            "{machine} is running a newer tty7 server ({build}) than this copy of tty7. \
             Update tty7 on this computer, or replace the server there with a matching one."
        }
        L10nKey::RemoteDaemonStartFailed => "tty7's local server could not be started: {error}",
        L10nKey::RemoteDaemonUnreachable => "could not reach tty7's local server: {error}",
        L10nKey::RemoteDaemonTooOld => {
            "this machine's tty7 daemon is an older build and cannot restart the server on \
             {machine}. Quit tty7 (which stops the daemon) and open it again, then retry."
        }
        L10nKey::RemoteProfileMissing => "that saved SSH profile no longer exists",
        L10nKey::RemoteAliasMissing => "`{alias}` is no longer in ~/.ssh/config",
        L10nKey::RemoteWslNoSsh => "a WSL workspace has no SSH connection",
        L10nKey::RemoteLocalStdioNoSsh => "a local --stdio workspace has no SSH connection",
        L10nKey::RemoteHostNotTty7 => "{machine} answered, but not as a tty7 server: {error}",
        L10nKey::RemoteWorkspaceListFailed => {
            "connected to {machine}, but its workspace list failed: {error}"
        }
        L10nKey::RemoteServerRestartFailed => {
            "could not restart tty7's server on {machine}: {error}"
        }
        L10nKey::RemoteNoRouteToHost => "tty7 no longer has a way to reach {machine}",
        L10nKey::RemoteMachineTreeUnexpectedReply => {
            "the server answered a machine tree with {reply}"
        }
        L10nKey::RemoteMismatchVersionFromExe => "{version} (from {exe})",
        L10nKey::AppNoRunningCodingAgent => {
            "No running coding agent found — start one (claude, codex, …) in a pane first."
        }
        L10nKey::SwitcherThisComputer => "This Computer",
        L10nKey::SwitcherRestartingServer => "Restarting tty7's server…",
        L10nKey::SwitcherDownloadingServerWithTotal => {
            "Downloading tty7's server… {done} / {total}"
        }
        L10nKey::SwitcherDownloadingServerNoTotal => "Downloading tty7's server… {done}",
        L10nKey::SwitcherCopyingServer => "Copying tty7's server… {done} / {total}",
        L10nKey::SwitcherThisWindow => "this window",
        L10nKey::SwitcherOpen => "open",
        L10nKey::SwitcherDisconnect => "Disconnect",
        L10nKey::SwitcherOpenInNewWindow => "Open in New Window",
        L10nKey::SwitcherRename => "Rename…",
        L10nKey::SwitcherPickAWorkspace => "Pick a workspace to see its tabs",
        L10nKey::SwitcherNoTabs => "No tabs in this workspace",
        L10nKey::SwitcherTabsAfterOpening => "Open this workspace to see its tabs",
        L10nKey::SwitcherTabCount => "{n} tabs",
        L10nKey::SwitcherTabCountOne => "1 tab",
        L10nKey::SwitcherActiveTab => "active",
        L10nKey::SwitcherHoldToSwitch => "Tab to move · release to switch",
        L10nKey::SshPromptPasswordFor => "Password for {user}@{host}",
        L10nKey::SshPromptPassphraseFor => "Passphrase for {key_path}",
        L10nKey::SshPromptTwoFactor => "Two-factor authentication",
        L10nKey::SshPromptUnknownHost => "Unknown host {host}",
        L10nKey::SshPromptHostKeyChanged => "Host key CHANGED — possible man-in-the-middle",
        L10nKey::SshPromptHostKeyChangedBody => {
            "The host key differs from the one previously trusted. This may be an attack."
        }
        L10nKey::SshPromptConnect => "Connect",
        L10nKey::SshPromptUnlock => "Unlock",
        L10nKey::SshPromptSubmit => "Submit",
        L10nKey::HostOpsError => "{context}: {error}",
        L10nKey::CmdGroupTabsPanes => "Tabs & Panes",
        L10nKey::CmdGroupWorkspaces => "Workspaces",
        L10nKey::CmdGroupView => "View",
        L10nKey::CmdGroupTerminal => "Terminal",
        L10nKey::CmdGroupSsh => "SSH",
        L10nKey::CmdGroupAgents => "Agents",
        L10nKey::CmdGroupApplication => "Application",
        L10nKey::CmdNewTab => "New Tab",
        L10nKey::CmdNewWorktreeTab => "New Worktree Tab",
        L10nKey::CmdNewWorktreeTabSubtitle => "isolated checkout on a fresh branch",
        L10nKey::CmdRenameTab => "Rename Tab…",
        L10nKey::CmdSplitRight => "Split Right",
        L10nKey::CmdSplitDown => "Split Down",
        L10nKey::CmdZoomPane => "Zoom Pane",
        L10nKey::CmdNextPane => "Next Pane",
        L10nKey::CmdPreviousPane => "Previous Pane",
        L10nKey::CmdFocusPaneLeft => "Focus Pane Left",
        L10nKey::CmdFocusPaneRight => "Focus Pane Right",
        L10nKey::CmdFocusPaneUp => "Focus Pane Up",
        L10nKey::CmdFocusPaneDown => "Focus Pane Down",
        L10nKey::CmdResizePaneLeft => "Resize Pane Left",
        L10nKey::CmdResizePaneRight => "Resize Pane Right",
        L10nKey::CmdResizePaneUp => "Resize Pane Up",
        L10nKey::CmdResizePaneDown => "Resize Pane Down",
        L10nKey::CmdSwapPaneNext => "Swap Pane Next",
        L10nKey::CmdSwapPanePrevious => "Swap Pane Previous",
        L10nKey::CmdNextTab => "Next Tab",
        L10nKey::CmdPreviousTab => "Previous Tab",
        L10nKey::CmdCopyWorkingDirectory => "Copy Working Directory",
        L10nKey::CmdCopySessionId => "Copy Session ID",
        L10nKey::CmdCopySessionIdSubtitle => "the coding agent's own session id",
        L10nKey::CmdForkSession => "Fork Session",
        L10nKey::CmdForkSessionSubtitle => "branch this agent session into a new tab",
        L10nKey::CmdMarkTabAsUnread => "Mark Tab as Unread",
        L10nKey::CmdClosePaneTab => "Close Pane / Tab",
        L10nKey::CmdCloseOtherTabs => "Close Other Tabs",
        L10nKey::CmdCloseTabsToTheRight => "Close Tabs to the Right",
        L10nKey::CmdReopenClosedTab => "Reopen Closed Tab",
        L10nKey::CmdNewWorkspace => "New Workspace",
        L10nKey::CmdSwitchWorkspace => "Switch Workspace…",
        L10nKey::CmdRenameWorkspace => "Rename Workspace…",
        L10nKey::CmdStopWorkspace => "Stop Workspace…",
        L10nKey::CmdStopWorkspaceSubtitle => "ends its shells, keeps the layout",
        L10nKey::CmdDeleteWorkspace => "Delete Workspace…",
        L10nKey::CmdDeleteWorkspaceSubtitle => "ends its shells and forgets the layout",
        L10nKey::CmdShowLeftSidebar => "Show Left Sidebar",
        L10nKey::CmdHideLeftSidebar => "Hide Left Sidebar",
        L10nKey::CmdHideRightPanel => "Hide Right Panel",
        L10nKey::CmdShowRightPanel => "Show Right Panel",
        L10nKey::CmdShowCodePanel => "Show Code Panel",
        L10nKey::CmdTabBarMoveToTop => "Tab Bar: Move to Top",
        L10nKey::CmdTabBarMoveToLeftSidebar => "Tab Bar: Move to Left Sidebar",
        L10nKey::CmdRightPanelInfo => "Right Panel: Info",
        L10nKey::CmdRightPanelChanges => "Right Panel: Changes",
        L10nKey::CmdRightPanelFiles => "Right Panel: Files",
        L10nKey::CmdChangeTheme => "Change Theme…",
        L10nKey::CmdResetFontSize => "Reset Font Size",
        L10nKey::CmdEnterFullScreen => "Enter Full Screen",
        L10nKey::CmdClearScrollback => "Clear Scrollback",
        L10nKey::CmdFindInTerminal => "Find in Terminal…",
        L10nKey::CmdFindNext => "Find Next",
        L10nKey::CmdFindPrevious => "Find Previous",
        L10nKey::CmdCopy => "Copy",
        L10nKey::CmdCut => "Cut",
        L10nKey::CmdPaste => "Paste",
        L10nKey::CmdSelectAll => "Select All",
        L10nKey::CmdSshAddConnection => "SSH: Add Connection…",
        L10nKey::CmdSshManageProfiles => "SSH: Manage Profiles…",
        L10nKey::CmdSshReconnect => "SSH: Reconnect",
        L10nKey::CmdSshRemoteFiles => "SSH: Remote Files",
        L10nKey::CmdSshPortForwarding => "SSH: Port Forwarding",
        L10nKey::CmdSshConnectWithInput => "SSH: Connect {input}",
        L10nKey::CmdAgentSendSelection => "Agent: Send Selection",
        L10nKey::CmdAgentSendSelectionSubtitle => "selection → running coding agent",
        L10nKey::CmdAgentSendGitDiffForReview => "Agent: Send Git Diff for Review",
        L10nKey::CmdAgentSendGitDiffSubtitle => "git diff → running coding agent",
        L10nKey::CmdSettings => "Settings…",
        L10nKey::CmdKeyboardShortcuts => "Keyboard Shortcuts",
        L10nKey::CmdAboutTty7 => "About tty7",
        L10nKey::CmdCheckForUpdates => "Check for Updates…",
        L10nKey::CmdDocumentation => "Documentation",
        L10nKey::CmdJoinDiscord => "Join the Discord",
        L10nKey::CmdReportIssue => "Report an Issue…",
        L10nKey::CmdRestartServer => "Restart Server…",
        L10nKey::CmdRestartServerSubtitle => "ends every running shell; layout is kept",
        L10nKey::CmdQuitTty7 => "Quit tty7",
        L10nKey::CmdQuitTty7Subtitle => "shells keep running",
        L10nKey::CmdQuickConnect => "Connect to \"{target}\"",
        L10nKey::CmdQuickConnectSaveProfile => "Save \"{target}\" as profile…",
        L10nKey::CmdRecent => "Recent",
        L10nKey::AppRestartServerTitle => "Restart Server?",
        L10nKey::AppRestartServerMismatchDetail => {
            "The server holding your shells is from another build (v{build}, protocol {protocol} — this app speaks {ours}). You can keep using it and your shells stay, but features whose wire format changed may misbehave until it's restarted. Restarting starts a clean server: tabs reopen with fresh shells and anything running in them is terminated."
        }
        L10nKey::AppRestartServerOldDetail => {
            "The server holding your shells is from an older version of the app. You can keep using it and your shells stay, but newer features may misbehave until it's restarted. Restarting starts a clean server: tabs reopen with fresh shells and anything running in them is terminated."
        }
        L10nKey::AppKeepShells => "Keep Shells",
        L10nKey::AppRestart => "Restart",
        L10nKey::AppRestartServerNotSsh => {
            "tty7 can only restart the server on machines it reaches over SSH. {label} is served from this computer — stop its workspace instead."
        }
        L10nKey::AppRestartServerBody => {
            "This stops every running shell on this computer — anything still running in them will be terminated. Your tabs and layout are kept and reopened with fresh shells."
        }
        L10nKey::AppWorktreeRemoveDetailDirty => {
            "The closed tab's worktree at {path} has uncommitted changes."
        }
        L10nKey::AppWorktreeRemoveDetailClean => "The closed tab's worktree at {path} is clean.",
        L10nKey::AppWorktreeRemoveTitle => "Remove worktree \"{branch}\"?",
        L10nKey::AppWorktreeDiscardAndRemove => "Discard Changes & Remove",
        L10nKey::AppWorktreeRemove => "Remove Worktree",
        L10nKey::AppWorktreeKeep => "Keep",
        L10nKey::AppReopenTabFailed => "Could not reopen the tab: no terminal started",
        L10nKey::AppOpenTerminalFailed => "Could not open a terminal: {error}",
        L10nKey::AppSshConnectionFailed => "SSH connection failed: {error}",
        L10nKey::AppSshReconnectFailed => "SSH reconnect failed: {error}",
        L10nKey::AppSplitPaneFailed => "Could not split the pane: {error}",
        L10nKey::AppWorktreeRemoved => "Removed worktree \"{branch}\"",
        L10nKey::AppWorktreeRemoveFailed => "Worktree removal failed: {error}",
        L10nKey::AppForkStillConnecting => "Could not fork: the pane is still connecting",
        L10nKey::AppPaneNoCodingAgent => "This pane isn't running a coding agent",
        L10nKey::AppForkNoCommand => "tty7 has no fork command for {name}",
        L10nKey::AppForkLocalOnly => "{name} sessions can only be forked from a local pane",
        L10nKey::AppForkNoSessionId => {
            "tty7 hasn't seen a {name} session id in this pane — install its hooks in Settings → Agents"
        }
        L10nKey::AppForkSessionIdNotToken => "{name}'s session id isn't a plain token",
        L10nKey::AppForkMidTurn => "{name} is mid-turn — the fork won't include the turn in flight",
        L10nKey::AppTabNoWorkingDirectory => "This tab has no working directory yet",
        L10nKey::AppNothingSelected => "Nothing selected — select some terminal output first.",
        L10nKey::AppPaneNoKnownDirectory => "This pane has no known directory.",
        L10nKey::AppNoUncommittedChanges => {
            "No uncommitted changes in {cwd} (or not a git repository)."
        }
        L10nKey::AppCmdSshProfileTitle => "SSH: {title}",
        L10nKey::AppCmdSwitchToTab => "Switch to Tab: {label}",
        L10nKey::AppPlaceholderDescription => "description",
        L10nKey::AppPlaceholderSshQuickConnect => "user@host  or  user@host:port",
        L10nKey::AppPlaceholderLoginShell => "login shell",
        L10nKey::AppPlaceholderNone => "none",
        L10nKey::AppPlaceholderOpenInDefaultApp => "open in default app",
        L10nKey::AppThemeColorBackground => "Background",
        L10nKey::AppThemeColorForeground => "Foreground",
        L10nKey::AppThemeColorAccent => "Accent",
        L10nKey::AppThemeColorCursor => "Cursor",
        L10nKey::AppThemeColorSelection => "Selection",
        L10nKey::AppAgentHooksThisComputer => "This Computer",
        L10nKey::AppAgentHooksRemoteMachine => "Remote machine",
        L10nKey::AppAgentHooksNoHomeDir => {
            "tty7 could not work out this computer's home directory, so there is nowhere to install to."
        }
        L10nKey::AppAgentHooksOffline => {
            "Not connected to this machine, so its agent config can't be read or written. Open a workspace on it and come back."
        }
        L10nKey::AppAgentHooksHomeDirUnresolved => "cannot resolve home directory",
        L10nKey::AppAgentHooksOpFailed => "Failed: {error}",
        L10nKey::AppKeybindingDisplacedNote => {
            "{action} took the shortcut from {previous}, which is now unset."
        }
        L10nKey::AppLocalServerName => "the local server",
        L10nKey::AppSshParseUnbalancedQuotes => "Unbalanced quotes in the SSH command",
        L10nKey::AppSshParseNoRemoteCommands => "Remote commands aren't supported here",
        L10nKey::AppSshParseFlagNeedsValue => "-{flag} needs a value",
        L10nKey::AppSshParseInvalidPort => "Invalid port \"{value}\"",
        L10nKey::AppSshParseUnsupportedOption => "Unsupported option \"{option}\"",
        L10nKey::AppSshParseEnterHost => "Enter a host to connect to",
        L10nKey::AppSshParseBadHost => "Can't parse host \"{host}\"",
        L10nKey::AppMenuMinimize => "Minimize",
        L10nKey::AppMenuZoom => "Zoom",
        L10nKey::SwitcherStatusRestarting => "restarting…",
        L10nKey::SwitcherStatusInstalling => "installing…",
        L10nKey::SwitcherStatusConnecting => "connecting…",
        L10nKey::SwitcherStatusConnectFailed => "couldn't connect",
        L10nKey::SwitcherStatusNotConnected => "not connected",
        L10nKey::SettingsFontDefault => "Default (match primary)",
        L10nKey::ForwardDescriptionPlaceholder => "what it's for",
        L10nKey::SettingsShellDefaultLoginShell => "your login shell",
        L10nKey::SftpErrorUnexpectedReply => "unexpected reply: {reply}",
        L10nKey::SftpErrorUnsafeRemoteName => "refusing unsafe remote name {name}",
        L10nKey::SftpErrorInvalidOctalMode => "invalid octal mode",
        L10nKey::PanelMoreChangedFiles => {
            "… and {count} more changed files — run `git diff` to see them."
        }
        L10nKey::PanelUntracked => "{count} untracked",
        L10nKey::AppMenuAbout => "About tty7",
        L10nKey::AppMenuCheckForUpdates => "Check for Updates…",
        L10nKey::AppMenuSettings => "Settings…",
        L10nKey::AppMenuServices => "Services",
        L10nKey::AppMenuHideApp => "Hide tty7",
        L10nKey::AppMenuHideOthers => "Hide Others",
        L10nKey::AppMenuShowAll => "Show All",
        L10nKey::AppMenuQuit => "Quit tty7",
        L10nKey::AppMenuFile => "File",
        L10nKey::AppMenuEdit => "Edit",
        L10nKey::AppMenuView => "View",
        L10nKey::AppMenuWindow => "Window",
        L10nKey::AppMenuHelp => "Help",
        L10nKey::AppMenuNewTab => "New Tab",
        L10nKey::AppMenuNewWorkspace => "New Workspace",
        L10nKey::AppMenuNewWorktreeTab => "New Worktree Tab",
        L10nKey::AppMenuSplitRight => "Split Right",
        L10nKey::AppMenuSplitDown => "Split Down",
        L10nKey::AppMenuRenameTab => "Rename Tab…",
        L10nKey::AppMenuCopyWorkingDirectory => "Copy Working Directory",
        L10nKey::AppMenuCopySessionId => "Copy Session ID",
        L10nKey::AppMenuForkSession => "Fork Session",
        L10nKey::AppMenuClosePaneTab => "Close Pane / Tab",
        L10nKey::AppMenuCloseOtherTabs => "Close Other Tabs",
        L10nKey::AppMenuCloseTabsRight => "Close Tabs to the Right",
        L10nKey::AppMenuReopenClosedTab => "Reopen Closed Tab",
        L10nKey::AppMenuRenameWorkspace => "Rename Workspace…",
        L10nKey::AppMenuStopWorkspace => "Stop Workspace…",
        L10nKey::AppMenuDeleteWorkspace => "Delete Workspace…",
        L10nKey::AppMenuUndo => "Undo",
        L10nKey::AppMenuRedo => "Redo",
        L10nKey::AppMenuCut => "Cut",
        L10nKey::AppMenuCopy => "Copy",
        L10nKey::AppMenuPaste => "Paste",
        L10nKey::AppMenuSelectAll => "Select All",
        L10nKey::AppMenuFind => "Find…",
        L10nKey::AppMenuFindNext => "Find Next",
        L10nKey::AppMenuFindPrevious => "Find Previous",
        L10nKey::AppMenuCommandPalette => "Command Palette…",
        L10nKey::AppMenuIncreaseFontSize => "Increase Font Size",
        L10nKey::AppMenuDecreaseFontSize => "Decrease Font Size",
        L10nKey::AppMenuResetFontSize => "Reset Font Size",
        L10nKey::AppMenuLeftSidebar => "Left Sidebar",
        L10nKey::AppMenuRightPanel => "Right Panel",
        L10nKey::AppMenuCodePanel => "Code Panel",
        L10nKey::AppMenuTabBarPosition => "Tab Bar Position",
        L10nKey::AppMenuFocusNextPane => "Focus Next Pane",
        L10nKey::AppMenuFocusPreviousPane => "Focus Previous Pane",
        L10nKey::AppMenuZoomPane => "Zoom Pane",
        L10nKey::AppMenuClearScrollback => "Clear Scrollback",
        L10nKey::AppMenuEnterFullscreen => "Enter Full Screen",
        L10nKey::AppMenuDocumentation => "tty7 Documentation",
        L10nKey::AppMenuKeyboardShortcuts => "Keyboard Shortcuts",
        L10nKey::AppMenuJoinDiscord => "Join the Discord",
        L10nKey::AppMenuReportIssue => "Report an Issue…",
        L10nKey::AppMenuRestartServer => "Restart Server…",
        L10nKey::WindowUntitled => "Untitled",
        L10nKey::TrayShowTty7 => "Show tty7",
        L10nKey::TrayNotifications => "Notifications",
        L10nKey::TrayAgentNeedsInput => "needs input",
        L10nKey::NotifyCommandFinished => "Command finished after {secs}s",
        L10nKey::NotifyCommandFinishedWithCommand => "{command} — finished after {secs}s",
        L10nKey::NotifyAgentFinished => "Finished after {secs}s",
        L10nKey::NotifyAgentWaiting => "Waiting for your input",
        L10nKey::NotifyTurnFinished => "Turn finished",
        L10nKey::TabTooltipMore => "More",
        L10nKey::TabTooltipShowSidebar => "Show Sidebar",
        L10nKey::TabTooltipHideSidebar => "Hide Sidebar",
        L10nKey::TabTooltipHideDetailPanel => "Hide Detail Panel",
        L10nKey::TabTooltipShowDetailPanel => "Show Detail Panel",
        L10nKey::TabUnnamedShell => "Shell {n}",
        L10nKey::ShellDefault => "default",
        L10nKey::SidebarScratchGroup => "Scratch",
        L10nKey::TabContextCloseTab => "Close Tab",
        L10nKey::TabContextCloseTabsBelow => "Close Tabs Below",
        L10nKey::TabContextMarkUnread => "Mark as Unread",
    }
}

pub fn translate_variant_en(key: L10nKey, branch: &'static str) -> Option<&'static str> {
    let res = match (key, branch) {
        (L10nKey::SettingsAliasesLinked, "zero") => "No aliases linked yet.",
        (L10nKey::SettingsAliasesLinked, "one") => "1 alias linked.",
        (L10nKey::SettingsAliasesLinked, "other") => "{count} aliases linked.",
        (L10nKey::SettingsRulesOpenedWithConnection, "zero") => {
            "0 rules, opened with the connection"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "one") => "1 rule, opened with the connection",
        (L10nKey::SettingsRulesOpenedWithConnection, "other") => {
            "{count} rules, opened with the connection"
        }
        (L10nKey::SettingsOfflineMachines, "zero") => {
            "0 more saved machines are not connected — open a workspace on one to install its hooks there."
        }
        (L10nKey::SettingsOfflineMachines, "one") => {
            "1 more saved machine is not connected — open a workspace on it to install its hooks there."
        }
        (L10nKey::SettingsOfflineMachines, "other") => {
            "{count} more saved machines are not connected — open a workspace on one to install its hooks there."
        }
        (L10nKey::PanelUntracked, "zero") => "0 untracked",
        (L10nKey::PanelUntracked, "one") => "1 untracked",
        (L10nKey::PanelUntracked, "other") => "{count} untracked",
        (L10nKey::PanelMoreChangedFiles, "zero") => {
            "… and 0 more changed files — run `git diff` to see them."
        }
        (L10nKey::PanelMoreChangedFiles, "one") => {
            "… and 1 more changed file — run `git diff` to see it."
        }
        (L10nKey::PanelMoreChangedFiles, "other") => {
            "… and {count} more changed files — run `git diff` to see them."
        }
        (L10nKey::DiffChangedFiles, "zero") => "0 changed files",
        (L10nKey::DiffChangedFiles, "one") => "1 changed file",
        (L10nKey::DiffChangedFiles, "other") => "{count} changed files",
        (L10nKey::DiffUntrackedCount, "zero") => " · 0 untracked",
        (L10nKey::DiffUntrackedCount, "one") => " · 1 untracked",
        (L10nKey::DiffUntrackedCount, "other") => " · {count} untracked",
        (L10nKey::DiffMoreFiles, "zero") => {
            "… and 0 more changed files — run `git diff` in the terminal to see them."
        }
        (L10nKey::DiffMoreFiles, "one") => {
            "… and 1 more changed file — run `git diff` in the terminal to see it."
        }
        (L10nKey::DiffMoreFiles, "other") => {
            "… and {count} more changed files — run `git diff` in the terminal to see them."
        }
        (L10nKey::DiffUntrackedHeader, "zero") => "Untracked files (0)",
        (L10nKey::DiffUntrackedHeader, "one") => "Untracked files (1)",
        (L10nKey::DiffUntrackedHeader, "other") => "Untracked files ({count})",
        (L10nKey::DiffMoreUntracked, "zero") => {
            "… and 0 more — run `git status` in the terminal to see them."
        }
        (L10nKey::DiffMoreUntracked, "one") => {
            "… and 1 more — run `git status` in the terminal to see it."
        }
        (L10nKey::DiffMoreUntracked, "other") => {
            "… and {count} more — run `git status` in the terminal to see them."
        }
        (L10nKey::DiffUntrackedSummary, "zero") => "0 untracked",
        (L10nKey::DiffUntrackedSummary, "one") => "1 untracked",
        (L10nKey::DiffUntrackedSummary, "other") => "{count} untracked",
        (L10nKey::HomeTimeMinutesAgo, "one") => "1 min ago",
        (L10nKey::HomeTimeMinutesAgo, "other") => "{count} min ago",
        (L10nKey::HomeTimeHoursAgo, "one") => "1 hour ago",
        (L10nKey::HomeTimeHoursAgo, "other") => "{count} hours ago",
        (L10nKey::HomeTimeDaysAgo, "one") => "1 day ago",
        (L10nKey::HomeTimeDaysAgo, "other") => "{count} days ago",
        (L10nKey::WindowStopShells, "zero") => {
            "Its layout and working directories will be forgotten."
        }
        (L10nKey::WindowStopShells, "one") => "1 running shell will be ended.",
        (L10nKey::WindowStopShells, "other") => "{count} running shells will be ended.",
        (L10nKey::WindowDeleteShells, "zero") => {
            "Its layout and working directories will be forgotten."
        }
        (L10nKey::WindowDeleteShells, "one") => {
            "1 running shell will be ended and its layout forgotten."
        }
        (L10nKey::WindowDeleteShells, "other") => {
            "{count} running shells will be ended and the layout forgotten."
        }
        _ => return None,
    };
    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_complete_table() {
        assert_eq!(translate_en(L10nKey::SearchTabs), "Search tabs…");
        assert_eq!(
            translate_variant_en(L10nKey::SettingsAliasesLinked, "one"),
            Some("1 alias linked.")
        );
        assert_eq!(translate_variant_en(L10nKey::SearchTabs, "one"), None);
    }
}
