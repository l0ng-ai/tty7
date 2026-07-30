# Usage: bundle-windows.ps1 <target-triple> <arch-label>
# Package the release binary twice from one staged payload:
#   dist/tty7-<version>-windows-<arch>.zip        portable (unzip anywhere)
#   dist/tty7-<version>-windows-<arch>-setup.exe  Inno Setup installer
#     (Program Files or per-user, Start Menu shortcut, "Apps" uninstall entry)
#
# Fonts are embedded via include_bytes! and the app icon is compiled into the
# executable as a resource (see build.rs). So the payload is tty7.exe plus a
# sibling completions\ dir (loaded at runtime — see terminal::signature) and the
# license/readme. Both artifacts are unsigned builds — SmartScreen will
# warn on first launch.
$ErrorActionPreference = 'Stop'

$Target = $args[0]
$Arch   = $args[1]
$Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$Name  = "tty7-$Version-windows-$Arch"
$Stage = "dist/$Name"

Remove-Item -Recurse -Force dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item "target/$Target/release/tty7.exe" "$Stage/tty7.exe"
New-Item -ItemType Directory -Force -Path "$Stage/completions" | Out-Null
Copy-Item "assets/completions/*.json" "$Stage/completions/"
Copy-Item LICENSE "$Stage/LICENSE.txt"
Copy-Item README.md "$Stage/README.md"

# The Linux musl `tty7-server`, staged at server/ so a WSL distro can be handed
# the binary this client shipped with (WSL downloads nothing). The
# lookup path is a contract with `daemon::install::wsl` — it searches
# <dir of tty7.exe>/server/<asset> first — so this directory name is not free to
# change on its own. Missing is a warning, not an error, matching `server-musl`'s
# own skip-don't-fail probe; WSL then fails at connect time with a message
# naming the directories it searched.
#
# The *filename* is a contract too, not just the directory: `wsl.rs` looks for
# `<dir>/<asset name>`, so this string has to stay whatever
# `install::asset::ASSET_X86_64` says it is.
$ServerAsset = "tty7-server-linux-x86_64-musl"
$ServerSrc = "bundled-server/$ServerAsset"
if (Test-Path $ServerSrc) {
    New-Item -ItemType Directory -Force -Path "$Stage/server" | Out-Null
    Copy-Item $ServerSrc "$Stage/server/$ServerAsset"
    Write-Host "OK bundled $ServerAsset"
} else {
    Write-Warning "no $ServerAsset to bundle - this build cannot serve WSL distros"
}

Compress-Archive -Path "$Stage/*" -DestinationPath "dist/$Name.zip" -Force

# Installer, built from the same staged payload. ISCC is on PATH on GitHub's
# windows-latest image; fall back to the default install location.
$Iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $Iscc) { $Iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" }
& $Iscc `
    "/DAppVersion=$Version" `
    "/DStageDir=$((Resolve-Path $Stage).Path)" `
    "/DOutputDir=$((Resolve-Path dist).Path)" `
    "/DOutputName=$Name-setup" `
    .github/scripts/windows-installer.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC exited with $LASTEXITCODE" }

Remove-Item -Recurse -Force $Stage
Write-Host "OK dist/$Name.zip"
Write-Host "OK dist/$Name-setup.exe"
