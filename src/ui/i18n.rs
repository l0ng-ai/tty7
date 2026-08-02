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
