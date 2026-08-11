use super::L10nKey;

pub fn translate_zh(key: L10nKey) -> Option<&'static str> {
    Some(match key {
        L10nKey::SearchTabs => "搜索标签页…",
        L10nKey::SearchFiles => "搜索文件…",
        L10nKey::SearchThemes => "搜索主题…",
        L10nKey::SearchSettings => "搜索设置…",
        L10nKey::FilterHosts => "筛选主机…",
        L10nKey::SearchCommandsOrHost => "搜索或输入 user@host 连接…",
        L10nKey::SearchTheme => "搜索…",
        L10nKey::SearchWorkspacesAndMachines => "搜索工作区、标签页和机器…",
        L10nKey::SearchFonts => "搜索字体…",
        L10nKey::SearchFind => "查找…",
        L10nKey::SearchMatchCase => "区分大小写",
        L10nKey::SearchUseRegex => "使用正则表达式",
        L10nKey::NewFolderName => "新文件夹名",
        L10nKey::NewFileName => "新文件名",
        L10nKey::HomeNewTab => "新标签页",
        L10nKey::HomeReopenClosedTab => "重新打开已关闭的标签页",
        L10nKey::HomeSwitchWorkspace => "切换工作区…",
        L10nKey::HomeCommandPalette => "命令面板…",
        L10nKey::HomeSplitRight => "向右分屏",
        L10nKey::HomeSplitDown => "向下分屏",
        L10nKey::HomeSettings => "设置…",
        L10nKey::TrayQuitStopServer => "退出并停止 server…",
        L10nKey::Reconnect => "重新连接",
        L10nKey::None => "无。",
        L10nKey::TryAgain => "重试",
        L10nKey::Refreshing => "正在刷新…",
        L10nKey::Binary => "二进制文件",
        L10nKey::Delete => "删除",
        L10nKey::NoMatchingCommands => "没有匹配的命令",
        L10nKey::ConnectSshHint => "输入 user@host 改为通过 SSH 连接。",
        L10nKey::EditHint => "编辑",
        L10nKey::OpenFileFromTree => "从文件树打开文件",
        L10nKey::TreeDirLoading => "读取中…",
        L10nKey::TreeDirEmpty => "空",
        L10nKey::TreeDirHiddenOnly => "只有隐藏文件",
        L10nKey::TreeDirUnreadable => "无法读取",
        L10nKey::TreeSearchCapped => "只显示前 {n} 个匹配",
        L10nKey::TreeSearchFailed => "搜索失败",
        L10nKey::FileChangedOnDisk => "文件在磁盘上已被修改",
        L10nKey::Reload => "重新加载",
        L10nKey::KeepMine => "保留我的版本",
        L10nKey::Dismiss => "关闭",
        L10nKey::StoredPasswordRejected => "已存储的密码被拒绝，请输入新密码。",
        L10nKey::Trust => "信任",
        L10nKey::Abort => "中止",
        L10nKey::HostKeyOverrideMessage => "输入 yes 覆盖并信任新密钥，或按 Esc 中止。",
        L10nKey::Override => "覆盖",
        L10nKey::RememberKeychain => "记住（钥匙串）",
        L10nKey::Cancel => "取消",
        L10nKey::Close => "关闭",
        L10nKey::QuitStopServerTitle => "退出并停止 server？",
        L10nKey::QuitStopServerBody => {
            "这会退出 tty7 并停止后台 server，所有仍在运行的 shell 都会被终止。你的标签页和布局会被保留，下次启动时以全新的 shell 重新打开。（普通退出会保持 shell 运行。）"
        }
        L10nKey::QuitAndStop => "退出并停止",
        L10nKey::CloseSshConnectionTitle => "关闭这个 SSH 连接？",
        L10nKey::CloseSshConnectionBody => "连接仍处于活动状态，关闭会断开它。",
        L10nKey::ClosePaneBusyTitle => "关闭这个窗格？",
        L10nKey::CloseTabBusyTitle => "关闭这个标签页？",
        L10nKey::CloseBusyCommandBody => "{what} 还在运行，关闭会终止它。",
        L10nKey::CloseBusyAgentBody => "{agent} 还在工作，关闭会中断这一轮。",
        L10nKey::Keep => "保留",
        L10nKey::SettingsNavAppearance => "外观",
        L10nKey::SettingsNavTerminal => "终端",
        L10nKey::SettingsNavInput => "输入",
        L10nKey::SettingsNavSsh => "SSH",
        L10nKey::SettingsNavAgents => "Agents",
        L10nKey::SettingsNavWindowTabs => "窗口与标签页",
        L10nKey::SettingsNavKeybindings => "按键绑定",
        L10nKey::SettingsNavAbout => "关于",
        L10nKey::SettingsHeader => "设置",
        L10nKey::Reset => "重置",
        L10nKey::Save => "保存",
        L10nKey::Connect => "连接",
        L10nKey::Download => "下载",
        L10nKey::Link => "关联",
        L10nKey::SettingsThemeIntroTitle => "主题",
        L10nKey::SettingsThemeIntroDesc => "选择配色主题。每个主题都有各自的浅色或深色外观。",
        L10nKey::SettingsTypography => "字体排版",
        L10nKey::SettingsFontSize => "字号",
        L10nKey::SettingsFontSizeDesc => "终端文字大小（像素）。",
        L10nKey::SettingsUiFontSize => "界面字号",
        L10nKey::SettingsUiFontSizeDesc => {
            "终端以外所有地方的文字大小——标签页、面板、设置。非 Retina 显示器上可以调大。"
        }
        L10nKey::SettingsLineHeight => "行高",
        L10nKey::SettingsLineHeightDesc => "行间距为字号的倍数。",
        L10nKey::SettingsFontFamily => "字体族",
        L10nKey::SettingsFontFamilyDesc => "从系统已安装的字体中选择。",
        L10nKey::SettingsBoldFont => "粗体字体",
        L10nKey::SettingsBoldFontDesc => "粗体文字使用的字体；默认由主字体合成。",
        L10nKey::SettingsItalicFont => "斜体字体",
        L10nKey::SettingsItalicFontDesc => "斜体文字使用的字体；默认由主字体合成。",
        L10nKey::SettingsFontLigatures => "字体连字",
        L10nKey::SettingsFontLigaturesDesc => "为终端文字启用常见的编程连字特性。",
        L10nKey::SettingsCursor => "光标",
        L10nKey::SettingsCursorShape => "光标形状",
        L10nKey::SettingsCursorShapeDesc => "终端光标的绘制方式。",
        L10nKey::SettingsCursorBlink => "光标闪烁",
        L10nKey::SettingsCursorBlinkDesc => "终端获得焦点时让光标闪烁。",
        L10nKey::SettingsLanguage => "语言",
        L10nKey::SettingsLanguageDesc => "选择 tty7 界面使用的语言。",
        L10nKey::SettingsLanguageEnglish => "English",
        L10nKey::SettingsLanguageChinese => "简体中文",
        L10nKey::SettingsLanguageJapanese => "日本語",
        L10nKey::SettingsSearchLanguageKeywords => {
            "语言 区域设置 英文 中文 language locale english chinese"
        }
        L10nKey::SettingsTransparency => "透明度",
        L10nKey::SettingsOpacity => "不透明度",
        L10nKey::SettingsOpacityDesc => {
            "窗口背景的不透明度，适用于所有主题。低于 100% 时可以看到桌面。"
        }
        L10nKey::SettingsBlur => "模糊",
        L10nKey::SettingsBlurDesc => "模糊半透明窗口背后的内容（macOS）。",
        L10nKey::SettingsBlurAutoDesc => {
            "模糊半透明窗口背后的内容。仅在「背景材质」为「自动」时生效。"
        }
        L10nKey::SettingsBackdrop => "背景材质",
        L10nKey::SettingsBackdropDesc => {
            "半透明窗口背后的原生 Windows 背景材质。云母需要 Windows 11 22H2，亚克力需要 1809；更旧的系统会自动回退。"
        }
        L10nKey::SettingsSearchBackdropKeywords => {
            "材质 背景 云母 亚克力 模糊 毛玻璃 窗口 material backdrop mica acrylic blur frosted window background"
        }
        L10nKey::SettingsBackdropAuto => "自动",
        L10nKey::SettingsBackdropBlur => "模糊",
        L10nKey::SettingsBackdropMica => "云母",
        L10nKey::SettingsBackdropMicaAlt => "云母 Alt",
        L10nKey::SettingsBackdropAcrylic => "亚克力",
        L10nKey::SettingsBackdropOff => "关闭",
        L10nKey::FollowTheme => "跟随主题",
        L10nKey::SettingsDimInactivePanes => "调暗非活动窗格",
        L10nKey::SettingsDimInactivePanesDesc => "在分屏中淡化未聚焦的窗格，让活动窗格更突出。",
        L10nKey::SettingsOpenThemesFolder => "打开主题文件夹",
        L10nKey::SettingsChangeThemeImage => "更改…",
        L10nKey::SettingsChooseThemeImage => "选择…",
        L10nKey::SettingsRemoveThemeImage => "移除",
        L10nKey::SettingsImageOpacity => "图片不透明度",
        L10nKey::SettingsImageOpacityDesc => "图片叠加在背景色上的显示强度。",
        L10nKey::SettingsEditTheme => "编辑主题",
        L10nKey::SettingsEditThemeIntro => {
            "你正在编辑一份副本。更改会保存到主题文件夹中的对应文件并实时生效。"
        }
        L10nKey::SettingsBackgroundImage => "背景图片",
        L10nKey::SettingsBackgroundImageDesc => "叠加在背景色之上、文字之下。",
        L10nKey::SettingsAnsiColors => "ANSI 颜色",
        L10nKey::SettingsCustomThemes => "自定义主题",
        L10nKey::SettingsThemesRejected => "主题文件夹里这些没能加载",
        L10nKey::ThemeDuplicateFailed => "无法复制主题",
        L10nKey::ThemeSaveFailed => "无法保存主题",
        L10nKey::OpenInFileManagerFailed => "无法打开 {path}",
        L10nKey::SettingsCustomThemesIntro => {
            "复制一个主题后可在此编辑其颜色，或者把自定义主题放入主题文件夹：tty7 YAML 主题或 iTerm2 的 .itermcolors 方案。"
        }
        L10nKey::SettingsDuplicateToEdit => "复制以编辑",
        L10nKey::SettingsHosts => "主机",
        L10nKey::SettingsDefaults => "默认值",
        L10nKey::SettingsInheritedByEveryHost => "对所有主机生效",
        L10nKey::SettingsNoSavedHosts => "还没有保存的主机。",
        L10nKey::SettingsNothingMatches => "没有匹配 {query} 的内容。",
        L10nKey::SettingsInTty7 => "在 tty7 中",
        L10nKey::SettingsImportFromSshConfig => "从 ~/.ssh/config 导入",
        L10nKey::SettingsExpandAllGroups => "展开所有分组",
        L10nKey::SettingsNoHostsYet => "还没有主机",
        L10nKey::SettingsNothingSelected => "未选择任何内容",
        L10nKey::SettingsTypeAddressToConnect => "输入地址即可立刻连接，之后 tty7 会提示保存。",
        L10nKey::SettingsMoreInSshConfig => "~/.ssh/config 中还有 {count} 个",
        L10nKey::SettingsAliasesLinked => "已关联 {count} 个别名。",
        L10nKey::SettingsImportAliases => "导入别名",
        L10nKey::SettingsImportAliasesDesc => {
            "重新读取文件并添加新内容。你在这里做的编辑由 tty7 保存——不会写入该文件本身。"
        }
        L10nKey::SettingsImportNow => "立即导入",
        L10nKey::SettingsDefaultsIntro => {
            "所有主机都从这些设置开始。每个主机都可以在自己的高级选项中覆盖某项。"
        }
        L10nKey::SettingsCopyAddress => "复制地址",
        L10nKey::SettingsDuplicate => "复制",
        L10nKey::SettingsForgetPassword => "清除已保存的密码",
        L10nKey::SettingsForgotPasswordFor => "已清除 {endpoint} 的已保存密码",
        L10nKey::SettingsDeleteProfileBody => {
            "为它保存的密码也会一并删除，除非还有别的连接用着同一个地址。"
        }
        L10nKey::SettingsCouldntForgetPassword => "无法清除 {endpoint} 的已保存密码：{error}",
        L10nKey::SettingsSecurity => "安全",
        L10nKey::SettingsSecurityIntro => "主机可以在自己的高级选项中覆盖这些设置。",
        L10nKey::SettingsVerifyHostKeys => "校验主机密钥",
        L10nKey::SettingsVerifyHostKeysDesc => {
            "在连接前对照 known_hosts 检查每台服务器的密钥，并确认未知或已更改的密钥。关闭后连接不做检查，被仿冒的服务器也不会被察觉。"
        }
        L10nKey::WarnBeforeClosing => "关闭前警告",
        L10nKey::SettingsWarnBeforeClosingDesc => {
            "在关闭带有活动 SSH 会话的标签页或窗格前请求确认。"
        }
        L10nKey::SettingsNewHost => "新主机",
        L10nKey::SettingsDiscardChangesTitle => "丢弃未保存的改动？",
        L10nKey::SettingsDiscardChangesBody => "你正在编辑的连接有还没保存的改动。",
        L10nKey::SettingsKeepEditing => "继续编辑",
        L10nKey::SettingsName => "名称",
        L10nKey::SettingsNameDesc => "此连接的标签。",
        L10nKey::SettingsHost => "主机",
        L10nKey::SettingsHostDesc => "主机名或 IP 地址。",
        L10nKey::SettingsUser => "用户",
        L10nKey::SettingsUserDesc => "登录用户（留空表示连接时解析）。",
        L10nKey::SettingsAuth => "认证",
        L10nKey::SettingsAuthDesc => "认证方式。自动会依次尝试所有适用的方式。",
        L10nKey::SettingsAuthModeAuto => "自动",
        L10nKey::SettingsAuthModePassword => "密码",
        L10nKey::SettingsAuthModeKey => "密钥",
        L10nKey::SettingsAuthModeAgent => "ssh-agent",
        L10nKey::SettingsAuthMode2Fa => "2FA",
        L10nKey::SettingsJumpHost => "跳板主机",
        L10nKey::SettingsJumpHostDesc => "用于中转的另一个主机配置的名称（留空 = 直连）。",
        L10nKey::SettingsNoneSummary => "（无）",
        L10nKey::SettingsPortForwarding => "端口转发",
        L10nKey::SettingsRulesOpenedWithConnection => "1 条规则，随连接打开",
        L10nKey::SettingsAddRule => "+ 添加规则",
        L10nKey::SettingsRemoveRule => "删除规则",
        L10nKey::SettingsFwdLegendLocal => "L — 本地端口可达远程侧",
        L10nKey::SettingsFwdLegendRemote => "R — 远程端口可达本机",
        L10nKey::SettingsFwdLegendDynamic => "D — 动态 SOCKS 代理",
        L10nKey::SettingsFwdNeedsBoth => "需要监听端口和目标 host:port——不会被保存。",
        L10nKey::SettingsFwdNeedsListen => "需要监听端口——不会被保存。",
        L10nKey::SettingsAdvanced => "高级",
        L10nKey::SettingsAdvancedSummary => "算法 / 保活 / 代理 / X11 / 登录脚本",
        L10nKey::SettingsGroupAuthentication => "认证",
        L10nKey::SettingsGroupProxies => "代理",
        L10nKey::SettingsGroupAlgorithms => "算法",
        L10nKey::SettingsGroupConnection => "连接",
        L10nKey::SettingsGroupSession => "会话",
        L10nKey::SettingsGroupSecurity => "安全",
        L10nKey::SettingsIdentityFiles => "身份文件",
        L10nKey::SettingsIdentityFilesDesc => "私钥路径，每行一个（支持 %h/%r 展开）。",
        L10nKey::SettingsAgentForwarding => "ssh-agent 转发",
        L10nKey::SettingsAgentForwardingDesc => "将本机 ssh-agent 转发到该连接。",
        L10nKey::SettingsProxyCommand => "代理命令",
        L10nKey::SettingsProxyCommandDesc => "传输命令（%h/%p/%r 会被替换）。",
        L10nKey::SettingsSocks5Proxy => "SOCKS5 代理",
        L10nKey::SettingsSocks5ProxyDesc => "host:port（留空 = 无）。",
        L10nKey::SettingsHttpProxy => "HTTP 代理",
        L10nKey::SettingsHttpProxyDesc => "host:port（留空 = 无）。",
        L10nKey::SettingsKexAlgorithms => "KEX 算法",
        L10nKey::SettingsKexAlgorithmsDesc => "逗号分隔（留空 = 库默认值）。",
        L10nKey::SettingsCiphers => "加密算法",
        L10nKey::SettingsCiphersDesc => "逗号分隔（留空 = 默认值）。",
        L10nKey::SettingsMacs => "MAC 算法",
        L10nKey::SettingsMacsDesc => "逗号分隔（留空 = 默认值）。",
        L10nKey::SettingsHostKeyAlgorithms => "主机密钥算法",
        L10nKey::SettingsHostKeyAlgorithmsDesc => "逗号分隔（留空 = 默认值）。",
        L10nKey::SettingsCompression => "压缩算法",
        L10nKey::SettingsJumpHostVia => "经由 {jump_name}",
        L10nKey::SettingsConnected => "已连接",
        L10nKey::SettingsProfileCopied => "{name}（副本）",
        L10nKey::SettingsCompressionDesc => "逗号分隔（留空 = 默认值）。",
        L10nKey::SettingsKeepaliveInterval => "保活间隔（秒）",
        L10nKey::SettingsKeepaliveIntervalDesc => "留空 = 库默认值。",
        L10nKey::SettingsKeepaliveCountMax => "最大保活次数",
        L10nKey::SettingsKeepaliveCountMaxDesc => "判定断连前允许丢失的保活次数。",
        L10nKey::SettingsConnectTimeout => "连接超时（秒）",
        L10nKey::SettingsConnectTimeoutDesc => "留空 = 库默认值。",
        L10nKey::SettingsX11Forwarding => "X11 转发",
        L10nKey::SettingsX11ForwardingDesc => "请求 X11 转发（macOS 上需要 XQuartz）。",
        L10nKey::SettingsShellIntegration => "Shell 集成",
        L10nKey::SettingsShellIntegrationDesc => "让远程 shell 报告提示符、退出码和工作目录。",
        L10nKey::SettingsLoginScripts => "登录脚本",
        L10nKey::SettingsLoginScriptsDesc => "shell 打开后发送的命令，每行一个。",
        L10nKey::SettingsSkipBanner => "跳过横幅",
        L10nKey::SettingsSkipBannerDesc => "抑制服务器登录横幅。",
        L10nKey::SettingsDefaultFollowsDefaults => "默认跟随默认设置，当前为 {value}。",
        L10nKey::SettingsValueOn => "开",
        L10nKey::SettingsValueOff => "关",
        L10nKey::SettingsDefault => "默认",
        L10nKey::SettingsOn => "开",
        L10nKey::SettingsOff => "关",
        L10nKey::SettingsShell => "Shell",
        L10nKey::SettingsShellIntro => {
            "每个新终端启动的程序。将“程序”留空可使用平台默认值（{default}）。"
        }
        L10nKey::SettingsProgram => "程序",
        L10nKey::SettingsProgramDesc => "PATH 中的可执行文件名或绝对路径，例如 zsh、fish、pwsh。",
        L10nKey::SettingsArguments => "参数",
        L10nKey::SettingsArgumentsDesc => "以空格分隔的启动参数，例如登录 shell 用 -l。",
        L10nKey::SettingsStartIn => "起始目录",
        L10nKey::SettingsStartInDesc => "新 shell 的启动目录：tty7 的启动目录、主目录或固定路径。",
        L10nKey::SettingsCustomPath => "自定义路径",
        L10nKey::SettingsCustomPathDesc => "新 shell 启动的目录。",
        L10nKey::SettingsWdInherit => "继承",
        L10nKey::SettingsWdHome => "主目录",
        L10nKey::SettingsWdCustom => "自定义",
        L10nKey::SettingsShellFooter => {
            "仅适用于没有可继承目录的 shell，例如窗口的第一个标签页。新标签页和分屏仍会继承活动窗格的目录，已经打开的 shell 会继续运行。"
        }
        L10nKey::SettingsScrolling => "滚动",
        L10nKey::SettingsScrollback => "回滚行数",
        L10nKey::SettingsScrollbackDesc => "每个窗格保留的历史行数。仅适用于新窗格。",
        L10nKey::SettingsScrollSpeed => "滚动速度",
        L10nKey::SettingsScrollSpeedDesc => "应用于鼠标滚轮滚动的倍率。",
        L10nKey::SettingsSmoothScroll => "平滑滚动",
        L10nKey::SettingsSmoothScrollDesc => {
            "滚轮每一格分几帧滚到位，而不是一次跳完。触控板本来就是连续滚动，不受影响。"
        }
        L10nKey::SettingsMouse => "鼠标",
        L10nKey::SettingsFocusFollowsMouse => "焦点跟随鼠标",
        L10nKey::SettingsFocusFollowsMouseDesc => "悬停窗格即聚焦，无需点击。",
        L10nKey::SettingsHideMouseWhileTyping => "输入时隐藏鼠标",
        L10nKey::SettingsHideMouseWhileTypingDesc => "输入时隐藏指针；下次移动鼠标时恢复。",
        L10nKey::SettingsReportMouseToApps => "向应用报告鼠标",
        L10nKey::SettingsReportMouseToAppsDesc => {
            "让全屏应用（如 vim、tmux）处理点击和滚动；按住 Shift 可让操作保持本地。"
        }
        L10nKey::SettingsBell => "铃声",
        L10nKey::SettingsTerminalBell => "终端铃声",
        L10nKey::SettingsTerminalBellDesc => {
            "铃声（^G）的通知方式：静音、短暂闪烁、系统声音，或两者同时。"
        }
        L10nKey::SettingsLinks => "链接",
        L10nKey::DetectUrls => "检测 URL",
        L10nKey::SettingsDetectUrlsDesc => "悬停时给链接加下划线，通过 {modifier}+点击 打开。",
        L10nKey::ForwardSshLoopbackLinks => "转发 SSH 回环链接",
        L10nKey::SettingsForwardSshLoopbackLinksDesc => {
            "当窗格处于 SSH 中时，通过临时端口转发打开 localhost 链接。"
        }
        L10nKey::OpenFilesWith => "打开文件方式",
        L10nKey::SettingsOpenFilesWithDesc => {
            "{modifier}+点击 文件链接时运行的命令，而不是默认应用。可使用 {path}、{line}、{column}；参数值缺失的标志会被丢弃（例如 herdr edit {path} --line={line}）。留空使用默认应用。"
        }
        L10nKey::SettingsBellModeOff => "关",
        L10nKey::SettingsBellModeVisual => "闪烁",
        L10nKey::SettingsBellModeAudible => "声音",
        L10nKey::SettingsBellModeBoth => "闪烁 + 声音",
        L10nKey::SettingsPrompt => "提示符",
        L10nKey::SettingsPromptIntro => {
            "shell 提示符处的 tty7 自带菜单。关闭某项即可把按键交还给 shell。"
        }
        L10nKey::SettingsTabCompletion => "Tab 补全",
        L10nKey::SettingsTabCompletionDesc => {
            "在提示符按 Tab 打开 tty7 的补全菜单。关闭后 Tab 交由 shell 自身的补全处理。"
        }
        L10nKey::SettingsHistorySearch => "历史搜索",
        L10nKey::SettingsHistorySearchDesc => {
            "在提示符按 ⌃R 打开 tty7 的模糊历史菜单。关闭后 ⌃R 交由 shell 处理——它自带的反向搜索，或你在那里绑定的其它功能（fzf、percol）。"
        }
        L10nKey::SettingsSelectionClipboard => "选择与剪贴板",
        L10nKey::SettingsSmartSelection => "智能选择",
        L10nKey::SettingsSmartSelectionDesc => {
            "双击选择光标下的完整 URL、文件路径、邮箱或成对的括号。"
        }
        L10nKey::SettingsCopyOnSelect => "选中即复制",
        L10nKey::SettingsCopyOnSelectDesc => "用鼠标选中文本时立即复制到剪贴板，无需按 ⌘C。",
        L10nKey::SettingsTrimTrailingSpaces => "复制时去除末尾空格",
        L10nKey::SettingsTrimTrailingSpacesDesc => "去除每行复制文本末尾的空白。",
        L10nKey::SettingsKeyboard => "键盘",
        L10nKey::SettingsOptionAsMeta => "Option (⌥) 作为 Meta",
        L10nKey::SettingsOptionAsMetaDesc => {
            "⌥+按键 发送 shell 期望的转义组合键（⌥B = 后退一个词），而不是输入特殊字符（∫）。"
        }
        L10nKey::SettingsAgentsIntro => "Agents",
        L10nKey::SettingsAgentsIntroDesc => {
            "hook 集成让标签栏中的窗格实时显示这些 agent 的会话状态（进行中 / 等待中 / 已完成）。仅在 tty7 内生效。"
        }
        L10nKey::SettingsReadingAgentConfig => "正在读取这台机器的 agent 配置…",
        L10nKey::SettingsStatusNotInstalled => "未安装",
        L10nKey::SettingsStatusInstalled => "已安装",
        L10nKey::SettingsStatusOutdated => "已过时",
        L10nKey::SettingsInstall => "安装",
        L10nKey::SettingsReinstall => "重新安装",
        L10nKey::SettingsUpdate => "更新",
        L10nKey::SettingsUninstall => "卸载",
        L10nKey::SettingsOfflineMachines => {
            "还有 {count} 台已保存的机器未连接——在其中一台上打开工作区，即可在那台机器上安装 hook。"
        }
        L10nKey::SettingsSyncWithSystem => "跟随系统",
        L10nKey::SettingsSyncWithSystemDesc => "跟随操作系统外观，并分别使用浅色与深色主题。",
        L10nKey::SettingsLegiblePalette => "低对比度颜色纠偏",
        L10nKey::SettingsLegiblePaletteDesc => "自动把主题背景上对比度不足的颜色调整到可读级别。",
        L10nKey::SettingsChangeTheme => "更换主题",
        L10nKey::SettingsThemes => "主题",
        L10nKey::SettingsThemesCloseTooltip => "关闭主题面板 (Esc)",
        L10nKey::SettingsThemePanelManual => "更改当前主题。",
        L10nKey::SettingsThemePanelLight => "选择浅色模式的主题。",
        L10nKey::SettingsThemePanelDark => "选择深色模式的主题。",
        L10nKey::SettingsCustom => "自定义",
        L10nKey::SettingsBuiltIn => "内置",
        L10nKey::SettingsDark => "深色",
        L10nKey::SettingsLight => "浅色",
        L10nKey::SettingsLightMode => "浅色模式",
        L10nKey::SettingsDarkMode => "深色模式",
        L10nKey::SettingsActive => "使用中",
        L10nKey::SettingsStartupWindow => "启动窗口",
        L10nKey::SettingsStartupWindowDesc => "tty7 启动时的窗口状态。",
        L10nKey::SettingsRememberWindowSize => "记住窗口大小与位置",
        L10nKey::SettingsRememberWindowSizeDesc => {
            "以 tty7 上次退出时窗口的大小和位置重新打开。关闭时以默认大小居中打开。"
        }
        L10nKey::SettingsRestoreLastLayout => "恢复上次布局",
        L10nKey::SettingsRestoreLastLayoutDesc => {
            "启动时恢复上次窗口的标签页、分屏和目录。关闭时从单个新终端开始。"
        }
        L10nKey::SettingsShowTrayIcon => "显示托盘图标",
        L10nKey::SettingsShowTrayIconDesc => {
            "在系统托盘/菜单栏保留状态项：当编码 agent 需要输入时发出提示，其菜单可跳转到该 agent 的窗格。"
        }
        L10nKey::SettingsTabs => "标签页",
        L10nKey::SettingsNewTabPosition => "新标签页位置",
        L10nKey::SettingsNewTabPositionDesc => "新打开的标签页插入的位置。",
        L10nKey::SettingsTabBarPosition => "标签栏位置",
        L10nKey::SettingsTabBarPositionDesc => "将标签页显示为顶部横向条或左侧垂直侧栏。",
        L10nKey::SettingsSidebarGrouping => "侧栏分组",
        L10nKey::SettingsSidebarGroupingDesc => {
            "按 git 仓库在标题下对侧栏标签页分组，非仓库标签页放在“草稿”分组。仅适用于左侧栏。"
        }
        L10nKey::SettingsDiffPreviewFromCounts => "从侧栏计数打开 diff 预览",
        L10nKey::SettingsDiffPreviewFromCountsDesc => {
            "点击行上的 +N −N 可在浮层中打开 worktree diff。关闭时行上仍显示分支和计数，但不再可点击。"
        }
        L10nKey::SettingsNotifications => "通知",
        L10nKey::SettingsNotifyOnCommandFinish => "命令完成时通知",
        L10nKey::SettingsNotifyOnCommandFinishDesc => "较长的前台命令完成后发出桌面提醒。",
        L10nKey::SettingsNotifyThreshold => "通知阈值",
        L10nKey::SettingsNotifyThresholdDesc => "命令需运行多久才能算作“较长”。",
        L10nKey::SettingsWindow => "窗口",
        L10nKey::NotifyModeNever => "从不",
        L10nKey::NotifyModeUnfocused => "窗口未聚焦时",
        L10nKey::NotifyModeAlways => "总是",
        L10nKey::SettingsStartupNormal => "普通",
        L10nKey::SettingsStartupMaximized => "最大化",
        L10nKey::SettingsStartupFullscreen => "全屏",
        L10nKey::SettingsAfterCurrent => "当前之后",
        L10nKey::SettingsAtEnd => "末尾",
        L10nKey::SettingsTop => "顶部",
        L10nKey::SettingsLeft => "左侧",
        L10nKey::SettingsByRepo => "按仓库",
        L10nKey::SettingsFlat => "平铺",
        L10nKey::SettingsPreset => "预设",
        L10nKey::SettingsPresetDesc => {
            "tmux 预设把窗格/标签页操作映射为前缀序列（例如 Ctrl-B 后按 C）。"
        }
        L10nKey::SettingsPrefix => "前缀",
        L10nKey::SettingsPressKeys => "按下按键…",
        L10nKey::SettingsPauseToSaveEsc => "暂停以保存 · Esc",
        L10nKey::SettingsKeybindingsIntroDesc => {
            "点击某个快捷键，然后按下新按键，短暂停顿后便会保存。可连续按键组成序列，例如 Ctrl-B 后按 X。Esc 取消；Backspace 移除最后一个按键，若最先按下则重置为默认。"
        }
        L10nKey::SettingsPrefixNote => {
            "启用前缀后，单独按前缀键约 1 秒后会传给 shell，前缀 + 未绑定的按键会直接发送到终端。"
        }
        L10nKey::SettingsRestoreAllDefaults => "恢复全部默认值",
        L10nKey::SettingsRestoreAllDefaultsBody => "你改过的每一个按键都会退回默认值，且无法撤销。",
        L10nKey::KeybindGoToTab => "跳到第 {n} 个标签页",
        L10nKey::KeybindGoToWorkspace => "跳到第 {n} 个工作区",
        L10nKey::KeybindInsertNewline => "插入换行",
        L10nKey::KeybindForkSessionRight => "向右 Fork 会话",
        L10nKey::KeybindForkSessionLeft => "向左 Fork 会话",
        L10nKey::KeybindForkSessionDown => "向下 Fork 会话",
        L10nKey::KeybindForkSessionUp => "向上 Fork 会话",
        L10nKey::SettingsAboutDesc1 => "终端工作台：常驻会话、远程工作、agent。",
        L10nKey::SettingsVersion => "版本",
        L10nKey::SettingsUpdates => "更新",
        L10nKey::SettingsUpdateAndRelaunch => "更新并重新启动",
        L10nKey::SettingsUpdateViewRelease => "查看发布页面",
        L10nKey::SettingsUpdateChecking => "正在检查更新…",
        L10nKey::SettingsUpdateUpToDate => "当前已是最新版本。",
        L10nKey::SettingsUpdateDownloadingPercent => "正在下载更新… {percent}%，共 {size}",
        L10nKey::SettingsUpdateDownloadingBytes => "正在下载更新… 已下载 {received}",
        L10nKey::SettingsUpdateVerifying => "正在校验下载的更新…",
        L10nKey::SettingsUpdateInstalling => "正在通过更新重新启动…",
        L10nKey::SettingsUpdateCheckNow => "立即检查",
        L10nKey::SettingsUpdateCancel => "取消下载",
        L10nKey::SettingsUpdateRetry => "重试",
        L10nKey::SettingsUpdateDismiss => "知道了",
        L10nKey::SettingsUpdateDownloadManually => "手动下载",
        L10nKey::SettingsUpdateFailedTitle => "更新到 {version} 失败。",
        L10nKey::SettingsUpdateReady => "{version} 已下载并校验完成，可以安装。",
        L10nKey::SettingsUpdateReadyNextLaunch => "下次启动 tty7 时会自动装上。",
        L10nKey::SettingsUpdateInstallNow => "安装并重启",
        L10nKey::SettingsUpdateDiscard => "丢弃",
        L10nKey::SettingsAutoDownload => "后台下载更新",
        L10nKey::SettingsAutoDownloadDesc => {
            "发现新版本就先下载并校验好，安装时只需重启一下。不会未经确认就安装。用移动流量时可以关掉——安装包约 30 MB。"
        }
        L10nKey::SettingsUpdateChannel => "更新通道",
        L10nKey::SettingsUpdateChannelDesc => {
            "Stable 跟随正式发布的版本，Nightly 跟随每晚从最新代码构建的版本——更新更快，但没有经过发布测试。"
        }
        L10nKey::SettingsUpdateChannelStable => "稳定版",
        L10nKey::SettingsUpdateChannelNightly => "每夜构建",
        L10nKey::SettingsDaemonStale => "后台 server 仍运行在 {build}。",
        L10nKey::SettingsDaemonStaleDesc => {
            "tty7 是原地升级的，界面已经是新版本，但各个 pane 仍由旧版本的后台 server 托管。重启 server 才能用上新版本，代价是 pane 里正在跑的进程全部结束——shell、agent、SSH 会话都算。不急，挑个 pane 空闲的时候再重启。"
        }
        L10nKey::UpdateDialogTitle => "有可用更新",
        L10nKey::UpdateDialogDetail => {
            "tty7 {version} 已发布，你现在是 {current}。安装会重启应用；后台 server 不动，pane 里开着的东西都还在。"
        }
        L10nKey::UpdateDialogDetailWindows => {
            "tty7 {version} 已发布，你现在是 {current}。安装会重启应用和后台 server：pane 里正在运行的进程会被结束，标签页和布局会以全新的 shell 恢复。"
        }
        L10nKey::UpdateDialogDetailManual => "tty7 {version} 已发布，你现在是 {current}。{hint}",
        L10nKey::UpdateDialogCannotSelfUpdate => "这份安装无法自行更新。",
        L10nKey::UpdateDialogLater => "以后再说",
        L10nKey::UpdateDialogNextLaunch => "下次启动时安装",
        L10nKey::SettingsUpdateCheckFailed => "无法检查更新：{error}",
        L10nKey::SettingsUpdatePrepareFailed => "更新失败：{error}",
        L10nKey::SettingsUpdateLaunchFailed => "无法启动安装程序：{error}",
        L10nKey::SettingsUpdateUnsupportedMacos => {
            "当前副本并非从可写的 tty7.app 包运行，直接替换并不安全。请将 tty7 移到“应用程序”或其他可写文件夹，或者打开发布页面安装更新。"
        }
        L10nKey::SettingsUpdateUnsupportedLinux => {
            "发布版本中没有适用于该架构的 Linux 包。请自行从源码构建，或使用包管理器。"
        }
        L10nKey::SettingsUpdateLinuxPackage => {
            "Linux 需要手动更新。请到发布页面下载 {name}，或使用包管理器。"
        }
        L10nKey::SettingsUpdateUnsupportedWindows => {
            "Windows 自动更新适用于可识别的 Inno Setup 安装版和便携 ZIP 版。当前副本缺少有效的安装标记、更新程序或可写的便携目录，请打开发布页面手动更新。"
        }
        L10nKey::SettingsUpdateWindowsAllUsers => {
            "tty7 是为所有用户安装的，替换它需要管理员权限。tty7 不会自行弹出提权请求，请打开发布页面并自行运行安装程序进行更新。"
        }
        L10nKey::SettingsUpdateUnsupportedPlatform => "此平台不支持自动安装，请打开发布页面。",
        L10nKey::SettingsUpdateMissingPackage => {
            "该版本没有适用于当前安装的 {name} 包。请打开发布页面选择其他包。"
        }
        L10nKey::SettingsUpdateMissingChecksums => {
            "该版本缺少 checksums.txt，因此 tty7 拒绝自动安装。"
        }
        L10nKey::SettingsVersionAvailable => "新版本 {version} 可用。",
        L10nKey::SettingsCheckUpdatesDesc => "无法就地更新的安装方式会改为打开发布页面。",
        L10nKey::SettingsCheckUpdatesOnLaunch => "启动时检查更新",
        L10nKey::SettingsCommandLine => "命令行",
        L10nKey::SettingsCommandLineDesc => {
            "启动时将自带的 tty7 命令加入 PATH，让脚本和编码 agent 可在任意终端驱动 tty7。在 tty7 窗格内两种情况都可用。如果你自己构建或安装了 tty7 且不希望被遮蔽，请关闭此选项。下次启动时生效。"
        }
        L10nKey::SettingsInstallCliOnPath => "将 `tty7` 命令安装到 PATH",
        L10nKey::SettingsServer => "Server",
        L10nKey::SettingsServerDesc => {
            "重启在后台维持 shell 运行的 server。这会结束这台计算机上所有正在运行的 shell；你的标签页和布局会以全新的 shell 重新打开。"
        }
        L10nKey::SettingsHowShellsWorkBody => {
            "你的 shell 跑在后台 server 里，不在这个窗口里。退出 tty7 后它们照常运行——再打开时标签页、布局和工作目录都会回来，里面还是原来那些 shell。关闭标签页会结束它那个 shell；“重启 server”和“退出并停止 server”会结束全部。"
        }
        L10nKey::SettingsRestartServer => "重启 server…",
        L10nKey::SettingsAppHttpProxy => "更新代理",
        L10nKey::SettingsAppHttpProxyDesc => {
            "仅用于 tty7 自身的更新检查和下载，不影响面板中运行的程序。留空则跟随系统代理。"
        }
        L10nKey::SettingsAppHttpProxyInvalid => "不是有效的代理地址，该值未保存。",
        L10nKey::SettingsAgentClaudeCode => "Claude Code",
        L10nKey::SettingsAgentCodex => "Codex",
        L10nKey::SettingsAgentCopilotCli => "Copilot CLI",
        L10nKey::SettingsAgentOpencode => "OpenCode",
        L10nKey::SettingsAgentPi => "Pi",
        L10nKey::SettingsAgentGrokBuild => "Grok Build",
        L10nKey::SettingsAgentOhMyPi => "Oh My Pi",
        L10nKey::SettingsSearchAboutKeywords => {
            "关于 版本 许可证 致谢 构建 更新 检查 github about version license credits update"
        }
        L10nKey::SettingsSearchAppHttpProxyKeywords => {
            "代理 proxy http https socks socks5 clash v2ray 网络 下载 更新"
        }
        L10nKey::SettingsSearchAnsiColorsKeywords => {
            "ANSI颜色 调色板 终端颜色 主题 ansi colors palette terminal theme"
        }
        L10nKey::SettingsSearchBackgroundImageKeywords => {
            "背景图片 背景图 壁纸 图片 照片 主题 background image wallpaper picture theme"
        }
        L10nKey::SettingsSearchImageOpacityKeywords => {
            "背景图片 不透明度 透明度 强度 壁纸 background image opacity strength fade"
        }
        L10nKey::SettingsSearchArgumentsKeywords => {
            "参数 shell 启动参数 登录参数 arguments shell flags login args"
        }
        L10nKey::SettingsSearchBlurKeywords => {
            "模糊 毛玻璃 半透明 窗口 背景 blur frosted vibrancy window background"
        }
        L10nKey::SettingsSearchBoldFontKeywords => "粗体 字体粗细 字重 bold font weight typeface",
        L10nKey::SettingsSearchClaudeCodeKeywords => {
            "Claude Code agent 集成 hook 安装 卸载 状态 会话 claude agent integration hooks install"
        }
        L10nKey::SettingsSearchCodexKeywords => {
            "Codex agent 集成 hook 安装 OpenAI codex agent integration hooks install"
        }
        L10nKey::SettingsSearchCommandLineToolKeywords => {
            "命令行工具 cli tty7 路径 shell 命令 安装 符号链接 terminal command line tool"
        }
        L10nKey::SettingsSearchCopilotCliKeywords => {
            "Copilot CLI agent 集成 hook 安装 GitHub copilot agent integration hooks install"
        }
        L10nKey::SettingsSearchCopyOnSelectKeywords => {
            "选中即复制 复制 剪贴板 选择 鼠标 copy on select clipboard yank"
        }
        L10nKey::SettingsSearchCursorBlinkKeywords => {
            "光标闪烁 闪烁 光标 blink cursor blinking flash"
        }
        L10nKey::SettingsSearchCursorShapeKeywords => {
            "光标形状 光标 块 竖线 下划线 cursor shape caret block bar underline beam"
        }
        L10nKey::SettingsSearchCustomThemesKeywords => {
            "自定义主题 复制 编辑 颜色 文件夹 背景图片 壁纸 yaml 导入 theme custom edit duplicate colors import background image wallpaper"
        }
        L10nKey::SettingsSearchDetectUrlsKeywords => {
            "检测URL 链接 超链接 可点击 打开 detect urls links hyperlink open"
        }
        L10nKey::SettingsSearchDiffPreviewFromCountsKeywords => {
            "从侧栏计数打开 diff 预览 diff 预览 侧栏 git diff preview sidebar counts git changes"
        }
        L10nKey::SettingsSearchDimInactivePanesKeywords => {
            "调暗 非活动窗格 淡化 未聚焦 分屏 高亮 active dimming pane focus"
        }
        L10nKey::SettingsSearchFocusFollowsMouseKeywords => {
            "焦点跟随鼠标 悬停 激活 窗格 focus follows mouse hover activate pane"
        }
        L10nKey::SettingsSearchFontFamilyKeywords => {
            "字体 字体族 等宽 排版 font family monospace typography typeface"
        }
        L10nKey::SettingsSearchFontLigaturesKeywords => {
            "字体连字 连字 字形 typography ligatures glyph fira"
        }
        L10nKey::SettingsSearchFontSizeKeywords => {
            "字号 字体大小 文字 放大 缩小 typography font size bigger smaller zoom"
        }
        L10nKey::SettingsSearchForwardSshLoopbackLinksKeywords => {
            "SSH回环链接 端口转发 隧道 localhost 转发 forward ssh loopback links tunnel"
        }
        L10nKey::SettingsSearchGrokBuildKeywords => {
            "Grok Build agent 集成 hook 安装 xai grok build agent integration hooks install"
        }
        L10nKey::SettingsSearchHideMouseWhileTypingKeywords => {
            "输入时隐藏鼠标 隐藏鼠标 指针 自动隐藏 hide mouse typing cursor pointer autohide"
        }
        L10nKey::SettingsSearchHistorySearchKeywords => {
            "历史搜索 反向搜索 模糊搜索 ctrl-r fzf history search recall"
        }
        L10nKey::SettingsSearchHostsKeywords => {
            "主机 SSH 连接 保存 主机配置 配置文件 导入 ssh_config 管理 添加 编辑 快速连接 hosts ssh profile import connect"
        }
        L10nKey::SettingsSearchHowShellsWorkKeywords => {
            "Shell工作原理 shell 会话 守护进程 持久化 后台 工作区 布局 survive reboot daemon how shells work"
        }
        L10nKey::SettingsSearchHowShellsWorkTitle => "Shell 工作原理",
        L10nKey::SettingsSearchItalicFontKeywords => "斜体 字体样式 italic oblique typeface",
        L10nKey::SettingsSearchKeybindingsKeywords => {
            "按键绑定 快捷键 热键 键盘 绑定 前缀 tmux keybindings shortcut hotkey binding prefix"
        }
        L10nKey::SettingsSearchKeybindingsTitle => "按键绑定",
        L10nKey::SettingsSearchLineHeightKeywords => {
            "行高 行间距 行距 typography line height spacing leading"
        }
        L10nKey::SettingsSearchNewTabPositionKeywords => {
            "新标签页位置 标签页 顺序 末尾 当前之后 new tab position tabs order end after current"
        }
        L10nKey::SettingsSearchNotifyOnCommandFinishKeywords => {
            "命令完成时通知 通知 提醒 命令 notify command finish notification alert desktop"
        }
        L10nKey::SettingsSearchNotifyThresholdKeywords => {
            "通知阈值 通知 秒数 时长 命令 notify threshold notification duration seconds"
        }
        L10nKey::SettingsSearchOpacityKeywords => {
            "不透明度 透明度 窗口 半透明 alpha opacity transparency translucent window"
        }
        L10nKey::SettingsSearchOpenFilesWithKeywords => {
            "打开文件 链接 编辑器 命令 外部应用 路径 行号 列号 open files editor command path line column"
        }
        L10nKey::SettingsSearchOpencodeKeywords => {
            "OpenCode agent 集成 插件 安装 opencode agent integration plugin install"
        }
        L10nKey::SettingsSearchOptionAsMetaKeywords => {
            "Option作为Meta 修饰键 alt option meta 转义 escape macos keyboard modifier"
        }
        L10nKey::SettingsSearchOhMyPiKeywords => {
            "Oh My Pi agent 集成 扩展 安装 omp oh my pi agent integration extension install"
        }
        L10nKey::SettingsSearchPiKeywords => {
            "Pi agent 集成 扩展 安装 pi agent integration extension install"
        }
        L10nKey::SettingsSearchPortForwardingKeywords => {
            "端口转发 SSH 隧道 本地 远程 动态 SOCKS 转发 port forwarding ssh tunnel local remote"
        }
        L10nKey::SettingsSearchProgramKeywords => {
            "程序 shell 二进制 zsh bash fish nu nushell pwsh powershell 可执行文件 启动 program shell binary launch"
        }
        L10nKey::SettingsSearchRememberWindowSizeKeywords => {
            "记住窗口大小位置 窗口 大小 位置 启动 记住 remember window size position geometry"
        }
        L10nKey::SettingsSearchReportMouseToAppsKeywords => {
            "鼠标报告 鼠标 vim tmux 点击 滚动 shift report mouse apps"
        }
        L10nKey::SettingsSearchRestoreLastLayoutKeywords => {
            "恢复上次布局 恢复 会话 标签页 分屏 布局 restore last layout tabs splits"
        }
        L10nKey::SettingsSearchScrollSpeedKeywords => {
            "滚动速度 鼠标滚轮 滚动倍率 scroll speed mouse wheel multiplier scrolling"
        }
        L10nKey::SettingsSearchSmoothScrollKeywords => {
            "平滑滚动 动画 缓动 滚轮 触控板 smooth animation ease wheel trackpad scrolling"
        }
        L10nKey::SettingsSearchUpdateChannelKeywords => {
            "更新通道 稳定版 每夜构建 预发布 版本 update channel stable nightly release"
        }
        L10nKey::SettingsSearchCheckUpdatesOnLaunchKeywords => {
            "启动时检查更新 自动 检查 启动 update check launch startup automatic"
        }
        L10nKey::SettingsSearchAutoDownloadKeywords => {
            "后台下载更新 下载 后台 安装 流量 update download background install metered"
        }
        L10nKey::SettingsSearchScrollbackKeywords => {
            "scrollback 回看 向上滚动 历史 缓冲区 行数 scrollback history buffer lines"
        }
        L10nKey::SettingsSearchShowTrayIconKeywords => {
            "显示托盘图标 托盘 菜单栏 状态 图标 show tray icon menu bar status"
        }
        L10nKey::SettingsSearchSidebarGroupingKeywords => {
            "侧栏分组 标签页 分组 仓库 git 侧栏 sidebar grouping tabs repo repository"
        }
        L10nKey::SettingsSearchSmartSelectionKeywords => {
            "智能选择 双击 选择 单词 URL 路径 邮箱 括号 smart selection double click"
        }
        L10nKey::SettingsSearchStartInKeywords => {
            "起始目录 工作目录 启动目录 主目录 继承 自定义 cwd working directory start home inherit custom"
        }
        L10nKey::SettingsSearchSyncWithSystemKeywords => {
            "主题 跟随系统 自动 深色 浅色 外观 模式 theme dark light auto follow system"
        }
        L10nKey::SettingsSearchLegiblePaletteKeywords => {
            "颜色 低对比度 可读 纠偏 调色板 修正 contrast legible palette parameter"
        }
        L10nKey::SettingsSearchTabBarPositionKeywords => {
            "标签栏位置 标签栏 侧边栏 左侧 顶部 布局 tab bar position tabs sidebar left top"
        }
        L10nKey::SettingsSearchTabCompletionKeywords => {
            "Tab补全 补全 菜单 建议 tab completion suggestions prompt"
        }
        L10nKey::SettingsSearchTerminalBellKeywords => {
            "终端铃声 铃声 提示音 闪烁 静音 两者 同时 beep bell terminal audible visual both"
        }
        L10nKey::SettingsSearchThemeKeywords => {
            "外观 颜色 主题 配色 深色 浅色 背景 前景 强调色 跟随系统 appearance color scheme dark light palette"
        }
        L10nKey::SettingsSearchTrimTrailingSpacesKeywords => {
            "复制时去除空格 去除末尾空格 剪贴板 空白 trim trailing spaces copy whitespace"
        }
        L10nKey::SettingsSearchVerifyHostKeysKeywords => {
            "校验主机密钥 主机密钥 known_hosts 指纹 mitm 安全 verification ssh host keys"
        }
        L10nKey::SettingsSearchWarnBeforeClosingKeywords => {
            "关闭前警告 确认关闭 SSH 标签页 窗格 会话 warn before closing ssh confirm"
        }
        L10nKey::SettingsSearchStartupWindowKeywords => {
            "启动窗口 启动 最大化 全屏 普通 startup window launch maximized fullscreen normal"
        }
        L10nKey::SwitcherNoMatch => "没有匹配的工作区或机器。",
        L10nKey::AddSshHost => "添加 SSH 主机…",
        L10nKey::ClickForNewWindow => "点击打开新窗口",
        L10nKey::RestartServer => "重启 server",
        L10nKey::OtherMachines => "其他机器",
        L10nKey::Ok => "确定",
        L10nKey::SftpNoTransfers => "还没有传输任务。",
        L10nKey::SftpPanelTitleFiles => "文件",
        L10nKey::SftpTooltipRefresh => "刷新",
        L10nKey::SftpTooltipMore => "更多",
        L10nKey::SftpMenuNewFolder => "新建文件夹",
        L10nKey::SftpMenuNewFile => "新建文件",
        L10nKey::SftpMenuUpload => "上传…",
        L10nKey::SftpMenuGotoShellCwd => "转到 shell 目录",
        L10nKey::SftpMenuHideTransferHistory => "隐藏传输历史",
        L10nKey::SftpMenuTransferHistory => "传输历史",
        L10nKey::SftpEditNewFolder => "新建文件夹",
        L10nKey::SftpEditNewFile => "新建文件",
        L10nKey::SftpEditRename => "重命名",
        L10nKey::SftpEditPermissions => "权限 · {mode}",
        L10nKey::SftpLoading => "加载中…",
        L10nKey::SftpEmptyDirectory => "空文件夹。",
        L10nKey::SftpContextOpen => "打开",
        L10nKey::SftpContextFollowSymlink => "跟随符号链接",
        L10nKey::SftpContextRename => "重命名",
        L10nKey::SftpContextChmod => "权限…",
        L10nKey::SftpTransferSummaryRunning => "{count} 个传输中 · {pct}%",
        L10nKey::SftpTransferSummaryFailed => "{count} 个失败",
        L10nKey::SftpTransferSummaryIdle => "传输",
        L10nKey::SftpTransferProgress => "{done} / {total} ({pct}%)",
        L10nKey::SftpTransferDone => "完成",
        L10nKey::SftpTransferCancelled => "已取消",
        L10nKey::SftpTransferError => "错误",
        L10nKey::SftpTransferListFailed => "无法获取传输状态：{error}",
        L10nKey::SftpImagePasteUploadFailed => "无法将粘贴的图片上传到 {host}：{error}",
        L10nKey::ForwardPanelTitle => "端口转发",
        L10nKey::ForwardDisconnected => "已断开",
        L10nKey::ForwardDisconnectedFrom => "与 {host} 的连接已断开",
        L10nKey::SshEditProfile => "编辑连接…",
        L10nKey::ForwardTooltipAdd => "添加转发",
        L10nKey::ForwardTooltipRemove => "移除",
        L10nKey::ForwardLocal => "本地",
        L10nKey::ForwardRemote => "远程",
        L10nKey::ForwardDynamic => "动态",
        L10nKey::ForwardBindLabel => "绑定",
        L10nKey::ForwardToLabel => "到",
        L10nKey::ForwardSocksLabel => "SOCKS",
        L10nKey::ForwardAdd => "添加",
        L10nKey::FileTreePlaceholderFileName => "文件名",
        L10nKey::FileTreePlaceholderFolderName => "文件夹名",
        L10nKey::FileTreePlaceholderNewName => "新名称",
        L10nKey::FileTreeDeleteTitle => "删除“{name}”？",
        L10nKey::FileTreeDeleteFolderBody => "该文件夹及其中的所有内容都将被删除。",
        L10nKey::FileTreeDeleteFileBody => "该文件将被删除。",
        L10nKey::SftpDeleteFolderBody => {
            "该文件夹及其中所有内容将在 {host} 上被删除。远端没有回收站。"
        }
        L10nKey::SftpDeleteFileBody => "该文件将在 {host} 上被删除。远端没有回收站。",
        L10nKey::FileTreeDeleteFailed => "无法删除 {name}",
        L10nKey::FileTreeCreateFailed => "无法创建 {name}",
        L10nKey::FileTreeRenameFailed => "无法重命名 {name}",
        L10nKey::FileTreeContextOpen => "打开",
        L10nKey::FileTreeContextCdHere => "cd 到此处",
        L10nKey::FileTreeContextInsertPath => "在终端中插入路径",
        L10nKey::FileTreeContextAttachAgent => "附加到 agent",
        L10nKey::FileTreeContextNewFile => "新建文件",
        L10nKey::FileTreeContextNewFolder => "新建文件夹",
        L10nKey::FileTreeContextRename => "重命名",
        L10nKey::FileTreeContextCopyPath => "复制路径",
        L10nKey::FileTreeContextHideDotfiles => "隐藏点文件",
        L10nKey::FileTreeContextShowDotfiles => "显示点文件",
        L10nKey::FileDropIntoItself => "文件夹不能复制到它自己里面。",
        L10nKey::FileDropNotHere => "不在这台机器上。",
        L10nKey::FileDropTooDeep => "文件夹嵌套超过 {n} 层。",
        L10nKey::FileDropTooLarge => "超过 {limit} MB，请改用 SFTP 传输。",
        L10nKey::FileDropReplaceTitle => "替换“{name}”？",
        L10nKey::FileDropReplaceManyTitle => "替换 {n} 个项目？",
        L10nKey::FileDropReplaceBody => "这个目录下已经有同名的东西了，替换后无法撤销。",
        L10nKey::FileDropReplace => "替换",
        L10nKey::FileDropFailed => "无法复制 {name}",
        L10nKey::FileDropFailedMany => "无法复制 {name}，另有 {n} 个也失败了",
        L10nKey::SshPromptNewKey => "新 {fingerprint}",
        L10nKey::SshPromptOldKey => "旧 {old_fingerprint}",
        L10nKey::EditorCantOpen => "无法打开 {path}：{e}",
        L10nKey::EditorCantRead => "无法读取 {path}：{e}",
        L10nKey::EditorNotUtf8 => "“{path}”不是有效的 UTF-8",
        L10nKey::EditorSaveFailed => "保存 {name} 失败",
        L10nKey::EditorUnsavedChanges => "“{name}”有未保存的更改",
        L10nKey::EditorDiscard => "放弃",
        L10nKey::EditorNoFileOpen => "没有打开的文件",
        L10nKey::EditorBackToTerminal => "返回终端 (Esc)",
        L10nKey::EditorLnCol => "行 {line}，列 {column}",
        L10nKey::EditorEdit => "编辑",
        L10nKey::EditorPreview => "预览",
        L10nKey::EditorWrapOn => "自动换行：开",
        L10nKey::EditorWrapOff => "自动换行：关",
        L10nKey::EditorFileTooLarge => "“{path}”太大，无法在编辑器中打开（{size} MB）",
        L10nKey::EditorBinaryFile => "“{path}”看起来是二进制文件",
        L10nKey::PanelInfoTitle => "信息",
        L10nKey::PanelChangesTitle => "源代码管理",
        L10nKey::PanelScmTitle => "源代码管理",
        L10nKey::PanelFilesTitle => "文件",
        L10nKey::PanelNoSession => "没有活动会话。",
        L10nKey::PanelNoSessionHint => "打开一个标签页以在此处查看其 shell、目录和进程。",
        L10nKey::PanelNoWorkingDirectory => "没有工作目录。",
        L10nKey::PanelNoWorkingDirectoryHint => "此窗格尚未报告工作目录。",
        L10nKey::PanelLoading => "加载中…",
        L10nKey::PanelNotAGitRepo => "不是 git 仓库。",
        L10nKey::PanelNotAGitRepoHint => "进入 git 仓库后，此标签页会列出未提交的变更。",
        L10nKey::PanelNoChanges => "没有未提交的变更。",
        L10nKey::PanelNoChangesHint => "worktree 是干净的。",
        L10nKey::PanelSessionSubtitle => "会话",
        L10nKey::PanelProcessesSubtitle => "进程",
        L10nKey::PanelPortsSubtitle => "端口",
        L10nKey::PanelCwd => "工作目录",
        L10nKey::PanelShell => "shell",
        L10nKey::PanelSsh => "ssh",
        L10nKey::PanelBranch => "分支",
        L10nKey::PanelChangesRow => "变更",
        L10nKey::PanelAgent => "agent",
        L10nKey::PanelAgentIdle => "空闲",
        L10nKey::PanelAgentWorking => "进行中",
        L10nKey::PanelAgentWaiting => "等待中",
        L10nKey::PanelAgentDone => "已完成",
        L10nKey::PanelRevealInFinder => "在 Finder 中显示",
        L10nKey::PanelOpenFolder => "打开文件夹",
        L10nKey::ScmGroupMerge => "合并冲突",
        L10nKey::ScmGroupStaged => "暂存的更改",
        L10nKey::ScmGroupChanges => "更改",
        L10nKey::ScmGroupUntracked => "未跟踪",
        L10nKey::ScmCommitPlaceholder => "说说改了什么…",
        L10nKey::ScmCommitButton => "提交",
        L10nKey::ScmCommitAllButton => "提交全部",
        L10nKey::ScmCommitAmendButton => "提交（修订）",
        L10nKey::ScmCommitAndPush => "提交并推送",
        L10nKey::ScmCommitAndSync => "提交并同步",
        L10nKey::ScmAmendLastCommit => "修订上一次提交",
        L10nKey::ScmCommitStaged => "提交已暂存的更改",
        L10nKey::ScmStashAll => "全部贮藏",
        L10nKey::ScmNothingToCommit => "没有可提交的内容",
        L10nKey::ScmNetworkBusy => "这个仓库还有一个网络操作在进行中",
        L10nKey::ScmCommitNeedsMessage => "先写一条提交信息",
        L10nKey::ScmDetailFilesFailed => "无法读取文件列表",
        L10nKey::ScmTimeNow => "刚刚",
        L10nKey::ScmTimeMinutes => "{n}分",
        L10nKey::ScmTimeHours => "{n}时",
        L10nKey::ScmTimeDays => "{n}天",
        // "个月" 而不是 "月":"3月" 会被读成月份名。
        L10nKey::ScmTimeMonths => "{n}个月",
        L10nKey::ScmTimeYears => "{n}年",
        L10nKey::ScmResetHardConfirm => {
            "把分支重置到这个提交?之后的提交会从分支上消失,未提交的更改会被丢弃。"
        }
        L10nKey::ScmReset => "重置",
        L10nKey::ScmChipStaged => "已暂存",
        L10nKey::ScmStage => "暂存更改",
        L10nKey::ScmStageAll => "暂存全部更改",
        L10nKey::ScmUnstage => "取消暂存",
        L10nKey::ScmUnstageAll => "取消暂存全部更改",
        L10nKey::ScmDiscard => "放弃更改",
        L10nKey::ScmDiscardAll => "放弃全部更改",
        L10nKey::ScmDiscardConfirm => "放弃对 {path} 的更改？此操作无法撤销。",
        L10nKey::ScmOpenConflict => "解决冲突",
        L10nKey::ScmMarkResolved => "标记为已解决",
        L10nKey::ScmUnrepresentablePath => "该路径不是合法的 UTF-8，无法传给 git —— 仅可查看。",
        L10nKey::ScmPublishBranch => "发布分支",
        L10nKey::ScmDetached => "游离头指针",
        L10nKey::ScmAmendBadge => "修订",
        L10nKey::ScmSync => "同步更改",
        L10nKey::ScmPush => "推送",
        L10nKey::ScmPull => "拉取",
        L10nKey::ScmFetch => "获取",
        L10nKey::ScmCheckoutBranch => "切换到…",
        L10nKey::ScmCreateBranch => "新建分支…",
        L10nKey::ScmSearchBranches => "搜索分支…",
        L10nKey::ScmStashAndSwitch => "贮藏并切换",
        L10nKey::ScmGraphTitle => "提交历史",
        L10nKey::ScmGraphLoadMore => "加载更多",
        L10nKey::ScmGraphFilterPlaceholder => "筛选提交…",
        L10nKey::ScmGraphAllBranches => "全部分支",
        L10nKey::ScmGraphEmpty => "还没有提交",
        L10nKey::ScmGraphCurrentBranch => "当前分支",
        L10nKey::ScmCheckoutCommit => "检出此提交",
        L10nKey::ScmCreateBranchHere => "在此创建分支…",
        L10nKey::ScmResetSoft => "重置（保留暂存）",
        L10nKey::ScmResetMixed => "重置（保留工作区）",
        L10nKey::ScmResetHard => "重置（丢弃更改）",
        L10nKey::ScmCommitDetailTitle => "提交",
        L10nKey::ScmCopyCommitSha => "复制提交 SHA",
        L10nKey::ScmCherryPick => "拣选提交",
        L10nKey::ScmRevertCommit => "还原提交",
        L10nKey::ScmResetToCommit => "重置到该提交",
        L10nKey::ScmRefresh => "刷新",
        L10nKey::ScmBackToChanges => "返回",
        L10nKey::ScmCommitParents => "父提交",
        L10nKey::ScmShowMore => "展开",
        L10nKey::ScmShowLess => "收起",
        L10nKey::ScmCommitNotFound => "本仓库中没有这个提交。",
        L10nKey::ScmTooManyChanges => "改动过多，仅显示前 {shown} 项（共 {total} 项）。",
        L10nKey::ScmOpenChanges => "查看改动",
        L10nKey::ScmDiscardAllConfirm => "放弃本仓库的全部改动？此操作无法撤销。",
        L10nKey::ScmAmendConfirm => {
            "修补上一次提交？它会被一个新提交取代，已经拿到旧提交的人需要自行处理。"
        }
        L10nKey::ScmOpMerge => "合并中",
        L10nKey::ScmOpRebase => "变基中",
        L10nKey::ScmOpCherryPick => "拣选中",
        L10nKey::ScmOpRevert => "还原中",
        L10nKey::ScmOpBisect => "二分查找中",
        L10nKey::ScmOpAm => "应用补丁中",
        L10nKey::ScmSwitchRepository => "切换仓库",
        L10nKey::WindowStop => "停止",
        L10nKey::WindowDelete => "删除",
        L10nKey::WindowThisWorkspace => "此工作区",
        L10nKey::WindowConfirmTitle => "{verb}工作区“{name}”？",
        L10nKey::WindowStopUnreachable => "无法连接到其所在机器。仍在运行的 shell 将会被终止。",
        L10nKey::WindowDeleteUnreachable => {
            "无法连接到其所在机器。仍在运行的 shell 将会被终止，布局也将被清除。"
        }
        L10nKey::WindowStopShells => "{count} 个正在运行的 shell 将会被终止。",
        L10nKey::WindowDeleteShells => "{count} 个正在运行的 shell 将会被终止，布局也将被清除。",
        L10nKey::DiffReading => "正在读取 diff…",
        L10nKey::DiffNotARepo => "不是 git 仓库",
        L10nKey::DiffReadFailed => "无法读取 worktree diff——下次刷新时重试。",
        L10nKey::DiffWorkingTreeClean => "worktree 干净",
        L10nKey::DiffCloseTooltip => "关闭 diff (Esc)",
        L10nKey::DiffChangedFiles => "{count} 个变更文件",
        L10nKey::DiffUntrackedCount => " · {count} 个未跟踪文件",
        L10nKey::DiffMoreFiles => "…还有 {count} 个变更文件——在终端中运行 git diff 查看。",
        L10nKey::DiffOversizedNotice => {
            "此 worktree 太大，无法高效渲染（{summary}）。每个文件都已折叠——可展开单个文件，或在终端中运行 git diff。"
        }
        L10nKey::DiffTruncatedPerFile => {
            "diff 在 {limit} 行处截断——在终端中运行 git diff 查看其余部分。"
        }
        L10nKey::DiffTruncatedBudget => {
            "内容未加载——此 worktree 已超出 tty7 的 diff 预算。在终端中运行 git diff 查看此文件。"
        }
        L10nKey::DiffUntrackedHeader => "未跟踪文件 ({count})",
        L10nKey::DiffMoreUntracked => "…还有 {count} 个——在终端中运行 git status 查看。",
        L10nKey::DiffLines => "{count} 行 diff",
        L10nKey::DiffChangedLines => "{total} 行变更，在 {cap} 截断前已加载 {loaded} 行 diff",
        L10nKey::DiffBudgetAndCap => "tty7 的预算和单文件上限",
        L10nKey::DiffBudget => "tty7 的预算",
        L10nKey::DiffPerFileCap => "单文件上限",
        L10nKey::DiffUntrackedSummary => "{count} 个未跟踪",
        L10nKey::DiffViewSplit => "并排",
        L10nKey::DiffViewUnified => "统一",
        L10nKey::PendingConnecting => "正在连接 {machine}…",
        L10nKey::PendingUnreachable => "无法连接到 {machine}",
        L10nKey::WorktreePromptNeedsName => "worktree 需要一个名称",
        L10nKey::WorktreePromptTitle => "新建 worktree 标签页",
        L10nKey::WorktreePromptName => "worktree 名称",
        L10nKey::WorktreePromptBranch => "新分支",
        L10nKey::WorktreePromptBase => "起始分支",
        L10nKey::WorktreePromptCreating => "正在创建…",
        L10nKey::WorktreePromptCreate => "创建",
        L10nKey::AppNewWorktreeFailed => "新建 worktree 失败：{error}",
        L10nKey::HomeTimeJustNow => "刚刚",
        L10nKey::HomeTimeMinutesAgo => "{count} 分钟前",
        L10nKey::HomeTimeHourAgo => "1 小时前",
        L10nKey::HomeTimeHoursAgo => "{count} 小时前",
        L10nKey::HomeTimeYesterday => "昨天",
        L10nKey::HomeTimeDaysAgo => "{count} 天前",
        L10nKey::HomeTimeWeeksAgo => "{count} 周前",
        L10nKey::HomeTimeMonthsAgo => "{count} 个月前",
        L10nKey::HomeTimeOverYearAgo => "一年多前",
        L10nKey::HomeReopenNamed => "重新打开“{name}”",
        L10nKey::RemoteStripDisconnected => "未连接到 {machine}",
        L10nKey::RemoteStripConnecting => "正在连接 {machine}…",
        L10nKey::RemoteStripReconnecting => "正在重新连接 {machine}…",
        L10nKey::RemoteStripReconnectingAttempt => "正在重新连接 {machine}…（第 {count} 次尝试）",
        L10nKey::RemoteStripPreempted => "此工作区已在 {by} 上打开",
        L10nKey::RemoteStripFailed => "未连接到 {machine}——{error}",
        L10nKey::RemoteNoticePreempted => "已在别处打开——输入无效",
        L10nKey::RemoteNoticeDisconnected => "未连接——输入无效",
        L10nKey::RemoteActionRetryNow => "立即重试",
        L10nKey::RemoteActionTakeBack => "收回",
        L10nKey::RemoteActionConnect => "连接",
        L10nKey::RemoteActionRetry => "重试",
        L10nKey::RemoteNoConnectionDetails => {
            "此窗口是 {machine} 上的工作区，但 tty7 已没有它的连接详情——\
             请检查其 SSH 主机配置或 ~/.ssh/config 条目是否仍然存在。"
        }
        L10nKey::RemoteThisComputer => "本机",
        L10nKey::RemoteRestartTitle => "重启“{machine}”上的 tty7 server？",
        L10nKey::RemoteRestartBody => {
            "这将停止 {machine} 上的所有 shell——其中仍在运行的任何内容都会被终止，\
             包括此窗口未显示的 shell。工作区和布局会被保留，并以全新的 shell 恢复。"
        }
        L10nKey::RemoteReplaceBody => {
            "tty7 会在 {machine} 上安装匹配的 server 并启动它。\n\
             \n\
             {machine} 上运行的所有会话都会结束，包括此窗口未连接的会话。"
        }
        L10nKey::RemoteRestartFailedTitle => "“{machine}”上的 tty7 server 未被重启",
        L10nKey::RemoteRestartFailedBody => {
            "{error}\n\
             \n\
             那里仍在运行的会话用的还是旧版本。如果它们已经结束，重新连接就会启动此版本的 server。"
        }
        L10nKey::RemoteHostUnreachable => "无法连接到 {machine}：{error}",
        L10nKey::RemoteInstallTitle => "在“{machine}”上安装 tty7 server？",
        L10nKey::RemoteInstallDetail => {
            "tty7 会将其 server 二进制文件写入 {machine}，以便本机可以在那里托管\
             工作区。{machine} 上的其他内容不会被修改，也不会使用 sudo。\n\
             \n\
             {path_label}\u{2003}{path}\n\
             {version_label}\u{2003}{version}\n\
             {size_label}\u{2003}{size}\n\
             {from_label}\u{2003}{from}\n\
             {sha_label}\u{2003}{sha256}\n\
             \n\
             {silent_upgrades}"
        }
        L10nKey::RemoteInstallPathLabel => "路径",
        L10nKey::RemoteInstallVersionLabel => "版本",
        L10nKey::RemoteInstallSizeLabel => "大小",
        L10nKey::RemoteInstallFromLabel => "来源",
        L10nKey::RemoteInstallShaLabel => "SHA-256",
        L10nKey::RemoteInstallSilentUpgrades => "此后在该机器上的升级将静默安装。",
        L10nKey::RemoteInstallBytes => "字节",
        L10nKey::RemoteMismatchTitle => "更新“{machine}”上的 tty7 server？",
        L10nKey::RemoteMismatchDetail => {
            "{machine} 正在使用 {running} 提供 tty7 会话，该版本使用的协议无法被\
             此客户端（{wanted}）识别。tty7 已在那里安装了匹配的 server，\
             但正在运行的是你当前会话所在的版本。\n\
             \n\
             {replace_server}\u{2003}会将其替换为 {wanted} 并结束其托管的所有会话。\n\
             {cancel}\u{2003}会保持 {machine} 现状不变。此窗口将不会连接。"
        }
        L10nKey::RemoteMismatchReplaceServer => "更新 server",
        L10nKey::RemoteMismatchUnknownBuild => "未知构建",
        L10nKey::RemoteMismatchUnknownBuildFromExe => "未知构建（来自 {exe}）",
        L10nKey::RemoteServerOutdated => {
            "{machine} 上的 tty7 server 太旧（{build}），当前这份 tty7 连不上它。\
             更新它才能连接。"
        }
        L10nKey::RemoteServerTooNew => {
            "{machine} 上的 tty7 server（{build}）比当前这份 tty7 还新。\
             请更新本机的 tty7，或把那边的 server 替换成匹配的版本。"
        }
        L10nKey::RemoteDaemonStartFailed => "无法启动 tty7 本地 server：{error}",
        L10nKey::RemoteDaemonUnreachable => "无法连接到 tty7 本地 server：{error}",
        L10nKey::RemoteDaemonTooOld => {
            "此机器上的 tty7 守护进程版本较旧，无法重启 {machine} 上的 server。\
             请退出 tty7（这会停止守护进程）并重新打开，然后重试。"
        }
        L10nKey::RemoteProfileMissing => "该已保存的 SSH 主机配置已不存在",
        L10nKey::RemoteAliasMissing => "“{alias}”已不再位于 ~/.ssh/config 中",
        L10nKey::RemoteWslNoSsh => "WSL 工作区没有 SSH 连接",
        L10nKey::RemoteLocalStdioNoSsh => "本地 --stdio 工作区没有 SSH 连接",
        L10nKey::RemoteHostNotTty7 => "{machine} 已响应，但并非作为 tty7 server：{error}",
        L10nKey::RemoteWorkspaceListFailed => "已连接到 {machine}，但其工作区列表获取失败：{error}",
        L10nKey::RemoteServerRestartFailed => "无法重启 {machine} 上的 tty7 server：{error}",
        L10nKey::RemoteNoRouteToHost => "tty7 已无法到达 {machine}",
        L10nKey::RemoteMachineTreeUnexpectedReply => "server 用 {reply} 回复了机器树请求",
        L10nKey::RemoteMismatchVersionFromExe => "{version}（来自 {exe}）",
        L10nKey::AppNoRunningCodingAgent => {
            "未找到运行中的编码 agent——请先在某个窗格中启动一个（claude、codex 等）。"
        }
        L10nKey::SwitcherThisComputer => "本机",
        L10nKey::SwitcherRestartingServer => "正在重启 tty7 server…",
        L10nKey::SwitcherDownloadingServerWithTotal => "正在下载 tty7 server… {done} / {total}",
        L10nKey::SwitcherDownloadingServerNoTotal => "正在下载 tty7 server… {done}",
        L10nKey::SwitcherCopyingServer => "正在复制 tty7 server… {done} / {total}",
        L10nKey::SwitcherThisWindow => "当前窗口",
        L10nKey::SwitcherOpen => "已打开",
        L10nKey::SwitcherDisconnect => "断开连接",
        L10nKey::SwitcherOpenInNewWindow => "在新窗口中打开",
        L10nKey::SwitcherRename => "重命名…",
        L10nKey::SwitcherPickAWorkspace => "选一个工作区查看它的标签页。",
        L10nKey::SwitcherNoTabs => "这个工作区没有标签页。",
        L10nKey::SwitcherNoTabMatch => "没有匹配的标签页。",
        L10nKey::SwitcherTabsAfterOpening => "打开这个工作区后才能看到它的标签页。",
        L10nKey::SwitcherOpenToManage => "打开这个工作区后才能重命名或停止它。",
        L10nKey::SwitcherConnectToUse => "连接这台机器后才能在上面新建工作区。",
        L10nKey::SwitcherTabCount => "{n} 个标签页",
        L10nKey::SwitcherTabCountOne => "1 个标签页",
        L10nKey::SwitcherActiveTab => "当前",
        L10nKey::SwitcherHoldToSwitch => "按 Tab 移动 · 松开切换",
        L10nKey::SwitcherTabToCrossColumns => "按 Tab 换到另一列",
        L10nKey::SshPromptPasswordFor => "{user}@{host} 的密码",
        L10nKey::SshPromptPassphraseFor => "{key_path} 的密码短语",
        L10nKey::SshPromptTwoFactor => "双因素认证",
        L10nKey::SshPromptUnknownHost => "未知主机 {host}",
        L10nKey::SshPromptHostKeyChanged => "主机密钥已更改——可能存在中间人攻击",
        L10nKey::SshPromptHostKeyChangedBody => "主机密钥与之前信任的密钥不同，这可能是一次攻击。",
        L10nKey::SshPromptConnect => "连接",
        L10nKey::SshPromptUnlock => "解锁",
        L10nKey::SshPromptSubmit => "提交",
        L10nKey::HostOpsError => "{context}：{error}",
        L10nKey::IoDenied => "没有权限。",
        L10nKey::IoGone => "它已经不在了。",
        L10nKey::IoNoSpace => "磁盘没有空间了。",
        L10nKey::IoReadOnly => "那个位置是只读的。",
        L10nKey::IoBusy => "有别的程序正占着它。",
        L10nKey::IoTimedOut => "对方没有在规定时间内响应。",
        L10nKey::TreeWindowOpenedEmpty => {
            "这个窗口的 server 没有交出标签页，所以窗口是空的。什么都没丢，它一响应就会回来。如果一直不回来，在命令面板里执行「重启 server」。"
        }
        L10nKey::CmdGroupTabsPanes => "标签页与窗格",
        L10nKey::CmdGroupWorkspaces => "工作区",
        L10nKey::CmdGroupView => "视图",
        L10nKey::CmdGroupGit => "Git",
        L10nKey::CmdGroupTerminal => "终端",
        L10nKey::CmdGroupSsh => "SSH",
        L10nKey::CmdGroupAgents => "Agents",
        L10nKey::CmdGroupApplication => "应用",
        L10nKey::CmdNewTab => "新标签页",
        L10nKey::CmdNewWorktreeTab => "新建 worktree 标签页…",
        L10nKey::CmdNewWorktreeTabSubtitle => "在全新分支上独立检出",
        L10nKey::CmdRenameTab => "重命名标签页…",
        L10nKey::CmdSplitRight => "向右分屏",
        L10nKey::CmdSplitDown => "向下分屏",
        L10nKey::CmdZoomPane => "缩放窗格",
        L10nKey::CmdNextPane => "下一窗格",
        L10nKey::CmdPreviousPane => "上一窗格",
        L10nKey::CmdFocusPaneLeft => "聚焦左侧窗格",
        L10nKey::CmdFocusPaneRight => "聚焦右侧窗格",
        L10nKey::CmdFocusPaneUp => "聚焦上方窗格",
        L10nKey::CmdFocusPaneDown => "聚焦下方窗格",
        L10nKey::CmdResizePaneLeft => "向左调整窗格",
        L10nKey::CmdResizePaneRight => "向右调整窗格",
        L10nKey::CmdResizePaneUp => "向上调整窗格",
        L10nKey::CmdResizePaneDown => "向下调整窗格",
        L10nKey::CmdSwapPaneNext => "与下一窗格交换",
        L10nKey::CmdSwapPanePrevious => "与上一窗格交换",
        L10nKey::CmdNextTab => "下一标签页",
        L10nKey::CmdPreviousTab => "上一标签页",
        L10nKey::CmdCopyWorkingDirectory => "复制工作目录",
        L10nKey::CmdCopySessionId => "复制会话 ID",
        L10nKey::CmdCopySessionIdSubtitle => "编码 agent 自身的会话 ID",
        L10nKey::CmdForkSession => "Fork 会话",
        L10nKey::CmdForkSessionSubtitle => "将此 agent 会话 fork 到新标签页",
        L10nKey::CmdMarkTabAsUnread => "将标签页标记为未读",
        L10nKey::CmdClosePaneTab => "关闭窗格/标签页",
        L10nKey::CmdCloseOtherTabs => "关闭其他标签页",
        L10nKey::CmdCloseTabsToTheRight => "关闭右侧标签页",
        L10nKey::CmdReopenClosedTab => "重新打开已关闭标签页",
        L10nKey::CmdNewWorkspace => "新建工作区",
        L10nKey::CmdSwitchWorkspace => "切换工作区…",
        L10nKey::CmdRenameWorkspace => "重命名工作区…",
        L10nKey::CmdStopWorkspace => "停止工作区…",
        L10nKey::CmdStopWorkspaceSubtitle => "结束其 shell，保留布局",
        L10nKey::CmdDeleteWorkspace => "删除工作区…",
        L10nKey::CmdDeleteWorkspaceSubtitle => "结束其 shell，清除布局",
        L10nKey::CmdShowLeftSidebar => "显示左侧边栏",
        L10nKey::CmdHideLeftSidebar => "隐藏左侧边栏",
        L10nKey::CmdHideRightPanel => "隐藏右侧面板",
        L10nKey::CmdShowRightPanel => "显示右侧面板",
        L10nKey::CmdShowCodePanel => "显示代码面板",
        L10nKey::CmdTabBarMoveToTop => "标签栏：移到顶部",
        L10nKey::CmdTabBarMoveToLeftSidebar => "标签栏：移到左侧边栏",
        L10nKey::CmdRightPanelInfo => "右侧面板：信息",
        L10nKey::CmdRightPanelChanges => "右侧面板：变更",
        L10nKey::CmdRightPanelFiles => "右侧面板：文件",
        L10nKey::CmdChangeTheme => "更改主题…",
        L10nKey::CmdResetFontSize => "重置字号",
        L10nKey::CmdEnterFullScreen => "进入全屏",
        L10nKey::CmdClearScrollback => "清除回滚内容",
        L10nKey::CmdToggleDiffViewMode => "切换统一 / 并排差异视图",
        L10nKey::CmdGitCommit => "Git：提交",
        L10nKey::CmdGitStageAll => "Git：暂存全部更改",
        L10nKey::CmdGitUnstageAll => "Git：取消暂存全部更改",
        L10nKey::CmdGitDiscardAll => "Git：放弃全部更改",
        L10nKey::CmdGitDiscardAllSubtitle => "丢弃工作区里所有未提交的更改。",
        L10nKey::CmdGitCheckoutTo => "Git：切换到…",
        L10nKey::CmdGitCreateBranch => "Git：新建分支…",
        L10nKey::CmdGitSync => "Git：同步",
        L10nKey::CmdGitSyncSubtitle => "先拉取，再推送。",
        L10nKey::CmdGitPush => "Git：推送",
        L10nKey::CmdGitPull => "Git：拉取",
        L10nKey::CmdGitFetch => "Git：获取",
        L10nKey::CmdGitToggleGraph => "Git：显示 / 隐藏提交历史",
        L10nKey::CmdClearScrollback => "清除 scrollback",
        L10nKey::CmdFindInTerminal => "在终端中查找…",
        L10nKey::CmdFindNext => "查找下一个",
        L10nKey::CmdFindPrevious => "查找上一个",
        L10nKey::CmdCopy => "复制",
        L10nKey::CmdCut => "剪切",
        L10nKey::CmdPaste => "粘贴",
        L10nKey::CmdSelectAll => "全选",
        L10nKey::CmdSshAddConnection => "SSH：添加连接…",
        L10nKey::CmdSshManageProfiles => "SSH：管理主机配置…",
        L10nKey::CmdSshReconnect => "SSH：重新连接",
        L10nKey::CmdSshRemoteFiles => "SSH：远程文件",
        L10nKey::CmdSshPortForwarding => "SSH：端口转发",
        L10nKey::CmdSshConnectWithInput => "SSH：连接 {input}",
        L10nKey::CmdAgentSendSelection => "Agent：发送选区",
        L10nKey::CmdAgentSendSelectionSubtitle => "选区 → 运行中的编码 agent",
        L10nKey::CmdAgentSendGitDiffForReview => "Agent：发送 git diff 以供审查",
        L10nKey::CmdAgentSendGitDiffSubtitle => "git diff → 运行中的编码 agent",
        L10nKey::CmdSettings => "设置…",
        L10nKey::CmdKeyboardShortcuts => "键盘快捷键",
        L10nKey::CmdAboutTty7 => "关于 tty7",
        L10nKey::CmdCheckForUpdates => "检查更新…",
        L10nKey::CmdDocumentation => "文档",
        L10nKey::CmdJoinDiscord => "加入 Discord",
        L10nKey::CmdReportIssue => "报告问题…",
        L10nKey::CmdRestartServer => "重启 server…",
        L10nKey::CmdRestartServerSubtitle => "结束所有运行中的 shell；保留布局",
        L10nKey::CmdQuitTty7 => "退出 tty7",
        L10nKey::CmdQuitTty7Subtitle => "shell 保持运行",
        L10nKey::CmdQuickConnect => "连接到“{target}”",
        L10nKey::CmdQuickConnectSaveProfile => "将“{target}”保存为主机配置…",
        L10nKey::CmdRecent => "最近使用",
        L10nKey::AppRestartServerTitle => "重启 server？",
        L10nKey::AppRestartServerFailed => "无法重启后台 server：{error}",
        L10nKey::AppRestartServerMismatchDetail => {
            "server 是 v{build}，协议 {protocol}；此应用使用 {ours}。两者无法对话，标签页取不出来。\n\n退出：什么都不变，server 和 shell 继续运行。\n重启：标签页带全新 shell 回来，现在跑着的东西会被杀掉。"
        }
        L10nKey::AppRestartServerDialectDetail => {
            "server 是 v{build}：control 方言 v{dialect}，而此应用使用 v{ours}。它交不出标签页，所以每个窗口都开成空的。\n\n退出：什么都不变，server 和 shell 继续运行。\n重启：标签页带全新 shell 回来，现在跑着的东西会被杀掉。"
        }
        L10nKey::AppRestartServerDialectNewerDetail => {
            "server 是 v{build}：control 方言 v{dialect}，而此应用使用 v{ours}。它交不出标签页，所以每个窗口都开成空的。\n\n退出并装上更新的构建：真正的解法，shell 全都还在。\n重启：标签页带全新 shell 回来，现在跑着的东西会被杀掉。"
        }
        L10nKey::AppRestartServerOldDetail => {
            "server 早于版本握手，此应用无从得知它说的是什么。\n\n退出：什么都不变，server 和 shell 继续运行。\n重启：标签页带全新 shell 回来，现在跑着的东西会被杀掉。"
        }
        L10nKey::AppRestart => "重启",
        L10nKey::AppRestartServerNoServer => {
            "{label} 上没有 tty7 自己的 server 可重启——它是本机通过 --stdio 运行的程序。请改为停止其工作区。"
        }
        L10nKey::AppRestartServerBody => {
            "这会停止本机上所有正在运行的 shell——其中仍在运行的任何内容都会被终止。你的标签页和布局会被保留，并以全新的 shell 重新打开。"
        }
        L10nKey::AppWorktreeRemoveDetailDirty => {
            "位于 {path} 的已关闭标签页的 worktree 有未提交的变更。"
        }
        L10nKey::AppWorktreeRemoveDetailClean => "位于 {path} 的已关闭标签页的 worktree 是干净的。",
        L10nKey::AppWorktreeRemoveTitle => "删除 worktree“{branch}”？",
        L10nKey::AppWorktreeDiscardAndRemove => "放弃变更并删除",
        L10nKey::AppWorktreeRemove => "删除 worktree",
        L10nKey::AppWorktreeKeep => "保留",
        L10nKey::AppReopenTabFailed => "无法重新打开标签页：没有启动终端",
        L10nKey::AppOpenTerminalFailed => "无法打开终端：{error}",
        L10nKey::AppTabsNotRestored => "上次的 {count} 个标签页没能重新打开",
        L10nKey::AppSshConnectionFailed => "SSH 连接失败：{error}",
        L10nKey::AppSshReconnectFailed => "SSH 重新连接失败：{error}",
        L10nKey::AppSplitPaneFailed => "无法拆分窗格：{error}",
        L10nKey::PaneDragHandleTooltip => "拖动可把这个窗格挪到别处",
        L10nKey::AppWorktreeRemoved => "已删除 worktree“{branch}”",
        L10nKey::AppWorktreeRemoveFailed => "删除 worktree 失败：{error}",
        L10nKey::AppForkStillConnecting => "无法 fork：窗格仍在连接中",
        L10nKey::AppPaneNoCodingAgent => "此窗格未运行编码 agent",
        L10nKey::AppForkNoCommand => "tty7 没有用于 {name} 的 fork 命令",
        L10nKey::AppForkLocalOnly => "{name} 会话只能从本地窗格 fork",
        L10nKey::AppForkNoSessionId => {
            "tty7 尚未在此窗格中看到 {name} 的会话 ID——请在设置 → Agents 中安装其 hook"
        }
        L10nKey::AppForkSessionIdNotToken => "{name} 的会话 ID 不是普通令牌",
        L10nKey::AppForkMidTurn => "{name} 正在处理中——fork 不会包含进行中的这一轮",
        L10nKey::AppTabNoWorkingDirectory => "此标签页还没有工作目录",
        L10nKey::AppNothingSelected => "未选择任何内容——请先选择一些终端输出。",
        L10nKey::AppPaneNoKnownDirectory => "此窗格没有已知的目录。",
        L10nKey::AppNoUncommittedChanges => "{cwd} 中没有未提交的更改（或不是 git 仓库）。",
        L10nKey::AppCmdSshProfileTitle => "SSH：{title}",
        L10nKey::AppCmdSwitchToTab => "切换到标签页：{label}",
        L10nKey::AppPlaceholderDescription => "描述",
        L10nKey::AppPlaceholderSshQuickConnect => "user@host  或  user@host:port",
        L10nKey::AppPlaceholderLoginShell => "登录 shell",
        L10nKey::AppPlaceholderNone => "无",
        L10nKey::AppPlaceholderOpenInDefaultApp => "在默认应用中打开",
        L10nKey::AppThemeColorBackground => "背景",
        L10nKey::AppThemeColorForeground => "前景",
        L10nKey::AppThemeColorAccent => "强调色",
        L10nKey::AppThemeColorCursor => "光标",
        L10nKey::AppThemeColorSelection => "选区",
        L10nKey::AppThemeAnsiBlack => "黑色",
        L10nKey::AppThemeAnsiRed => "红色",
        L10nKey::AppThemeAnsiGreen => "绿色",
        L10nKey::AppThemeAnsiYellow => "黄色",
        L10nKey::AppThemeAnsiBlue => "蓝色",
        L10nKey::AppThemeAnsiMagenta => "洋红色",
        L10nKey::AppThemeAnsiCyan => "青色",
        L10nKey::AppThemeAnsiWhite => "白色",
        L10nKey::AppThemeAnsiBrightBlack => "亮黑色",
        L10nKey::AppThemeAnsiBrightRed => "亮红色",
        L10nKey::AppThemeAnsiBrightGreen => "亮绿色",
        L10nKey::AppThemeAnsiBrightYellow => "亮黄色",
        L10nKey::AppThemeAnsiBrightBlue => "亮蓝色",
        L10nKey::AppThemeAnsiBrightMagenta => "亮洋红色",
        L10nKey::AppThemeAnsiBrightCyan => "亮青色",
        L10nKey::AppThemeAnsiBrightWhite => "亮白色",
        L10nKey::AppAgentHooksThisComputer => "本机",
        L10nKey::AppAgentHooksRemoteMachine => "远程机器",
        L10nKey::AppAgentHooksNoHomeDir => {
            "tty7 无法确定这台计算机的主目录，因此没有可安装的位置。"
        }
        L10nKey::AppAgentHooksOffline => {
            "未连接到这台机器，因此无法读取或写入其 agent 配置。请在其上打开一个工作区后再回来。"
        }
        L10nKey::AppAgentHooksHomeDirUnresolved => "无法解析主目录",
        L10nKey::AppAgentHooksInstalled => "已安装",
        L10nKey::AppAgentHooksInstalledEnableCodexThere => {
            "已安装 —— 需要在那台机器上运行一次 `codex features enable hooks`"
        }
        L10nKey::AppAgentHooksInstalledCodexEnableFailed => {
            "已安装，但无法运行 `codex features enable hooks`（{error}）—— 请手动运行一次"
        }
        L10nKey::AppAgentHooksRemoved => "已移除",
        L10nKey::AppAgentHooksNothingInstalled => "没有安装任何内容，无需移除",
        L10nKey::AppAgentHooksNoTty7Hooks => "没有找到 tty7 钩子，无需移除",
        L10nKey::AppAgentHooksInstallFailed => "无法安装钩子：{error}",
        L10nKey::AppAgentHooksRemoveFailed => "无法移除钩子：{error}",
        L10nKey::AppKeybindingDisplacedNote => {
            "{action} 占用了原属于 {previous} 的快捷键，{previous} 现在没有快捷键了。"
        }
        L10nKey::AppLocalServerName => "本地 server",
        L10nKey::AppSshParseUnbalancedQuotes => "SSH 命令中的引号不匹配",
        L10nKey::AppSshParseNoRemoteCommands => "此处不支持远程命令",
        L10nKey::AppSshParseFlagNeedsValue => "-{flag} 需要一个值",
        L10nKey::AppSshParseInvalidPort => "无效端口“{value}”",
        L10nKey::AppSshParseUnsupportedOption => "不支持的选项“{option}”",
        L10nKey::AppSshParseEnterHost => "输入要连接的主机",
        L10nKey::AppSshParseBadHost => "无法解析主机“{host}”",
        L10nKey::AppMenuMinimize => "最小化",
        L10nKey::AppMenuZoom => "缩放",
        L10nKey::SwitcherStatusRestarting => "正在重启…",
        L10nKey::SwitcherStatusInstalling => "正在安装…",
        L10nKey::SwitcherStatusConnecting => "正在连接…",
        L10nKey::SwitcherStatusConnectFailed => "连接失败",
        L10nKey::SwitcherStatusNotConnected => "未连接",
        L10nKey::SettingsFontDefault => "默认（匹配主字体）",
        L10nKey::ForwardDescriptionPlaceholder => "用途说明",
        L10nKey::SettingsShellDefaultLoginShell => "你的登录 shell",
        L10nKey::SettingsShellDetected => "tty7 检测到的 shell",
        L10nKey::SftpErrorUnexpectedReply => "意外回复：{reply}",
        L10nKey::SftpErrorUnsafeRemoteName => "拒绝不安全的远程名称 {name}",
        L10nKey::SftpErrorNoFreeLocalName => {
            "下载目录里 {name} 已无可用名称——请先移走或删除旧的副本"
        }
        L10nKey::SftpReplaceTitle => "覆盖已有的文件？",
        L10nKey::SftpReplaceBody => "{names} 在这个文件夹里已经存在，上传会覆盖它们。",
        L10nKey::Replace => "覆盖",
        L10nKey::SftpErrorInvalidOctalMode => "无效的八进制模式",
        L10nKey::SettingsDaemonStaleDescInPlace => {
            "tty7 是原地更新的，所以应用是新的，但你的面板仍由上一个版本在服务。\
             server 可以在不停止的情况下把自己换成新版本：你的 shell 和里面正在跑的东西会直接延续下来。\
             用 tty7 内置 SSH 客户端的面板除外——那些连接会断开，需要重新打开。"
        }
        L10nKey::AppRestartServerBodyInPlace => {
            "后台 server 会在不停止的情况下把自己换成当前这个版本。\
             你的 shell 会继续运行——面板里的命令、agent、`ssh` 会话都不会被打断——窗口稍后会重新连上它们。\
             用 tty7 内置 SSH 客户端的面板除外：那些连接会断开，需要重新打开。"
        }
        L10nKey::PaneRestoredScreenBanner => {
            "已恢复的画面 —— 下面是新的 shell，上面的内容都已不在运行"
        }
        L10nKey::SettingsPerPaneHistory => "每个面板用自己的 shell 历史",
        L10nKey::SettingsPerPaneHistoryDescription => {
            "上方向键翻的是这个面板里跑过的命令，而不是所有面板混在一起的结果。\
             新面板会从你已有的历史开始，而不是一片空白；面板关闭时，它新增的部分会写回原来的历史文件，不会丢。\
             只对 tty7 能接管的 bash 和 zsh 面板生效；用你自己参数启动的 shell 不受影响。"
        }
        L10nKey::PanelMoreChangedFiles => "…还有 {count} 个变更文件——运行 git diff 查看。",
        L10nKey::PanelMoreChangedFiles => "…还有 {count} 个变更文件——运行 `git diff` 查看。",
        L10nKey::ScmFilesChanged => "{count} 个文件改动",
        L10nKey::ScmStagedFileCount => "已暂存 {count} 个文件",
        L10nKey::AppMenuAbout => "关于 tty7",
        L10nKey::AppMenuCheckForUpdates => "检查更新…",
        L10nKey::AppMenuSettings => "设置…",
        L10nKey::AppMenuServices => "服务",
        L10nKey::AppMenuHideApp => "隐藏 tty7",
        L10nKey::AppMenuHideOthers => "隐藏其他",
        L10nKey::AppMenuShowAll => "显示全部",
        L10nKey::AppMenuQuit => "退出 tty7",
        L10nKey::AppMenuFile => "文件",
        L10nKey::AppMenuEdit => "编辑",
        L10nKey::AppMenuView => "视图",
        L10nKey::AppMenuWindow => "窗口",
        L10nKey::AppMenuHelp => "帮助",
        L10nKey::AppMenuNewTab => "新标签页",
        L10nKey::AppMenuNewWorkspace => "新工作区",
        L10nKey::AppMenuNewWorktreeTab => "新 worktree 标签页…",
        L10nKey::AppMenuSplitRight => "向右分屏",
        L10nKey::AppMenuSplitLeft => "向左分屏",
        L10nKey::AppMenuSplitDown => "向下分屏",
        L10nKey::AppMenuSplitUp => "向上分屏",
        L10nKey::AppMenuRenameTab => "重命名标签页…",
        L10nKey::AppMenuCopyWorkingDirectory => "复制工作目录",
        L10nKey::AppMenuCopySessionId => "复制会话 ID",
        L10nKey::AppMenuForkSession => "Fork 会话",
        L10nKey::AppMenuClosePaneTab => "关闭窗格 / 标签页",
        L10nKey::AppMenuCloseOtherTabs => "关闭其他标签页",
        L10nKey::AppMenuCloseTabsRight => "关闭右侧标签页",
        L10nKey::AppMenuReopenClosedTab => "重新打开已关闭的标签页",
        L10nKey::AppMenuRenameWorkspace => "重命名工作区…",
        L10nKey::AppMenuStopWorkspace => "停止工作区…",
        L10nKey::AppMenuDeleteWorkspace => "删除工作区…",
        L10nKey::AppMenuUndo => "撤销",
        L10nKey::AppMenuRedo => "重做",
        L10nKey::AppMenuCut => "剪切",
        L10nKey::AppMenuCopy => "复制",
        L10nKey::AppMenuPaste => "粘贴",
        L10nKey::AppMenuSelectAll => "全选",
        L10nKey::AppMenuFind => "查找…",
        L10nKey::AppMenuFindNext => "查找下一个",
        L10nKey::AppMenuFindPrevious => "查找上一个",
        L10nKey::AppMenuCommandPalette => "命令面板…",
        L10nKey::AppMenuIncreaseFontSize => "增大字号",
        L10nKey::AppMenuDecreaseFontSize => "减小字号",
        L10nKey::AppMenuResetFontSize => "重置字号",
        L10nKey::AppMenuLeftSidebar => "左侧边栏",
        L10nKey::AppMenuRightPanel => "右侧面板",
        L10nKey::AppMenuCodePanel => "代码面板",
        L10nKey::AppMenuTabBarPosition => "标签栏位置",
        L10nKey::AppMenuFocusNextPane => "聚焦下一个窗格",
        L10nKey::AppMenuFocusPreviousPane => "聚焦上一个窗格",
        L10nKey::AppMenuZoomPane => "缩放窗格",
        L10nKey::AppMenuClearScrollback => "清除回滚内容",
        L10nKey::AppMenuDocumentation => "tty7 文档",
        L10nKey::AppMenuKeyboardShortcuts => "键盘快捷键",
        L10nKey::AppMenuJoinDiscord => "加入 Discord",
        L10nKey::AppMenuReportIssue => "报告问题…",
        L10nKey::AppMenuRestartServer => "重启 server…",
        L10nKey::WindowUntitled => "未命名",
        L10nKey::TrayShowTty7 => "显示 tty7",
        L10nKey::TrayNotifications => "通知",
        L10nKey::TrayAgentNeedsInput => "需要输入",
        L10nKey::AgentStatusWorking => "运行中",
        L10nKey::AgentStatusWaiting => "需要输入",
        L10nKey::AgentStatusDone => "已完成",
        L10nKey::NotifyCommandFinished => "命令运行完成，用时 {secs} 秒",
        L10nKey::NotifyCommandFinishedWithCommand => "{command} 已完成，用时 {secs} 秒",
        L10nKey::NotifyAgentFinished => "已完成，用时 {secs} 秒",
        L10nKey::NotifyAgentWaiting => "等待你的输入",
        L10nKey::NotifyTurnFinished => "本轮已完成",
        L10nKey::TabTooltipMore => "更多",
        L10nKey::TabTooltipShowSidebar => "显示侧栏",
        L10nKey::TabTooltipHideSidebar => "隐藏侧栏",
        L10nKey::TabTooltipHideDetailPanel => "隐藏详情面板",
        L10nKey::TabTooltipShowDetailPanel => "显示详情面板",
        L10nKey::TabUnnamedShell => "终端 {n}",
        L10nKey::ShellDefault => "默认",
        L10nKey::SidebarScratchGroup => "草稿",
        L10nKey::TabContextCloseTab => "关闭标签页",
        L10nKey::TabContextCloseTabsBelow => "关闭下方标签页",
        L10nKey::AppAgentHooksOpFailed => "失败：{error}",
        L10nKey::AppMenuEnterFullscreen => "进入全屏",
        L10nKey::HomeTimeOverWeekAgo => "一周多前",
        L10nKey::Search => "搜索",
        L10nKey::SettingsDaemonStaleRestart => "重启 server",
        L10nKey::SettingsNoneLower => "无",
        L10nKey::SettingsSearchCommandLineToolTitle => "命令行工具",
        L10nKey::TabContextMarkUnread => "标记为未读",
    })
}

pub fn translate_variant_zh(key: L10nKey, branch: &'static str) -> Option<&'static str> {
    let res = match (key, branch) {
        (L10nKey::SettingsAliasesLinked, "zero") => "还没有关联别名。",
        (L10nKey::SettingsAliasesLinked, "one") => "已关联 1 个别名。",
        (L10nKey::SettingsAliasesLinked, "other") => "已关联 {count} 个别名。",
        (L10nKey::SettingsRulesOpenedWithConnection, "zero") => "0 条规则，随连接打开",
        (L10nKey::SettingsRulesOpenedWithConnection, "one") => "1 条规则，随连接打开",
        (L10nKey::SettingsRulesOpenedWithConnection, "other") => "{count} 条规则，随连接打开",
        (L10nKey::SettingsOfflineMachines, "zero") => {
            "还有 0 台已保存的机器未连接——在其中一台上打开工作区，即可在那台机器上安装 hook。"
        }
        (L10nKey::SettingsOfflineMachines, "one") => {
            "还有 1 台已保存的机器未连接——在那台机器上打开工作区，即可在那里安装 hook。"
        }
        (L10nKey::SettingsOfflineMachines, "other") => {
            "还有 {count} 台已保存的机器未连接——在其中一台上打开工作区，即可在那台机器上安装 hook。"
        }
        (L10nKey::SftpReplaceBody, "one") => "{names} 在这个文件夹里已经存在，上传会覆盖它。",
        (L10nKey::SftpReplaceBody, "other") => "{names} 在这个文件夹里已经存在，上传会覆盖它们。",
        (L10nKey::AppTabsNotRestored, "one") => "上次的 1 个标签页没能重新打开",
        (L10nKey::AppTabsNotRestored, "other") => "上次的 {count} 个标签页没能重新打开",
        (L10nKey::PanelMoreChangedFiles, "zero") => "…还有 0 个变更文件——运行 git diff 查看。",
        (L10nKey::PanelMoreChangedFiles, "one") => "…还有 1 个变更文件——运行 git diff 查看。",
        (L10nKey::ScmFilesChanged, "zero") => "没有文件改动",
        (L10nKey::ScmFilesChanged, "one") => "1 个文件改动",
        (L10nKey::ScmFilesChanged, "other") => "{count} 个文件改动",
        (L10nKey::ScmStagedFileCount, "zero") => "没有暂存的更改",
        (L10nKey::ScmStagedFileCount, "one") => "已暂存 1 个文件",
        (L10nKey::ScmStagedFileCount, "other") => "已暂存 {count} 个文件",
        (L10nKey::PanelMoreChangedFiles, "zero") => "…还有 0 个变更文件——运行 `git diff` 查看。",
        (L10nKey::PanelMoreChangedFiles, "one") => "…还有 1 个变更文件——运行 `git diff` 查看。",
        (L10nKey::PanelMoreChangedFiles, "other") => {
            "…还有 {count} 个变更文件——运行 git diff 查看。"
        }
        (L10nKey::DiffChangedFiles, "zero") => "0 个变更文件",
        (L10nKey::DiffChangedFiles, "one") => "1 个变更文件",
        (L10nKey::DiffChangedFiles, "other") => "{count} 个变更文件",
        (L10nKey::DiffUntrackedCount, "zero") => " · 0 个未跟踪文件",
        (L10nKey::DiffUntrackedCount, "one") => " · 1 个未跟踪文件",
        (L10nKey::DiffUntrackedCount, "other") => " · {count} 个未跟踪文件",
        (L10nKey::DiffMoreFiles, "zero") => "…还有 0 个变更文件——在终端中运行 git diff 查看。",
        (L10nKey::DiffMoreFiles, "one") => "…还有 1 个变更文件——在终端中运行 git diff 查看。",
        (L10nKey::DiffMoreFiles, "other") => {
            "…还有 {count} 个变更文件——在终端中运行 git diff 查看。"
        }
        (L10nKey::DiffUntrackedHeader, "zero") => "未跟踪文件 (0)",
        (L10nKey::DiffUntrackedHeader, "one") => "未跟踪文件 (1)",
        (L10nKey::DiffUntrackedHeader, "other") => "未跟踪文件 ({count})",
        (L10nKey::DiffMoreUntracked, "zero") => "…还有 0 个——在终端中运行 git status 查看。",
        (L10nKey::DiffMoreUntracked, "one") => "…还有 1 个——在终端中运行 git status 查看。",
        (L10nKey::DiffMoreUntracked, "other") => "…还有 {count} 个——在终端中运行 git status 查看。",
        (L10nKey::DiffUntrackedSummary, "zero") => "0 个未跟踪",
        (L10nKey::DiffUntrackedSummary, "one") => "1 个未跟踪",
        (L10nKey::DiffUntrackedSummary, "other") => "{count} 个未跟踪",
        (L10nKey::HomeTimeMinutesAgo, "one") => "1 分钟前",
        (L10nKey::HomeTimeMinutesAgo, "other") => "{count} 分钟前",
        (L10nKey::HomeTimeHoursAgo, "one") => "1 小时前",
        (L10nKey::HomeTimeHoursAgo, "other") => "{count} 小时前",
        (L10nKey::HomeTimeDaysAgo, "one") => "1 天前",
        (L10nKey::HomeTimeDaysAgo, "other") => "{count} 天前",
        (L10nKey::HomeTimeWeeksAgo, "one") => "1 周前",
        (L10nKey::HomeTimeWeeksAgo, "other") => "{count} 周前",
        (L10nKey::HomeTimeMonthsAgo, "one") => "1 个月前",
        (L10nKey::HomeTimeMonthsAgo, "other") => "{count} 个月前",
        (L10nKey::WindowStopShells, "zero") => "其布局和工作目录将被清除。",
        (L10nKey::WindowStopShells, "one") => "1 个正在运行的 shell 将会被终止。",
        (L10nKey::WindowStopShells, "other") => "{count} 个正在运行的 shell 将会被终止。",
        (L10nKey::WindowDeleteShells, "zero") => "其布局和工作目录将被清除。",
        (L10nKey::WindowDeleteShells, "one") => {
            "1 个正在运行的 shell 将会被终止，其布局也将被清除。"
        }
        (L10nKey::WindowDeleteShells, "other") => {
            "{count} 个正在运行的 shell 将会被终止，布局也将被清除。"
        }
        _ => return None,
    };
    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_covers_every_key() {
        assert_eq!(translate_zh(L10nKey::SearchTabs), Some("搜索标签页…"));
    }
}
