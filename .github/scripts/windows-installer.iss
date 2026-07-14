; tty7 Windows installer (Inno Setup 6 — preinstalled on GitHub's
; windows-latest runners). Compiled by bundle-windows.ps1, which stages the
; payload and passes every path in via /D defines:
;
;   /DAppVersion=<semver>   version parsed from Cargo.toml
;   /DStageDir=<abs path>   staged payload (tty7.exe, completions\, LICENSE.txt, README.md)
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

[Setup]
; Never change AppId: it is how Windows ties upgrades + the uninstall entry
; to previous installs of tty7.
AppId={{9A3F6C1E-4B7D-4E2A-8C5F-D01B92E64A37}
AppName=tty7
AppVersion={#AppVersion}
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
UninstallDisplayIcon={app}\tty7.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The persistent daemon (tty7.exe --daemon) is a detached background process
; that outlives the GUI and holds the running image of tty7.exe, so Windows
; locks the file and an upgrade can't replace it. We stop it explicitly in
; PrepareToInstall below; keep the Restart Manager as a backstop but don't let
; it relaunch anything (the GUI respawns the daemon itself on next start).
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#StageDir}\tty7.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\completions\*"; DestDir: "{app}\completions"; Flags: ignoreversion recursesubdirs
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\tty7"; Filename: "{app}\tty7.exe"
Name: "{autodesktop}\tty7"; Filename: "{app}\tty7.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\tty7.exe"; Description: "{cm:LaunchProgram,tty7}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the daemon before the uninstaller deletes tty7.exe — the running daemon
; is the locked image of that file, so removing it fails otherwise. This runs at
; the start of uninstallation, before any files are removed. The installed binary
; is this version, which understands the flag; runhidden suppresses any flash and
; the call returns without opening a window. RunOnceId keys the entry so a repeated
; uninstall doesn't run it twice.
Filename: "{app}\tty7.exe"; Parameters: "--stop-daemon"; Flags: runhidden waituntilterminated; RunOnceId: "StopDaemon"

[Code]
{ Gracefully stop the persistent daemon before we overwrite tty7.exe. We can't
  run the *installed* binary here — on an upgrade from an older build it may not
  understand --stop-daemon and would launch the GUI instead — so we extract the
  *new* tty7.exe to {tmp} and run that. It connects to the running daemon, hangs
  up every shell, waits for it to exit (releasing the file lock), then returns
  without opening a window. Best effort: any failure falls through to the Restart
  Manager backstop, and a fresh install simply has no daemon to stop. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  ExtractTemporaryFile('tty7.exe');
  Exec(ExpandConstant('{tmp}\tty7.exe'), '--stop-daemon', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;
