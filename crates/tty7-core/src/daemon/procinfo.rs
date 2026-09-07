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

/// The executable name behind a pid.
///
/// One copy, shared with `pane.rs`. There used to be two, and each carried a
/// guard the other lacked — this one had no `pid <= 0` check, and its Linux
/// arm had no `/proc/<pid>/comm` fallback — so the two disagreed about the
/// name of the same process whenever the executable link was unreadable.
///
/// Callers may still layer their own fallback on top: `process_table` reaches
/// for the kernel's short name when this returns `None`, which is what covers
/// a process whose path this cannot read at all.
#[cfg(target_os = "macos")]
pub(super) fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// See the macOS arm above.
///
/// `/proc/<pid>/exe` is a link the kernel refuses to resolve for a process
/// owned by someone else, so the `comm` fallback is what keeps a differently
/// owned process from coming back nameless.
#[cfg(target_os = "linux")]
pub(super) fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name = name.strip_suffix(" (deleted)").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
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

/// Windows has no `lsof`, and the Ports section was simply never drawn there —
/// the daemon answered `QueryProcs` with an empty list no matter what the pane
/// was running, so a `npm run dev` in a Windows pane showed processes and no
/// port to click.
///
/// `GetExtendedTcpTable` is the same answer without a subprocess: the kernel's
/// own table of listening sockets, each already tagged with the pid that owns
/// it. The table is machine-wide, so the filter against the pane's tree below
/// is the whole difference between this panel and `netstat -ano`.
///
/// **Cost.** Two calls per poll — one per address family — into a buffer sized
/// for far more listeners than a real machine has; a family only pays for a
/// second call when its table outgrew that. The Info tab re-polls every two
/// seconds while it is open, so this is a fixed handful of microseconds, with
/// no process spawn and nothing allocated per pid.
#[cfg(windows)]
fn listening_ports(procs: &[ProcEntry]) -> Vec<PortEntry> {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::NetworkManagement::IpHelper::{
        MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_TCPTABLE_OWNER_PID,
    };

    if procs.is_empty() {
        return Vec::new();
    }
    let by_pid: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    let mut ports: Vec<PortEntry> = Vec::new();

    let v4 = tcp_table(AF_INET);
    // SAFETY: `tcp_table` hands back either an empty buffer or one the kernel
    // filled with a `MIB_TCPTABLE_OWNER_PID`; the `Vec<u32>` gives it the 4-byte
    // alignment every field of that struct wants, and `rows` is clamped to what
    // the buffer can actually hold before anything is read out of it.
    unsafe {
        if let Some((rows, count)) = table_rows::<MIB_TCPTABLE_OWNER_PID, MIB_TCPROW_OWNER_PID>(&v4)
        {
            for i in 0..count {
                let row = &*rows.add(i);
                // Filter before spelling the address: the table is the whole
                // machine's, and formatting a string for every stranger's
                // socket is the one avoidable allocation on this path.
                let Some(name) = by_pid.get(&row.dwOwningPid) else {
                    continue;
                };
                let addr = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
                record_listener(
                    &mut ports,
                    name,
                    local_port(row.dwLocalPort),
                    row.dwOwningPid,
                    spell_v4(addr),
                );
            }
        }
    }

    let v6 = tcp_table(AF_INET6);
    // SAFETY: as above, for the IPv6 shape of the same table.
    unsafe {
        if let Some((rows, count)) =
            table_rows::<MIB_TCP6TABLE_OWNER_PID, MIB_TCP6ROW_OWNER_PID>(&v6)
        {
            for i in 0..count {
                let row = &*rows.add(i);
                let Some(name) = by_pid.get(&row.dwOwningPid) else {
                    continue;
                };
                let addr = Ipv6Addr::from(row.ucLocalAddr);
                record_listener(
                    &mut ports,
                    name,
                    local_port(row.dwLocalPort),
                    row.dwOwningPid,
                    spell_v6(addr),
                );
            }
        }
    }

    ports.sort_by_key(|e| (e.port, e.pid));
    ports
}

/// The two Winsock address families, named here rather than by switching on
/// `windows-sys`'s `Win32_Networking_WinSock`: the whole socket module is a
/// long compile for two integers the ABI froze decades ago.
#[cfg(windows)]
const AF_INET: u32 = 2;
#[cfg(windows)]
const AF_INET6: u32 = 23;

/// One `GetExtendedTcpTable` snapshot of the listening sockets in `family`, as
/// the raw buffer the kernel filled, or an empty buffer if it would not answer.
///
/// Failure is soft, the way an absent `lsof` is soft on unix: the panel shows no
/// ports rather than an error. A machine with IPv6 disabled takes that path for
/// `AF_INET6` alone and still gets its IPv4 ports.
#[cfg(windows)]
fn tcp_table(family: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER,
    };

    // 8 KiB: room for ~340 IPv4 or ~146 IPv6 listeners, where a busy desktop has
    // a few dozen. Sizing it up front is what keeps the common poll to one call
    // per family instead of the usual size-then-fetch pair.
    let mut buf = vec![0u32; 2048];
    // Two rounds, not a loop until it fits: the table can keep growing between
    // calls, and this runs on a 2 s timer where giving up costs one poll.
    for _ in 0..2 {
        let mut size = (buf.len() * std::mem::size_of::<u32>()) as u32;
        // SAFETY: `buf` is at least `size` bytes, 4-aligned, and writable; the
        // kernel writes no more than `size` and reports what it needed instead.
        let rc = unsafe {
            GetExtendedTcpTable(
                buf.as_mut_ptr().cast(),
                &mut size,
                // No kernel-side sort: the rows are ordered by (port, pid) below
                // anyway, and this one is over the address, not the port.
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match rc {
            NO_ERROR => return buf,
            ERROR_INSUFFICIENT_BUFFER => {
                buf = vec![0u32; (size as usize).div_ceil(std::mem::size_of::<u32>()) + 64]
            }
            _ => break,
        }
    }
    Vec::new()
}

/// Where the rows of a `MIB_*TABLE_OWNER_PID` start in `buf`, and how many of
/// them the buffer can be trusted for.
///
/// The count is `min`'d against the buffer's own capacity rather than taken from
/// `dwNumEntries` alone: that field is the kernel's, but the read that follows
/// it is ours, and a table shorter than its header claims must not walk off the
/// end of the allocation.
///
/// # Safety
///
/// `buf` must be empty or hold a `Table` the kernel filled.
#[cfg(windows)]
unsafe fn table_rows<Table, Row>(buf: &[u32]) -> Option<(*const Row, usize)> {
    let bytes = std::mem::size_of_val(buf);
    if bytes < std::mem::size_of::<Table>() {
        return None;
    }
    let table = buf.as_ptr().cast::<Table>();
    // Every `MIB_*TABLE_OWNER_PID` is `{ dwNumEntries: u32, table: [Row; 1] }`,
    // so the count is the first word and the rows begin where the padding ends.
    let count = buf[0] as usize;
    let offset = std::mem::size_of::<Table>() - std::mem::size_of::<Row>();
    let capacity = (bytes - offset) / std::mem::size_of::<Row>();
    let rows = unsafe { table.cast::<u8>().add(offset) }.cast::<Row>();
    Some((rows, count.min(capacity)))
}

/// `dwLocalPort` carries the port in *network* byte order in its low 16 bits.
/// Reading it as a plain number is the classic way to end up showing 41247 for
/// a server on 8099.
#[cfg(windows)]
fn local_port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// The wildcard binds are spelled `*`, exactly as `lsof -n` spells them on the
/// other platforms, so the same server reads the same in the panel wherever it
/// runs — and so `PortEntry::authority` resolves it to `localhost`.
#[cfg(windows)]
fn spell_v4(addr: std::net::Ipv4Addr) -> String {
    match addr.is_unspecified() {
        true => "*".to_string(),
        false => addr.to_string(),
    }
}

/// See `spell_v4`. A specific IPv6 address keeps `lsof`'s brackets, which is
/// what makes `[::1]:5173` a pastable authority.
#[cfg(windows)]
fn spell_v6(addr: std::net::Ipv6Addr) -> String {
    match addr.is_unspecified() {
        true => "*".to_string(),
        false => format!("[{addr}]"),
    }
}

/// Adds one listening socket to the list, merging it with a row already there
/// for the same port and pid.
///
/// The merge rule is the unix path's, for the same reason: a process bound to
/// both `192.168.1.5` and `*` is on localhost, and the row the panel turns into
/// a clickable URL should say so rather than whichever address the kernel
/// happened to list first.
#[cfg(windows)]
fn record_listener(ports: &mut Vec<PortEntry>, name: &str, port: u16, pid: u32, addr: String) {
    if let Some(seen) = ports.iter_mut().find(|e| e.port == port && e.pid == pid) {
        if !PortEntry::reaches_loopback(&seen.addr) && PortEntry::reaches_loopback(&addr) {
            seen.addr = addr;
        }
        return;
    }
    ports.push(PortEntry {
        port,
        pid,
        addr,
        name: name.to_string(),
    });
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(all(test, windows))]
mod windows_tests {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use super::*;

    /// The port lives in the low half of a DWORD in *network* order. Getting
    /// this wrong does not fail loudly — it yields a plausible port number for
    /// a socket nobody is listening on.
    #[test]
    fn a_local_port_is_read_out_of_network_order() {
        // 8099 == 0x1FA3, so the wire spells it 0xA31F.
        assert_eq!(local_port(0x0000_A31F), 8099);
        assert_eq!(local_port(0xBB01), 443);
        assert_eq!(local_port(0x5000), 80);
    }

    /// The panel renders one server the same way on every platform, so a
    /// Windows wildcard bind has to arrive spelled the way `lsof -n` spells it.
    #[test]
    fn addresses_are_spelled_the_way_lsof_spells_them() {
        assert_eq!(spell_v4("0.0.0.0".parse().unwrap()), "*");
        assert_eq!(spell_v4("127.0.0.1".parse().unwrap()), "127.0.0.1");
        assert_eq!(spell_v4("192.168.1.20".parse().unwrap()), "192.168.1.20");
        assert_eq!(spell_v6("::".parse().unwrap()), "*");
        assert_eq!(spell_v6("::1".parse().unwrap()), "[::1]");
    }

    /// A dual-stack server shows up twice in the kernel's tables, once per
    /// family, and is one row in the panel — the reachable one.
    #[test]
    fn a_dual_stack_listener_collapses_to_its_reachable_address() {
        let mut ports = Vec::new();
        record_listener(&mut ports, "node.exe", 3000, 42, "192.168.1.5".into());
        record_listener(&mut ports, "node.exe", 3000, 42, "*".into());
        assert_eq!(ports.len(), 1, "one port, not one per address family");
        assert_eq!(ports[0].addr, "*", "the loopback-reachable bind wins");

        // ...and never the other way round: a wildcard already recorded is not
        // downgraded to an interface nobody can reach on localhost.
        let mut ports = Vec::new();
        record_listener(&mut ports, "node.exe", 3000, 42, "[::]".into());
        record_listener(&mut ports, "node.exe", 3000, 42, "192.168.1.5".into());
        assert_eq!(ports[0].addr, "[::]");

        // Two processes on the same port number are two rows.
        record_listener(&mut ports, "python.exe", 3000, 43, "127.0.0.1".into());
        assert_eq!(ports.len(), 2);
    }

    /// The whole feature, against the live kernel table: a real
    /// `cmd.exe -> powershell.exe` chain holding a real socket.
    ///
    /// Both halves matter. The port has to show up with the right number and
    /// the right owner — that is the half that was missing entirely, since
    /// `listening_ports` was `#[cfg(unix)]` and Windows got an empty list. And
    /// a socket held *outside* the tree must not show up, because
    /// `GetExtendedTcpTable` answers for the whole machine: without the pid
    /// filter this panel would list every port on the box and still pass the
    /// first assertion.
    #[test]
    fn listening_ports_finds_the_pane_tree_s_socket_and_only_its_tree_s() {
        // The out-of-tree listener. It belongs to the test process, which is
        // the parent of the chain and so is never inside the tree rooted at it.
        let outsider = TcpListener::bind("127.0.0.1:0").expect("bind an out-of-tree listener");
        let outside_port = outsider.local_addr().expect("read its port").port();

        let stamp = format!("tty7-ports-{}", std::process::id());
        let dir = std::env::temp_dir();
        let script = dir.join(format!("{stamp}.ps1"));
        let port_file = dir.join(format!("{stamp}.port"));
        let _ = std::fs::remove_file(&port_file);
        // The port comes back through a file rather than a pipe: PowerShell
        // buffers redirected stdout, and a test that waits on a flush that
        // never comes is a test that hangs.
        let mut f = std::fs::File::create(&script).expect("write the listener script");
        write!(
            f,
            "$l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)\r\n\
             $l.Start()\r\n\
             [System.IO.File]::WriteAllText('{}', [string]$l.LocalEndpoint.Port)\r\n\
             while ($true) {{ Start-Sleep -Seconds 1 }}\r\n",
            port_file.display().to_string().replace('\'', "''")
        )
        .expect("write the listener script");
        drop(f);

        // `cmd.exe` in front of PowerShell is what makes this a *tree* and not
        // one child: the listener sits at depth 1, reached only by walking.
        let mut child = std::process::Command::new("cmd.exe")
            .args([
                "/c",
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the cmd -> powershell chain");
        let root = child.id();

        let cleanup = |child: &mut std::process::Child| {
            let doomed = crate::daemon::winproc::descendants(
                &crate::daemon::winproc::snapshot(),
                child.id(),
            );
            let _ = child.kill();
            let _ = child.wait();
            crate::daemon::winproc::terminate_and_wait_all(
                &doomed,
                Instant::now() + Duration::from_secs(5),
            );
        };

        // PowerShell's startup is measured in seconds on a cold machine, and
        // `WriteAllText` can be observed mid-write, so parse until it parses.
        let deadline = Instant::now() + Duration::from_secs(60);
        let inside_port = loop {
            if let Some(port) = std::fs::read_to_string(&port_file).ok().and_then(|text| {
                text.trim()
                    .trim_start_matches('\u{feff}')
                    .parse::<u16>()
                    .ok()
            }) {
                break port;
            }
            if Instant::now() >= deadline {
                cleanup(&mut child);
                let _ = std::fs::remove_file(&script);
                panic!("the in-tree listener never reported its port");
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let got = snapshot(root, None);
        cleanup(&mut child);
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&port_file);
        drop(outsider);

        assert!(
            got.procs.iter().any(|p| p.depth > 0),
            "the chain must be walked past its root: {:?}",
            got.procs
        );
        let found = got
            .ports
            .iter()
            .find(|e| e.port == inside_port)
            .unwrap_or_else(|| {
                panic!("port {inside_port} is missing from {:?}", got.ports);
            });
        assert_eq!(
            found.addr, "127.0.0.1",
            "a loopback bind keeps its address, as it does under lsof"
        );
        assert!(
            got.procs.iter().any(|p| p.pid == found.pid),
            "the port's owner must be one of the pane's own processes"
        );
        assert!(
            found.name.eq_ignore_ascii_case("powershell.exe"),
            "the row names the process holding the socket, got {:?}",
            found.name
        );
        assert!(
            !got.ports.iter().any(|e| e.port == outside_port),
            "port {outside_port} is held outside the tree and must not be listed: {:?}",
            got.ports
        );
        assert!(
            got.ports.windows(2).all(|w| w[0].port <= w[1].port),
            "rows arrive ordered by port, as they do on unix"
        );
    }
}
