# Verifies that the Windows release artifacts carry everything the in-app
# updater requires, immediately after bundle-windows.ps1 produces them.
#
# The updater refuses to install a package it cannot recognise, and it does so
# on the user's machine, after the download, after the GUI has exited. Every
# fact it checks there is checked here instead, so a packaging mistake fails
# the release build rather than every user's next update.
#
# Mirrors, in order:
#   core::update::windows_update_layout_for      — the install marker
#   core::update::package_for_current_install    — tty7-updater.exe beside the app
#   tty7-updater `windows::verify_portable_payload`  — portable layout + versions
#   tty7-updater `windows::extract_portable_archive` — ZIP entry rules
#   tty7-updater `windows::verify_file_version`      — setup.exe PE version
#
# Usage: verify-windows-package.ps1 <arch> [version]
# `version` defaults to the version in Cargo.toml, which is what the bundle
# script stamped into the artifact names.
$ErrorActionPreference = 'Stop'

$Arch = $args[0]
if (-not $Arch) { throw "usage: verify-windows-package.ps1 <arch> [version]" }
$Version = $args[1]
if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
}
# The PE fixed-version resource carries only numeric components, so the updater
# compares the release version's numeric core against it. Keep the same split.
$VersionCore = ($Version -split '[-+]', 2)[0]

$Name  = "tty7-$Version-windows-$Arch"
$Zip   = "dist/$Name.zip"
$Setup = "dist/$Name-setup.exe"
$Stage = "dist/$Name"

$failures = New-Object System.Collections.Generic.List[string]
function Fail([string]$message) { $failures.Add($message) }

function Get-ProductVersion([string]$path) {
    # The same string the updater reads back with VerQueryValueW
    # (\StringFileInfo\<lang><cp>\ProductVersion).
    (Get-Item -LiteralPath $path).VersionInfo.ProductVersion
}

function Assert-BinaryVersion([string]$path, [string]$label) {
    if (-not (Test-Path -LiteralPath $path)) { Fail "$label is missing: $path"; return }
    $actual = Get-ProductVersion $path
    if ($actual -ne $Version) {
        Fail "$label reports ProductVersion '$actual', expected '$Version'"
    }
}

# Microsoft ships conpty.dll and OpenConsole.exe as one supported unit, and the
# app degrades quietly without them: `portable-pty` falls back to the in-box
# conhost, which swallows the OSC 11 background query all over again (#345).
# Neither the missing case nor the mismatched case is visible at runtime, so
# both fail the release here instead.
function Assert-ConptyPair([string]$directory, [string]$label) {
    $versions = @{}
    foreach ($file in 'conpty.dll', 'OpenConsole.exe') {
        $path = Join-Path $directory $file
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Fail "$label is missing the bundled $file"
            return
        }
        $versions[$file] = (Get-Item -LiteralPath $path).VersionInfo.FileVersion
    }
    if ($versions['conpty.dll'] -ne $versions['OpenConsole.exe']) {
        Fail ("$label carries a mismatched ConPTY pair: conpty.dll is " +
              "$($versions['conpty.dll']), OpenConsole.exe is $($versions['OpenConsole.exe'])")
    }
    if (-not (Test-Path -LiteralPath (Join-Path $directory 'LICENSE-ConPTY.txt') -PathType Leaf)) {
        Fail "$label ships the bundled ConPTY without its MIT notice"
    }
}

# ---- Portable ZIP --------------------------------------------------------
# Update rules live in the updater's extractor; the ones that can be broken by
# packaging alone are re-stated here.
if (-not (Test-Path -LiteralPath $Zip)) {
    Fail "the portable archive is missing: $Zip"
} else {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead((Resolve-Path $Zip).Path)
    try {
        $entries = @($archive.Entries | ForEach-Object { $_.FullName })
    } finally {
        $archive.Dispose()
    }

    # `extract_portable_archive` rejects a backslash outright: the ZIP spec
    # names '/' as the separator, and a mixed archive is one the updater will
    # not unpack. PowerShell's archive writer has emitted both over the years.
    $backslashed = @($entries | Where-Object { $_.Contains('\') })
    if ($backslashed.Count -gt 0) {
        Fail ("the portable archive uses backslash separators the updater rejects: " +
              ($backslashed -join ', '))
    }

    # `validate_portable_relative_path` allows only these top-level names.
    $managed = @(
        'tty7-app.exe', 'tty7.exe', 'tty7-updater.exe', '.tty7-portable',
        'completions', 'server', 'LICENSE.txt', 'README.md',
        'conpty.dll', 'OpenConsole.exe', 'LICENSE-ConPTY.txt'
    )
    $roots = @($entries |
        ForEach-Object { ($_ -split '[\\/]', 2)[0] } |
        Sort-Object -Unique)
    foreach ($root in $roots) {
        if ($managed -notcontains $root) {
            Fail "the portable archive has a top-level entry the updater rejects: $root"
        }
    }

    # The Inno marker and the portable marker are mutually exclusive: whichever
    # one is present decides how the updater replaces this installation.
    if ($entries -contains '.tty7-inno-install') {
        Fail "the portable archive carries the Inno install marker"
    }

    $unzipped = Join-Path ([System.IO.Path]::GetTempPath()) "tty7-verify-portable-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Force -Path $unzipped | Out-Null
    try {
        [System.IO.Compression.ZipFile]::ExtractToDirectory(
            (Resolve-Path $Zip).Path, $unzipped)

        # `verify_portable_payload`: every required member, then the marker
        # content, then the complete version of both executables.
        foreach ($required in @('tty7-app.exe', 'tty7.exe', 'tty7-updater.exe',
                                '.tty7-portable', 'LICENSE.txt', 'README.md')) {
            if (-not (Test-Path -LiteralPath (Join-Path $unzipped $required) -PathType Leaf)) {
                Fail "the portable archive is missing the required file $required"
            }
        }
        if (-not (Test-Path -LiteralPath (Join-Path $unzipped 'completions') -PathType Container)) {
            Fail "the portable archive is missing the required directory completions"
        }

        $markerPath = Join-Path $unzipped '.tty7-portable'
        if (Test-Path -LiteralPath $markerPath) {
            $marker = [System.IO.File]::ReadAllBytes($markerPath)
            $expected = [System.Text.Encoding]::ASCII.GetBytes('portable-v1')
            if (@(Compare-Object $marker $expected -SyncWindow 0).Count -ne 0) {
                Fail "the portable marker does not contain exactly 'portable-v1'"
            }
        }

        Assert-BinaryVersion (Join-Path $unzipped 'tty7-app.exe') 'the portable tty7-app.exe'
        Assert-BinaryVersion (Join-Path $unzipped 'tty7-updater.exe') 'the portable tty7-updater.exe'
        Assert-ConptyPair $unzipped 'the portable archive'
    } finally {
        Remove-Item -Recurse -Force $unzipped -ErrorAction SilentlyContinue
    }
}

# ---- Inno payload --------------------------------------------------------
# ISCC compiled the installer from this staging directory, so what it holds is
# what lands in {app}. Reading the compiled setup.exe back would need
# innoextract, which the runners do not carry.
if (-not (Test-Path -LiteralPath $Stage -PathType Container)) {
    Fail "the Inno staging directory is missing: $Stage"
} else {
    if (-not (Test-Path -LiteralPath (Join-Path $Stage '.tty7-inno-install') -PathType Leaf)) {
        Fail "the Inno payload is missing the .tty7-inno-install marker; installed copies would never be offered an in-app update"
    }
    if (Test-Path -LiteralPath (Join-Path $Stage '.tty7-portable')) {
        Fail "the Inno payload carries the portable marker, which would misroute the updater"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $Stage 'tty7-updater.exe') -PathType Leaf)) {
        Fail "the Inno payload is missing tty7-updater.exe"
    }
    Assert-BinaryVersion (Join-Path $Stage 'tty7-app.exe') 'the installed tty7-app.exe'
    Assert-BinaryVersion (Join-Path $Stage 'tty7-updater.exe') 'the installed tty7-updater.exe'
    Assert-ConptyPair $Stage 'the Inno payload'
}

# ---- Setup executable ----------------------------------------------------
# `verify_update` re-reads this numeric version after the GUI exits and before
# it runs the installer, so a mis-stamped VersionInfoVersion is an update that
# aborts on the user's machine.
if (-not (Test-Path -LiteralPath $Setup -PathType Leaf)) {
    Fail "the Windows installer is missing: $Setup"
} else {
    $info = (Get-Item -LiteralPath $Setup).VersionInfo
    $actual = "$($info.FileMajorPart).$($info.FileMinorPart).$($info.FileBuildPart)"
    if ($actual -ne $VersionCore) {
        Fail "$Setup reports file version '$actual', expected '$VersionCore'"
    }
}

if ($failures.Count -gt 0) {
    foreach ($failure in $failures) { Write-Output "::error::$failure" }
    throw "the Windows release package would not be updatable in place ($($failures.Count) problem(s))"
}

Write-Output "Windows package verified: markers, tty7-updater.exe and versions match $Version"
