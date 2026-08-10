use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetProcessMitigationPolicy, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcess, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, PROCESS_CREATE_PROCESS, PROCESS_INFORMATION,
    ProcessRedirectionTrustPolicy, STARTUPINFOEXW, UpdateProcThreadAttribute,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

/// The ordinary daemon flags plus the attribute list this path exists for.
///
/// Derived from the shared constant rather than spelled out again: the two
/// spawn paths differing here is how one of them could quietly go back to
/// creating the daemon into a process group where Ctrl+C is disabled.
pub(super) const SPAWN_FLAGS: u32 = super::DAEMON_CREATION_FLAGS | EXTENDED_STARTUPINFO_PRESENT;

/// Returns whether the current process has the enforcing Redirection Trust bit.
///
/// Query failures deliberately fall back to the ordinary spawn path. The
/// alternate parent is only necessary when Windows confirms the policy is on.
pub(super) fn redirection_trust_enforced() -> bool {
    let mut flags = 0u32;
    // SAFETY: GetCurrentProcess returns a pseudo-handle that must not be closed,
    // and `flags` is the exact DWORD-sized buffer required by this policy.
    let ok = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessRedirectionTrustPolicy,
            (&raw mut flags).cast(),
            size_of::<u32>(),
        )
    };
    ok != 0 && flags & 0x1 != 0
}

/// Creates a detached process using the interactive desktop shell as its
/// logical parent, which supplies the normal user token, device map, and
/// mitigation policy instead of inheriting a hardened shell broker's policy.
pub(super) fn spawn_detached_with_clean_parent(
    program: &Path,
    args: &[OsString],
) -> io::Result<()> {
    let parent_pid = desktop_shell_pid()?;
    spawn_detached_with_parent(program, args, parent_pid)
}

/// Returns the process id that owns the current interactive desktop shell.
///
/// GetShellWindow identifies the correct Explorer instance for the active
/// desktop and avoids accidentally selecting a transient `/factory` broker.
fn desktop_shell_pid() -> io::Result<u32> {
    // SAFETY: Both calls only query the interactive desktop, and `pid` points
    // to a valid writable DWORD for the duration of the call.
    let window = unsafe { GetShellWindow() };
    if window.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows desktop shell window is unavailable",
        ));
    }

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    if pid == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Owns a Windows handle and closes it exactly once on every return path.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Each wrapped handle comes from a successful OpenProcess
            // or CreateProcessW call and is moved into exactly one owner.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The only attribute a daemon spawn sets.
///
/// Deliberately just the logical parent. Handing the child a `NUL` handle for
/// its standard streams — the Win32 spelling of the ordinary path's
/// `Stdio::null()` — is not possible here: naming a logical parent makes
/// handle inheritance follow *that* process, not tty7, so a handle from our
/// own table would not survive the transition. The daemon therefore starts
/// with no standard handles at all, and `daemon::server` is written to
/// tolerate that.
const ATTRIBUTE_COUNT: u32 = 1;

/// Owns an initialized PROC_THREAD_ATTRIBUTE_LIST and the values it points at.
struct SpawnAttributes {
    // The native structure is opaque but contains pointer-width fields. usize
    // storage guarantees the required alignment instead of relying on Vec<u8>.
    storage: Vec<usize>,
    // UpdateProcThreadAttribute stores pointers to these values rather than
    // copying them. Keep their addresses stable until CreateProcessW returns.
    _parent_value: Box<HANDLE>,
}

impl SpawnAttributes {
    fn new(parent: HANDLE) -> io::Result<Self> {
        let mut bytes = 0usize;
        // The first call is the documented size query and is expected to fail
        // while filling `bytes` with the required allocation size.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), ATTRIBUTE_COUNT, 0, &raw mut bytes)
        };
        if bytes == 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("query process attribute-list size: {error}"),
            ));
        }

        let words = bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; words];
        let list = storage.as_mut_ptr().cast();
        // SAFETY: The buffer is aligned, large enough for the reported byte
        // count, and will not be reallocated while the native list is alive.
        if unsafe { InitializeProcThreadAttributeList(list, ATTRIBUTE_COUNT, 0, &raw mut bytes) }
            == 0
        {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("initialize process attribute list: {error}"),
            ));
        }

        let parent_value = Box::new(parent);
        // PARENT_PROCESS supplies the token, device map, and mitigation policy.
        let attributes: [(u32, *const c_void, usize); ATTRIBUTE_COUNT as usize] = [(
            PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
            (&raw const *parent_value).cast(),
            size_of::<HANDLE>(),
        )];
        for (attribute, value, size) in attributes {
            // SAFETY: `list` is initialized, and each value has the exact size
            // and a stable address that outlives the attribute list.
            let updated = unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    attribute as usize,
                    value,
                    size,
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if updated == 0 {
                let error = io::Error::last_os_error();
                // SAFETY: Initialization succeeded, so the native list must be
                // released before returning the attribute update error.
                unsafe { DeleteProcThreadAttributeList(list) };
                return Err(io::Error::new(
                    error.kind(),
                    format!("set process attribute {attribute:#x}: {error}"),
                ));
            }
        }

        Ok(Self {
            storage,
            _parent_value: parent_value,
        })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: Construction only succeeds after native initialization, and
        // the backing storage remains allocated until this destructor returns.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// Appends one argument using the Windows CRT command-line quoting rules.
fn push_quoted_arg(command_line: &mut Vec<u16>, arg: &OsStr) {
    command_line.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in arg.encode_wide() {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            command_line.push(unit);
        } else {
            command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            command_line.push(unit);
        }
        backslashes = 0;
    }
    // Backslashes immediately before the closing quote must be doubled so
    // they cannot escape that quote and merge adjacent arguments.
    command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    command_line.push(b'"' as u16);
}

/// Creates a detached child whose logical parent is `parent_pid`.
///
/// The actual caller remains tty7, but Windows derives the child token, device
/// map, and mitigation policy from the process named by PARENT_PROCESS.
pub(super) fn spawn_detached_with_parent(
    program: &Path,
    args: &[OsString],
    parent_pid: u32,
) -> io::Result<()> {
    // PROCESS_CREATE_PROCESS is the only access right required when a process
    // handle is supplied through PROC_THREAD_ATTRIBUTE_PARENT_PROCESS.
    // SAFETY: OpenProcess only reads `parent_pid` and returns an owned handle
    // or NULL; nothing here outlives the wrapper installed below.
    let parent = unsafe { OpenProcess(PROCESS_CREATE_PROCESS, 0, parent_pid) };
    if parent.is_null() {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("open logical parent process {parent_pid}: {error}"),
        ));
    }
    let parent = OwnedHandle(parent);
    let mut attributes = SpawnAttributes::new(parent.0)?;

    let application: Vec<u16> = program.as_os_str().encode_wide().chain([0]).collect();
    let mut command_line = Vec::new();
    push_quoted_arg(&mut command_line, program.as_os_str());
    for arg in args {
        command_line.push(b' ' as u16);
        push_quoted_arg(&mut command_line, arg);
    }
    command_line.push(0);

    // SAFETY: Zero initialization is the required baseline for both Win32
    // structures. The size and initialized attribute-list pointer are then set.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.as_mut_ptr();
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let flags = SPAWN_FLAGS;
    // SAFETY: Both strings are NUL-terminated, the command line is writable,
    // and every pointer, handle, and output structure remains alive throughout
    // CreateProcessW. Handle inheritance is intentionally disabled.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            flags,
            ptr::null(),
            ptr::null(),
            &raw const startup.StartupInfo,
            &raw mut process_info,
        )
    };
    if created == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("CreateProcessW with logical parent {parent_pid}: {error}"),
        ));
    }

    // The child is intentionally independent and long-lived. Closing our two
    // handles mirrors dropping std::process::Child without waiting or killing.
    let _process = OwnedHandle(process_info.hProcess);
    let _thread = OwnedHandle(process_info.hThread);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(arg: &str) -> String {
        let mut units = Vec::new();
        push_quoted_arg(&mut units, OsStr::new(arg));
        String::from_utf16(&units).expect("the quoter only emits units it was given plus ASCII")
    }

    /// The daemon's `--config-dir` argument is a user path, so it can end in a
    /// backslash or carry a quote. Both need the CRT's doubling rules; getting
    /// them wrong silently merges the argument with the next one.
    #[test]
    fn arguments_survive_the_crt_quoting_rules() {
        assert_eq!(quoted("--daemon"), r#""--daemon""#);
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted(r"C:\Users\me\tty7"), r#""C:\Users\me\tty7""#);
        // A trailing backslash must not escape the closing quote.
        assert_eq!(quoted(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        // A literal quote takes one escape, and the run before it doubles.
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
    }
}
