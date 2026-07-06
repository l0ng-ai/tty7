# Usage: bundle-windows.ps1 <target-triple> <arch-label>
# Package the release binary into a zip:
#   dist/tty7-<version>-windows-<arch>.zip
#
# Fonts are embedded via include_bytes! and the app icon is compiled into the
# executable as a resource (see build.rs). So the archive is tty7.exe plus a
# sibling completions\ dir (loaded at runtime — see terminal::signature) and the
# license/readme. This is an unsigned build — SmartScreen will
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

Compress-Archive -Path "$Stage/*" -DestinationPath "dist/$Name.zip" -Force
Remove-Item -Recurse -Force $Stage
Write-Host "OK dist/$Name.zip"
