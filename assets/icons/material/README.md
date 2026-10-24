# Material icon pack

Vendored from [`material-icon-theme`](https://github.com/material-extensions/vscode-material-icon-theme)
v5.39.0 — the SVGs and `material-icons.json` exactly as the npm package
(`npm pack material-icon-theme@5.39.0`) publishes them, not re-exported or
re-optimised here, so a difference from upstream is always a bug.

MIT, `Copyright (c) 2025 Material Extensions` — see `LICENSE`.

`src/ui/file_icons.rs` reads the mapping; `build.rs` turns the SVGs into the
table `src/ui/assets.rs` serves under `icons/material/<name>.svg`.

To re-sync, unpack a newer tarball over this directory and run
`cargo test -p tty7 --bin tty7-app file_icons`. The test fails if the mapping
names an icon the pack does not ship, which is the one way this can rot
quietly.
