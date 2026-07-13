use std::path::Path;

use crate::daemon::protocol::{RemoteContext, RemoteKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshInvocation {
    pub context: RemoteContext,
    pub forward_args: Vec<String>,
}

pub(crate) fn parse_ssh_invocation(argv: &[String]) -> Option<SshInvocation> {
    let program = argv.first()?;
    let name = Path::new(program).file_name()?.to_string_lossy();
    if name != "ssh" {
        return None;
    }

    let mut forward_args = Vec::new();
    let mut target = None;
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            target = Some(arg.clone());
            i += 1;
            break;
        }

        if arg == "-W" || arg == "-w" || arg == "-L" || arg == "-R" || arg == "-D" {
            return None;
        }
        if arg == "-N" || arg == "-f" {
            return None;
        }
        if let Some(short) = arg.strip_prefix('-')
            && !short.starts_with('-')
            && short.len() > 1
        {
            let mut chars = short.chars();
            let Some(flag) = chars.next() else {
                return None;
            };
            if option_takes_value(flag) {
                if chars.as_str().is_empty() {
                    i += 1;
                    if i >= argv.len() {
                        return None;
                    }
                    forward_args.push(arg.clone());
                    forward_args.push(argv[i].clone());
                } else {
                    forward_args.push(arg.clone());
                }
            } else {
                forward_args.push(arg.clone());
            }
            i += 1;
            continue;
        }

        if arg.starts_with("--") {
            return None;
        }

        forward_args.push(arg.clone());
        if arg.len() == 2 {
            let flag = arg.as_bytes()[1] as char;
            if option_takes_value(flag) {
                i += 1;
                if i >= argv.len() {
                    return None;
                }
                forward_args.push(argv[i].clone());
            }
        }
        i += 1;
    }

    let target = target?;
    if i < argv.len() {
        // Remote command present. Do not try to reuse this invocation for `-N`.
        return None;
    }

    Some(SshInvocation {
        context: RemoteContext {
            kind: RemoteKind::Ssh,
            argv: argv.to_vec(),
            target: target.clone(),
            control_path: None,
        },
        forward_args,
    })
}

fn option_takes_value(flag: char) -> bool {
    matches!(
        flag,
        'B' | 'b'
            | 'c'
            | 'D'
            | 'E'
            | 'e'
            | 'F'
            | 'I'
            | 'i'
            | 'J'
            | 'L'
            | 'l'
            | 'm'
            | 'O'
            | 'o'
            | 'p'
            | 'Q'
            | 'R'
            | 'S'
            | 'W'
            | 'w'
    )
}

pub(crate) fn foreground_argv(pid: i32) -> Option<Vec<String>> {
    platform_foreground_argv(pid)
}

#[cfg(target_os = "linux")]
fn platform_foreground_argv(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(target_os = "macos")]
fn platform_foreground_argv(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut len = 0usize;
    // SAFETY: first sysctl call requests the required buffer length.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || len < std::mem::size_of::<libc::c_int>()
    {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buffer is allocated to the size returned by sysctl above.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    parse_macos_procargs(&buf)
}

#[cfg(target_os = "macos")]
fn parse_macos_procargs(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < std::mem::size_of::<libc::c_int>() {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if i > start {
            argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        }
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_foreground_argv(_pid: i32) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_basic_ssh_invocation() {
        let inv = parse_ssh_invocation(&argv(&["ssh", "user@dev"])).unwrap();
        assert_eq!(inv.context.target, "user@dev");
        assert_eq!(inv.forward_args, Vec::<String>::new());
    }

    #[test]
    fn preserves_safe_options_before_target() {
        let inv = parse_ssh_invocation(&argv(&[
            "/usr/bin/ssh",
            "-F",
            "/tmp/config",
            "-p",
            "2222",
            "-Jjump",
            "dev",
        ]))
        .unwrap();
        assert_eq!(inv.context.target, "dev");
        assert_eq!(
            inv.forward_args,
            argv(&["-F", "/tmp/config", "-p", "2222", "-Jjump"])
        );
    }

    #[test]
    fn rejects_remote_commands_and_existing_forward_modes() {
        assert!(parse_ssh_invocation(&argv(&["ssh", "dev", "htop"])).is_none());
        assert!(parse_ssh_invocation(&argv(&["ssh", "-N", "dev"])).is_none());
        assert!(parse_ssh_invocation(&argv(&["ssh", "-W", "host:22", "dev"])).is_none());
        assert!(parse_ssh_invocation(&argv(&["scp", "dev:/x", "."])).is_none());
    }
}
