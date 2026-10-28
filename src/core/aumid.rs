//! Windows toast branding: an App User Model ID (AUMID) of our own.
//!
//! Without one, notify-rust falls back to PowerShell's AUMID and every toast
//! shows the PowerShell icon and name. Windows only honors an unpackaged app's
//! AUMID when a Start Menu shortcut carries the matching
//! `System.AppUserModel.ID`, so `init()` — called once at GUI startup — brands
//! the process and, when nothing else already supplies that shortcut, writes
//! one.
//!
//! It is deliberately reluctant to write. The installer stamps its own
//! shortcuts (see `windows-installer.iss`), so the runtime write only has to
//! cover the portable zip and installs that predate that change. It therefore
//! touches at most the single per-user `tty59.lnk` that Inno's default install
//! owns anyway — never a second Start Menu entry beside an all-users install
//! (which would show "tty59" twice and outlive the uninstaller), and never
//! anything at all from a `cargo` build directory (which would repoint the
//! user's installed shortcut at `target\debug`).
//!
//! The same shortcut also names tty59's toast activator
//! (`System.AppUserModel.ToastActivatorCLSID`), and `init()` registers that
//! class as a per-user COM local server. That pair is what makes a click on a
//! toast sitting in notification history reach us: the shell activates the
//! class over COM — launching tty59 when it is not running — instead of firing
//! the `Activated` event of a `ToastNotification` object that is long gone.
//! See `core::toast_activator`.
//!
//! Everything is best-effort. `toast_app_id()` yields the AUMID only once a
//! shortcut carrying it is in place *and* the shell has had time to index it;
//! otherwise callers keep the PowerShell identity, which looks wrong but still
//! shows up. That distinction matters: an AUMID the shell has not indexed does
//! not make `Toast::show()` fail, it makes it return success and drop the
//! toast on the floor.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The toast/taskbar identity. Keep in sync with the `AppUserModelID` on the
/// installer shortcuts in `.github/scripts/windows-installer.iss` — a
/// mismatch silently splits the identity in two (a unit test checks this).
pub(crate) const AUMID: &str = "com.github.tty59";

/// The toast activator class (`core::toast_activator`). Keep in sync with the
/// `AppUserModelToastActivatorCLSID` on the installer shortcuts and the key
/// its uninstaller removes — a unit test checks both. Never change it: older
/// shortcuts and registrations in the field name this exact class.
pub(crate) const TOAST_ACTIVATOR_CLSID: windows::core::GUID =
    windows::core::GUID::from_u128(0x4E1F6381_E90B_4BED_9156_A9013F718FE6);

/// The argument the COM registration launches us with. COM appends
/// `-Embedding` after it; neither changes how the launch is handled — it is a
/// pathless launch whose window the activation then brings forward — but they
/// make the process's origin legible in a process list or a crash report.
const TOAST_ACTIVATED_ARG: &str = "--toast-activated";

/// The CLSID as the registry and the installer spell it, without braces:
/// `4E1F6381-E90B-4BED-9156-A9013F718FE6`.
fn clsid_text(clsid: &windows::core::GUID) -> String {
    let hex = format!("{:032X}", clsid.to_u128());
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// How long we keep using the PowerShell identity after writing the shortcut
/// ourselves. The shell picks a new `.lnk` up asynchronously and silently
/// discards toasts for an AUMID it has not indexed yet; the measured lag on
/// Windows 11 was a few seconds. Overshooting only costs an unbranded toast
/// in the opening seconds of a first-ever run, so the bound is generous.
const INDEX_GRACE: Duration = Duration::from_secs(30);

/// One-time GUI-startup hook; see the module docs. Cheap after the first call.
pub(crate) fn init() {
    // First, so a launch that COM made to deliver a toast click has its class
    // object up as early as possible; COM is waiting on it.
    crate::core::toast_activator::start();
    let _ = toast_app_id();
}

/// The AUMID to put on toasts, when (and only when) a shortcut the shell has
/// seen carries it. Memoized — the first call does the COM work.
pub(crate) fn toast_app_id() -> Option<&'static str> {
    static STATE: OnceLock<Branding> = OnceLock::new();
    match STATE.get_or_init(setup) {
        Branding::Unavailable => None,
        Branding::Ready => Some(AUMID),
        Branding::Pending(written_at) => (written_at.elapsed() >= INDEX_GRACE).then_some(AUMID),
    }
}

enum Branding {
    /// Nothing carries our AUMID and we are not in a position to add it.
    Unavailable,
    /// A shortcut carrying it was already on disk before we started, so the
    /// shell has had it since long before this process existed.
    Ready,
    /// We wrote that shortcut just now — see `INDEX_GRACE`.
    Pending(Instant),
}

fn setup() -> Branding {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        // Branding the process is also what groups the taskbar button under
        // our own identity, and it is safe everywhere — including the build
        // directories the shortcut half below refuses to touch.
        let _ = SetCurrentProcessExplicitAppUserModelID(&windows::core::HSTRING::from(AUMID));
    }
    register_local_server();
    match decide() {
        Ok(Decision::Branded) => Branding::Ready,
        Ok(Decision::Skip(why)) => {
            log::debug!("keeping the PowerShell toast identity: {why}");
            Branding::Unavailable
        }
        Ok(Decision::Write { lnk, indexed }) => match write_shortcut(&lnk) {
            // Refreshing a shortcut that already carried the AUMID (to add the
            // toast activator) leaves the identity the shell indexed intact.
            Ok(()) if indexed => Branding::Ready,
            Ok(()) => Branding::Pending(Instant::now()),
            Err(e) => {
                log::warn!("toast branding disabled, keeping the PowerShell identity: {e}");
                Branding::Unavailable
            }
        },
        Err(e) => {
            log::warn!("toast branding disabled, keeping the PowerShell identity: {e}");
            Branding::Unavailable
        }
    }
}

enum Decision {
    /// A shortcut already carries our AUMID; write nothing.
    Branded,
    /// Create or refresh this per-user shortcut, in place. `indexed` when it
    /// already carries our AUMID, so the shell knows the identity already.
    Write { lnk: PathBuf, indexed: bool },
    /// Nothing we may safely write. The string is for the log.
    Skip(&'static str),
}

fn decide() -> Result<Decision, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;

    // An elevated install owns `%ProgramData%\...\tty59.lnk`, which we cannot
    // rewrite unelevated. A per-user twin beside it would list "tty59" twice in
    // the Start Menu and survive the uninstaller, so that file settles the
    // question on its own: branded if the installer stamped our AUMID on it,
    // unbranded until the user upgrades to an installer that does.
    if let Some(lnk) = all_users_shortcut_path()
        && lnk.is_file()
    {
        let stamped = read_shortcut(&lnk)
            .map_err(|e| format!("read {}: {e}", lnk.display()))?
            .aumid
            .as_deref()
            == Some(AUMID);
        return Ok(if stamped {
            Decision::Branded
        } else {
            Decision::Skip("an all-users Start Menu shortcut owns the entry")
        });
    }

    let lnk = start_menu_shortcut_path().ok_or("APPDATA is not set")?;
    let existing = if lnk.is_file() {
        Some(read_shortcut(&lnk).map_err(|e| format!("read {}: {e}", lnk.display()))?)
    } else {
        None
    };
    let stamped = existing
        .as_ref()
        .is_some_and(|s| s.aumid.as_deref() == Some(AUMID));
    // A shortcut from before the toast activator existed brands toasts fine,
    // but a click on one in notification history goes nowhere until the
    // shortcut also names the class — so it is rewritten like an off-target one.
    let activates = existing
        .as_ref()
        .is_some_and(|s| s.toast_activator == Some(TOAST_ACTIVATOR_CLSID));
    let on_target = existing
        .as_ref()
        .and_then(|s| s.target.as_deref())
        .is_some_and(|t| same_path(t, &exe));
    if stamped && activates && on_target {
        return Ok(Decision::Branded);
    }

    if is_build_output(&exe) {
        // `cargo run` must never repoint the installed Start Menu shortcut at
        // `target\debug`. If an install already left a stamped shortcut here,
        // toasts from the dev build still brand correctly off it — Windows
        // only asks that the AUMID be registered, not that it point at us.
        return Ok(if stamped {
            Decision::Branded
        } else {
            Decision::Skip("running from a cargo build directory")
        });
    }

    Ok(Decision::Write {
        lnk,
        indexed: stamped,
    })
}

/// `HKCU\Software\Classes\CLSID\{…}\LocalServer32` — what COM runs to
/// deliver a toast click while tty59 is not running. Per-user, so it needs no
/// elevation whatever the install shape; the uninstaller removes the CLSID key.
fn local_server_key() -> String {
    format!(
        r"Software\Classes\CLSID\{{{}}}\LocalServer32",
        clsid_text(&TOAST_ACTIVATOR_CLSID)
    )
}

fn local_server_command(exe: &Path) -> std::ffi::OsString {
    let mut command = std::ffi::OsString::from("\"");
    command.push(exe.as_os_str());
    command.push("\" ");
    command.push(TOAST_ACTIVATED_ARG);
    command
}

/// Point the toast activator's COM registration at the running exe, unless
/// it already does. Never from a cargo build directory, for the same reason
/// `decide()` never writes the shortcut from one: a `cargo run` must not leave
/// the user's toast clicks launching `target\debug`. A dev build still
/// receives clicks while it is running, through its class object.
fn register_local_server() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log::warn!("toast activator not registered: current exe: {e}");
            return;
        }
    };
    if is_build_output(&exe) {
        return;
    }
    let key = local_server_key();
    let command = local_server_command(&exe);
    if read_default_string(&key).as_deref() == Some(command.as_os_str()) {
        return;
    }
    if let Err(e) = write_default_string(&key, &command) {
        log::warn!("toast activator not registered: {e}");
    }
}

fn read_default_string(subkey: &str) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};

    let subkey = wide_z(OsStr::new(subkey));
    let mut buf = vec![0u16; 1024];
    let mut bytes = u32::try_from(buf.len() * 2).ok()?;
    // SAFETY: `subkey` is a live NUL-terminated UTF-16 string; the default
    // value is named by a null pointer; `buf` holds `bytes` writable bytes.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if code != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(bytes as usize / 2);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(std::ffi::OsString::from_wide(&buf))
}

fn write_default_string(subkey: &str, value: &OsStr) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};

    let subkey_w = wide_z(OsStr::new(subkey));
    let value = wide_z(value);
    let bytes = u32::try_from(value.len() * 2).map_err(|_| "value too long".to_string())?;
    // SAFETY: both buffers are live NUL-terminated UTF-16 strings, `bytes`
    // covers `value` including its terminator, and a null value name is the
    // key's default value. `RegSetKeyValueW` creates missing keys.
    let code = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            std::ptr::null(),
            REG_SZ,
            value.as_ptr().cast(),
            bytes,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "write HKCU\\{subkey}: {}",
            std::io::Error::from_raw_os_error(code as i32)
        ))
    }
}

/// True when `exe` sits in a `cargo` build directory rather than an install.
///
/// Two independent signals. The layout check is the offline half:
/// `target[\<triple>]\{debug,release}\tty59-app.exe`. `CACHEDIR.TAG` is the
/// half that does not care about names — cargo writes it into every build
/// directory precisely to mark the tree as derived, so it also covers a
/// renamed `CARGO_TARGET_DIR` and the `target\...\deps\` binaries the test
/// harness runs from.
fn is_build_output(exe: &Path) -> bool {
    has_build_layout(exe)
        || exe
            .ancestors()
            .any(|dir| dir.join("CACHEDIR.TAG").is_file())
}

fn has_build_layout(exe: &Path) -> bool {
    fn named(dir: Option<&Path>, name: &str) -> bool {
        dir.and_then(Path::file_name)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    }
    let profile = exe.parent();
    (named(profile, "debug") || named(profile, "release"))
        && exe.ancestors().any(|dir| named(Some(dir), "target"))
}

/// Windows paths are case-insensitive and a `.lnk` may hold a short (8.3) or
/// otherwise unnormalized form of the same file, so compare canonically and
/// only fall back to text when the target no longer exists.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
    }
}

fn programs_dir(env_var: &str) -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os(env_var)?)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs"),
    )
}

/// The only shortcut we ever write: the per-user Start Menu, which is also
/// where Inno's default (non-elevated) install puts `tty59.lnk` — so refreshing
/// it adds no entry the uninstaller does not already know how to remove.
fn start_menu_shortcut_path() -> Option<PathBuf> {
    Some(programs_dir("APPDATA")?.join("tty59.lnk"))
}

/// The all-users twin an elevated install writes. Read-only for us.
fn all_users_shortcut_path() -> Option<PathBuf> {
    Some(programs_dir("ProgramData")?.join("tty59.lnk"))
}

#[derive(Default)]
struct Shortcut {
    aumid: Option<String>,
    toast_activator: Option<windows::core::GUID>,
    target: Option<PathBuf>,
}

fn wide(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().collect()
}

fn wide_z(s: &OsStr) -> Vec<u16> {
    let mut w = wide(s);
    w.push(0);
    w
}

fn hstring(path: &Path) -> Result<windows::core::HSTRING, String> {
    windows::core::HSTRING::from_wide(&wide(path.as_os_str()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Any prior COM init on this thread (`S_FALSE`) or another model
/// (`RPC_E_CHANGED_MODE`) is fine — we only need an initialized thread, and
/// `ShellLink` is apartment-threaded either way.
fn co_init() {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Read back the properties `decide()` cares about. Absent ones come back
/// as `None`; only genuine COM failures are errors.
fn read_shortcut(lnk: &Path) -> Result<Shortcut, String> {
    use windows::Win32::Storage::EnhancedStorage::{
        PKEY_AppUserModel_ID, PKEY_AppUserModel_ToastActivatorCLSID,
    };
    use windows::Win32::System::Com::StructuredStorage::PropVariantToGUID;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
    use windows::core::{BSTR, Interface};

    let lnk_w = hstring(lnk)?;
    co_init();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("ShellLink: {e}"))?;
        let persist: IPersistFile = link.cast().map_err(|e| format!("IPersistFile: {e}"))?;
        persist
            .Load(&lnk_w, STGM_READ)
            .map_err(|e| format!("Load: {e}"))?;

        let store: IPropertyStore = link.cast().map_err(|e| format!("IPropertyStore: {e}"))?;
        // `BSTR::try_from` goes through `PropVariantToBSTR`, so it copes with
        // both the VT_LPWSTR the installer writes and the VT_BSTR we write.
        let aumid = store
            .GetValue(&PKEY_AppUserModel_ID)
            .ok()
            .and_then(|v| BSTR::try_from(&v).ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        // VT_CLSID whichever side wrote it. An absent property is VT_EMPTY,
        // which `PropVariantToGUID` converts to the nil GUID rather than an
        // error, so that is what "not set" looks like here.
        let toast_activator = store
            .GetValue(&PKEY_AppUserModel_ToastActivatorCLSID)
            .ok()
            .and_then(|v| PropVariantToGUID(&v).ok())
            .filter(|clsid| *clsid != windows::core::GUID::zeroed());

        // SLGP_RAWPATH returns the stored target verbatim; without it the
        // shell may go looking for a moved file, including over the network.
        let mut buf = [0u16; 1024];
        let target = link
            .GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()
            .map(|()| {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                PathBuf::from(String::from_utf16_lossy(&buf[..len]))
            })
            .filter(|p| !p.as_os_str().is_empty());

        Ok(Shortcut {
            aumid,
            toast_activator,
            target,
        })
    }
}

/// Create or overwrite `lnk`, pointing at the running exe and carrying our
/// AUMID and toast activator. The exe has the icon compiled in (build.rs), which is what the toast
/// header shows.
fn write_shortcut(lnk: &Path) -> Result<(), String> {
    use windows::Win32::Storage::EnhancedStorage::{
        PKEY_AppUserModel_ID, PKEY_AppUserModel_ToastActivatorCLSID,
    };
    use windows::Win32::System::Com::StructuredStorage::InitPropVariantFromCLSID;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{Interface, PROPVARIANT};

    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let exe_w = hstring(&exe)?;
    let lnk_w = hstring(lnk)?;

    co_init();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("ShellLink: {e}"))?;
        link.SetPath(&exe_w).map_err(|e| format!("SetPath: {e}"))?;
        let store: IPropertyStore = link.cast().map_err(|e| format!("IPropertyStore: {e}"))?;
        store
            .SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from(AUMID))
            .map_err(|e| format!("SetValue: {e}"))?;
        let clsid = InitPropVariantFromCLSID(&TOAST_ACTIVATOR_CLSID)
            .map_err(|e| format!("InitPropVariantFromCLSID: {e}"))?;
        store
            .SetValue(&PKEY_AppUserModel_ToastActivatorCLSID, &clsid)
            .map_err(|e| format!("SetValue: {e}"))?;
        store.Commit().map_err(|e| format!("Commit: {e}"))?;
        let persist: IPersistFile = link.cast().map_err(|e| format!("IPersistFile: {e}"))?;
        persist
            .Save(&lnk_w, true)
            .map_err(|e| format!("Save: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn aumid_matches_the_installer_shortcuts() {
        let iss = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.github/scripts/windows-installer.iss"
        ))
        .expect("read windows-installer.iss");
        let needle = format!("AppUserModelID: \"{}\"", super::AUMID);
        assert!(
            iss.contains(&needle),
            "windows-installer.iss must set {needle} on its shortcuts"
        );
    }

    #[test]
    fn toast_activator_matches_the_installer() {
        let iss = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.github/scripts/windows-installer.iss"
        ))
        .expect("read windows-installer.iss");
        let clsid = super::clsid_text(&super::TOAST_ACTIVATOR_CLSID);
        let stamp = format!("AppUserModelToastActivatorCLSID: \"{clsid}\"");
        assert_eq!(
            iss.matches(&stamp).count(),
            iss.matches("AppUserModelID:").count(),
            "every windows-installer.iss shortcut must set {stamp}"
        );
        let cleanup = format!(r"'Software\Classes\CLSID\{{{clsid}}}'");
        assert!(
            iss.contains(&cleanup),
            "the uninstaller must remove {cleanup}"
        );
    }

    #[test]
    fn the_clsid_is_spelled_the_registry_way() {
        assert_eq!(
            super::clsid_text(&super::TOAST_ACTIVATOR_CLSID),
            "4E1F6381-E90B-4BED-9156-A9013F718FE6"
        );
        assert_eq!(
            super::local_server_key(),
            r"Software\Classes\CLSID\{4E1F6381-E90B-4BED-9156-A9013F718FE6}\LocalServer32"
        );
    }

    /// COM runs this string after appending `-Embedding`; an unquoted path
    /// with a space in it would launch `C:\Program` instead.
    #[test]
    fn the_local_server_command_quotes_the_exe() {
        assert_eq!(
            super::local_server_command(Path::new(r"C:\Program Files\tty59\tty59-app.exe")),
            r#""C:\Program Files\tty59\tty59-app.exe" --toast-activated"#
        );
    }

    #[test]
    fn start_menu_shortcut_lives_under_programs() {
        let Some(lnk) = super::start_menu_shortcut_path() else {
            return; // no APPDATA in this environment — nothing to assert
        };
        assert_eq!(lnk.file_name().and_then(|n| n.to_str()), Some("tty59.lnk"));
        assert_eq!(
            lnk.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("Programs")
        );
    }

    /// The two Start Menu roots must be distinct files, or the "an all-users
    /// shortcut owns the entry" branch would swallow the per-user one too.
    #[test]
    fn the_two_start_menu_roots_are_distinct() {
        let (Some(user), Some(all)) = (
            super::start_menu_shortcut_path(),
            super::all_users_shortcut_path(),
        ) else {
            return;
        };
        assert_ne!(user, all);
    }

    #[test]
    fn cargo_layouts_are_recognized_as_build_output() {
        for exe in [
            r"C:\src\tty59\target\debug\tty59-app.exe",
            r"C:\src\tty59\target\release\tty59-app.exe",
            r"C:\src\tty59\target\x86_64-pc-windows-msvc\release\tty59-app.exe",
            r"C:\src\tty59\TARGET\Debug\tty59-app.exe",
        ] {
            assert!(
                super::has_build_layout(Path::new(exe)),
                "{exe} should look like a build directory"
            );
        }
    }

    #[test]
    fn install_layouts_are_not_build_output() {
        for exe in [
            r"C:\Program Files\tty59\tty59-app.exe",
            r"C:\Users\me\AppData\Local\Programs\tty59\tty59-app.exe",
            // Portable zip, extracted anywhere the user likes.
            r"D:\tools\tty59\tty59-app.exe",
            // A "release" *install* directory with no `target` above it.
            r"D:\tty59\release\tty59-app.exe",
        ] {
            let exe = Path::new(exe);
            assert!(!super::has_build_layout(exe), "{} misread", exe.display());
            assert!(!super::is_build_output(exe), "{} misread", exe.display());
        }
    }

    /// The test harness itself runs out of `target\...\deps\`, whose parent is
    /// neither `debug` nor `release` — so this is the `CACHEDIR.TAG` half of
    /// `is_build_output` proving itself against a real cargo layout.
    #[test]
    fn the_test_binary_counts_as_build_output() {
        let exe = std::env::current_exe().expect("current exe");
        assert!(
            super::is_build_output(&exe),
            "{} should be detected as a cargo build",
            exe.display()
        );
    }

    #[test]
    fn same_path_ignores_case_for_paths_that_do_not_exist() {
        assert!(super::same_path(
            Path::new(r"C:\Program Files\tty59\TTY59-APP.EXE"),
            Path::new(r"c:\program files\tty59\tty59-app.exe"),
        ));
        assert!(!super::same_path(
            Path::new(r"C:\Program Files\tty59\tty59-app.exe"),
            Path::new(r"C:\src\tty59\target\debug\tty59-app.exe"),
        ));
    }

    /// Round-trips a shortcut through the real shell into a temp directory:
    /// `write_shortcut` must produce something `read_shortcut` recognizes as
    /// ours, or `decide()` would rewrite it on every single launch. Touches no
    /// Start Menu.
    #[test]
    fn a_written_shortcut_reads_back_as_ours() {
        let dir = std::env::temp_dir().join(format!("tty59-aumid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let lnk = dir.join("tty59.lnk");

        let written = super::write_shortcut(&lnk);
        let read = written.as_ref().ok().map(|()| super::read_shortcut(&lnk));
        let _ = std::fs::remove_dir_all(&dir);

        written.expect("write shortcut");
        let shortcut = read.expect("read attempted").expect("read shortcut");
        assert_eq!(shortcut.aumid.as_deref(), Some(super::AUMID));
        // Without it a click on the toast in notification history reaches
        // nobody (#1179), and `decide()` would rewrite the shortcut forever.
        assert_eq!(shortcut.toast_activator, Some(super::TOAST_ACTIVATOR_CLSID));
        let exe = std::env::current_exe().expect("current exe");
        let target = shortcut.target.unwrap_or_default();
        assert!(
            super::same_path(&target, &exe),
            "target {} should be {}",
            target.display(),
            exe.display()
        );
    }

    /// Manual end-to-end check, never run by CI (`--ignored` to opt in). Prints
    /// what `decide()` chose and pops a real toast, which should carry the tty59
    /// icon and name rather than PowerShell's. On a machine that had no
    /// shortcut yet the first run writes one and stays unbranded for
    /// `INDEX_GRACE`; run it a second time to see the branded toast.
    #[test]
    #[ignore = "may touch the real Start Menu and pops a real toast"]
    fn sends_a_branded_toast() {
        let decision = match super::decide().expect("decide") {
            super::Decision::Branded => "already branded".to_string(),
            super::Decision::Write { lnk, .. } => format!("writing {}", lnk.display()),
            super::Decision::Skip(why) => format!("skipping: {why}"),
        };
        println!("decision: {decision}");
        println!("toast_app_id: {:?}", super::toast_app_id());
        crate::terminal::notify_desktop(Some("tty59"), "AUMID toast test");
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
