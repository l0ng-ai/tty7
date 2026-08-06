; tty7 Windows installer (Inno Setup 6 — preinstalled on GitHub's
; windows-latest runners). Compiled by bundle-windows.ps1, which stages the
; payload and passes every path in via /D defines:
;
;   /DAppVersion=<semver>   display version parsed from Cargo.toml
;   /DVersionInfoVersion=<numeric version> PE-compatible file version
;   /DStageDir=<abs path>   staged payload (app, CLI, updater, marker, resources)
;   /DOutputDir=<abs path>  where the setup exe is written
;   /DOutputName=<basename> setup exe filename, without ".exe"
;
; Defaults to a per-user install ({localappdata}\Programs\tty7 — no UAC
; prompt), with an "install for all users" escape hatch in the dialog. The
; build is unsigned, so SmartScreen warns on first launch either way — same as
; the portable zip.

#ifndef AppVersion
  #error Missing /DAppVersion — this script is meant to be compiled via bundle-windows.ps1
#endif
#ifndef VersionInfoVersion
  #error Missing /DVersionInfoVersion — this script is meant to be compiled via bundle-windows.ps1
#endif

[Setup]
; Never change AppId: it is how Windows ties upgrades + the uninstall entry
; to previous installs of tty7.
AppId={{9A3F6C1E-4B7D-4E2A-8C5F-D01B92E64A37}
AppName=tty7
AppVersion={#AppVersion}
VersionInfoVersion={#VersionInfoVersion}
AppPublisher=tty7 contributors
AppPublisherURL=https://github.com/l0ng-ai/tty7
AppSupportURL=https://github.com/l0ng-ai/tty7/issues
AppUpdatesURL=https://github.com/l0ng-ai/tty7/releases
DefaultDirName={autopf}\tty7
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile={#StageDir}\LICENSE.txt
SetupIconFile=..\..\assets\favicon.ico
UninstallDisplayIcon={app}\tty7-app.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The persistent daemon (tty7-app.exe --daemon) is a detached background process
; that outlives the GUI and holds the running image of tty7-app.exe, so Windows
; locks the file and an upgrade can't replace it. We stop it explicitly in
; PrepareToInstall below; keep the Restart Manager as a backstop but don't let
; it relaunch anything (the GUI respawns the daemon itself on next start).
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
; Writing shell verbs is an install-time decision, the way VS Code and Git for
; Windows treat theirs — not a runtime preference, so tty7 has no setting for
; it. Off by default: the registry is user-visible system state. The keys land
; under HKCU even for an all-users install, so this only ever affects whoever
; ran the installer. Inno restores the previous choice when upgrading.
Name: "explorermenu"; Description: "Add ""Open in tty7"" to the folder context menu"; GroupDescription: "Shell integration:"; Flags: unchecked

; Builds before the tty7/tty7-app split installed the GUI as tty7.exe. Upgrading
; only *adds* tty7-app.exe, so the old binary would stay on disk — and a taskbar
; pin (which Inno cannot rewrite, unlike the [Icons] shortcuts) still points at
; it. The user would keep launching the previous version from their pinned icon,
; against the same daemon endpoint as the new one. Delete it on upgrade; a fresh
; install simply has nothing to remove.
;
; That name is now reused by the CLI below, which lands at the very same path.
; The order saves us: [InstallDelete] runs before [Files], so the stale GUI is
; gone before the CLI is written, and what survives is never a mix of the two.
; The taskbar pin is the loose end — post-upgrade it points at a CLI, so
; clicking it flashes a console instead of opening a window. That is louder
; than the bug it replaces (silently running last release's GUI), and the pin
; is not ours to rewrite; the Start Menu entry in [Icons] is correct either way.
[InstallDelete]
Type: files; Name: "{app}\tty7.exe"

[Files]
Source: "{#StageDir}\tty7-app.exe"; DestDir: "{app}"; Flags: ignoreversion
; The CLI. `core::cli_install` adds {app} to the user's PATH at first launch,
; so this is not registered as an [Env] change here — the portable zip has no
; installer to do it, and one code path serving both is one behaviour to debug.
; The uninstaller takes that entry back out; see RemoveAppDirFromUserPath below.
Source: "{#StageDir}\tty7.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\tty7-updater.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; This installer-only marker is the authority for enabling automatic Windows
; updates. The portable archive is created before the marker enters the stage.
Source: "{#StageDir}\.tty7-inno-install"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\completions\*"; DestDir: "{app}\completions"; Flags: ignoreversion recursesubdirs
; Microsoft's redistributable ConPTY. It has to land in {app} rather than a
; subdirectory: `portable-pty` finds it through the DLL search path, which
; starts at the directory of the executable that loads it (the daemon is
; {app}\tty7-app.exe). The two files are a matched pair — never update one
; alone. See assets\windows\conpty\README.md.
Source: "{#StageDir}\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE-ConPTY.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
; The Linux musl tty7-server, for serving WSL distros without a download.
; `skipifsourcedoesntexist` because a build whose server-musl leg was skipped
; still has to produce an installer. See bundle-windows.ps1.
Source: "{#StageDir}\server\*"; DestDir: "{app}\server"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist

; AppUserModelID is what lets toast notifications carry the tty7 name and icon
; instead of the notify-rust PowerShell fallback: Windows only honors an
; unpackaged app's toast identity when a shortcut stamps it. Must match
; `core::aumid::AUMID` (src/core/aumid.rs), which at startup stamps the
; per-user shortcut below if some older installer left it unstamped, and
; writes one from scratch for the portable zip. It deliberately leaves an
; all-users install alone — it cannot write {commonprograms} unelevated, and a
; per-user twin would both duplicate the Start Menu entry and outlive this
; uninstaller — so an elevated install depends on the stamp right here.
[Icons]
Name: "{autoprograms}\tty7"; Filename: "{app}\tty7-app.exe"; AppUserModelID: "com.github.tty7"
Name: "{autodesktop}\tty7"; Filename: "{app}\tty7-app.exe"; Tasks: desktopicon; AppUserModelID: "com.github.tty7"

[Run]
; The registry shape lives in core::explorer_context_menu, not here: the app
; reads those same keys to decide whether an existing registration still points
; at this install, and two hand-kept copies of the layout would drift. Runs
; before the launch entry below so a first start already sees the final state.
Filename: "{app}\tty7-app.exe"; Parameters: "--register-explorer-menu"; Tasks: explorermenu; Flags: runhidden waituntilterminated
Filename: "{app}\tty7-app.exe"; Description: "{cm:LaunchProgram,tty7}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the daemon before the uninstaller deletes tty7-app.exe — the running daemon
; is the locked image of that file, so removing it fails otherwise. This runs at
; the start of uninstallation, before any files are removed. The installed binary
; is this version, which understands the flag; runhidden suppresses any flash and
; the call returns without opening a window. RunOnceId keys the entry so a repeated
; uninstall doesn't run it twice.
Filename: "{app}\tty7-app.exe"; Parameters: "--stop-daemon"; Flags: runhidden waituntilterminated; RunOnceId: "StopDaemon"
; Unconditional, and deliberately not gated on the task: an install that had the
; menu registered and was later upgraded without the box ticked still holds the
; keys, and verbs pointing at a deleted exe are worse than a no-op. Removing keys
; that were never written succeeds silently.
Filename: "{app}\tty7-app.exe"; Parameters: "--unregister-explorer-menu"; Flags: runhidden waituntilterminated; RunOnceId: "UnregisterExplorerMenu"

[Code]
(* Gracefully stop the persistent daemon before we overwrite tty7-app.exe. We can't
  run the *installed* binary here — on an upgrade from an older build it may not
  understand --stop-daemon and would launch the GUI instead — so we extract the
  *new* tty7-app.exe to {tmp} and run that. It connects to the running daemon, hangs
  up every shell, waits for it to exit (releasing the file lock), then returns
  without opening a window. Best effort: any failure falls through to the Restart
  Manager backstop, and a fresh install simply has no daemon to stop. *)
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  ExtractTemporaryFile('tty7-app.exe');
  Exec(ExpandConstant('{tmp}\tty7-app.exe'), '--stop-daemon', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;

(* Take {app} back out of the user's PATH.

  `core::cli_install` puts it there at first launch rather than the installer
  doing it, because the portable zip has no installer — but that leaves nobody
  to undo it, and an uninstall that permanently grows the user's PATH by one
  dead entry is not an uninstall. So the removal lives here, on the one install
  shape that has an uninstaller at all. (Unix has no equivalent hook: deleting
  the .app or the tarball leaves the symlink behind for the user to remove.)

  HKCU even for an all-users install: cli_install only ever writes the user
  hive, so that is the only place an entry can be. On a machine where several
  users ran tty7, this clears the one uninstalling — the others keep a dead
  entry, which is the price of not needing elevation to install in the first
  place.

  Entries are compared case-insensitively and ignoring a trailing backslash,
  and every other entry is written back verbatim: this is somebody's PATH, and
  we are here to remove one thing from it, not to tidy it. No
  WM_SETTINGCHANGE broadcast — the entry now names a deleted directory, so
  nothing is waiting on the news, and it is gone from new shells at next
  sign-in regardless. *)
procedure RemoveAppDirFromUserPath();
var
  Existing, Rebuilt, Raw, Entry, Target: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Existing) then
    exit;

  Target := RemoveBackslashUnlessRoot(ExpandConstant('{app}'));
  Rebuilt := '';
  (* The trailing ';' makes the last entry look like every other one. *)
  Existing := Existing + ';';
  repeat
    P := Pos(';', Existing);
    Raw := Copy(Existing, 1, P - 1);
    Existing := Copy(Existing, P + 1, Length(Existing));
    Entry := Trim(Raw);
    if (Entry <> '') and
       (CompareText(RemoveBackslashUnlessRoot(Entry), Target) <> 0) then
    begin
      if Rebuilt <> '' then
        Rebuilt := Rebuilt + ';';
      Rebuilt := Rebuilt + Raw;
    end;
  until Existing = '';

  if Rebuilt = '' then
    RegDeleteValue(HKEY_CURRENT_USER, 'Environment', 'Path')
  (* Inno cannot read a value's type back, so infer it: a PATH holding a '%' is
     one that has to stay expandable, and rewriting it as REG_SZ would freeze
     every other entry's variable at today's value. *)
  else if Pos('%', Rebuilt) > 0 then
    RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Rebuilt)
  else
    RegWriteStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Rebuilt);
end;

(* Remove the optional Explorer verbs if this user enabled them in Settings.

  The application owns only the final `tty7` subkeys. Deleting those trees
  removes their command children without touching another application's verb
  or a shared `shell` parent. Missing keys are the normal default and make both
  calls harmless no-ops. This cleanup is uninstall-only: upgrades keep the
  user's explicit registration, and the next app launch reports "Needs update"
  if an install path ever changes. *)
procedure RemoveExplorerContextMenu();
begin
  RegDeleteKeyIncludingSubkeys(
    HKEY_CURRENT_USER, 'Software\Classes\Directory\shell\tty7');
  RegDeleteKeyIncludingSubkeys(
    HKEY_CURRENT_USER, 'Software\Classes\Directory\Background\shell\tty7');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    RemoveAppDirFromUserPath();
    RemoveExplorerContextMenu();
  end;
end;
