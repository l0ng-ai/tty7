# Bundled ConPTY

Microsoft's redistributable pseudoconsole, shipped beside `tty7-app.exe` so that
Windows panes do not run on the in-box `conhost.exe`.

| | |
| --- | --- |
| Version | `1.24.260710001` (file version `1.24.2607.10001`) |
| Source | `Microsoft.Windows.Console.ConPTY` on nuget.org, also a release asset of [microsoft/terminal](https://github.com/microsoft/terminal/releases) |
| License | MIT — see `LICENSE.txt`, staged into the package as `LICENSE-ConPTY.txt` |

## Why it is here

The in-box `conhost.exe` **swallows** a pane process's `OSC 11` background-color
query: the sequence never reaches tty7's emulator and no reply is ever written
back, so applications that pick a light/dark UI from the terminal background
(codex, Neovim's background detection, …) render a dark UI under a light theme.
That is issue #345. The redistributable forwards the query and routes the answer
back to the client, which is all tty7 needs — `terminal::view` already answers
`OSC 10/11/12` from the live theme.

Measured on Windows 11 26200, same binary, only this pair added beside it:

```
in-box conhost:  terminal side sees <ESC>[?9001h<ESC>[?1004h   -> client: no reply
sideloaded:      terminal side sees ... <ESC>]11;?<BEL>        -> client: rgb:efef/f1f1/f5f5
```

`portable-pty` prefers a sideloaded `conpty.dll` over `kernel32` on its own
(`src/win/psuedocon.rs`), so nothing in tty7 loads these files explicitly. They
are found through the ordinary DLL search path, which starts at the directory of
the running executable — which is why they sit beside `tty7-app.exe` (the daemon
is `tty7-app.exe --daemon`) rather than in a subdirectory, and why `build.rs`
copies them next to `cargo`'s output so a development build behaves like a
packaged one.

## Updating

Both files are one unit: Microsoft supports the pair, not the halves, and a
mismatched `conpty.dll`/`OpenConsole.exe` misbehaves in ways that surface as pty
bugs. Replace both, from the same package version, and update the table above.

```powershell
$v = '1.24.260710001'
curl.exe -sL -o conpty.zip "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$v/microsoft.windows.console.conpty.$v.nupkg"
Expand-Archive conpty.zip -DestinationPath conpty-pkg
Copy-Item conpty-pkg/runtimes/win-x64/native/conpty.dll        x64/conpty.dll
Copy-Item conpty-pkg/build/native/runtimes/x64/OpenConsole.exe x64/OpenConsole.exe
```

Only `x86_64-pc-windows-msvc` is released today, so only `x64/` is vendored. The
package also carries `arm64` and `x86`; adding an architecture means adding the
directory here and one `Copy-Item` in `.github/scripts/bundle-windows.ps1`.
