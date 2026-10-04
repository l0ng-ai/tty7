//! What a phone calls this machine, and how it tells this computer from
//! another one by the same name.
//!
//! The gateway's key is what a phone trusts, but a key belongs to one tty7
//! config dir, not to the computer: a reinstall, a second config dir or a dev
//! build beside the installed app each make a new one. To a phone that is a
//! second machine, listed under the same name as the first, with nothing to
//! say which of the two still answers. The name here is the one people gave
//! the computer, and the fingerprint is the same for every key on it.

use sha2::{Digest as _, Sha256};

/// This machine's name as the phone lists it. On macOS that is the name in
/// System Settings → General → About ("Thomas's Mac mini"): the host name is
/// often whatever the router's DHCP handed out (`Mac.lan`), which says little
/// and changes with the network.
pub fn name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = computer_name() {
        return name;
    }
    host_name()
}

fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its whole length, and gethostname
        // writes at most that many bytes.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]);
            // `studio.local`, `Mac.lan`: the domain is the network's, not the
            // machine's name.
            let name = name.split('.').next().unwrap_or_default();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "tty7".to_string())
}

#[cfg(target_os = "macos")]
fn computer_name() -> Option<String> {
    use system_configuration::core_foundation::base::TCFType as _;
    use system_configuration::core_foundation::string::CFString;
    use system_configuration::sys::dynamic_store_copy_specific::SCDynamicStoreCopyComputerName;

    // SAFETY: a null store asks the system's own, and a null encoding pointer
    // is allowed. The result follows the Create rule: owned, or null.
    let name = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null(), std::ptr::null_mut()) };
    if name.is_null() {
        return None;
    }
    // SAFETY: non-null and owned by us, per the Create rule above.
    let name = unsafe { CFString::wrap_under_create_rule(name) }.to_string();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// This computer, as the same short hex string from every tty7 on it: a hash
/// of the OS's own machine id, so the id itself never leaves the machine and
/// the value means nothing outside tty7. `None` where the OS has no id to
/// read, and a phone then goes by key alone, as it did before.
pub fn fingerprint() -> Option<String> {
    machine_id().map(|id| fingerprint_of(&id))
}

fn fingerprint_of(machine_id: &str) -> String {
    let digest = Sha256::new()
        .chain_update(b"tty7 machine fingerprint\0")
        .chain_update(machine_id.trim().as_bytes())
        .finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The hardware UUID, as `ioreg` prints it under IOPlatformUUID.
#[cfg(target_os = "macos")]
fn machine_id() -> Option<String> {
    let mut uuid = [0u8; 16];
    let wait = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // SAFETY: `uuid` is the 16 bytes gethostuuid writes, and `wait` outlives
    // the call.
    let rc = unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &wait) };
    (rc == 0 && uuid != [0; 16]).then(|| uuid.iter().map(|b| format!("{b:02x}")).collect())
}

/// systemd's machine id, or D-Bus's where there is no systemd.
#[cfg(all(unix, not(target_os = "macos")))]
fn machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|id| id.trim().to_string())
        .find(|id| !id.is_empty())
}

/// The id Windows mints at install, which every user on the machine reads.
#[cfg(windows)]
fn machine_id() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};

    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .ok()?;
    let id: String = key.get_value("MachineGuid").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(not(any(unix, windows)))]
fn machine_id() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_short_hex_and_hides_the_id() {
        let id = "4C4C4544-0047-3510-8052-B4C04F4E4D32";
        let print = fingerprint_of(id);
        assert_eq!(print.len(), 32);
        assert!(print.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!print.contains(&id.to_ascii_lowercase().replace('-', "")[..8]));
        // A trailing newline, as /etc/machine-id has, is the same machine.
        assert_eq!(fingerprint_of(&format!("{id}\n")), print);
        assert_ne!(fingerprint_of("another machine"), print);
    }

    /// Every tty7 on this computer reads the same one, call after call.
    #[test]
    fn this_machine_has_one_fingerprint() {
        if let Some(print) = fingerprint() {
            assert_eq!(fingerprint(), Some(print));
        }
    }

    #[test]
    fn this_machine_has_a_name() {
        assert!(!name().trim().is_empty());
    }
}
