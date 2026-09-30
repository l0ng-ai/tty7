//! The commands the installer sends to a remote Windows host.
//!
//! Windows OpenSSH runs an exec request through whatever `DefaultShell` the
//! machine is configured with — `cmd.exe` out of the box, PowerShell or even a
//! Git-for-Windows `bash` on machines that changed it. None of the POSIX
//! scripts in [`super`] survive any of those, and no single quoting scheme
//! survives all three. So every command here is a PowerShell script shipped as
//! `-EncodedCommand`: base64 of UTF-16LE, which is nothing but letters, digits,
//! `+`, `/` and `=` — words every one of those shells passes through untouched.
//! Windows PowerShell 5.1 ships with every supported Windows, so there is
//! nothing to install first.
//!
//! Each script opens with a `# tty7:<tag>` comment. It costs nothing on the far
//! end, and it is how a log line — or a test's fake host — can tell which
//! script an opaque blob of base64 is.
//!
//! Paths arrive in the SFTP spelling the installer keeps (`/C:/Users/me/…`) and
//! are turned into native ones (`C:\Users\me\…`) here, at the last moment.

use base64::Engine as _;

use super::asset;

/// Starts the line [`probe_command`] prints, so nothing a profile or banner
/// prints can pass for the answer.
pub(crate) const PROBE_MARK: &str = "__tty7_windows__";

const TAG_PREFIX: &str = "# tty7:";

/// Quiet progress bars (Windows PowerShell serialises them to stderr as CLIXML
/// when output is redirected) and answer in UTF-8, so a profile directory with
/// a non-ASCII user name survives the trip.
const PREAMBLE: &str = "$ProgressPreference='SilentlyContinue'\n\
     try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}\n";

/// `powershell.exe … -EncodedCommand <base64>` for `body`, tagged `tag`.
pub(crate) fn powershell(tag: &str, body: &str) -> String {
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        encode(&script(tag, body))
    )
}

fn script(tag: &str, body: &str) -> String {
    format!("{TAG_PREFIX}{tag}\n{PREAMBLE}{body}")
}

/// What `-EncodedCommand` takes: base64 over UTF-16LE.
fn encode(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// The tag and script behind a command [`powershell`] built, or `None` for
/// anything else. The inverse the tests and the logs read commands through.
pub(crate) fn decode(command: &str) -> Option<(String, String)> {
    let b64 = command
        .strip_prefix("powershell.exe ")?
        .rsplit_once("-EncodedCommand ")?
        .1
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let script = String::from_utf16(&units).ok()?;
    let tag = script.lines().next()?.strip_prefix(TAG_PREFIX)?.to_string();
    Some((tag, script))
}

/// How to name `command` in a message: an encoded script by its tag, since a
/// few kilobytes of base64 tell a reader nothing; anything else as itself.
pub(crate) fn label(command: &str) -> std::borrow::Cow<'_, str> {
    match decode(command) {
        Some((tag, _)) => format!("powershell `tty7:{tag}` script").into(),
        None => command.into(),
    }
}

/// A PowerShell single-quoted literal. PowerShell treats the typographic
/// single quotes as quote characters too, so each of them is doubled along
/// with the ASCII one — a path is data, and must never end the literal.
pub(crate) fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// The native spelling of an installer path, quoted for PowerShell.
fn native(sftp: &str) -> String {
    ps_quote(&asset::windows_native_path(sftp).unwrap_or_else(|| sftp.to_string()))
}

/// Asks the machine what it is. Printed rather than returned through the exit
/// status: a machine that is not Windows at all fails to run `powershell.exe`,
/// and that failure is the answer too.
///
/// `PROCESSOR_ARCHITEW6432` first: a 32-bit shell on a 64-bit machine reports
/// `x86` in `PROCESSOR_ARCHITECTURE`, and the machine is what the server has to
/// run on.
pub(crate) fn probe_command() -> String {
    powershell(
        "probe",
        &format!(
            "$a = $env:PROCESSOR_ARCHITEW6432\n\
             if (-not $a) {{ $a = $env:PROCESSOR_ARCHITECTURE }}\n\
             [Console]::Out.Write('{PROBE_MARK} ' + $a + \"`n\")\n"
        ),
    )
}

/// The architecture [`probe_command`] reported, or `None` when nothing
/// answered in its words.
pub(crate) fn parse_probe(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix(PROBE_MARK)
            .map(|arch| arch.trim().to_string())
    })
}

/// `<exe> --protocol`, exiting with its status.
pub(crate) fn protocol_command(exe: &str) -> String {
    powershell(
        "protocol",
        &format!(
            "& {} {}\nexit $LASTEXITCODE\n",
            native(exe),
            super::PROTOCOL_FLAG
        ),
    )
}

/// `<binary> --stdio --bridge` with stdin closed — the Windows spelling of the
/// unix probe's `< /dev/null`. Started through .NET rather than PowerShell's
/// own native-command plumbing, which is the one way to be sure the child
/// sees end-of-file on stdin rather than PowerShell's console.
pub(crate) fn control_probe_command(binary: &str) -> String {
    powershell(
        "control-probe",
        &format!(
            "$p = New-Object System.Diagnostics.Process\n\
             $p.StartInfo.FileName = {}\n\
             $p.StartInfo.Arguments = '--stdio --bridge'\n\
             $p.StartInfo.UseShellExecute = $false\n\
             $p.StartInfo.RedirectStandardInput = $true\n\
             $p.StartInfo.RedirectStandardOutput = $true\n\
             $p.StartInfo.RedirectStandardError = $true\n\
             try {{ [void]$p.Start() }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 127 }}\n\
             $p.StandardInput.Close()\n\
             $err = $p.StandardError.ReadToEndAsync()\n\
             [void]$p.StandardOutput.ReadToEnd()\n\
             $p.WaitForExit()\n\
             [Console]::Error.Write($err.Result)\n\
             exit $p.ExitCode\n",
            native(binary)
        ),
    )
}

/// The image of the tty7 daemon this account is running, native-spelled, or
/// nothing.
///
/// Only a `--daemon`: the `--stdio` bridges every connected client keeps
/// running are `tty7-server-*` processes too, and one of them is not the
/// server. Only this account's: an administrator can see every user's
/// processes, and another user's daemon is not the one this client talks to.
pub(crate) fn running_exe_command() -> String {
    powershell(
        "running-exe",
        "$me = $env:USERNAME\n\
         Get-CimInstance Win32_Process -Filter \"Name LIKE 'tty7-server-%'\" -ErrorAction SilentlyContinue |\n\
           Where-Object { $_.ExecutablePath -and $_.CommandLine -like '*--daemon*' } |\n\
           Where-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner -ErrorAction SilentlyContinue).User -eq $me } |\n\
           Select-Object -First 1 |\n\
           ForEach-Object { [Console]::Out.Write($_.ExecutablePath) }\n\
         exit 0\n",
    )
}

/// Ask the running daemon to stop, through `binary --stop`.
///
/// Windows has no SIGTERM to send, and `Stop-Process` is `TerminateProcess`:
/// the daemon would die without closing its ConPTYs and leave every pane's
/// shell orphaned. `--stop` goes through the daemon's own endpoint instead —
/// the graceful shutdown a local tty7 uses on Windows, with its reap as the
/// fallback. Always exits 0, like the unix command ending in `true`: a daemon
/// that was not running is not a failure to stop it.
pub(crate) fn stop_command(binary: &str) -> String {
    powershell("stop", &format!("& {} --stop\nexit 0\n", native(binary)))
}

/// Start the daemon detached from this SSH session, keeping what it says and
/// how it ends — the Windows counterpart of the unix `setsid`/`nohup` launch.
///
/// Detached is the hard part. Windows OpenSSH puts everything a session starts
/// into a job object and closes it with the session, so a daemon started from
/// here — `Start-Process` included — dies the moment this command's channel
/// does. `Win32_Process.Create` asks the WMI service to start the process
/// instead, and the WMI service's children belong to no session's job.
///
/// WMI would start it with a bare environment, so this session's is handed
/// over explicitly. It is the environment the pane shells should inherit, and
/// it is where the daemon finds `%APPDATA%` — the config directory its
/// endpoint files live in, and which the `--stdio` bridges this session starts
/// will look in.
///
/// What WMI starts is a supervisor, not the daemon: a hidden PowerShell that
/// runs it through `cmd` (which is what redirects stdout and stderr into one
/// log file), waits, and records the exit status stamped with this launch's
/// nonce, exactly like the unix wrapper.
pub(crate) fn launch_command(binary: &str, log: &str, exit: &str, nonce: &str) -> String {
    let exe = asset::windows_native_path(binary).unwrap_or_else(|| binary.to_string());
    let log_native = asset::windows_native_path(log).unwrap_or_else(|| log.to_string());
    // `cmd /c ""<exe>" --daemon 1>>"<log>" 2>&1"`: with more than two quotes
    // after `/c`, cmd strips the first and the last and runs what is between
    // verbatim — the standard way to hand it a quoted program and a quoted
    // redirect target in one line.
    let with_log = format!("/d /c \"\"{exe}\" --daemon 1>>\"{log_native}\" 2>&1\"");
    let without_log = format!("/d /c \"\"{exe}\" --daemon 1>NUL 2>&1\"");
    let supervisor = script(
        "supervise",
        &format!(
            "$log = {log}\n\
             $exit = {exit}\n\
             Remove-Item -LiteralPath $log, $exit -Force -ErrorAction SilentlyContinue\n\
             $a = {without}\n\
             try {{ [IO.File]::WriteAllText($log, ''); [IO.File]::WriteAllText($exit, ''); $a = {with} }} catch {{}}\n\
             $p = Start-Process -FilePath $env:ComSpec -ArgumentList $a -WindowStyle Hidden -PassThru\n\
             $null = $p.Handle\n\
             $p.WaitForExit()\n\
             try {{ [IO.File]::WriteAllText($exit, {nonce} + ' ' + $p.ExitCode) }} catch {{}}\n",
            log = native(log),
            exit = native(exit),
            with = ps_quote(&with_log),
            without = ps_quote(&without_log),
            nonce = ps_quote(nonce),
        ),
    );
    // The supervisor is encoded on the far end rather than here: encoding it
    // twice would put it on the wire as base64 of base64 of UTF-16, and the
    // outer line has to fit cmd.exe's 8191 characters.
    powershell(
        "launch",
        &format!(
            "$s = {supervisor}\n\
             $inner = 'powershell.exe -NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($s))\n\
             $vars = [string[]](Get-ChildItem Env: | ForEach-Object {{ $_.Name + '=' + $_.Value }})\n\
             $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{{ ShowWindow = [uint16]0; EnvironmentVariables = $vars }}\n\
             $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = $inner; CurrentDirectory = $env:USERPROFILE; ProcessStartupInformation = $startup }}\n\
             if (-not $r -or $r.ReturnValue -ne 0) {{\n\
               [Console]::Error.WriteLine('Win32_Process.Create could not start the daemon (' + $r.ReturnValue + ')')\n\
               exit 1\n\
             }}\n\
             exit 0\n",
            supervisor = ps_quote(&supervisor),
        ),
    )
}

/// The recorded exit status, or nothing. Read shared, since the supervisor may
/// still hold it.
pub(crate) fn read_exit_command(exit: &str) -> String {
    powershell(
        "read-exit",
        &format!(
            "try {{\n\
               $f = [IO.File]::Open({}, 'Open', 'Read', 'ReadWrite')\n\
               $r = New-Object IO.StreamReader($f)\n\
               [Console]::Out.Write($r.ReadToEnd())\n\
               $r.Close()\n\
             }} catch {{}}\n\
             exit 0\n",
            native(exit)
        ),
    )
}

/// The last `bytes` of the startup log. Opened for shared reading: `cmd` still
/// holds it open for writing while the daemon runs, and a plain read would be
/// refused for exactly as long as the log is interesting.
pub(crate) fn tail_command(log: &str, bytes: usize) -> String {
    powershell(
        "tail",
        &format!(
            "try {{\n\
               $f = [IO.File]::Open({}, 'Open', 'Read', 'ReadWrite')\n\
               $n = [Math]::Min([long]{bytes}, $f.Length)\n\
               [void]$f.Seek(-$n, 'End')\n\
               $buf = New-Object byte[] $n\n\
               $got = $f.Read($buf, 0, $n)\n\
               $f.Close()\n\
               [Console]::Out.Write([Text.Encoding]::UTF8.GetString($buf, 0, $got))\n\
             }} catch {{}}\n\
             exit 0\n",
            native(log)
        ),
    )
}

/// The command a routed link runs to reach the server: `"<exe>" --stdio`.
///
/// Not PowerShell. The link carries a binary protocol on stdin and stdout, and
/// Windows PowerShell re-encodes a native command's output as text whenever its
/// own output is redirected — which, under sshd, it always is. `cmd.exe`, the
/// stock `DefaultShell`, hands the server the channel's pipes untouched. With
/// exactly two quotes on the line, cmd keeps them when the path has a space in
/// it and strips them harmlessly when it does not.
pub(crate) fn stdio_command(binary: &str) -> String {
    let exe = asset::windows_native_path(binary).unwrap_or_else(|| binary.to_string());
    format!("\"{exe}\" --stdio")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = "/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c3p4.exe";

    #[test]
    fn an_encoded_command_round_trips_and_carries_its_tag() {
        let cmd = powershell("probe", "Write-Output 'héllo ✓'");
        let (tag, script) = decode(&cmd).expect("our own encoding decodes");
        assert_eq!(tag, "probe");
        assert!(script.ends_with("Write-Output 'héllo ✓'"), "{script}");
        assert!(script.contains("OutputEncoding"), "{script}");
        assert_eq!(decode("uname -sm"), None);
    }

    /// The whole point of the encoding: whatever the far end's default shell
    /// is, nothing in the line means anything to it.
    #[test]
    fn an_encoded_command_is_inert_in_every_windows_shell() {
        for cmd in [
            probe_command(),
            protocol_command(BINARY),
            control_probe_command(BINARY),
            running_exe_command(),
            stop_command(BINARY),
            launch_command(
                BINARY,
                &format!("{BINARY}.startup.log"),
                &format!("{BINARY}.startup.exit"),
                "0123456789abcdef0123456789abcdef",
            ),
            read_exit_command(&format!("{BINARY}.startup.exit")),
            tail_command(&format!("{BINARY}.startup.log"), 4096),
        ] {
            let (_, b64) = cmd.rsplit_once(' ').unwrap();
            assert!(
                b64.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
                "{cmd}"
            );
            assert!(
                cmd.len() < 8000,
                "cmd.exe refuses lines over 8191 characters: {} for {:?}",
                cmd.len(),
                decode(&cmd).map(|(tag, _)| tag)
            );
        }
    }

    #[test]
    fn quoting_doubles_every_single_quote_powershell_knows() {
        assert_eq!(ps_quote(r"C:\Users\me"), r"'C:\Users\me'");
        assert_eq!(ps_quote("O'Brien"), "'O''Brien'");
        assert_eq!(ps_quote("O\u{2019}Brien"), "'O\u{2019}\u{2019}Brien'");
        assert_eq!(ps_quote("$env:x `n"), "'$env:x `n'");
    }

    #[test]
    fn commands_name_the_native_path() {
        let (tag, script) = decode(&protocol_command(BINARY)).unwrap();
        assert_eq!(tag, "protocol");
        assert!(
            script.contains(
                r"& 'C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe' --protocol"
            ),
            "{script}"
        );

        let (tag, script) = decode(&stop_command(BINARY)).unwrap();
        assert_eq!(tag, "stop");
        assert!(script.contains("tty7-server-c3p4.exe' --stop"), "{script}");

        let (tag, script) = decode(&control_probe_command(BINARY)).unwrap();
        assert_eq!(tag, "control-probe");
        assert!(script.contains("'--stdio --bridge'"), "{script}");
        assert!(script.contains("StandardInput.Close()"), "{script}");
    }

    #[test]
    fn the_probe_answer_is_found_among_banner_noise() {
        let out = "Windows PowerShell\r\nCopyright (C) Microsoft\r\n__tty7_windows__ AMD64\r\n";
        assert_eq!(parse_probe(out).as_deref(), Some("AMD64"));
        assert_eq!(parse_probe("__tty7_windows__ \n").as_deref(), Some(""));
        assert_eq!(
            parse_probe("'powershell.exe' is not recognized as an internal or external command"),
            None
        );
    }

    #[test]
    fn the_launch_goes_through_wmi_and_supervises_with_the_nonce() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let cmd = launch_command(
            BINARY,
            &format!("{BINARY}.startup.log"),
            &format!("{BINARY}.startup.exit"),
            nonce,
        );
        let (tag, outer) = decode(&cmd).unwrap();
        assert_eq!(tag, "launch");
        assert!(
            outer.contains("Win32_Process -MethodName Create"),
            "{outer}"
        );
        assert!(outer.contains("EnvironmentVariables"), "{outer}");

        // The supervisor rides inside as a quoted literal, its own quotes
        // doubled once more.
        assert!(outer.contains("# tty7:supervise"), "{outer}");
        assert!(outer.contains(nonce), "{outer}");
        assert!(
            outer.contains(
                r#"/d /c ""C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe" --daemon 1>>"C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe.startup.log" 2>&1""#
            ),
            "{outer}"
        );
        assert!(
            outer.contains("-WindowStyle Hidden -EncodedCommand"),
            "{outer}"
        );
    }

    #[test]
    fn the_link_command_is_a_plain_cmd_line() {
        assert_eq!(
            stdio_command(BINARY),
            r#""C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe" --stdio"#
        );
        assert_eq!(
            stdio_command("/C:/Users/John Smith/AppData/Local/tty7/bin/x.exe"),
            r#""C:\Users\John Smith\AppData\Local\tty7\bin\x.exe" --stdio"#
        );
    }
}
