---
name: tty7 mobile
description: A second window onto the desktop's live panes, laid out like an installed phone app in the desktop's own Light and Dark presets.
colors:
  accent: "#1f6bf0"
  accent-dark: "#78a8f5"
  accent-ink: "#ffffff"
  accent-ink-dark: "#0f1419"
  tint: "rgb(31 107 240 / 0.1)"
  tint-dark: "rgb(120 168 245 / 0.14)"
  working: "#3b82f6"
  waiting: "#f59e0b"
  done: "#22c55e"
  warn-ink: "#8a5a0c"
  warn-ink-dark: "#e3bd5d"
  ok-ink: "#1f7a47"
  ok-ink-dark: "#78bd95"
  danger: "#c43c43"
  danger-dark: "#f07878"
  canvas: "#f2f3f6"
  canvas-dark: "#0f1013"
  cell: "#ffffff"
  cell-dark: "#191b20"
  cell-press: "#e6e8ed"
  cell-press-dark: "#24272e"
  raised: "#ffffff"
  raised-dark: "#22252b"
  term: "#ffffff"
  term-dark: "#191b20"
  ink: "#0f1419"
  ink-dark: "#e2e5eb"
  ink-2: "#59616d"
  ink-2-dark: "#9ba2ae"
  ink-3: "#9aa1ab"
  ink-3-dark: "#5f6672"
  hair: "rgb(15 20 25 / 0.1)"
  hair-dark: "rgb(226 229 235 / 0.09)"
typography:
  display:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "34px"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.024em"
  headline:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 700
    letterSpacing: "-0.014em"
  title:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 600
    lineHeight: 1.35
  body:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.35
  body-row:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 500
  secondary:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Roboto, Segoe UI, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 600
  mono:
    fontFamily: "Hack, Menlo, ui-monospace, monospace"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  xs: "6px"
  sm: "10px"
  tile: "11px"
  md: "14px"
  lg: "16px"
  tile-lg: "18px"
  full: "9999px"
spacing:
  xxs: "4px"
  xs: "8px"
  sm: "12px"
  md: "16px"
  lg: "20px"
  group: "26px"
  touch: "44px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-ink}"
    typography: "{typography.title}"
    rounded: "{rounded.md}"
    padding: "0 26px"
    height: "50px"
  button-primary-disabled:
    backgroundColor: "{colors.hair}"
    textColor: "{colors.ink-2}"
    rounded: "{rounded.md}"
    height: "50px"
  button-tinted:
    backgroundColor: "{colors.tint}"
    textColor: "{colors.accent}"
    rounded: "{rounded.md}"
    height: "50px"
  button-tinted-small:
    backgroundColor: "{colors.tint}"
    textColor: "{colors.accent}"
    rounded: "{rounded.sm}"
    padding: "0 14px"
    height: "34px"
  chip:
    backgroundColor: "{colors.tint}"
    textColor: "{colors.accent}"
    rounded: "{rounded.full}"
    padding: "0 11px 0 8px"
    height: "28px"
  card:
    backgroundColor: "{colors.cell}"
    rounded: "{rounded.lg}"
  row:
    backgroundColor: "{colors.cell}"
    textColor: "{colors.ink}"
    typography: "{typography.body-row}"
    padding: "10px 12px 10px 14px"
    height: "62px"
  row-pressed:
    backgroundColor: "{colors.cell-press}"
  tile:
    backgroundColor: "{colors.tint}"
    textColor: "{colors.accent}"
    rounded: "{rounded.tile}"
    size: "38px"
  avatar:
    textColor: "{colors.ink-2}"
    rounded: "{rounded.full}"
    size: "38px"
  field-input:
    backgroundColor: "{colors.cell}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
    rounded: "{rounded.lg}"
    padding: "14px 16px"
  key:
    backgroundColor: "{colors.cell}"
    textColor: "{colors.ink}"
    typography: "{typography.mono}"
    rounded: "{rounded.sm}"
    padding: "0 9px"
    height: "40px"
  key-latched:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-ink}"
  menu:
    backgroundColor: "{colors.raised}"
    rounded: "{rounded.md}"
    width: "232px"
  notice:
    backgroundColor: "{colors.cell}"
    rounded: "{rounded.lg}"
    padding: "14px 16px"
---

# Design System: tty7 mobile

## Overview

**Creative North Star: "The Second Window"**

The phone is another window onto the same live panes the desktop shows, so it wears the desktop's clothes: the tty7 Light and Dark presets, following the system appearance, the same ANSI palettes in the terminal, the same agent avatars and status dots, the same status words. Around that it behaves like an installed phone app, not a web page: pushed screens that slide over one another, a large title that folds into the bar, inset grouped lists on a tinted canvas, and a row of keycaps above the keyboard. One neutral language serves iOS and Android alike; nothing is styled per OS.

Density is calm and list-shaped. Colour is almost entirely neutral; the one accent blue marks what can be tapped, and the only other colour on screen is information: an agent's brand field, its status dot, a connection dot, a warning. The terminal screen strips everything back to the pane itself, a slim header and the keybar.

**Key Characteristics:**
- Desktop tty7 Light/Dark presets, switched by `prefers-color-scheme`, never a toggle.
- Grouped white (or charcoal) cells on a faintly cool canvas; no borders around cards.
- One accent blue for interaction; status colours reserved for agent and connection state.
- System UI face for chrome, Hack for anything that is code, a path, or a key.
- Motion is the platform's push/pop slide and a few press responses, all off under reduced motion.

## Colors

A cool neutral canvas with white cells, one saturated blue for action, and a fixed set of desktop status colours that mean the same thing everywhere. Every neutral and the accent have a dark counterpart (the `-dark` tokens) that swaps in under `prefers-color-scheme: dark`; the status colours do not swap.

### Primary
- **tty7 Blue** (accent / accent-dark): the desktop preset's accent. Back links, the + and ⋯ bar icons, the primary button, the latched ctrl key, machine tiles, the "Working" status word, the terminal cursor. Paired with **Accent Ink** (accent-ink / accent-ink-dark) for text on a filled accent: white in light, near-black in dark because the dark accent is pale.
- **Blue Wash** (tint / tint-dark): the accent at 10% (14% in dark). The quiet fill behind tinted buttons, the Paste chip, pairing-step numerals and machine tiles.

### Secondary: the desktop status dots
Fixed across both themes, taken verbatim from the desktop's `cli_agent.rs`.
- **Working Blue** (working): the blinking badge on an agent that is running. Also distinct from tty7 Blue on purpose; it is the desktop's dot, not the app's accent.
- **Waiting Amber** (waiting): the badge on an agent that needs input, always drawn with a white hole so it differs from Done in shape, not only hue. Also the relay-path connection dot and the 13% wash behind warning notices.
- **Done Green** (done): the badge on a finished agent; also the Direct and Live connection dots.

### Tertiary: status ink
Status dots are too light to read as text on a white cell, so the words beside them use darker (in dark, lighter) inks.
- **Waiting Ink** (warn-ink / warn-ink-dark): the "Needs input" word and the icon in a warning notice.
- **Done Ink** (ok-ink / ok-ink-dark): the "Done" word.
- **Alarm Red** (danger / danger-dark): offline dot, "Closed"/"Offline" terminal state, field errors, the Forget menu item, the icon and 10% wash of an error notice.

### Neutral
- **Canvas** (canvas / canvas-dark): the screen background and the bar at rest. Also the `theme-color` of the OS status bar.
- **Cell** (cell / cell-dark): grouped-list cards, fields, keycaps, the ring around a status badge. Numerically identical to the desktop preset's background.
- **Cell Pressed** (cell-press / cell-press-dark): the instant fill of a row, menu item or key under the finger.
- **Raised** (raised / raised-dark): floating surfaces, the ⋯ menu and the terminal banner. White in light; a step lighter than the cell in dark, so it lifts without relying on the shadow.
- **Terminal** (term / term-dark): the pane background, identical to the desktop preset so a pane reads the same on both.
- **Ink** (ink / ink-dark): primary text. The desktop preset's foreground.
- **Ink 2** (ink-2 / ink-2-dark): secondary text: row subtitles, group titles, notes, placeholders, the terminal sub-line.
- **Ink 3** (ink-3 / ink-3-dark): non-text marks only: row chevrons, the idle connection dot.
- **Hairline** (hair / hair-dark): ink at ~10%. Row separators, the folded bar's underline, code and tag backgrounds, skeleton bones, the disabled primary fill.

The terminal carries the desktop presets' ANSI-16 palettes unchanged (light: GitHub-style `#24292e`…`#8c959f`; dark: pastel `#616161`…`#feffff`), with the accent as cursor and the accent at ~20–33% alpha as selection.

### Named Rules
**The Borrowed Palette Rule.** Every surface, ink, accent and ANSI value is the desktop preset's, or a step derived from it. A new colour needs a desktop counterpart first.

**The Status Is Not Decoration Rule.** Working blue, waiting amber and done green appear only to report agent or connection state. Never use them for emphasis, illustration or branding.

**The Shape-Not-Only-Hue Rule.** Waiting is the amber dot with a white hole (white in both themes, as the desktop draws it); Done is solid. Any new status marker must also differ by shape.

## Typography

**UI Font:** the system face (`-apple-system`, SF Pro Text, Roboto, Segoe UI, `system-ui`)
**Mono Font:** Hack (with Menlo, `ui-monospace`), bundled regular and bold, the desktop's own terminal face

**Character:** Native, unbranded chrome in the phone's own face, and the desktop's terminal font wherever the content is machine text. The contrast between the two is the whole typographic idea.

### Hierarchy
- **Display** (700, 34px, 1.12, −0.024em): the large screen title ("Machines", a machine's name, "Pair a machine"). It folds into a 17px/600 bar title as it scrolls away.
- **Headline** (700, 22px, −0.014em): empty-state titles.
- **Title** (600, 17px): bar title, buttons, notice titles (16px), terminal header title (16px).
- **Body** (400–500, 17px, 1.35): row titles at 500, field text and menu items at 400. 16px/1.45 for pairing steps and empty-state body (capped at 30ch).
- **Secondary** (400, 14–15px, 1.45): row subtitles, notice body, field errors; the connection line at 15px with tabular numerals.
- **Label** (600, 13px): group titles above cards, sentence case, never uppercase. Tags ("Asleep") at 11px/600.
- **Mono** (Hack 400, 14px): keycaps and the pairing-code field; inline `code` at 0.82em; the terminal header's cwd at 11.5px. The terminal itself runs Hack between 4px and 14px, never below 11px unless the user asks to fit the whole width.

### Named Rules
**The Code Wears Hack Rule.** Commands, paths, codes and keys are set in Hack; everything a person would say is set in the system face.

**The Sentence Case Rule.** Titles, labels and buttons are sentence case. Status words are the desktop's: "Working", "Needs input", "Done".

## Layout

A single column, full-width on the phone, with every edge padded by `env(safe-area-inset-*)` so nothing sits under a notch, home indicator or rounded corner. The page itself never scrolls; each screen is a fixed bar, one scrolling `main`, and an optional docked footer (the Pair button) or keybar.

- **The bar** is 48px plus the top inset, a three-column grid: back link, centred title, trailing icons. Tap targets in it are 44×44.
- **Title block** sits at 20px side padding, 18px beneath.
- **Groups** are inset 16px from the screen edge with 26px between them; the label sits 4px further in, 8px above its card.
- **Rows** are at least 62px tall, 12px gap between avatar and text, 14px leading pad. Separators start at 64px, aligned with the text, not the avatar.
- **The terminal** fills everything between its header and the keybar with 8px/6px padding; the keybar is 40px keys, 5px apart, 8px padding plus the bottom inset.
- The spacing rhythm is 4/8/12/16/20/26px; 44px is the minimum touch target.

**The Inset Group Rule.** Content lives in rounded cards inset from the edges on the canvas; nothing is a full-bleed bordered box.

## Elevation & Depth

Flat by default, with depth from tone: white cells on a cool canvas (charcoal on near-black in dark). Shadows appear only on things that float above the screen, and on keycaps to make them read as physical keys.

### Shadow Vocabulary
- **Float** (`0 12px 32px rgb(15 20 25 / 0.16), 0 2px 6px rgb(15 20 25 / 0.08)`; dark `0 12px 32px rgb(0 0 0 / 0.5), 0 2px 6px rgb(0 0 0 / 0.3)`): the ⋯ menu and the terminal banner.
- **Keycap** (`0 1px 1.5px rgb(0 0 0 / 0.18), inset 0 0 0 0.5px hair`): extra-keys only.
- **Hairline edge** (`0 0.5px 0 hair`): the folded bar and terminal header underline; the keybar's top edge (−0.5px).
- **Push edge** (`-12px 0 32px rgb(0 0 0 / 0.14)`): the leading edge of a screen sliding over another during navigation.

The folded bar and keybar are translucent-looking: cell mixed 78% into canvas.

**The Flat-At-Rest Rule.** Cards, rows, tiles and fields never carry a shadow. A shadow means "this is floating over the screen" or "this is a key".

## Shapes

Soft, continuous corners throughout, scaled with the element: 6px for inline code, tags and skeleton bones; 10px for keys, small buttons and the back link's press area; 11px for 38px machine tiles; 14px for buttons, the menu and the banner; 16px for cards and notices; 18px for the 64px empty-state tile. Agent and pane avatars, status badges, connection dots, the chip and step numerals are full circles or pills. There are no outline borders on containers; separation is by tone and 0.5px hairlines.

Icons follow the desktop's grammar: a 24-unit box, 1.8 stroke (2–2.2 for chevrons, arrows and back), round caps and joins, `currentColor`. Agent marks are the desktop's own SVGs, drawn as masks in the agent's ink on its brand field.

**The Square Tile, Round Face Rule.** A machine is a rounded-square tile; a pane is a circle. The shape alone tells a machine row from a pane row.

## Components

### Buttons
Solid, rounded, native-feeling; they press by shrinking, not by changing colour.
- **Shape:** 14px corners, 50px tall, 26px side padding, 17px/600 text. Small: 34px tall, 10px corners, 15px text.
- **Primary:** accent fill, accent-ink text. One per screen: the Pair action (docked, full width) or the empty state's "Pair a machine".
- **Tinted:** blue wash fill, accent text. Secondary actions such as "Try again", "Pair again", "Reconnect". Inside a notice the small tinted button sits on a cell-coloured fill with a 1px shadow so it reads on the tinted notice.
- **Press / disabled:** `:active` scales to 0.97 at 80% opacity (0.2s, the app's ease). Disabled primary turns to a hairline fill with ink-2 text; a busy button dims to 70% and relabels ("Pairing…").
- **Focus:** a 2px accent outline, 2px offset.

### Bar icons and back link
Borderless, accent-coloured, 44×44. The back link is a 26px chevron plus the previous screen's name. Press dims to 45% opacity.

### Chips
- **Style:** 28px pill, blue wash, accent text 14px/600, a 17px leading icon. Used for "Paste" beside the pairing-code label. Press dims to 60%.
- **Tag:** a quieter sibling: 11px/600 ink-2 text on hairline, 6px corners, e.g. "Asleep".

### Cards / Containers
- **Corner Style:** 16px, clipped.
- **Background:** cell on canvas.
- **Shadow Strategy:** none (Flat-At-Rest).
- **Border:** none; rows inside are divided by 0.5px hairlines inset to the text.
- **Internal Padding:** comes from the rows (10px/12px/14px).

### Rows
The unit of every list. Avatar or tile, a title (17px/500) over a subtitle (14px, ink-2), and an ink-3 chevron. Title and subtitle each truncate to one line. A pane's subtitle leads with its coloured status word, then " · "-joined detail. Press fills cell-press instantly and fades back over 0.35s. A hibernated tab dims its avatar to 45% and carries the "Asleep" tag. Holding a pane row (half a second, still) asks to close that pane in a sheet with a danger button; the lift that ends the hold does not open the pane.

### Avatars and the status badge (signature)
A pane is a 38px circle (30px in the terminal header): neutral (ink at 7% over cell) with a terminal glyph for a plain shell, or the agent's brand field with its mark in white (or the agent's own ink). The status badge is a 14px dot at the bottom-right, ringed 2.5px in the cell colour so it cuts into the avatar, exactly as on a desktop tab. Working blinks (1.4s, stepped); Waiting has the white hole; Done is solid; Idle has no badge.

### Inputs / Fields
- **Style:** a card (16px, cell) holding a borderless input with 14px/16px padding, 17px system text; the pairing code uses 14px/1.5 Hack and breaks anywhere.
- **Focus:** the whole card takes the 2px accent outline, flush.
- **Error:** a 14px danger line under the card, hidden when empty.

### Notices
A 16px-cornered block inset like a card, tinted by tone: error is danger at 10% over cell with a danger alert icon; warning is amber at 13% with a warn-ink icon. A 16px/600 title, a 14px ink-2 body that names the fix (usually a `tty7-gateway` command in code), and small tinted actions.

### Menu
The ⋯ drops a 232px raised panel, 14px corners, Float shadow, from the top-right; 48px items, label left and 21px icon right, hairline between. Destructive items are danger. It scales in from 0.9 over 0.28s.

### Connection line (signature)
Under a machine's title: an 8px dot plus words, 15px ink-2 with tabular numerals. "Direct · 4 ms" (done green), "Relay · n ms" (waiting amber), "Connecting…" (ink-3, breathing), "Offline" (danger). The terminal header repeats it compactly as a 7px dot and a 600-weight word ("Live", "Connecting", "Offline", "Closed").

### Terminal screen and keybar (signature)
The header is a tinted bar with back chevron, small avatar, title over the state line and Hack cwd, and a single fit/zoom toggle shown only when the pane is too wide to read. The pane keeps the desktop's cols×rows; the font shrinks to fit, or holds at 11px and pans to follow the cursor. Errors float in as a raised banner over the top of the pane; a dropped connection is retried on its own (1s, 2s, 4s … 15s, and at once when the network comes back), with the last good screen left up under a "Reconnecting…" banner until the new replay replaces it. A phone key beside it takes the pane over: it fills tint while held, the pane runs at the phone's grid at 11px with nothing to fit or pan, and the grid follows the keyboard and rotation once they settle. When the desktop takes it back, the banner says so with "Take over again"; taking over again is always a tap, never automatic. A copy key beside fit/zoom lays the whole buffer out as a plain page over the pane, wrapped to the phone and rejoined where the terminal wrapped, so the phone's own selection handles and Copy work on it. The keybar is a horizontally scrolling row of keycaps (esc, tab, ⇧tab, ctrl, ^C, arrows, paste, | / ~ -) fading out at the right edge, plus two fixed accent keys: compose and keyboard. Keys press to 0.94 scale in cell-press; the ctrl latch and an open compose box fill their key accent. The compose box docks on the keybar in the same surface: a pill field (16px system face, so iOS never zooms on focus) growing to about five lines, and a round accent send button. Send types the text, then Enter; several lines go in as one bracketed paste where the program asked for it. An agent's pane opens on the box with the keyboard down, so the screen is read before anything is typed.

### Empty and loading states
Empty: centred, 56px top padding, the Duo logo mark (76px) or a 64px accent tile, a 22px headline, a 16px ink-2 sentence, and at most one primary button. Loading: three skeleton rows with hairline avatars and 12px bones breathing at 1.6s.

### Navigation motion
Every screen change is a push or pop via View Transitions: 0.44s on `cubic-bezier(0.32, 0.72, 0, 1)`. The incoming screen slides in from the right with a soft leading shadow while the old one recedes 28% and dims to 90% brightness; back reverses it. `prefers-reduced-motion` removes every animation and transition.

## Do's and Don'ts

### Do:
- **Do** take every surface, ink, accent and ANSI colour from the desktop Light/Dark presets, and follow the system appearance.
- **Do** put content in 16px-cornered cards inset 16px on the canvas, with 0.5px hairlines between rows starting at the text.
- **Do** report agent state with the desktop's avatar, badge and words: Working (blue, blinking), Needs input (amber, hollow), Done (green), Idle (nothing).
- **Do** set commands, paths, codes and keys in Hack; everything else in the system face.
- **Do** keep every tap target at least 44px and pad every edge with the safe-area insets.
- **Do** say in words whether keystrokes will land: the connection line and terminal state word are always present.
- **Do** move between screens with the push/pop slide, and honour reduced motion.

### Don't:
- **Don't** style the app per platform; one language for iOS and Android.
- **Don't** draw bordered boxes or give cards a shadow; depth is tone, and shadows are only for floating layers and keycaps.
- **Don't** use working blue, waiting amber or done green for anything but state, and don't set text in them on a cell; use the status inks.
- **Don't** use ink-3 for text; it is for chevrons and idle dots.
- **Don't** uppercase labels or add eyebrow text above titles; group titles are 13px/600 sentence case.
- **Don't** resize a pane to the phone unasked; it keeps the desktop's size and the phone fits or pans, until the user takes it over.
