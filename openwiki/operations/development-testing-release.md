# Development, testing, and release operations

## Local development loop

Use these commands (the `cargo dev` alias is defined in `.cargo/config.toml`):

```bash
cargo dev          # cargo run -- --config-dir .tty7-dev
cargo test         # unit tests
cargo fmt          # format; CI enforces cargo fmt --check
cargo clippy       # advisory but useful before PRs
```

`cargo dev` is important. Plain `cargo run` uses the real user config directory by default, which can pollute daily `config.json`, `session.json`, history, and daemon endpoint state. `.tty7-dev/` is gitignored and safe to delete for a clean dev state.

Linux development needs system packages for GPUI's x11/wayland/font backends. The list is in `README.md` and mirrored in `.github/workflows/ci.yml`.

## Dependency posture

`Cargo.toml` pins several important git dependencies:

- `gpui` and `gpui_platform` from a specific Zed revision.
- `alacritty_terminal` from Zed's fork.
- `gpui-component` and `gpui-component-assets` from the `l0ng-ai/gpui-component` `tty7` branch.

A repository convention: reuse gpui-component widgets when possible instead of hand-rolling UI equivalents. If a custom widget is necessary, explain why in code.

## Test coverage map

There are many unit tests embedded near pure logic. Use targeted tests when changing a subsystem, then run broader checks before handing off.

| Area | Files to check first | Relevant behavior |
| --- | --- | --- |
| Protocol/IPC | `src/daemon/protocol.rs`, `src/daemon/server.rs`, `src/daemon/transport.rs` | Frame roundtrips, malformed/oversized frames, socket lifecycle, connection role handling. |
| Pane lifecycle | `src/daemon/pane.rs`, `src/daemon/spawn.rs` | Replay ring, attach ordering, detach epochs, EOF/reclaim, process teardown, OSC sniffing. |
| Shell integration | `src/daemon/shell_integration.rs`, `src/core/osc.rs` | zsh/bash/fish setup, prompt marks, cwd escaping, tokenizer recovery. |
| Config/session | `src/core/config.rs`, `src/core/session.rs` | Defaults, lenient enums, sanitization, atomic writes, restore model. |
| Terminal input/editor | `src/terminal/input.rs`, `src/terminal/cmd_editor.rs`, `src/terminal/preinit.rs`, pure helpers in `src/terminal/view.rs` | Legacy/Kitty key encoding, editing, selections, paste, startup typeahead. |
| History/completion/search | `src/terminal/history.rs`, `src/terminal/completion.rs`, `src/terminal/search.rs`, `src/terminal/reverse_search.rs`, `src/terminal/highlight.rs` | Frecency, path/command completion, Ctrl+R, scrollback search, URL detection, syntax tiling. |
| Rendering helpers | `src/terminal/element.rs`, `src/terminal/palette.rs`, `src/terminal/fps.rs`, `src/terminal/view.rs` | Palette conversion, cell/selection/search/link helper behavior, scroll math. |
| UI helpers | `src/ui/home.rs`, `src/ui/keymap.rs`, `src/ui/tab_strip.rs`, `src/ui/presets.rs`, `src/ui/settings.rs`, `src/ui/hints.rs` | Home state, key token formatting, tab labels/icons, theme presets, settings helpers. |

## CI

`.github/workflows/ci.yml` runs on push, pull request, and manual dispatch:

- `cargo fmt --check` on Ubuntu.
- Build and test matrix:
  - `aarch64-apple-darwin` on macOS 14,
  - `x86_64-pc-windows-msvc` on Windows latest,
  - `x86_64-unknown-linux-gnu` on Ubuntu latest.

The Linux job installs the required x11/wayland/xkb/font/SSL/zstd development packages before building.

## Release packaging

macOS packaging is handled by `.github/scripts/bundle.sh` and `.github/workflows/release.yml`.

Release workflow:

- Triggered by `v*` tags or manual dispatch.
- Builds macOS arm64 and x86_64 targets.
- Bundles `dist/tty7.app` and `dist/tty7-<version>-macos-<arch>.zip`.
- Uploads zips to GitHub Releases for tag builds.

The bundler:

- Reads version from `Cargo.toml`.
- Creates `Info.plist` with bundle id `com.github.tty7`.
- Copies `assets/tty7.icns`.
- If Developer ID secrets are present, signs with hardened runtime and notarizes/staples.
- Otherwise uses ad hoc signing for local dev builds.

For a local macOS package, build the target, run `bash .github/scripts/bundle.sh <target-triple> <arch-label>`, then inspect `dist/tty7.app` and `dist/tty7-<version>-macos-<arch>.zip`. To replace an installed local copy, remove `/Applications/tty7.app`, copy the newly built bundle there, and fully quit the running app before reopening; tty7's single-instance behavior can otherwise activate the old process instead of starting the upgraded binary.

Windows icon embedding is handled separately by `build.rs` via `winresource` when building on Windows.

## Security-sensitive areas

`SECURITY.md` highlights a terminal emulator's unusual input surface: untrusted escape sequences can come from `cat`, `ssh`, remote programs, and paste. Be extra careful with:

- VT/CSI/OSC parsing and scanner recovery,
- clipboard store/load and paste handling,
- bracketed paste escape stripping,
- shell-integration rc/bootstrap scripts,
- daemon socket/transport protocol,
- process lifecycle and kill/reclaim logic,
- history/config/session data privacy.

Never document or read live secrets. Release workflow secrets in `.github/workflows/release.yml` are referenced only by name; do not inspect secret values.

## Change-oriented guidance for future agents

Before editing:

1. Read the relevant OpenWiki page and the module-level docs in source files.
2. Check recent git history for the touched subsystem if behavior seems surprising.
3. Prefer minimal, well-tested changes; this repository has many pure helper tests for a reason.

When changing runtime/daemon behavior:

- Test protocol and pane lifecycle behavior.
- Preserve detach-vs-kill semantics.
- Preserve attach replay order and replay side-effect suppression.
- Think about stale endpoints, dead detached panes, and single-subscriber races.

When changing prompt/editor behavior:

- Test unsupported or not-yet-integrated shell fallback paths conceptually.
- Keep local editor active only when safe: at prompt, not alternate screen, not exited, search not focused.
- Remember that paste and history may contain sensitive data.

When changing UI/settings:

- Reuse gpui-component where possible.
- Keep zero-tab home behavior working.
- Distinguish settings that apply live from settings that affect only new panes.
- Avoid assuming there is always an active terminal tab.

When changing cross-platform code:

- Check cfg-specific dependencies in `Cargo.toml`.
- Run or rely on CI matrix for macOS/Windows/Linux differences.
- Avoid adding Unix-only assumptions to shared daemon/terminal code.
