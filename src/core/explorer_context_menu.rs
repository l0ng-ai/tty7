//! Optional Windows Explorer context-menu integration.
//!
//! tty7 deliberately does not register shell verbs during installation or
//! startup. The registry is user-visible system state, so only the explicit
//! buttons in Settings call [`register`] or [`unregister`]. Both verbs invoke
//! the GUI-subsystem `tty7-app.exe` directly so Explorer never allocates a
//! transient console. The app first offers the path to an already running GUI
//! through `GuiOpen`, then continues normal startup when no GUI receives it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

const DIRECTORY_KEY: &str = r"Software\Classes\Directory\shell\tty7";
const BACKGROUND_KEY: &str = r"Software\Classes\Directory\Background\shell\tty7";

/// The state shown in Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Neither tty7 verb exists for this user.
    NotRegistered,
    /// Both verbs exactly describe the currently running tty7 installation.
    Registered,
    /// At least one verb exists, but the pair is incomplete or points elsewhere.
    NeedsUpdate,
    /// Explorer shell verbs are unavailable on this operating system.
    Unsupported,
}

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

    fn label(self) -> &'static str {
        match self {
            Self::Directory => "Open in tty7",
            Self::Background => "Open tty7 here",
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegistryShape {
    values: u32,
    subkeys: u32,
}

fn registration_tree_is_exact(root: RegistryShape, command: RegistryShape) -> bool {
    root == (RegistryShape {
        values: 2,
        subkeys: 1,
    }) && command
        == (RegistryShape {
            values: 1,
            subkeys: 0,
        })
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

/// Return the live per-user registration state.
pub fn status() -> Result<Status> {
    platform_status()
}

/// Register both Explorer verbs for the current user.
pub fn register() -> Result<()> {
    platform_register()
}

/// Remove only the two Explorer verb trees owned by tty7.
pub fn unregister() -> Result<()> {
    platform_unregister()
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EntryState {
        Missing,
        Matching,
        Different,
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn units(value: &OsStr) -> Vec<u16> {
        value.encode_wide().collect()
    }

    fn io_error(action: &str, code: u32) -> anyhow::Error {
        anyhow::anyhow!(
            "{action}: {}",
            std::io::Error::from_raw_os_error(code as i32)
        )
    }

    fn open_key(path: &str) -> Result<Option<RegistryKey>> {
        let path = wide(OsStr::new(path));
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and alive for the call; `key` is a
        // valid out-parameter and is wrapped only when the call succeeds.
        let code =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) };
        match code {
            ERROR_SUCCESS => Ok(Some(RegistryKey(key))),
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
            other => Err(io_error("opening the tty7 Explorer registry key", other)),
        }
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

    fn query_string(key: &RegistryKey, name: Option<&OsStr>) -> Result<Option<Vec<u16>>> {
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
        let mut kind = 0u32;
        let mut bytes = 0u32;
        // SAFETY: the key is live, the optional value-name pointer is either
        // null or NUL-terminated, and the size/type out-parameters are valid.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name_ptr,
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            return Err(io_error("reading a tty7 Explorer registry value", code));
        }
        if kind != REG_SZ || !bytes.is_multiple_of(2) {
            return Ok(None);
        }

        let mut value = vec![0u16; (bytes as usize).div_ceil(2)];
        // SAFETY: `value` is sized from the preceding query and remains live;
        // Win32 receives its capacity in bytes through `bytes`.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name_ptr,
                std::ptr::null(),
                &mut kind,
                value.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io_error("reading a tty7 Explorer registry value", code));
        }
        value.truncate(bytes as usize / 2);
        while value.last() == Some(&0) {
            value.pop();
        }
        Ok(Some(value))
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

    fn key_shape(key: &RegistryKey) -> Result<RegistryShape> {
        let mut subkeys = 0u32;
        let mut values = 0u32;
        // SAFETY: `key` is live and the two count pointers reference writable
        // locals. Every optional output that is not needed is passed as null,
        // which `RegQueryInfoKeyW` explicitly permits.
        let code = unsafe {
            RegQueryInfoKeyW(
                key.0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut subkeys,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut values,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if code == ERROR_SUCCESS {
            Ok(RegistryShape { values, subkeys })
        } else {
            Err(io_error("inspecting the tty7 Explorer registry key", code))
        }
    }

    fn entry_state(registration: &Registration) -> Result<EntryState> {
        let Some(root) = open_key(registration.location.key())? else {
            return Ok(EntryState::Missing);
        };
        let label = query_string(&root, None)?;
        let icon = query_string(&root, Some(OsStr::new("Icon")))?;

        let command_path = format!(r"{}\command", registration.location.key());
        let command_key = match open_key(&command_path)? {
            Some(key) => key,
            None => return Ok(EntryState::Different),
        };
        let command = query_string(&command_key, None)?;
        let matches = label.as_deref() == Some(&units(OsStr::new(registration.location.label())))
            && icon.as_deref() == Some(&units(&registration.icon))
            && command.as_deref() == Some(&units(&registration.command))
            && registration_tree_is_exact(key_shape(&root)?, key_shape(&command_key)?);
        Ok(if matches {
            EntryState::Matching
        } else {
            EntryState::Different
        })
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

    pub(super) fn status() -> Result<Status> {
        let app = application_path()?;
        let states = registrations(&app).map(|entry| entry_state(&entry));
        let [directory, background] = states;
        let (directory, background) = (directory?, background?);
        Ok(match (directory, background) {
            (EntryState::Missing, EntryState::Missing) => Status::NotRegistered,
            (EntryState::Matching, EntryState::Matching) => Status::Registered,
            _ => Status::NeedsUpdate,
        })
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
}

#[cfg(windows)]
fn platform_status() -> Result<Status> {
    windows::status()
}

#[cfg(windows)]
fn platform_register() -> Result<()> {
    windows::register()
}

#[cfg(windows)]
fn platform_unregister() -> Result<()> {
    windows::unregister()
}

#[cfg(not(windows))]
fn platform_status() -> Result<Status> {
    Ok(Status::Unsupported)
}

#[cfg(not(windows))]
fn platform_register() -> Result<()> {
    anyhow::bail!("Windows Explorer integration is only available on Windows")
}

#[cfg(not(windows))]
fn platform_unregister() -> Result<()> {
    anyhow::bail!("Windows Explorer integration is only available on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_targets_both_directory_surfaces() {
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

    #[test]
    fn commands_quote_even_paths_without_spaces() {
        assert_eq!(
            quoted_open_command(Path::new(r"C:\tty7\tty7-app.exe"), "%1"),
            r#""C:\tty7\tty7-app.exe" --open-path "%1""#
        );
    }

    #[test]
    fn registration_shape_rejects_every_extra_value_or_subkey() {
        assert!(registration_tree_is_exact(
            RegistryShape {
                values: 2,
                subkeys: 1,
            },
            RegistryShape {
                values: 1,
                subkeys: 0,
            },
        ));
        assert!(!registration_tree_is_exact(
            RegistryShape {
                values: 3,
                subkeys: 1,
            },
            RegistryShape {
                values: 1,
                subkeys: 0,
            },
        ));
        assert!(!registration_tree_is_exact(
            RegistryShape {
                values: 2,
                subkeys: 1,
            },
            RegistryShape {
                values: 2,
                subkeys: 0,
            },
        ));
        assert!(!registration_tree_is_exact(
            RegistryShape {
                values: 2,
                subkeys: 2,
            },
            RegistryShape {
                values: 1,
                subkeys: 0,
            },
        ));
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
