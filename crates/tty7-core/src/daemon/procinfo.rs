use std::collections::HashMap;

use crate::daemon::protocol::{PaneProcs, PortEntry, ProcEntry};

const MAX_DEPTH: u8 = 6;

const MAX_PROCS: usize = 64;

pub fn snapshot(shell_pid: u32, fg_pgid: Option<i32>) -> PaneProcs {
    let table = process_table();
    let procs = walk(&table, shell_pid, fg_pgid);
    let ports = listening_ports(&procs);
    PaneProcs { procs, ports }
}

struct Row {
    ppid: u32,
    pgid: u32,
    name: String,
}

fn walk(table: &HashMap<u32, Row>, shell_pid: u32, fg_pgid: Option<i32>) -> Vec<ProcEntry> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, row) in table {
        children.entry(row.ppid).or_default().push(*pid);
    }
    for kids in children.values_mut() {
        kids.sort_unstable();
    }

    let mut out = Vec::new();
    let mut stack = vec![(shell_pid, 0u8)];
    while let Some((pid, depth)) = stack.pop() {
        let Some(row) = table.get(&pid) else { continue };
        if out.len() >= MAX_PROCS {
            break;
        }
        out.push(ProcEntry {
            pid,
            name: row.name.clone(),
            depth,
            foreground: fg_pgid.is_some_and(|g| g as u32 == row.pgid),
        });
        if depth + 1 > MAX_DEPTH {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            for kid in kids.iter().rev() {
                stack.push((*kid, depth + 1));
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn process_table() -> HashMap<u32, Row> {
    let mut table = HashMap::new();
    let bytes = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return table;
    }
    let cap = (bytes as usize / std::mem::size_of::<libc::c_int>()) + 64;
    let mut pids = vec![0 as libc::c_int; cap];
    let written = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if written <= 0 {
        return table;
    }
    let n = written as usize / std::mem::size_of::<libc::c_int>();
    for &pid in pids.iter().take(n.min(cap)) {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            continue;
        }
        let name = proc_name(pid).unwrap_or_else(|| cstr_field(&info.pbi_comm));
        table.insert(
            pid as u32,
            Row {
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                name,
            },
        );
    }
    table
}

#[cfg(target_os = "macos")]
fn cstr_field(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
fn process_table() -> HashMap<u32, Row> {
    let mut table = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        let (Some(_state), Some(ppid), Some(pgid)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(ppid), Ok(pgid)) = (ppid.parse::<u32>(), pgid.parse::<u32>()) else {
            continue;
        };
        let name = proc_name(pid as i32).unwrap_or_else(|| {
            stat[..close]
                .rfind('(')
                .map_or_else(|| String::new(), |open| stat[open + 1..close].to_string())
        });
        table.insert(pid, Row { ppid, pgid, name });
    }
    table
}

#[cfg(windows)]
fn process_table() -> HashMap<u32, Row> {
    crate::daemon::winproc::snapshot()
        .into_iter()
        .map(|p| {
            (
                p.pid,
                Row {
                    ppid: p.parent,
                    pgid: 0,
                    name: p.name,
                },
            )
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn process_table() -> HashMap<u32, Row> {
    HashMap::new()
}

#[cfg(target_os = "macos")]
fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

#[cfg(target_os = "linux")]
fn proc_name(pid: i32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(unix)]
fn listening_ports(procs: &[ProcEntry]) -> Vec<PortEntry> {
    use std::process::{Command, Stdio};

    if procs.is_empty() {
        return Vec::new();
    }
    let pid_list = procs
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out = Command::new("lsof")
        .args([
            "-nP",
            "-iTCP",
            "-sTCP:LISTEN",
            "-a",
            "-p",
            &pid_list,
            "-Fpn",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);

    let by_pid: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    let mut ports: Vec<PortEntry> = Vec::new();
    let mut current = 0u32;
    for line in text.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => current = rest.parse().unwrap_or(0),
            "n" => {
                let Some((addr, port)) = parse_listen_addr(rest) else {
                    continue;
                };
                // One process listening on the same port over IPv4 and IPv6 is
                // one port to show. Which of the two lines survives used to be
                // whichever lsof printed first; now that the address is carried
                // through to a clickable URL, the reachable one wins — a
                // process bound to both `192.168.1.5` and `*` is on localhost,
                // and the row should say so.
                if let Some(seen) = ports
                    .iter_mut()
                    .find(|e| e.port == port && e.pid == current)
                {
                    if !PortEntry::reaches_loopback(&seen.addr) && PortEntry::reaches_loopback(addr)
                    {
                        seen.addr = addr.to_string();
                    }
                    continue;
                }
                ports.push(PortEntry {
                    port,
                    pid: current,
                    addr: addr.to_string(),
                    name: by_pid
                        .get(&current)
                        .copied()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            _ => {}
        }
    }
    ports.sort_by_key(|e| (e.port, e.pid));
    ports
}

#[cfg(not(unix))]
fn listening_ports(_procs: &[ProcEntry]) -> Vec<PortEntry> {
    Vec::new()
}

/// The address and port `lsof -Fn` reports a listener on — `*:3000`,
/// `127.0.0.1:8080`, `[::1]:5173`.
///
/// The address used to be dropped on the floor, which was harmless while the
/// port was a number to read. It stopped being harmless when the panel started
/// handing the port over as an address to open: a server bound only to
/// `172.17.0.1` or a LAN address is not on `localhost`, and offering it as one
/// sends the browser to a refused connection or, worse, to whatever else holds
/// that port on loopback.
fn parse_listen_addr(name: &str) -> Option<(&str, u16)> {
    let name = name.split_whitespace().next()?;
    let (addr, port) = name.rsplit_once(':')?;
    Some((addr, port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ppid: u32, name: &str) -> Row {
        Row {
            ppid,
            pgid: 0,
            name: name.to_string(),
        }
    }

    #[test]
    fn walk_is_depth_first_from_the_shell() {
        let table: HashMap<u32, Row> = [
            (100, row(1, "zsh")),
            (200, row(100, "make")),
            (300, row(200, "cc")),
            (400, row(100, "vim")),
            (500, row(1, "Finder")),
        ]
        .into_iter()
        .collect();

        let got = walk(&table, 100, None);
        let names: Vec<_> = got.iter().map(|p| (p.name.as_str(), p.depth)).collect();
        assert_eq!(
            names,
            vec![("zsh", 0), ("make", 1), ("cc", 2), ("vim", 1)],
            "depth-first, ascending pid, shell's tree only"
        );
    }

    #[test]
    fn walk_marks_the_foreground_process_group() {
        let mut table: HashMap<u32, Row> = [(100, row(1, "zsh")), (200, row(100, "vim"))]
            .into_iter()
            .collect();
        table.get_mut(&100).unwrap().pgid = 100;
        table.get_mut(&200).unwrap().pgid = 200;

        let got = walk(&table, 100, Some(200));
        assert!(
            !got[0].foreground,
            "the shell is backgrounded while vim runs"
        );
        assert!(got[1].foreground, "vim's group owns the terminal");
    }

    #[test]
    fn walk_survives_a_cycle_in_the_table() {
        let table: HashMap<u32, Row> = [(100, row(200, "a")), (200, row(100, "b"))]
            .into_iter()
            .collect();
        let got = walk(&table, 100, None);
        assert!(got.len() <= MAX_PROCS, "bounded, not infinite");
    }

    #[test]
    fn parses_lsof_listen_addresses() {
        assert_eq!(parse_listen_addr("*:3000"), Some(("*", 3000)));
        assert_eq!(
            parse_listen_addr("127.0.0.1:8080"),
            Some(("127.0.0.1", 8080))
        );
        assert_eq!(parse_listen_addr("[::1]:5173"), Some(("[::1]", 5173)));
        assert_eq!(parse_listen_addr("*:5432 (LISTEN)"), Some(("*", 5432)));
        assert_eq!(parse_listen_addr("/tmp/some.sock"), None);
    }

    #[test]
    fn an_address_only_becomes_localhost_when_localhost_reaches_it() {
        // What the panel copies and opens. A wildcard or a loopback bind is
        // spelled the way anyone would type it; an interface-specific bind is
        // kept, because `localhost` is not that server.
        let entry = |addr: &str| PortEntry {
            port: 8080,
            pid: 1,
            addr: addr.into(),
            name: "server".into(),
        };
        for addr in ["", "*", "0.0.0.0", "::", "[::]", "127.0.0.1", "[::1]"] {
            assert_eq!(
                entry(addr).authority(),
                "localhost:8080",
                "{addr} is reachable on loopback"
            );
        }
        assert_eq!(entry("172.17.0.1").authority(), "172.17.0.1:8080");
        assert_eq!(entry("192.168.1.20").authority(), "192.168.1.20:8080");
    }
}
