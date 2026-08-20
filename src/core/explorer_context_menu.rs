//! Optional Windows Explorer context-menu integration.
//!
//! The registry is user-visible system state, so tty7 never writes these verbs
//! on its own: the Windows installer offers a task checkbox and invokes
//! [`register`], and its uninstaller always invokes [`unregister`]. There is no
//! runtime setting — the same install-time-only treatment VS Code and Git for
//! Windows give their shell entries. Keeping the key layout here rather than in
//! the .iss keeps one description of it in the tree.
//!
//! Both verbs invoke the GUI-subsystem `tty7-app.exe` directly so Explorer never
//! allocates a transient console. The app first offers the path to an already
//! running GUI through `GuiOpen`, then continues normal startup when no GUI
//! receives it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::ui::i18n::{L10nKey, t};

const DIRECTORY_KEY: &str = r"Software\Classes\Directory\shell\tty7";
const BACKGROUND_KEY: &str = r"Software\Classes\Directory\Background\shell\tty7";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    Directory,
    Background,
}

impl Location {
    fn key(self) -> &'static str {
        match self {
            Self::Directory => DIRECTORY_KEY,
            Self::Background => BACKGROUND_KEY,
        }
    }

    /// The wording Explorer shows, in the language the app is set to.
    ///
    /// Explorer reads this string from the registry, not from tty7, so what is
    /// written here is what a user sees until something writes it again — an
    /// install-time snapshot of the locale. Two things keep that snapshot
    /// honest: `register` sets the locale from the config before building the
    /// entries, and [`refresh_labels`] restates them when the language changes
    /// in Settings.
    fn label(self) -> &'static str {
        t(match self {
            Self::Directory => L10nKey::ExplorerMenuOpenIn,
            Self::Background => L10nKey::ExplorerMenuOpenHere,
        })
    }

    fn placeholder(self) -> &'static str {
        match self {
            // `%1` is the selected directory passed to a normal static verb.
            Self::Directory => "%1",
            // Explorer expands `%V` to the folder whose background was clicked.
            Self::Background => "%V",
        }
    }
}

#[derive(Debug)]
struct Registration {
    location: Location,
    icon: OsString,
    command: OsString,
}

fn replace_entry_with(
    registration: &Registration,
    delete: impl FnOnce(&str) -> Result<()>,
    write: impl FnOnce(&Registration) -> Result<()>,
) -> Result<()> {
    // The verb root belongs exclusively to tty7. Replacing it at the operation
    // boundary prevents stale shell values or handler subkeys from surviving
    // an update and changing visibility or execution semantics.
    delete(registration.location.key())?;
    write(registration)
}

impl Registration {
    fn new(location: Location, app: &Path) -> Self {
        Self {
            location,
            icon: app.as_os_str().to_os_string(),
            command: quoted_open_command(app, location.placeholder()),
        }
    }
}

/// Register both Explorer verbs for the current user.
pub fn register() -> Result<()> {
    platform_register()
}

/// Remove only the two Explorer verb trees owned by tty7.
pub fn unregister() -> Result<()> {
    platform_unregister()
}

/// Restate the verb labels in the language the UI now runs in.
///
/// The registry holds whatever wording was current when the installer ran, so
/// without this a user who switches tty7 to Chinese keeps English entries in
/// Explorer for the life of the install — the one place in the product where
/// the language setting would not reach.
///
/// Only keys that already exist are rewritten. Offering the menu is the
/// installer's checkbox and declining it is the user's decision; changing a
/// language must never be what puts the verbs back.
///
/// Best-effort by design: a failure here costs a log line, never a language
/// change the user asked for.
pub fn refresh_labels() {
    if let Err(error) = platform_refresh_labels() {
        log::warn!("could not restate the Explorer context-menu labels: {error}");
    }
}

/// Build a command line without converting the executable path through UTF-8.
///
/// Quotes are unconditional: both the executable and the Explorer-substituted
/// folder can contain spaces. Windows file names cannot contain a quote, so the
/// resulting command is unambiguous without an additional escape layer.
fn quoted_open_command(executable: &Path, placeholder: &str) -> OsString {
    let mut command = OsString::from("\"");
    command.push(executable.as_os_str());
    command.push("\" --open-path \"");
    command.push(placeholder);
    command.push("\"");
    command
}

fn application_path() -> Result<PathBuf> {
    let app = std::env::current_exe().context("locating the running tty7 application")?;
    app.parent()
        .context("the running tty7 application has no parent directory")?;
    Ok(app)
}

fn registrations(app: &Path) -> [Registration; 2] {
    [
        Registration::new(Location::Directory, app),
        Registration::new(Location::Background, app),
    ]
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW,
        RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            // SAFETY: `RegistryKey` is created only from a successful Win32
            // open/create call and owns exactly one handle.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn io_error(action: &str, code: u32) -> anyhow::Error {
        anyhow::anyhow!(
            "{action}: {}",
            std::io::Error::from_raw_os_error(code as i32)
        )
    }

    fn create_key(path: &str) -> Result<RegistryKey> {
        let path = wide(OsStr::new(path));
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: all input pointers reference live locals; the optional class,
        // security and disposition pointers are null as permitted by the API.
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io_error("creating the tty7 Explorer registry key", code));
        }
        Ok(RegistryKey(key))
    }

    fn set_string(key: &RegistryKey, name: Option<&OsStr>, value: &OsStr) -> Result<()> {
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
        let value = wide(value);
        let byte_len = u32::try_from(value.len() * 2)
            .context("the tty7 Explorer registry value is too long")?;
        // SAFETY: the key is writable, and both optional name and value point
        // to live NUL-terminated UTF-16 buffers for the duration of the call.
        let code =
            unsafe { RegSetValueExW(key.0, name_ptr, 0, REG_SZ, value.as_ptr().cast(), byte_len) };
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io_error("writing a tty7 Explorer registry value", code))
        }
    }

    fn write_entry_contents(registration: &Registration) -> Result<()> {
        let root = create_key(registration.location.key())?;
        set_string(&root, None, OsStr::new(registration.location.label()))?;
        set_string(&root, Some(OsStr::new("Icon")), &registration.icon)?;

        let command_path = format!(r"{}\command", registration.location.key());
        let command = create_key(&command_path)?;
        set_string(&command, None, &registration.command)
    }

    fn delete_tree(path: &str) -> Result<()> {
        let path = wide(OsStr::new(path));
        // SAFETY: `path` is a live, NUL-terminated UTF-16 string. Only tty7's
        // own verb key is named, never a shared parent such as `shell`.
        let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr()) };
        match code {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(()),
            other => Err(io_error("removing the tty7 Explorer registry key", other)),
        }
    }

    fn notify_explorer() {
        // SAFETY: `SHCNE_ASSOCCHANGED` with `SHCNF_IDLIST` carries no item
        // pointers. This is a cache invalidation hint after the registry write.
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            );
        }
    }

    pub(super) fn register() -> Result<()> {
        let app = application_path()?;
        for registration in registrations(&app) {
            replace_entry_with(&registration, delete_tree, write_entry_contents)?;
        }
        notify_explorer();
        Ok(())
    }

    pub(super) fn unregister() -> Result<()> {
        // Try both removals even when one fails, so a damaged first key cannot
        // strand the independent second menu entry forever.
        let directory = delete_tree(DIRECTORY_KEY);
        let background = delete_tree(BACKGROUND_KEY);
        directory?;
        background?;
        notify_explorer();
        Ok(())
    }

    /// The verb key, or `None` when tty7's menu is not installed.
    ///
    /// Deliberately open rather than create: this is the call that makes
    /// [`refresh_labels`] unable to resurrect a menu the user removed.
    fn open_existing(path: &str) -> Result<Option<RegistryKey>> {
        let path = wide(OsStr::new(path));
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `path` is a live, NUL-terminated UTF-16 string and `key` is a
        // live local the API fills in only on success.
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_READ | KEY_WRITE,
                &mut key,
            )
        };
        match code {
            ERROR_SUCCESS => Ok(Some(RegistryKey(key))),
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
            other => Err(io_error("opening the tty7 Explorer registry key", other)),
        }
    }

    pub(super) fn refresh_labels() -> Result<()> {
        let mut restated = false;
        for location in [Location::Directory, Location::Background] {
            let Some(key) = open_existing(location.key())? else {
                continue;
            };
            set_string(&key, None, OsStr::new(location.label()))?;
            restated = true;
        }
        // Only worth waking the shell when something actually moved; a user who
        // never installed the menu changes languages for free.
        if restated {
            notify_explorer();
        }
        Ok(())
    }
}

#[cfg(windows)]
fn platform_register() -> Result<()> {
    windows::register()
}

#[cfg(windows)]
fn platform_unregister() -> Result<()> {
    windows::unregister()
}

#[cfg(windows)]
fn platform_refresh_labels() -> Result<()> {
    windows::refresh_labels()
}

#[cfg(not(windows))]
fn platform_register() -> Result<()> {
    anyhow::bail!("Windows Explorer integration is only available on Windows")
}

#[cfg(not(windows))]
fn platform_unregister() -> Result<()> {
    anyhow::bail!("Windows Explorer integration is only available on Windows")
}

/// Nothing to restate: the verbs exist only on Windows.
///
/// Silent rather than an error like the two above, because this one is called
/// on every language change on every platform. Refusing here would put a
/// warning in the log of every macOS and Linux user who picks a language.
#[cfg(not(windows))]
fn platform_refresh_labels() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_targets_both_directory_surfaces() {
        crate::ui::i18n::set_locale("en");
        let app = Path::new(r"C:\Program Files\tty7\tty7-app.exe");
        let [directory, background] = registrations(app);

        assert_eq!(directory.location.key(), DIRECTORY_KEY);
        assert_eq!(directory.location.label(), "Open in tty7");
        assert_eq!(directory.icon, app.as_os_str());
        assert_eq!(
            directory.command,
            r#""C:\Program Files\tty7\tty7-app.exe" --open-path "%1""#
        );

        assert_eq!(background.location.key(), BACKGROUND_KEY);
        assert_eq!(background.location.label(), "Open tty7 here");
        assert_eq!(background.icon, app.as_os_str());
        assert_eq!(
            background.command,
            r#""C:\Program Files\tty7\tty7-app.exe" --open-path "%V""#
        );
    }

    /// The entries Explorer shows were English whatever language tty7 ran in,
    /// because the labels were string literals. They are the only wording in
    /// the product that outlives the process that wrote it, so the guard is on
    /// the label rather than on the registry write it feeds.
    #[test]
    fn the_verb_labels_follow_the_ui_language() {
        crate::ui::i18n::set_locale("zh-CN");
        assert_eq!(Location::Directory.label(), "在 tty7 中打开");
        assert_eq!(Location::Background.label(), "在此处打开 tty7");

        crate::ui::i18n::set_locale("ja-JP");
        assert_eq!(Location::Directory.label(), "tty7 で開く");
        assert_eq!(Location::Background.label(), "ここで tty7 を開く");

        crate::ui::i18n::set_locale("en");
        assert_eq!(Location::Directory.label(), "Open in tty7");
        assert_eq!(Location::Background.label(), "Open tty7 here");
    }

    #[test]
    fn commands_quote_even_paths_without_spaces() {
        assert_eq!(
            quoted_open_command(Path::new(r"C:\tty7\tty7-app.exe"), "%1"),
            r#""C:\tty7\tty7-app.exe" --open-path "%1""#
        );
    }

    #[test]
    fn registration_replaces_the_owned_tree_before_writing() {
        let registration = Registration::new(
            Location::Directory,
            Path::new(r"C:\Program Files\tty7\tty7-app.exe"),
        );
        let operations = std::cell::RefCell::new(Vec::new());

        replace_entry_with(
            &registration,
            |key| {
                operations.borrow_mut().push(format!("delete:{key}"));
                Ok(())
            },
            |_| {
                operations.borrow_mut().push("write".to_string());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            operations.into_inner(),
            vec![format!("delete:{DIRECTORY_KEY}"), "write".to_string()]
        );
    }
}
