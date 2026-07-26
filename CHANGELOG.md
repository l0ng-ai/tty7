# Changelog

All notable changes to tty7 are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Taskbar status dot** (Windows) — each window's taskbar button now carries
  a status overlay in the same colors as the in-window dots: blue while a
  command or agent is working, green when work finished in the background
  (cleared the moment you activate the window), amber when an agent needs
  your input. Settings → Window grows a "Taskbar status dot" toggle
  (`taskbar_status_icon`, on by default). (#199)

## [26.7.4] - 2026-07-26

### Added

- **Grok Build gets the full hook integration** — Settings → Agents grows a
  sixth row, installing tty7's hooks to `~/.grok/hooks/tty7.json`. A grok pane
  now carries the same live status the other agents do (blue "working" → green
  "done", amber "needs you" when grok asks a question), and after a restart it
  relaunches `grok --resume <id>` with the original launch flags instead of a
  bare shell. Panes that only ever had the Claude Code hooks installed are
  relabeled to grok rather than reporting as "Claude Code", and grok's brand
  mark replaces the generic robot avatar. (#174)
- **Edit, File and Help menus** — copy/cut/paste/select-all, undo/redo and
  find/find-next now live in a real Edit menu; `About tty7`, `Check for
  Updates…`, `Hide tty7` and Services sit in the app menu; Docs, Discord and
  `Report an Issue` are reachable from Help; and tab actions that used to exist
  only in a context menu (Rename Tab, New Worktree Tab, Close Other Tabs, Close
  Tabs to the Right, Copy Working Directory) are real actions you can also find
  in the palette and rebind in Settings. (#175)

### Changed

- **The menu bar follows the HIG** — `tty7 · File · Edit · View · Window ·
  Help` replaces `tty7 · Shell · Window · View`. The `Shell` menu is gone: its
  new / close / split / rename items are File's job everywhere else, and the
  name collided with Settings → Shell, which configures something entirely
  different. View gained the layout toggles and pane commands that were
  chord-only, and `Delete Workspace…` moved behind its own rule at the bottom
  of File. (#175)
- **The command palette is ranked, banded, and says what it will do** — results
  are scored rather than filtered, so word-initials and prefixes outrank
  scattered letters; the idle list opens on `Recent` (frecency) followed by
  Tabs & Panes · Workspaces · View · Terminal · SSH · Agents · Application
  instead of 47 flat rows. Names describe the outcome and the current state
  (`Hide Left Sidebar`, `Tab Bar: Move to Left Sidebar`) rather than the switch,
  one namespace prefix per subsystem, and the naming grammar is written down at
  the top of `palette.rs`. (#175)
- **Settings re-sectioned around what you're configuring** — the orphan `Shell`
  page folds into Terminal's first group, a new `Input` page collects the prompt
  editor, selection/clipboard and keyboard settings, Terminal drops from seven
  groups to four, and notifications moved to Window & Tabs where the rest of the
  app-level behaviour lives. The search index grew from 14 entries to 51 with
  titles pinned to the rendered row labels, so `opacity`, `blur`, `completion`,
  `ctrl-r` and `grouping` now find their rows. (#175)
- **The tab bar defaults to the left sidebar** — a fresh install opens with the
  vertical tab rail instead of the horizontal title-bar strip. Anyone who has
  flipped the setting or hit `ToggleTabSidebar` has an explicit value persisted
  and keeps their layout. (#171)
- **Idle chrome tiles paint at the rail's ink weight** — title-bar toggles,
  panel tabs, `+` and their siblings now draw their glyph in
  `sidebar_foreground` instead of the near-black `foreground`, with full
  strength reserved for the selected tile. The chrome no longer reads a step
  darker than everything it neighbours, and "on" gets a second cue beyond the
  grey capsule. (#176)

### Fixed

- **Non-ASCII output survives a Finder/Dock launch** — macOS GUI launches can
  omit every locale variable, leaving the shell in the C locale: `ls` printed
  one `?` per non-ASCII byte, so a CJK filename came out as `??????`, and tmux
  replaced each Unicode cell with `_`. New shells now receive a UTF-8 character
  locale (`LC_CTYPE` only, so message/date/number localization is untouched)
  derived from the system locale the way Terminal.app does it, and only when no
  inherited or configured locale exists — explicit locale choices still win.
  The derived name is checked against the locales actually installed before it
  is exported, so it stays loadable on remote hosts too, where ssh forwards
  `LC_*` by default.
  ([#178](https://github.com/l0ng-ai/tty7/issues/178), #173, #180)
- **Right-click Paste dropped images, and Copy looked disabled** — copy, cut,
  paste and undo each had two implementations, a chord handler and an action
  handler, which had drifted: the context menu's Paste skipped the image branch
  ⌘V had, so pasting a screenshot into an agent pane worked by keyboard but not
  by menu, and Copy rendered disabled whenever the selection lived in the prompt
  editor rather than the terminal grid. Both now route through one method each.
  (#175)
- **The palette named the other window's sidebar** — the stateful palette titles
  read the config copies of `sidebar_collapsed` / `right_panel_visible`, which
  only record whichever window toggled them last. With two windows in different
  states, one window's palette offered "Show Left Sidebar" while its rail was
  already out. Each window now labels the toggles from its own chrome state.
  (#175)
- **No more update prompts for a half-built release** — the release step ran
  inside the build matrix, so the first platform to finish published a release
  carrying only its own assets, and that release became `/releases/latest`
  immediately. The in-app update check polls exactly that endpoint, so users
  were prompted to download a version whose `.dmg` was still notarizing. All
  four platforms now upload artifacts and a single gated job assembles one
  **draft** release with every asset. (#172)

## [26.7.3] - 2026-07-25

### Added

- **Multiple windows, one workspace each** — `New Workspace` (⌘⇧N) opens a
  second window with its own tabs, splits, and chrome state. A workspace is
  the persistent thing: closing its window puts it away with its panes still
  running in the daemon, and the title bar's workspace menu, the command
  palette, and the home page's picker all bring it back. `Stop Workspace`
  ends one for real (no default chord — it kills sessions), `Delete
  Workspace` also forgets the layout, and a workspace can be renamed from
  the title-bar chip. (#169)
- **Workspace switcher, in the two places you'd look** — a title-bar chip
  (monogram + chevron) whose menu lists every workspace with a monogram badge
  and a corner dot for the ones whose shells are still running, and the macOS
  **Window** menu listing the same set: on screen first, then the detached ones
  with how long ago you left them. Both show the first nine. (#169)
- **Detail panel** — a docked right-hand column that shows what the active
  pane *is* rather than what it's printing: **Info** (cwd, branch, shell,
  agent, the process tree and the TCP ports the pane is listening on),
  **Outline**, **Changes**, and **Files** (a local file tree). Opening a file
  from it lifts a code editor overlay over the terminal. (#158)
- **Port forwarding and SFTP live in the detail panel** — a native-SSH pane's
  forwards are listed, added and removed from the Info tab, and its remote
  filesystem browses from the Files tab, with transfers reported in a footer
  that rides under every tab. (#166)
- **Shell integration in native SSH panes** — OSC 133 prompt marks, exit
  codes and cwd reporting now reach zsh, bash and fish on the far side, so
  the inline line editor works in an SSH pane exactly as it does locally.
  (#152)
- **Live drag-to-reorder** — tabs rearrange under the cursor the way Chrome
  and Warp do (the dragged tab stays in the list and the ones it passes slide
  out of the way) in the strip, the sidebar rows, and whole repo groups.
  (#151)
- **The rail's top strip drags the window** — the sidebar's title-bar-height
  top zone now moves the window on drag and zooms it on double-click, like
  the title bar it sits level with. (#153)
- **The ⌃R history menu can be switched off** — Settings → Terminal →
  Keyboard, or `history_search` in `config.json`. With it off, the prompt
  line is handed to the shell and the raw `^R` follows it, so a binding
  of your own (fzf, percol, plain reverse-i-search) answers instead of
  tty7's menu. (#163, #170)

### Changed

- **Sidebar and right-panel visibility are per-window** — toggling one
  window's chrome leaves the others alone. The config value is now what a
  *newly opened* window starts with; panel width stays shared. (#169)
- **Chrome icons draw at the size they were asked to** — every tile glyph in
  the window (title bar, rail, panel tabs, overlay headers) had been rendering
  at 12px regardless of the size its call site set, because the button widget
  overwrites its icon's size from its own. The tile rhythm is now stated once
  and applied from one helper, so the marks read at their intended weight and
  the hover capsule keeps a consistent gap from the window edge. (#169)
- **The panel and chrome glyphs are drawn to one spec** — the detail panel's
  tab marks and the title bar's own were redrawn so a row of icons stops
  reading as a row of icons from different sets, and every icon tile shares
  one soft hover fill. (#161)
- **Multi-line commands submit as one bracketed paste** — a 30-line block
  costs one prompt cycle instead of thirty, so it no longer retypes itself
  down the screen on Enter. (#164)
- **The file tree lists directories off the render thread** — `read_dir`, the
  `.gitignore` chain and the search walk left the paint, so a cold cache or a
  large repo no longer stalls a frame. (#160)
- **The LSP client is gone** — opening a `.rs` file in the code panel silently
  spawned rust-analyzer, which indexed the whole workspace for hundreds of
  megabytes of RAM. A terminal shouldn't do that to you on a click, and the
  fix is removal rather than a switch for something nobody asked for. (#159)
- **Sidebar rows float their close button** — the ✕ no longer reserves a
  permanent trailing column, so labels and branch lines get the full rail
  width, and it stays clear of a row's `+n −n` counts. (#145)
- **Daemon wire protocol is now v2** — WSL panes carry a remote-context kind
  that a v1 client can't decode, so it would drop the pane's connection
  instead of ignoring the unknown value. The version handshake now sees that
  skew and offers to restart the daemon, rather than letting a downgraded
  build lose panes silently. (#169)
- **`Cargo.lock` is checked against `Cargo.toml` in CI** — release builds now
  run `--locked`, so a drifted lockfile fails the build instead of quietly
  resolving something else. (#150)

### Fixed

- **⌃J and ⌃M submit the line again** — accept-line's control codes were
  swallowed at the prompt as unrecognized Ctrl chords, so the keys did
  nothing. They now take the same path Enter does, completion picker and
  history menu included. (#163, #170)
- **The sidebar's git counts keep up** — `+N −N` refreshes as an agent's tool
  calls land (throttled per repo) and when the window regains focus, instead
  of going stale until you happened to run a command in that pane. (#149)
- **`ToggleSftp` earns its name** — pressing it while the panel is already on
  Files closes the panel rather than being a dead press, and the remote
  browser's 500ms transfer poll now ends itself when the panel closes instead
  of depending on the render path to retire it. (#167)
- **Closing the last window from the home page quits** — it used to leave a
  windowless process in the Dock that no longer responded to being clicked.
  (#147, #148)
- **Windows titlebar chrome** — the `⋯` menu sits back on the native
  window-control rhythm, and the sidebar's collapse / new-tab buttons take
  their clicks again instead of being swallowed by the drag region. (#162)
- **Windows: the theme panel's close button did nothing** — and the repo group
  header showed a plain arrow where every other row shows a pointer. (#155,
  #156)
- **Settings keeps its stock glyphs** — the page's own icons stopped being
  restyled by the chrome tile pass, and its close button is sized to the title
  bar's rhythm rather than reading undersized beside the window controls.
  (#157, #165)

## [26.7.2] - 2026-07-21

### Added

- **WSL shell integration** — prompt marks, cwd tracking, and agent
  detection now work inside WSL distros. The integration tags the shell
  it actually launches, never blocks the spawn path while probing the
  distro's shell, and declines untranslatable cwds rather than
  misreporting them. (#134)
- **Git Bash shell integration on Windows** — Git Bash panes get the
  full integration, reporting proper Windows paths for cwd-dependent
  features. (#130)
- **Smart double-click selection** — double-click selects the word under
  the cursor with language-aware boundaries, including CJK segmentation
  (the OS tokenizer on macOS, jieba elsewhere); angle-bracket pairs must
  hug their contents and contraction apostrophes stay out of quote
  pairing. (#128)
- **Configurable file-link command** — file links can open with a custom
  command instead of the default editor. Contributed by @ayamir. (#143)
- **Agent resume keeps launch flags** — resuming an agent session
  carries the flags the agent was originally launched with onto the
  resume command. (#144)
- **Sidebar git line follows the agent's cwd** — the branch/status line
  under an agent tab tracks the directory the agent is actually working
  in, not just the pane's shell. (#127)

### Changed

- **Tab close button floats over the label** — the close affordance no
  longer reserves a slot in every tab, so labels get the full width
  until hovered. (#126)
- **Dependency updates** — resvg 0.47, sha2 0.11, a cargo minor-patch
  group bump, and newer CI artifact actions. (#137, #138, #139, #140,
  #141)

### Fixed

- **Tab completion falls through to the shell** — a Tab tty7 has no
  candidates for now hands the line to the shell's own completion
  instead of being swallowed (remote panes included); `cd`/`pushd` stop
  offering files; and the whole feature can be turned off with
  `tab_completion` in config or Settings → Terminal → Keyboard. (#146)
- **Remote panes never leak local filesystem operations** — a remote
  pane's cwd is no longer used for local path lookups, and the agent-cwd
  bypass validates inherited directories. (#133)
- **macOS text input goes through the IME** — synthesized keystrokes
  keep their text (remote/automated input no longer degrades), and Kitty
  full mode stays off the IME path. (#132)
- **No more console flashes on Windows git probes** — git status probes
  and ssh ProxyCommand children no longer flash console windows. (#129)
- **Windows titlebar menu spacing** — the titlebar "..." aligns with the
  window-control rhythm. (#131)
- **Codex avatar and macOS tray sizing** — the Codex tab avatar uses its
  black brand field, and the tray icon renders at 22pt instead of the
  hardcoded 18pt. (#142)
- **Agents settings layout** — rows stay aligned and notes terse. (#125)

## [26.7.1] - 2026-07-17

### Added

- **Follow the OS appearance** — a "Sync with system" mode with separate
  light and dark theme slots: the theme flips live when the OS switches
  appearance, native chrome follows along, and picking a theme while
  syncing writes the slot matching the current mode. Old configs are
  unchanged (sync defaults off). (#121)
- **Mark as Unread on tabs** — the tab context menu can re-flag a finished
  agent result you've already looked at, re-arming the unread badge on the
  Done dot. Agent tabs only; disabled while the agent is still working.
  (#120)

### Changed

- **"Duo" logo refresh** — a new brand mark (two offset session panes,
  mint behind ink, with a prompt chevron) replaces the orange window
  identity across every icon asset: app icon, logo, tray glyph, favicon,
  and social preview. Bare macOS binaries (`cargo run`) now show the icon
  in the Dock too. (#124)

### Fixed

- **Linked git worktrees group under their main repository** — the sidebar
  keys groups on the repository home instead of each worktree's own root,
  so a repo and its worktrees share one header while branch status stays
  per-worktree. (#118)
- **macOS tray icon stays a template image in every state** — the
  attention state no longer swaps in a grey non-template glyph that was
  illegible on many menu-bar appearances; agent status lives in the
  tooltip and tray menu. (#122)
- **No more console-window flashes from Windows agent hooks** — each hook
  emitter frees its throwaway console before it can paint (debug builds
  only; release builds were unaffected). (#119)

## [26.7.0] - 2026-07-16

First CalVer release: versions are now `YY.M.PATCH`, so the number says when
it shipped rather than what changed.

### Added

- **Coding-agent detection on Windows** — the agent status dot now works on
  Windows: agents are detected from the shell-integration command capture
  (no `/proc` there), hook status events reach the daemon via the agent's
  ancestor console, and mark-derived detection only re-fires when the
  command capture actually changes. (#115)
- **Clipboard image paste for agents off macOS** — pasting a screenshot into
  a coding-agent pane on Windows/Linux stages the image to a temp file and
  pastes its shell-escaped path (the same route drag-and-drop uses); Windows
  BMP screenshots are transcoded to PNG since agent vision rejects BMP.
  macOS keeps the higher-fidelity pasteboard read. (#117)
- **Nightly build channel** — `main` is built every night into a rolling
  `nightly` prerelease with all six platform artifacts; the in-app update
  check is prerelease-aware so nightly users ride the channel while stable
  users never see it. (#114, #116)
- **Sidebar groups tabs by git repository** — vertical tabs cluster under a
  repo header, groups persist across restarts, ⌘N numbering follows visual
  order, and same-named repos are disambiguated by their parent directory.
  (#110)
- **System tray icon with agent status menu** — a tray/menu-bar icon
  summarizes agent activity across sessions and jumps to a pane from its
  menu. (#105, #109)
- **Gradient and image theme backgrounds** — themes can render gradient or
  image backgrounds with global window opacity and blur, hot-reloaded like
  the rest of the theme. (#106)
- **Double-click a tab to zoom the window** — matching titlebar behavior;
  rename moved to the context menu. (#103)

### Changed

- **Settings sheet refinements** — controls right-align in their rows with
  hover feedback, the column is tighter, and the theme card is richer.
  (#108)
- **Settings copy polish** — clearer wording throughout, and the SSH
  security defaults stay visible instead of hiding behind a toggle. (#104)

### Fixed

- **Ctrl+C after a copy sends SIGINT again** — copying with Ctrl+C and
  pasting to the PTY now consume the selection, so the next Ctrl+C reaches
  the program instead of copying the same text twice. (#113)
- **Shell vi mode is supported** — vi-mode prompts are detected via durable
  signals and no longer confuse the prompt gap hold. (#102, thanks @ayamir)
- **App cursor shape is respected** — programs that set the cursor shape
  (e.g. vim) see it honored. (#101, thanks @ayamir)

## [0.17.0] - 2026-07-16

### Added

- **Per-tab context menu with worktree tabs** — right-clicking a tab chip or
  sidebar row opens a menu: rename, splits, copy working directory, and a
  close group. *New Worktree Tab* creates a git worktree under the repo's own
  `.tty7/worktrees/<name>` (kept out of `git status` by a self-ignoring
  `.gitignore`), with editable name / branch / start point and a live path
  preview. Closing a tab that sat in a managed worktree offers to remove the
  checkout — unless another pane still lives in it, and dirty checkouts
  default to keeping. (#96)
- **Unread pane count in the tab status dot** — a split tab can finish
  several agent turns while you're away; the green Done dot now swells into
  a badge counting the unread panes (clamped at 9) and shrinks back once
  every pane has been seen. (#98)

### Changed

- **The sidebar diff overlay is per-tab and side-by-side** — the overlay now
  lives on its tab: switching away hides it, switching back restores it
  (re-probing when the status cache disagrees), closing the tab drops it,
  and Esc keeps working. The body switches from a unified diff to a
  GitHub-PR-style side-by-side view with positionally aligned
  removed / added columns. (#100)
- **macOS-style popup panels** — menus get a rounded pill highlight, inset
  hairline separators, a 10px panel radius, and a floatier shadow;
  searchable lists get a taller Spotlight-style search row, and the palette
  viewport holds a whole number of rows so the last visible row is never
  cut mid-height. (#97)
- **README rewritten as a minimal index** — feature details, keybindings,
  and performance notes moved to `docs/features.md` (en + zh-CN); the
  tagline now positions tty7 as a terminal workbench.

### Fixed

- **The agent dot no longer sticks on Waiting after you approve a
  permission** — Claude Code has no "permission replied" hook, so a new
  PostToolUse hook emits a tool-complete event and the first tool that
  finishes after approval flips the dot back to Working. Existing hook
  installs surface as Outdated in Settings → Agents with an Update
  button. (#99)

## [0.16.1] - 2026-07-15

### Fixed

- **Sidebar diff overlay only opens from the `+N`/`−N` counts** — clicking
  anywhere on a tab row's git line (branch icon, branch name) used to toggle
  the diff overlay, hijacking ordinary clicks on the lower half of the row.
  Now only the diff counts are the click target; the rest of the line
  activates the tab like the rest of the row. (#95)

## [0.16.0] - 2026-07-15

### Added

- **Click a sidebar git line to open a working-tree diff overlay** — the
  branch/diff row in the sidebar is now clickable and opens an in-app overlay
  showing the working-tree diff against `HEAD`, file by file with expandable
  hunks. It rides the shared git-status signal: when fresh numbers land that
  disagree with what it shows, it re-probes the full diff so the overlay stays
  live. (#92)
- **Window size and position are remembered across launches** — tty7 saves
  the window geometry on quit and restores it next launch, re-centering if the
  saved bounds no longer overlap any display. Can be toggled off with the
  `remember_window_size` config key. (#94)

### Fixed

- **Attach replay no longer duplicates TUI output into scrollback** — the
  daemon's replay ring is now segmented by the geometry its bytes were
  recorded under, and attach replays a `Size` → `Snapshot` pair per segment.
  Previously the whole ring replayed at the final PTY width, so any resize
  during a session (a pane split, a window drag) re-wrapped older output and
  a TUI's cursor-up redraws (Claude Code's inline renderer, most visibly)
  landed mid-frame, flooding the reattached pane's scrollback with stale
  frame copies that never existed live. The ring also caps its segment count
  so a long-lived session with many resizes can't grow it without bound. (#91)
- **Agent hooks no longer hang when stdin is a terminal** — the hook runner
  skips the stdin read when fd 0 is a tty, so a hook invoked interactively
  (rather than with a piped payload) emits the bare event instead of blocking
  forever on a read that never returns. (#93)

## [0.15.0] - 2026-07-15

### Added

- **Daemon protocol version handshake** — the GUI asks a running daemon
  which wire protocol it speaks before reusing it; after an app upgrade a
  mismatched daemon is kept alive and a prompt offers Keep Sessions or
  Restart Daemon instead of silently killing every persisted session. (#90)

### Changed

- **Tab close affordance** hides until hover on the active tab too, so the
  sidebar and tab strip read clean. (#90)
- **Command palette** no longer offers the Claude-only hook install entry;
  Settings → Agents owns hook installs with per-agent state.

### Fixed

- **SSH connection state** shows as a corner status dot on the tab avatar
  (amber connecting, green connected, red failed) in the same semantic
  colors as agent dots, replacing a theme-grey border ring that read as no
  state at all. (#90)
- **Sidebar git branch/diff line** is shared per repository: panes in one
  work tree read one snapshot refreshed by whichever pane probed last, so
  rows for the same directory no longer show stale or missing +/− counts.
  (#90)
- **Command palette** no longer pins Connect/Save rows above command
  matches for bare words like `java`; QuickConnect rows require a
  host-like query (`@`, `:` or `.`). (#90)

## [0.15.0-beta.1] - 2026-07-15

### Added

- Per-agent hook integrations (Claude Code, Codex, Copilot CLI, OpenCode,
  Pi) with install state and actions in Settings → Agents. (#87)
- CLI coding agents and the git branch are recognized and shown in the
  sidebar. (#85)
- Multi-line prompt editor, plus an I-beam mouse pointer over text. (#80)
- SSH: Unix GSSAPI (Kerberos) authentication. (#81)

### Changed

- Splitting an SSH pane opens another SSH pane on the same host. (#83)

### Fixed

- The grid shifts up when wrapped command input overflows the bottom of
  the screen, keeping the caret visible. (#86)
- Each tab keeps its own active pane across tab switches. (#84)
- The theme panel stays on-screen on narrow windows. (#82)

## [0.14.0] - 2026-07-14

### Added

- SSH connection manager: a native russh client with saved connection
  profiles, password and public-key auth, port forwarding, and SFTP. (#74)
- Buffer search overhaul — richer in-terminal search with rebindable,
  cross-platform shortcuts. (#75)
- Vertical tab sidebar, and Settings reworked into a full-window page. (#70)
- Tab title now follows the active pane. (#73)
- New Settings controls: bell, notify threshold, mouse reporting, and
  session restore. (#68)

### Changed

- SSH saved profiles are now the single source of truth for connections. (#77)
- Bump memchr 2.8.2 → 2.8.3. (#69)

### Fixed

- SSH auth-sheet polish and softer primary buttons. (#79)
- The Cmd+F find bar now owns the top-right slot over the SSH action
  icons. (#76, #78)
- Windows: stop the daemon before install/uninstall so it can replace
  `tty7.exe`. (#72)
- Moved SSH forwards into the pane context. (#71)

## [0.13.0] - 2026-07-13

### Added

- SSH loopback forwarding for links. (#58)
- Editable keybindings: rebindable shortcuts, pane/tab actions, and a tmux
  preset. (#65)

### Fixed

- Trim the first tab's left gap flush to the traffic-light reserve. (#62)
- Remove the active-pane corner indicator dot. (#63)
- CI: format code and platform-gate the ctrl glyph in the keymap test. (#67)

## [0.12.0] - 2026-07-13

### Added

- Ship a Linux AppImage alongside the tarball. (#55)
- Title-bar overflow menu for the command palette and settings. (#57)

### Changed

- Redesigned theme picker with a slide-in panel. (#56)

### Fixed

- Stop the title-bar strip from clipping the Windows close button. (#60)
- Restore the original terminal-window logo, reverting the branding
  change. (#59)

## [0.11.0] - 2026-07-12

### Added

- File-based themes, an in-app theme editor, and a UI/branding refresh. (#54)

## [0.10.0] - 2026-07-11

### Added

- Tab completion now executes the completion specs' *dynamic generators*:
  positions whose candidates come from the live system get real values — git
  branches on `git checkout <Tab>`, container names for docker/podman,
  `package.json` scripts for npm/pnpm/bun/yarn, cargo/rustup/tmux/brew/apt/pip
  listings, and more. Scripts run off the main thread in the session's cwd
  (800 ms timeout, output capped, short-lived cache) and their results merge
  into the already-open menu as they arrive; a slow or failing generator just
  contributes nothing. (#52)
- When shell integration never engages in a pane, pressing Ctrl+R now explains
  why the history menu can't appear (once per pane, dismissed by the next
  keystroke) instead of failing silently — naming the wrapper when a
  figterm-style PTY shim (`kiro-cli-term`, `figterm`, `qterm`) is intercepting
  the shell's OSC 133 reports. The chord still reaches the shell, so its own
  reverse-i-search keeps working. (#46)

### Fixed

- `ssh <Tab>` (and scp/sftp/rsync) now completes host aliases from
  `~/.ssh/config` — `Include` files honored, wildcard patterns skipped — and
  hosts from `known_hosts`, instead of falling back to listing the current
  directory. (#51)

## [0.9.0] - 2026-07-10

### Changed

- Ctrl+R history search is now a browsable menu: matching is fuzzy
  (subsequence with word-boundary/consecutive bonuses; space-separated terms
  must all match) blended with frecency, and the ranked candidates float
  beside the prompt — matched characters highlighted, selection moved by
  Ctrl+R/↓ and Ctrl+S/↑, Enter to edit, Cmd+Enter to run outright. An empty
  query lists the whole history by frecency, so bare Ctrl+R is a "recent &
  relevant" browser. The classic `(reverse-i-search)` line stays. (#45)
- History records now carry when the command ran and its exit code: new
  entries are `<ts>\t<exit>\t<cwd>\t<command>`, written when the command
  *finishes* (zsh `INC_APPEND_HISTORY_TIME`-style, exit code sniffed from
  OSC 133;D daemon-side); older formats still load. The Ctrl+R menu shows
  "ran 3h ago" and a `✗` badge on commands whose last run failed; timestamps
  from zsh/bash history files are carried over when seeding. (#45)

## [0.8.0] - 2026-07-10

### Added

- Copy on select: an opt-in Settings → Terminal → Clipboard toggle (config
  key `copy_on_select`) that copies a mouse selection — drag, double-click
  word, or triple-click line, over terminal output or the prompt's command
  editor — to the clipboard the moment the gesture ends, no ⌘C needed. Off
  by default so a stray selection never overwrites the clipboard. (#34)

### Fixed

- The held-⌘ tab-number badges no longer stick on after the window loses key
  status mid-hold (⌘-Tab, Spotlight, a click into another app). The ⌘ release
  goes to whatever app is key by then, so the window never saw it; the badges
  — and any pending reveal — are now dismissed on the activation flip itself.

## [0.7.0] - 2026-07-10

### Added

- Terminal ANSI colors (`color0`–`color15`) can now be overridden individually
  via `ansi_colors.*` in `config.json`, layered on top of the active theme
  preset — with a color picker per slot under Settings → Appearance → ANSI
  Colors. Malformed values are ignored, and clearing an override falls back to
  the preset's palette. (#37)

- Font ligatures can now be enabled for terminal text. A new optional
  `font_features` config passes OpenType features (e.g. `{"calt": true}`)
  through to the renderer, and Settings → Appearance grows a toggle for the
  common `calt`/`liga` pair. Ligatures stay disabled by default for cell-grid
  safety, and changes hot-apply to open panes. (#38)

### Fixed

- Ctrl+L now clears the screen while the prompt-local line editor is active.
  The readline dispatcher used to swallow it as an unrecognized chord; it now
  forwards the same form-feed byte the raw terminal path sends, so the shell
  repaints its prompt as expected. (#36)

## [0.6.2] - 2026-07-08

### Changed

- Context menus and the "+" dropdown now highlight the hovered row with the
  same soft fill the command palette uses for its selected row, instead of the
  stock saturated accent that snapped hard against the rest of the UI. The
  hover text stays at the normal foreground so it reads clearly on the quieter
  fill.

### Fixed

- On Windows, a pane no longer hangs open when its shell exits on its own.
  Typing `exit`, pressing Ctrl-D, or a shell crash ends the shell, but ConPTY's
  output pipe never reports EOF on a natural exit — and tty7 detected a shell's
  death solely from that EOF — so the pane was left wedged open, dead but
  visible. A Windows-only monitor now waits on the shell process directly and
  reports the exit through the same path a Unix `read()` EOF drives, so the pane
  closes as it does everywhere else. Closing a tab from the UI was already
  unaffected; macOS and Linux are unchanged. (#30)

- Nerd Font prompt icons no longer render sliced off on the right. A non-Mono
  Nerd Font (and the proportional `➜`/`❯` the OS cascade hands back when nothing
  in your font list covers them) gives an icon a single-cell *advance* but draws
  ink up to ~1.9 cells wide, and tty7 clipped every lone glyph to exactly one
  cell — severing the overflow into the half-icons and cut-off arrow from the
  report. A single glyph now paints into a two-cell window, so it renders whole
  (bleeding into the trailing blank the way iTerm2 and Terminal.app do), bounded
  at two cells so a stray face can't smear across the row. Pairs with the native
  powerline separators from #19; Mono Nerd Fonts are unchanged. (#17)

- New tabs and splits no longer stall for seconds while a zsh plugin manager
  reinstalls itself. tty7 launches zsh through a throwaway `ZDOTDIR` (so it can
  layer its shell integration on top of your config), but it used to leave
  `ZDOTDIR` pointing at that empty temp dir the whole time — so tools that find
  their own state via `${ZDOTDIR:-$HOME}` (Zim, oh-my-zsh, `compinit`'s
  `.zcompdump`) looked in the wrong place and rebuilt from scratch on every
  pane, e.g. Zim reprinting `modules/…: Installed` and hanging for ~3s. Each
  redirector now points `ZDOTDIR` back at your real config dir while your
  startup files run, and restores it for the live session, so plugin managers
  and completion caches resolve correctly and load instantly. As a bonus this
  also fixes the classic relocated-config layout (a tiny `~/.zshenv` that sets
  `ZDOTDIR=~/.config/zsh`), which previously loaded your config but silently
  dropped tty7's integration. (#15)

## [0.6.1] - 2026-07-08

### Fixed

- Tab completion (and other line editing) now stays out of the way over `ssh`.
  A remote shell that emits its own prompt marks — fish 4.x on a Linux server,
  most visibly, which ships OSC 133 on by default — used to engage tty7's
  *local* line editor, so Tab ran completion against the local machine's
  filesystem instead of reaching the remote shell. tty7 now only drives the
  inline editor while the shell it launched is itself idle at its prompt;
  whenever a foreground command (ssh, a TUI, a nested shell) owns the terminal,
  keystrokes pass straight through to it. (#26, follow-up to #18)

## [0.6.0] - 2026-07-08

### Added

- The "+" button now opens a shell picker: tty7 detects the shells installed
  on this machine (on Unix the login shell, `/etc/shells`, plus well-known
  shells found on `PATH` — fish, nushell, pwsh and friends installed by
  Homebrew/nix are never registered in `/etc/shells`; on Windows PowerShell 7,
  Windows PowerShell, Command Prompt, Git Bash and WSL distributions)
  and lists them in a dropdown, so opening a tab in a different shell
  no longer requires editing `config.json`. The default entry leads the menu,
  ⌘T / Ctrl+T still opens a default tab in one keystroke, and splitting a pane
  inherits its shell — a fish tab splits into more fish, not back to the
  default. Shells picked this way aren't remembered across restarts (restored
  panes re-attach to their still-running shells anyway).

### Changed

- The Windows default shell now prefers PowerShell 7 (`pwsh.exe`) when
  installed — probed across Program Files (x64/x86/ARM), the Microsoft Store,
  scoop, dotnet tools and `PATH` — and falls back to Windows PowerShell as
  before. Set `shell` in `config.json` to override, as ever.

### Fixed

- Powerline prompt separators (powerlevel10k, oh-my-posh, oh-my-zsh) now render
  pixel-perfect at any font, size and line-height: the eight solid separators
  (sharp triangles, round caps, slants) are drawn natively as fill paths sized
  to the exact cell instead of relying on a Nerd Font, so segments meet their
  backgrounds cleanly with no gaps, narrow wedges or tofu. The bundled Hack font
  is also appended to every font-fallback chain, so common prompt glyphs (➜, ❯,
  box drawing) no longer render truncated or missing when no Nerd Font is
  installed. (#17)
- A URL glued directly to a full-width bracket with no space — e.g.
  `…/pull/343（fix/… → dev）` — no longer swallows the bracket text into the
  link. URL detection now stops at the first non-URL character (a CJK glyph,
  full-width bracket, arrow or emoji), while interior ASCII parens
  (Wikipedia-style URLs) are still preserved.
- Fish, nushell, pwsh and other shells installed by Homebrew or nix now appear
  in the "+" shell picker even when they aren't registered in `/etc/shells`
  (which those package managers leave to the user): a curated set of well-known
  shells is now probed on `PATH` as a catch-all, after the `/etc/shells`
  entries. (#18)
- Upgrading tty7 while an older daemon is still running in the background no
  longer breaks new tabs. A stale daemon that accepts the connection but can't
  serve the new client's request is now restarted once and retried
  automatically; on macOS the GUI also forwards the shell it was launched with
  to the detached daemon, so panes use the right shell instead of a stale
  `$SHELL` inherited from LaunchServices.

## [0.5.0] - 2026-07-07

### Added

- Windows releases now ship an Inno Setup installer
  (`tty7-<version>-windows-x86_64-setup.exe`) alongside the portable zip. It
  installs per-user by default (no admin prompt, with an all-users option),
  adds a Start Menu shortcut and an "Apps" uninstall entry, and offers an
  optional desktop icon. Still unsigned, so SmartScreen warns on first launch.
- Startup update check: tty7 asks GitHub once, in the background, whether a
  newer release has shipped. If so, it pops a one-time "Update available" dialog
  (once per version — remembered in `update.json`, so it never nags twice for
  the same release) and keeps a persistent "Download" prompt in Settings →
  About. Both open the Releases page; tty7 never downloads or updates itself —
  you still install by hand. Turn the check off with `check_for_updates` in
  `config.json` or the "Check for updates on launch" toggle in About. A failed
  or offline check is silent.
- ⌘K (Ctrl+K on Windows/Linux) clears the screen and scrollback — the same
  "Clear" the right-click menu already offered, now on the keyboard shortcut
  Terminal.app, iTerm2, and Ghostty users expect. Also available from the
  command palette, and remappable as `ClearScrollback` in `keybindings`.
- ⌘⏎ toggles window fullscreen (new `ToggleFullscreen` action, also in the
  View menu and command palette), matching the Ghostty/iTerm2 default. It
  previously toggled pane maximize — which silently did nothing in a
  single-pane tab, so the chord felt dead.
- The right-click menu now shows each item's keyboard shortcut. Copy, Paste,
  Select All, and Find previously showed nothing (they're dispatched inline,
  with no bound key for the menu to read a hint from) while the other items
  did, so the menu looked half-labelled. ⌘A / ⌘F stay hint-less on
  Windows/Linux, where those chords keep their readline meaning.

### Changed

- Maximize / restore pane moved from ⌘⏎ to ⌘⇧⏎ (Ghostty's `toggle_split_zoom`
  default), making room for fullscreen on the bare chord. An existing
  `ToggleMaximizePane` override in `keybindings` still wins.

### Fixed

- Windows: launching tty7 no longer opens a stray console window behind the
  app. Release builds are now linked with the `windows` subsystem; debug
  builds keep the console so `println!` output stays visible. (#10)
- The right-click "Select All" now matches the ⌘A shortcut: at the prompt it
  selects the edited command line, otherwise the whole terminal buffer. It
  previously always selected the whole buffer, so click and keystroke behaved
  differently at the prompt.
- Ctrl+R reverse-search now accepts plain ASCII keystrokes. The query only
  took text from the IME commit path, so a non-CJK input source on macOS — and
  all typing on Linux — was swallowed: the search box opened but ate every key.
  Reported on V2EX.

## [0.3.0] - 2026-07-07

### Added

- PowerShell shell integration: `powershell.exe` and `pwsh` now emit the OSC 133
  semantic-prompt marks and OSC 7 cwd that zsh/bash/fish already do, injected
  via `-EncodedCommand` after the user's profile loads (their config is never
  touched). This turns on the inline line editor at the PowerShell prompt — so
  clicking positions the caret and new tabs/splits inherit the working
  directory — which is what previously made mouse clicks a no-op at the prompt
  on Windows.

### Fixed

- Typing `exit` (or Ctrl-D) left a dead "process exited" pane behind instead
  of closing it. A pane whose shell genuinely ends now closes itself —
  collapsing its split, or closing the tab when it was the only pane (the
  last tab falls back to the home page), like every other terminal. A pane
  that merely *lost its daemon connection* still stays visible: auto-closing
  those would silently discard — and kill — sessions that may still be alive
  daemon-side. Panes that died while detached clean themselves up on the next
  attach the same way.

- A full-screen TUI dying without restoring the terminal — the canonical case
  being an ssh session dropping mid-`htop`/`vim` — left the pane stranded on
  the alt screen with a hidden cursor and live mouse reporting: a visible
  prompt with no cursor anywhere, mouse clicks echoing `0;19;42M`-style junk,
  and broken scrollback. The client now scrubs this residue the moment the
  shell reports its next prompt (OSC 133): it leaves the stranded alt screen,
  re-shows the DECTCEM-hidden cursor, and disables stale mouse/focus reporting
  and kitty keyboard flags — each reset only when its mode is actually set.
  Reattach self-heals the same way, since the daemon replays the prompt state
  after the ring.

- Windows shell integration never engaged even for the default shell: detection
  keyed off `portable-pty`'s `get_shell()`, which reports `%ComSpec%` (cmd.exe)
  regardless of what's actually spawned, so the PowerShell default was mistaken
  for an unsupported shell. It now resolves to `powershell.exe` directly.

## [0.2.0] - 2026-07-04

### Added

- Underline styles: undercurl, double, dotted, and dashed underlines render distinctly.
- `config.json` hot reload — edits apply to the running app without a restart.
- Desktop notifications driven by OSC 9 / OSC 777 escape sequences.
- Kitty keyboard protocol (CSI u progressive enhancement) for TUI apps like Neovim and Helix.
- Shell integration for bash and fish, alongside the existing zsh support.
- Windows support: cross-platform daemon, PowerShell as the default shell, embedded app icon.
- Linux support: builds against gpui's x11/wayland backends, `/proc`-based foreground cwd + pane-title tracking, Linux CI job, and documented build dependencies.
- Downloadable builds for every platform: the release workflow now packages and uploads all four targets — signed/notarized macOS DMGs (arm64 + x86_64) plus unsigned archives for Windows (`.zip`) and Linux (`.tar.gz`), each via its own `.github/scripts/bundle-<os>` script.
- Settings UI: terminal / appearance / behavior options are configurable from the GUI, with a searchable font-family dropdown and a wider theme gallery.
- Configurable default shell.

### Changed

- Project renamed to **tty7**.
- macOS releases ship as drag-to-Applications DMGs instead of zips, and the
  Intel build moved to the `macos-15-intel` runner (`macos-13` was retired,
  which had silently kept x86_64 assets from ever publishing).
- Pixel-smooth scrollback: scrolling carries a sub-line fraction and shifts the paint instead of jumping whole lines.
- Smoother scrolling on dense screens: glyph shaping is batched and wakeups are coalesced.
- CJK-dense screens paint ~2.4× faster: consecutive wide glyphs batch into single shaped runs (two columns per glyph) instead of painting cell-by-cell; the grid snapshot buffer is reused across frames and the selection/search overlay scans are skipped when nothing is highlighted. Release builds now use thin LTO.
- Type-ahead is integrated into the line editor instead of being stranded on zle's line.
- New tabs open next to the active tab instead of at the end.
- Terminal throughput ~12× faster (11 MB `cat`: ~2.0 s → ~0.16 s; DOOM-fire: ~47 fps → ~920 fps, both at 155×40 on an M1 Pro — now ahead of Alacritty/Ghostty on the same machine): the daemon's replay ring is a `VecDeque` so a full ring no longer memmoves 8 MiB per ~1 KiB PTY read, and the per-connection writer coalesces queued `Output` frames (≤256 KiB) so a flood reaches the client as a few large frames instead of thousands of tiny ones. A backpressure gate (4 MiB high-water) pauses the PTY reader while the client catches up, so a runaway `yes` can't grow daemon memory without bound. `TTY7_TRACE=1` prints per-second reader-loop accounting on both sides for future diagnosis.
- Second throughput pass, another ~1.4× on bulk output (11 MB `cat`: ~160 ms → ~100 ms; sustained plaintext drain 124 → 148 MB/s, vs ~170 MB/s for a raw do-nothing PTY reader on the same machine; DOOM-fire is unchanged — it is producer-bound at ~96 MB/s): the backpressure high-water grows to 16 MiB so a big burst drains at PTY speed while the client parses in its own time; daemon⇄GUI socket buffers grow from macOS's 8 KiB default to 256 KiB; the client applies consecutive `Output` frames as one batched parser pass (one term-lock + wakeup per burst, latency-free — the batch never waits for unarrived bytes); the shared OSC tokenizer skips Ground/Ignore runs with SIMD `memchr`; the gate's hot path is a lock-free atomic (previously a Mutex plus an unconditional `notify_all` per socket write); and the four threads on the interactive output path ask macOS for `USER_INTERACTIVE` QoS to stay off the efficiency cores (`TTY7_NO_QOS=1` opts out).

### Fixed

- A long `--config-dir` path crashed the GUI at startup ("path must be shorter than SUN_LEN"): when `<config>/daemon.sock` would exceed the OS socket-path limit (104 bytes on macOS), the endpoint now falls back to a short per-user path keyed by a stable hash of the config dir ($XDG_RUNTIME_DIR, else the OS temp dir). Short paths keep the original layout, so existing daemons stay reachable.
- Typing right after a command finished could leave a stray echoed character plus zsh's reverse-video `%` in the scrollback: the "command finished" mark (OSC 133;D) is now emitted the instant the command exits — prepended ahead of the user's precmd hooks (zsh/bash) — instead of after slow prompt frameworks (oh-my-zsh git status, conda), so the local input editor takes keystrokes back hundreds of milliseconds sooner.
- Typing while a command was still running stranded those keystrokes on zle's line at the next prompt — un-editable and double-drawn under the line editor's overlay. Type-ahead adoption (wipe the shell's line, seed the editor) now runs at every prompt, not just the shell's first, and the wipe waits until zle is actually reading (the live `133;B` mark) so it is consumed silently instead of being kernel-echoed into the scrollback as a literal `^U`.
- Typing ahead of a fast command left kernel-echoed debris in the scrollback (`ls` plus zsh's reverse-video `%`). Reconstructable gap input is now held client-side for up to 150 ms: a command that finishes inside the window hands the keystrokes straight to the line editor with the PTY untouched — zero echo; a longer command (or one reading stdin) gets the bytes released verbatim, so REPLs and password prompts still work.
- fish shell integration silently never installed, so fish users got no prompt marks or cwd tracking.
- **Security:** pasted clipboard content is stripped of ESC bytes, closing a bracketed-paste escape that could inject auto-executing commands.
- Crash when copying/cutting right after a forward word/line delete left a stale selection anchor.
- `Ctrl+Alt+<letter>` was indistinguishable from `Ctrl+<letter>` because the legacy key encoder dropped the Alt ESC prefix.
- Plain Enter/Tab/Backspace were wrongly CSI-u-encoded at the kitty-keyboard DISAMBIGUATE level, which could wedge the shell after a crashed TUI.
- No-op edits (e.g. Backspace at the start of the line) no longer swallow the first undo.
- OSC scanners (daemon-side and notification-side) dropped a well-formed sequence that directly followed an unterminated one.
- Daemon pane teardown is hardened: process-group kill, bounded join, dead panes are reclaimed.
- New shells default to `$HOME` when launched from the app bundle with cwd `/`.

## [0.1.0] - 2026-06-30

Initial release.

- Sessions live in a persistent daemon and survive window close / app restart.
- GPU-rendered terminal grid on [gpui], backed by Zed's `alacritty_terminal` fork.
- Tabs and pane splits (split right/down, maximize, focus movement).
- Command palette with fuzzy search over every action.
- Smart line editing: inline completion, syntax highlighting, history, in-terminal search.
- zsh shell integration (OSC 7 cwd + OSC 133 prompt marks) via a throwaway `ZDOTDIR`.
- Native macOS light/dark themes that follow the system appearance.

[Unreleased]: https://github.com/l0ng-ai/tty7/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/l0ng-ai/tty7/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/l0ng-ai/tty7/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/l0ng-ai/tty7/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/l0ng-ai/tty7/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/l0ng-ai/tty7/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/l0ng-ai/tty7/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/l0ng-ai/tty7/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/l0ng-ai/tty7/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/l0ng-ai/tty7/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/l0ng-ai/tty7/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/l0ng-ai/tty7/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/l0ng-ai/tty7/releases/tag/v0.1.0
[gpui]: https://github.com/zed-industries/zed
