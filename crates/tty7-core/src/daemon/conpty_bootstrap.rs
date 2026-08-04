//! Initializes Win32 console colors before the real pane command starts.
//!
//! ConPTY consumes OSC default-color queries and its host-side pipe handles
//! are not console screen-buffer handles. The first process *inside* the
//! pseudoconsole can use the console API, though, so Windows pane commands are
//! prefixed with this executable's private bootstrap mode. It updates the
//! palette and then runs the original argv with inherited stdio.

#![cfg(windows)]

use std::ffi::{OsStr, OsString};

use portable_pty::CommandBuilder;

use crate::core::machine::Appearance;

const BOOTSTRAP_ARG: &str = "--tty7-conpty-bootstrap";
const PALETTE_ENV: &str = "TTY7_CONPTY_PALETTE";
const PID_CHANNEL_ENV: &str = "TTY7_CONPTY_PID_CHANNEL";

pub(crate) struct BootstrapReceiver(std::net::TcpListener);

impl BootstrapReceiver {
    pub(crate) fn receive(self, bootstrap_pid: u32) -> Option<u32> {
        use std::io::Read as _;

        self.0.set_nonblocking(true).ok()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut stream = loop {
            match self.0.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .ok()?;
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).ok()?;
        let shell_pid = u32::from_le_bytes(bytes);
        crate::daemon::winproc::snapshot()
            .iter()
            .any(|p| p.pid == shell_pid && p.parent == bootstrap_pid)
            .then_some(shell_pid)
    }
}

/// Prefix a pane command with the current tty7 executable's private bootstrap.
pub(crate) fn wrap_command(
    cmd: &mut CommandBuilder,
    appearance: Appearance,
) -> anyhow::Result<Option<BootstrapReceiver>> {
    let Some(mut colors) = appearance.ansi16 else {
        return Ok(None);
    };
    let (foreground_index, background_index) = if appearance.dark { (15, 0) } else { (0, 15) };
    if let Some(foreground) = appearance.foreground {
        colors[foreground_index] = foreground;
    }
    if let Some(background) = appearance.background {
        colors[background_index] = background;
    }
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let exe = std::env::current_exe()?;
    cmd.get_argv_mut()
        .splice(0..0, [exe.into_os_string(), OsString::from(BOOTSTRAP_ARG)]);
    cmd.env(PALETTE_ENV, encode_palette(appearance.dark, colors));
    cmd.env(PID_CHANNEL_ENV, address.to_string());
    Ok(Some(BootstrapReceiver(listener)))
}

/// Run the private bootstrap mode, returning `None` for every normal launch.
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(BOOTSTRAP_ARG)) {
        return None;
    }
    let Some(program) = args.next() else {
        return Some(1);
    };

    if let Some((dark, colors)) = std::env::var(PALETTE_ENV)
        .ok()
        .as_deref()
        .and_then(decode_palette)
        && let Err(e) = set_console_palette(dark, colors)
    {
        eprintln!("tty7: could not initialize the ConPTY palette: {e}");
    }

    match std::process::Command::new(program)
        .args(args)
        .env_remove(PALETTE_ENV)
        .env_remove(PID_CHANNEL_ENV)
        .spawn()
    {
        Ok(mut child) => {
            if let Ok(address) = std::env::var(PID_CHANNEL_ENV)
                && let Ok(mut stream) = std::net::TcpStream::connect(address)
            {
                use std::io::Write as _;
                let _ = stream.write_all(&child.id().to_le_bytes());
            }
            Some(
                child
                    .wait()
                    .ok()
                    .and_then(|status| status.code())
                    .unwrap_or(1),
            )
        }
        Err(e) => {
            eprintln!("tty7: could not start pane command: {e}");
            Some(1)
        }
    }
}

fn encode_palette(dark: bool, colors: [u32; 16]) -> String {
    let mut encoded = String::with_capacity(1 + 16 * 7);
    encoded.push(if dark { '1' } else { '0' });
    for color in colors {
        use std::fmt::Write as _;
        let _ = write!(encoded, ";{:06x}", color & 0x00ff_ffff);
    }
    encoded
}

fn decode_palette(encoded: &str) -> Option<(bool, [u32; 16])> {
    let mut fields = encoded.split(';');
    let dark = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let mut colors = [0; 16];
    for color in &mut colors {
        *color = u32::from_str_radix(fields.next()?, 16).ok()?;
        if *color > 0x00ff_ffff {
            return None;
        }
    }
    if fields.next().is_some() {
        return None;
    }
    Some((dark, colors))
}

fn set_console_palette(dark: bool, colors: [u32; 16]) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, TRUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFOEX, GetConsoleScreenBufferInfoEx, GetStdHandle,
        STD_OUTPUT_HANDLE, SetConsoleScreenBufferInfoEx,
    };

    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut info: CONSOLE_SCREEN_BUFFER_INFOEX = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32;
    if unsafe { GetConsoleScreenBufferInfoEx(handle, &mut info) } != TRUE {
        return Err(std::io::Error::last_os_error());
    }

    info.ColorTable = colors.map(rgb_to_colorref);
    // Match the COLORFGBG convention already advertised to pane children:
    // dark themes use bright-white-on-black, light themes black-on-bright-white.
    // The actual entries now come from the active tty7 theme rather than the
    // process-wide conhost defaults.
    info.wAttributes = (info.wAttributes & !0x00ff) | if dark { 0x000f } else { 0x00f0 };

    // SetConsoleScreenBufferInfoEx historically interprets the inclusive
    // srWindow bottom as exclusive. This is the same compensation used by
    // Microsoft's ColorTool and prevents the visible viewport shrinking.
    info.srWindow.Bottom = info.srWindow.Bottom.saturating_add(1);
    if unsafe { SetConsoleScreenBufferInfoEx(handle, &info) } != TRUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_encoding_round_trips() {
        let colors = std::array::from_fn(|i| (i as u32 * 0x10203) & 0x00ff_ffff);
        assert_eq!(
            decode_palette(&encode_palette(true, colors)),
            Some((true, colors))
        );
        assert_eq!(decode_palette("1;000000"), None);
        assert_eq!(decode_palette("x;000000"), None);
    }

    #[test]
    fn colorref_uses_win32_bgr_byte_order() {
        assert_eq!(rgb_to_colorref(0x123456), 0x563412);
    }

    #[test]
    fn wrapping_preserves_the_original_command() {
        let mut command = CommandBuilder::new("pwsh.exe");
        command.args(["-NoLogo", "-NoProfile"]);
        command.cwd(r"C:\work");
        command.env("TTY7_TEST_VALUE", "kept");
        let appearance = Appearance {
            dark: true,
            ansi16: Some([0x123456; 16]),
            foreground: Some(0xfefefe),
            background: Some(0x010101),
        };

        let receiver = wrap_command(&mut command, appearance)
            .expect("prepare bootstrap")
            .expect("palette needs a bootstrap");
        drop(receiver);
        let argv = command.get_argv();
        assert_eq!(argv[0], std::env::current_exe().unwrap());
        assert_eq!(argv[1], OsStr::new(BOOTSTRAP_ARG));
        assert_eq!(&argv[2..], ["pwsh.exe", "-NoLogo", "-NoProfile"]);
        assert_eq!(
            command.get_cwd().map(OsString::as_os_str),
            Some(OsStr::new(r"C:\work"))
        );
        assert_eq!(command.get_env("TTY7_TEST_VALUE"), Some(OsStr::new("kept")));
        let (_, colors) = decode_palette(command.get_env(PALETTE_ENV).unwrap().to_str().unwrap())
            .expect("decode wrapped palette");
        assert_eq!(colors[0], 0x010101);
        assert_eq!(colors[15], 0xfefefe);
    }

    #[test]
    fn pid_channel_reports_only_the_bootstraps_direct_child() {
        const PHASE: &str = "TTY7_CONPTY_PID_TEST_PHASE";
        const ADDRESS: &str = "TTY7_CONPTY_PID_TEST_ADDRESS";
        const TEST_NAME: &str =
            "daemon::conpty_bootstrap::tests::pid_channel_reports_only_the_bootstraps_direct_child";

        if std::env::var(PHASE).as_deref() == Ok("report") {
            use std::io::Write as _;
            let mut stream = std::net::TcpStream::connect(std::env::var(ADDRESS).unwrap()).unwrap();
            stream.write_all(&std::process::id().to_le_bytes()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            return;
        }

        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(PHASE, "report")
            .env(ADDRESS, address.to_string())
            .spawn()
            .unwrap();
        let reported = BootstrapReceiver(listener)
            .receive(std::process::id())
            .expect("accept the direct child's pid");
        assert_eq!(reported, child.id());
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn console_palette_is_inherited_by_the_real_pane_process() {
        const PHASE: &str = "TTY7_CONPTY_PALETTE_TEST_PHASE";
        const TEST_NAME: &str = "daemon::conpty_bootstrap::tests::console_palette_is_inherited_by_the_real_pane_process";
        let colors = std::array::from_fn(|i| 0x102030 + i as u32 * 0x030201);

        match std::env::var(PHASE).as_deref() {
            Ok("verify") => {
                let info = console_info().expect("the real pane process has a console buffer");
                assert_eq!(info.ColorTable, colors.map(rgb_to_colorref));
                assert_eq!(info.wAttributes & 0x00ff, 0x00f0);
                return;
            }
            Ok("initialize") => {
                set_console_palette(false, colors).expect("initialize the ConPTY color table");
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST_NAME, "--nocapture"])
                    .env(PHASE, "verify")
                    .status()
                    .expect("spawn the real pane process");
                assert!(status.success());
                return;
            }
            _ => {}
        }

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a ConPTY");
        let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
        command.args(["--exact", TEST_NAME, "--nocapture"]);
        command.env(PHASE, "initialize");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn the palette bootstrap inside ConPTY");
        drop(pair.slave);
        let status = child.wait().expect("wait for the palette probe");
        assert_eq!(status.exit_code(), 0);
    }

    fn console_info()
    -> std::io::Result<windows_sys::Win32::System::Console::CONSOLE_SCREEN_BUFFER_INFOEX> {
        use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, TRUE};
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFOEX, GetConsoleScreenBufferInfoEx, GetStdHandle,
            STD_OUTPUT_HANDLE,
        };

        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFOEX = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32;
        if unsafe { GetConsoleScreenBufferInfoEx(handle, &mut info) } != TRUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info)
    }
}

fn rgb_to_colorref(rgb: u32) -> u32 {
    ((rgb & 0x0000_00ff) << 16) | (rgb & 0x0000_ff00) | ((rgb & 0x00ff_0000) >> 16)
}
