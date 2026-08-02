use std::sync::atomic::{AtomicU8, Ordering};

const EN: u8 = 0;
const ZH_HANS: u8 = 1;

static CURRENT: AtomicU8 = AtomicU8::new(EN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L10nKey {
    SearchTabs,
    SearchFiles,
    SearchThemes,
    SearchSettings,
    FilterHosts,
    SearchCommandsOrHost,
    SearchTheme,
    Search,
    SearchWorkspacesAndMachines,
    SearchFonts,
    NewFolderName,
    NewFileName,
    HomeNewTab,
    HomeReopenClosedTab,
    HomeSwitchWorkspace,
    HomeCommandPalette,
    HomeSplitRight,
    HomeSplitDown,
    HomeSettings,
    TrayQuitStopServer,
    Reconnect,
    None,
    TryAgain,
    Refreshing,
    Binary,
    Delete,
    NoMatchingCommands,
    ConnectSshHint,
    EditHint,
    OpenFileFromTree,
    FileChangedOnDisk,
    Reload,
    KeepMine,
    Dismiss,
    StoredPasswordRejected,
    Trust,
    Abort,
    HostKeyOverrideMessage,
    Override,
    RememberKeychain,
    CloseWindowTitle,
    CloseWindowBody,
    Cancel,
    Close,
    QuitStopServerTitle,
    QuitStopServerBody,
    QuitAndStop,
    CloseSshConnectionTitle,
    CloseSshConnectionBody,
    Keep,
}

pub fn set_locale(gui_language: &str) {
    let locale = if gui_language == "auto" {
        detect_system_language()
    } else if matches!(
        gui_language,
        "zh-CN" | "zh-Hans" | "zh_CN" | "zh_Hans" | "zh"
    ) {
        ZH_HANS
    } else {
        EN
    };
    CURRENT.store(locale, Ordering::Relaxed);
}

pub fn t(key: L10nKey) -> &'static str {
    translate(current_locale(), key)
}

pub fn is_zh_hans() -> bool {
    current_locale() == Locale::ZhHans
}

fn current_locale() -> Locale {
    if CURRENT.load(Ordering::Relaxed) == ZH_HANS {
        Locale::ZhHans
    } else {
        Locale::En
    }
}

fn detect_system_language() -> u8 {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.to_ascii_lowercase();
            if value.starts_with("zh_") || value.starts_with("zh-") {
                return ZH_HANS;
            }
        }
    }
    EN
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Locale {
    En,
    ZhHans,
}

fn translate(locale: Locale, key: L10nKey) -> &'static str {
    let (en, zh) = match key {
        L10nKey::SearchTabs => ("Search tabs…", "搜索标签页…"),
        L10nKey::SearchFiles => ("Search files…", "搜索文件…"),
        L10nKey::SearchThemes => ("Search themes…", "搜索主题…"),
        L10nKey::SearchSettings => ("Search settings…", "搜索设置…"),
        L10nKey::FilterHosts => ("Filter hosts…", "筛选主机…"),
        L10nKey::SearchCommandsOrHost => (
            "Search or type user@host to connect…",
            "搜索或输入 user@host 连接…",
        ),
        L10nKey::SearchTheme => ("Search…", "搜索…"),
        L10nKey::Search => ("Search", "搜索"),
        L10nKey::SearchWorkspacesAndMachines => {
            ("Search workspaces and machines", "搜索工作区与机器")
        }
        L10nKey::SearchFonts => ("Search fonts…", "搜索字体…"),
        L10nKey::NewFolderName => ("New folder name", "新文件夹名称"),
        L10nKey::NewFileName => ("New file name", "新文件名称"),
        L10nKey::HomeNewTab => ("New Tab", "新标签页"),
        L10nKey::HomeReopenClosedTab => ("Reopen Closed Tab", "重新打开已关闭的标签页"),
        L10nKey::HomeSwitchWorkspace => ("Switch Workspace", "切换工作区"),
        L10nKey::HomeCommandPalette => ("Command Palette", "命令面板"),
        L10nKey::HomeSplitRight => ("Split Right", "向右分屏"),
        L10nKey::HomeSplitDown => ("Split Down", "向下分屏"),
        L10nKey::HomeSettings => ("Settings…", "设置…"),
        L10nKey::TrayQuitStopServer => ("Quit and Stop Server…", "退出并停止服务器…"),
        L10nKey::Reconnect => ("Reconnect", "重新连接"),
        L10nKey::None => ("None.", "无。"),
        L10nKey::TryAgain => ("Try Again", "重试"),
        L10nKey::Refreshing => ("refreshing…", "正在刷新…"),
        L10nKey::Binary => ("binary", "二进制文件"),
        L10nKey::Delete => ("Delete", "删除"),
        L10nKey::NoMatchingCommands => ("No matching commands", "没有匹配的命令"),
        L10nKey::ConnectSshHint => (
            "Type user@host to connect over SSH instead.",
            "输入 user@host 改为通过 SSH 连接。",
        ),
        L10nKey::EditHint => ("→ edit", "→ 编辑"),
        L10nKey::OpenFileFromTree => ("Open a file from the file tree", "从文件树打开文件"),
        L10nKey::FileChangedOnDisk => ("File changed on disk", "文件在磁盘上已被修改"),
        L10nKey::Reload => ("Reload", "重新加载"),
        L10nKey::KeepMine => ("Keep mine", "保留我的版本"),
        L10nKey::Dismiss => ("Dismiss", "关闭"),
        L10nKey::StoredPasswordRejected => (
            "The stored password was rejected. Enter a new one.",
            "已存储的密码被拒绝，请输入新密码。",
        ),
        L10nKey::Trust => ("Trust", "信任"),
        L10nKey::Abort => ("Abort", "中止"),
        L10nKey::HostKeyOverrideMessage => (
            "Type \"yes\" to override and trust the new key, or Esc to abort.",
            "输入 yes 覆盖并信任新密钥，或按 Esc 中止。",
        ),
        L10nKey::Override => ("Override", "覆盖"),
        L10nKey::RememberKeychain => ("Remember (keychain)", "记住（钥匙串）"),
        L10nKey::CloseWindowTitle => ("Close Window?", "是否关闭窗口？"),
        L10nKey::CloseWindowBody => (
            "Your sessions keep running in the background. This workspace will be \
             waiting on the home page, and in the workspace menu in the title bar, the \
             next time you open tty7.",
            "你的会话会继续在后台运行。此工作区将保留，下次启动时可在主页和标题栏工作区菜单中找到。",
        ),
        L10nKey::Cancel => ("Cancel", "取消"),
        L10nKey::Close => ("Close", "关闭"),
        L10nKey::QuitStopServerTitle => ("Quit and Stop Server?", "退出并停止服务器？"),
        L10nKey::QuitStopServerBody => (
            "This quits tty7 and stops the background server — anything still running \
             in your shells is terminated. Your tabs and layout are kept and reopen with \
             fresh shells next launch. (Plain Quit keeps shells running.)",
            "这会退出 tty7 并停止后台服务器，所有仍在运行的 shell 都会被终止。你的标签页和布局会被保留，下次启动时以全新的 shell 重新打开。（普通退出会保持 shell 运行。）",
        ),
        L10nKey::QuitAndStop => ("Quit and Stop", "退出并停止"),
        L10nKey::CloseSshConnectionTitle => ("Close this SSH connection?", "关闭这个 SSH 连接？"),
        L10nKey::CloseSshConnectionBody => (
            "The connection is live. Closing will end it.",
            "连接仍处于活动状态。关闭将结束它。",
        ),
        L10nKey::Keep => ("Keep", "保留"),
    };
    match locale {
        Locale::En => en,
        Locale::ZhHans => zh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_translations_cover_the_initial_keys() {
        for key in [
            L10nKey::SearchTabs,
            L10nKey::SearchFiles,
            L10nKey::SearchThemes,
            L10nKey::SearchSettings,
            L10nKey::FilterHosts,
            L10nKey::SearchCommandsOrHost,
            L10nKey::SearchTheme,
            L10nKey::Search,
            L10nKey::SearchWorkspacesAndMachines,
            L10nKey::SearchFonts,
            L10nKey::NewFolderName,
            L10nKey::NewFileName,
            L10nKey::HomeNewTab,
            L10nKey::HomeReopenClosedTab,
            L10nKey::HomeSwitchWorkspace,
            L10nKey::HomeCommandPalette,
            L10nKey::HomeSplitRight,
            L10nKey::HomeSplitDown,
            L10nKey::HomeSettings,
            L10nKey::TrayQuitStopServer,
            L10nKey::Reconnect,
            L10nKey::None,
            L10nKey::TryAgain,
            L10nKey::Refreshing,
            L10nKey::Binary,
            L10nKey::Delete,
            L10nKey::NoMatchingCommands,
            L10nKey::ConnectSshHint,
            L10nKey::EditHint,
            L10nKey::OpenFileFromTree,
            L10nKey::FileChangedOnDisk,
            L10nKey::Reload,
            L10nKey::KeepMine,
            L10nKey::Dismiss,
            L10nKey::StoredPasswordRejected,
            L10nKey::Trust,
            L10nKey::Abort,
            L10nKey::HostKeyOverrideMessage,
            L10nKey::Override,
            L10nKey::RememberKeychain,
            L10nKey::CloseWindowTitle,
            L10nKey::CloseWindowBody,
            L10nKey::Cancel,
            L10nKey::Close,
            L10nKey::QuitStopServerTitle,
            L10nKey::QuitStopServerBody,
            L10nKey::QuitAndStop,
            L10nKey::CloseSshConnectionTitle,
            L10nKey::CloseSshConnectionBody,
            L10nKey::Keep,
        ] {
            assert_ne!(translate(Locale::ZhHans, key), translate(Locale::En, key));
        }
    }

    #[test]
    fn explicit_languages_select_the_right_locale() {
        set_locale("zh-CN");
        assert_eq!(current_locale(), Locale::ZhHans);
        set_locale("en");
        assert_eq!(current_locale(), Locale::En);
        set_locale("ko");
        assert_eq!(current_locale(), Locale::En);
    }
}
