# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

One design language on both phones: the app is a Tauri 2 WebView shipped to iOS and
Android, deliberately not styled per OS. It must still behave like an installed app —
safe areas, touch targets, no web-page chrome — rather than a website in a frame.

## Users

People who run tty7 on their desktop (often with coding agents such as Claude Code or
Codex in its panes) and are away from that desk. The primary job is **operating a
terminal remotely**: open a pane on the desktop and type into it from the phone —
run commands, answer an agent, interrupt something.

## Product Purpose

tty7 mobile lets a paired phone watch and drive the desktop's tty7 panes. Success is
reaching a pane and typing into it within seconds, from anywhere, without a VPN or
port forwarding.

## Positioning

The phone is a second window onto the same live panes, not an SSH client: it sees the
desktop's workspaces, tabs, panes and agent status exactly as tty7 organises them, and
connects peer-to-peer over iroh by public key, end-to-end encrypted.

## Operating Context

- Pairing: `tty7-gateway pair` on the desktop prints a QR code and a `tty7pair:` code;
  the app currently accepts the pasted code (QR scanning is planned).
- A paired machine shows its tree live: workspaces → tabs → panes, with each pane's
  title, cwd, and agent status, under the workspace it belongs to.
- A pane is **observed, not attached**: it keeps the desktop's size (cols × rows), and
  the phone fits that width by shrinking the font. Input goes in beside the desktop.
- **Take over** runs the pane at the phone's size until the phone lets go or leaves. The
  desktop keeps its window, says the phone has the pane, and can **Take Back** at any
  time; only that explicit button takes it back, never typing on the desktop.
- Connection state matters and is shown: direct vs relay path, round-trip time,
  connecting, offline.

## Capabilities and Constraints

- Screens today: machines (paired list + pair form), one machine's tree, one pane's
  terminal (xterm.js) with an extra-keys bar (esc, tab, ⇧tab, ctrl latch, arrows, ^C, paste,
  | / ~ -) and a compose box that sends a whole message, then Enter. A copy view shows the pane's text
  for native selection.
- Forget a machine (requires re-pairing).
- Close a pane on the machine — hold its row, or Close pane in its ⋯ menu — after a
  confirmation, since it ends whatever runs there; the desktop loses it too.
- No framework: vanilla TypeScript with a tiny DOM helper; xterm.js for the terminal.
- Not yet: QR scanning, push notifications, keychain storage.

## Brand Commitments

- Carries the desktop tty7 look: its Light and Dark presets (following the system
  appearance), their ANSI palettes for the terminal, and the agent status dots —
  Working blue `#3B82F6`, Waiting amber `#F59E0B` drawn **hollow** so it differs in
  shape from Done, Done green `#22C55E`, Idle no dot.
- The "Duo" logo mark: green `#3FDD8C` tile behind a near-black `#17171A` tile with a
  light `›` chevron (`assets/logo.svg`).
- Agent brand icons exist in `assets/icons/agents/` (claude, codex, gemini, …).

## Evidence on Hand

Real data only comes from a paired desktop; there are no screenshots, testimonials or
metrics to show and none should be invented.

## Product Principles

1. The terminal is the product: every other screen is a way to reach a pane fast.
2. Same truth as the desktop: names, grouping, and status mean exactly what they mean
   on the desktop app.
3. Honest connection state: the user always knows whether keystrokes will land.
4. Never take the pane away from the desktop: a take-over only borrows its size, and the
   desktop can always take it back.
