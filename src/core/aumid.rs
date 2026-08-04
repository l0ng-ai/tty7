//! Windows toast branding: an App User Model ID (AUMID) of our own.
//!
//! Without one, notify-rust falls back to PowerShell's AUMID and every toast
//! shows the PowerShell icon and name. Windows only honors an unpackaged
//! app's AUMID when a Start Menu shortcut carries the matching
//! `System.AppUserModel.ID`, so `init()` — called once at GUI startup —
//! brands the process and (re)writes that shortcut. Rewriting on every
//! launch, not just when the file is missing: installers before this change
//! created `tty7.lnk` without the property, and upgrades can move the exe.
//!
//! Everything is best-effort. `toast_app_id()` yields the AUMID only when the
//! shortcut is in place; otherwise callers fall back to the PowerShell
//! identity and the toast still arrives, just with the wrong icon.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The toast/taskbar identity. Keep in sync with the `AppUserModelID` on the
/// installer shortcuts in `.github/scripts/windows-installer.iss` — a
/// mismatch silently splits the identity in two (a unit test checks this).
pub(crate) const AUMID: &str = "com.github.tty7";

/// One-time GUI-startup hook; see the module docs. Cheap after the first call.
pub(crate) fn init() {
    let _ = toast_app_id();
}

/// The AUMID to put on toasts, when (and only when) the supporting shortcut
/// is in place. Memoized — the first call does the COM work.
pub(crate) fn toast_app_id() -> Option<&'static str> {
    static READY: OnceLock<bool> = OnceLock::new();
    READY.get_or_init(setup).then_some(AUMID)
}

fn setup() -> bool {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        // Branding the process also fixes taskbar grouping; toasts only need
        // the shortcut, so a failure here is not fatal.
        let _ = SetCurrentProcessExplicitAppUserModelID(&windows::core::HSTRING::from(AUMID));
    }
    match write_start_menu_shortcut() {
        Ok(()) => true,
        Err(e) => {
            log::warn!("toast branding disabled, keeping the PowerShell identity: {e}");
            false
        }
    }
}

fn start_menu_shortcut_path() -> Option<PathBuf> {
    // Per-user {autoprograms} — the same directory Inno's per-user install
    // targets. For all-users installs this simply adds a user-level twin.
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("tty7.lnk"),
    )
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().collect()
}

/// (Re)create the Start Menu shortcut pointing at the running exe, carrying
/// our AUMID. The exe has the icon compiled in (build.rs), which is what the
/// toast header shows.
fn write_start_menu_shortcut() -> Result<(), String> {
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        IPersistFile,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface, PROPVARIANT};

    let lnk = start_menu_shortcut_path().ok_or("APPDATA is not set")?;
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let exe_w = HSTRING::from_wide(&wide(exe.as_os_str())).map_err(|e| format!("exe path: {e}"))?;
    let lnk_w = HSTRING::from_wide(&wide(lnk.as_os_str())).map_err(|e| format!("lnk path: {e}"))?;

    unsafe {
        // Any prior COM init on this thread (S_FALSE) or another model
        // (RPC_E_CHANGED_MODE) is fine — we only need an initialized thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("ShellLink: {e}"))?;
        link.SetPath(&exe_w).map_err(|e| format!("SetPath: {e}"))?;
        let store: IPropertyStore = link.cast().map_err(|e| format!("IPropertyStore: {e}"))?;
        store
            .SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from(AUMID))
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
    fn start_menu_shortcut_lives_under_programs() {
        let Some(lnk) = super::start_menu_shortcut_path() else {
            return; // no APPDATA in this environment — nothing to assert
        };
        assert_eq!(lnk.file_name().and_then(|n| n.to_str()), Some("tty7.lnk"));
        assert_eq!(
            lnk.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("Programs")
        );
    }

    /// Manual end-to-end check, never run by CI (`--ignored` to opt in):
    /// writes the real per-user Start Menu shortcut and pops a real toast —
    /// it should carry the tty7 icon and name, not PowerShell's.
    #[test]
    #[ignore = "touches the real Start Menu and pops a real toast"]
    fn writes_shortcut_and_sends_a_branded_toast() {
        super::write_start_menu_shortcut().expect("write Start Menu shortcut");
        let lnk = super::start_menu_shortcut_path().unwrap();
        assert!(lnk.exists(), "shortcut should exist at {}", lnk.display());
        assert_eq!(super::toast_app_id(), Some(super::AUMID));
        crate::terminal::notify_desktop(Some("tty7"), "AUMID toast test");
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
