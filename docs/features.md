# Features

<sub>English · [简体中文](features.zh-CN.md)</sub>

## Input

- **Ghost suggestions** — your history completes the whole line as you type; <kbd>→</kbd> to accept
- **Explained tab completion** — every flag and subcommand with its description, for ~100 common commands; when tty7 has nothing to offer the Tab falls through to your shell's own completion, and the whole feature can be turned off (Settings → Terminal → Keyboard, or `tab_completion` in `config.json`)
- **Syntax highlighting** — as you type, nothing to install
- **Fuzzy history search** — <kbd>⌃ R</kbd> shows what you ran, where, and whether it failed; turn it off (Settings → Terminal → Keyboard, or `history_search` in `config.json`) and <kbd>⌃ R</kbd> goes to your shell instead, so an fzf / percol binding keeps working
- **History from day one** — your existing shell history works as-is and carries across sessions
- **Line editing** — click to place the caret, mouse selection, word motion, undo
- **Multi-line editing** — wrapped and multi-line commands edit in place; the grid shifts to keep the caret visible

## In the window

- **Tabs & splits** — always open in the current directory
- **Repo-grouped sidebar** — the left tab sidebar groups rows under a header per git repository, non-repo tabs in a trailing *Scratch* section; branch switches and in-repo `cd`s never move a row (`sidebar_grouping` in `config.json`: `repo` default, `none` for a flat list)
- **Command palette** <kbd>⌘ P</kbd> · scrollback search <kbd>⌘ F</kbd>
- **⌘/Ctrl-click links** (⌘ on macOS, Ctrl on Windows/Linux) · desktop notifications · copy on select (opt-in, Settings → Terminal → Clipboard)
- **Smart double-click selection** — double-click grabs the whole URL, file path, bracket/quote pair, or dictionary-segmented CJK word under the cursor; Shift-click extends a selection (toggle in Settings → Terminal → Mouse; word separators via `word_separators` in `config.json`)
- **Nine themes, plus your own** — YAML seed themes with solid, gradient, or image backgrounds; iTerm2 `.itermcolors` import; in-app color editor with a background-image picker
- **Sync with system** — Settings → Appearance; pick separate light and dark themes and tty7 follows the OS appearance live (`theme_follow_system`, `theme_preset_light` / `theme_preset_dark` in `config.json`)
- **Window opacity & blur** — Settings → Appearance → Window; applies to every theme, *Follow theme* returns to the theme's own `opacity` / `blur`
- **CJK / IME input**

## Fonts

- **Hack is bundled** — it ships inside the binary, so the default renders identically everywhere without relying on a system install
- **Primary + ordered fallbacks** — `font_family` and `font_fallbacks` in `config.json`; optional `font_family_bold` / `font_family_italic` for distinct faces, and `font_features` to pass OpenType features through (contextual ligatures stay off unless you ask for them)
- **Platform-aware defaults** — the fallback list names faces the host OS actually ships (PingFang SC / Apple Color Emoji on macOS, Microsoft YaHei / Segoe UI Emoji on Windows, Noto on Linux). Those stock names are appended to a hand-written list too, so a `config.json` written on another platform still resolves

### CJK and the two-column grid

A cell is one advance of the primary face, and a wide (CJK) character is pinned
to exactly two of them. A CJK fallback therefore sits flush in its slot only if
its ideographs advance **twice** the primary's Latin advance.

Bundled Hack advances 0.60205em, so a two-column slot is 1.2041em — while every
stock CJK face (Microsoft YaHei, PingFang SC, Noto Sans CJK) advances 1.0em.
Those glyphs get left-aligned in the slot and the leftover ~0.2em lands as a gap
on the right of every character.

[Maple Mono NF CN](https://github.com/subframe7536/maple-font) is tried first on
every platform for exactly this reason — 0.6em Latin, 1.2em CJK, an exact
two-cell fit against Hack. It is referenced by name only, never bundled (~20MB
per weight): install it and tty7 picks it up with no config change.

For CJK set *tight* rather than merely even, change the primary face instead —
one that advances 0.5em (Sarasa Mono SC, say) makes two columns exactly 1.0em.

## Coding agents

tty7 recognizes third-party coding agents running in a pane (Claude Code,
Codex, Gemini CLI, Aider, Amp, OpenCode, and ~10 more) and adds around them —
it never wraps or replaces the agent.

- **Brand avatars** — the tab chip / sidebar row shows which agent runs where; custom wrappers map in via `agent_commands` in `config.json`
- **Status dot** — working (blue) / needs your input (amber) / done (green), driven by agent-reported events over an OSC channel; Settings → Agents installs the hooks that feed it (Claude Code, Codex, Copilot CLI, OpenCode, Pi, Grok Build)
- **Notifications** — "needs your permission…" the moment an agent blocks on you, and "finished after Ns" per turn, honoring your notification policy
- **Branch at a glance** — each sidebar row shows its pane's git branch and working-tree diff (`+N −M`), refreshed on `cd` and when a command finishes
- **Session resume** — panes lost to a reboot re-launch their agent conversation on restore, carrying the original launch flags (`claude --dangerously-skip-permissions --resume …`) (`restore_agent_sessions`, on by default)
- **Fork session** — branch a live agent conversation into a second, independent one by shelling the agent's own fork command (`codex fork <id>`, `claude --resume <id> --fork-session`, also OpenCode and Grok Build); the original is untouched and both continue separately. Right-click a pane to pick a split placement, or right-click the tab / sidebar row to open the fork in a new tab. Needs the agent's hooks installed, since the fork targets the session id they report; a remote pane can't fork, because the command would run against the local agent — and note a fork copies the whole transcript, so repeated forking costs real disk in the agent's own session store
- **Copy Session ID** — put the agent's native session id on the clipboard, beside *Copy Working Directory*, for pasting into `codex resume`, a bug report, or another tool
- **Context feed** — palette commands send the current selection or the repo's `git diff` to the running agent as a ready-made prompt
- **Tray icon** — a system tray / menu bar item that flips to an attention state the moment any agent needs your input; its menu lists every agent pane (brand avatar + status dot, click to reveal), switches the notification policy, and offers *Quit and Stop Daemon* alongside the plain session-keeping quit (`show_tray_icon`, on by default)

## SSH

A native Rust SSH stack (russh) is the **only** path — profiles, credentials,
and SFTP without shelling out to `ssh`. There is no system-ssh compat mode.

- **QuickConnect** — type `user@host[:port]` in the palette and connect; IPv6 `[::1]:port` supported
- **Saved profiles** — full connection config with passwords / passphrases in the OS keychain, never on disk
- **`~/.ssh/config` aliases** — type one to connect (resolved natively — common fields, best-effort — over russh), or import them as profiles in Settings
- **GUI auth** — in-pane sheets for password, key passphrase, 2FA, and host-key confirmation (new vs. changed)
- **Built-in SFTP** — a slide-in file panel: browse, upload / download, rename / delete / chmod, drag to Finder
- **Port forwarding** — Local / Remote / Dynamic, preconfigured or added live, plus ⌘/Ctrl-click `localhost:PORT` to auto-forward
- **Jump hosts & proxies** — multi-hop via profile references or `ProxyJump`, ProxyCommand, SOCKS5 / HTTP

| Entry point | Connects via |
|---|---|
| Saved profiles · QuickConnect · typed `user@host[:port]` | Native russh — SFTP · keychain · GUI auth · L/R/D forwards |
| `~/.ssh/config` aliases | Resolved natively, then russh (`Match`/canonicalize/GSSAPI unsupported — no fallback) |

## Keybindings

Keys are shown in macOS notation — on Windows and Linux, read <kbd>⌘</kbd> as
<kbd>Ctrl</kbd>. The essentials:

| | |
|---|---|
| <kbd>⌘ T</kbd> · <kbd>⌘ W</kbd> · <kbd>⌘ ⇧ T</kbd> | new tab · close tab · reopen closed tab |
| <kbd>⌘ 1</kbd>…<kbd>⌘ 9</kbd> · <kbd>⌃ ⇥</kbd> · <kbd>⌃ ⇧ ⇥</kbd> | jump to tab 1–9 · next tab · previous tab |
| <kbd>⌘ D</kbd> · <kbd>⌘ ⇧ D</kbd> | split right · split down |
| <kbd>⌘ ]</kbd> · <kbd>⌘ [</kbd> | next pane · previous pane |
| <kbd>⌘ ⌥ ←→↑↓</kbd> | focus the pane in that direction |
| <kbd>⌘ ⏎</kbd> · <kbd>⌘ ⇧ ⏎</kbd> | toggle fullscreen · maximize / restore the pane |
| <kbd>⌘ K</kbd> | clear the screen and scrollback |
| <kbd>⌘ P</kbd> | command palette |
| <kbd>⌘ F</kbd> | search the scrollback |
| <kbd>⌃ R</kbd> | fuzzy-search shell history |
| <kbd>⌘ +</kbd> · <kbd>⌘ −</kbd> · <kbd>⌘ 0</kbd> | font size up · down · reset |

**Settings → Keybindings** (<kbd>⌘ ,</kbd>) lists every shortcut. Click one,
press the new keys (<kbd>Esc</kbd> cancels, <kbd>Backspace</kbd> resets to
default), and it takes effect immediately. Pane resize and swap have no default
keys — bind them here or run them from the command palette.

**tmux preset** — remaps pane/tab actions onto a prefix (default <kbd>⌃ B</kbd>):
<kbd>⌃ B</kbd> <kbd>C</kbd> opens a tab, <kbd>⌃ B</kbd> <kbd>%</kbd> splits,
<kbd>⌃ B</kbd> then an arrow moves focus. A bare prefix reaches the shell after
a brief pause; `prefix` + an unbound key passes straight through.

## Performance notes

- The PTY is read at device speed and parsed in large batches, off the render path
- Hot paths are lock-free — a big `cat` never waits on drawing
- The daemon buffers up to 16 MiB ahead of the window before backpressure applies
