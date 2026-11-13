// An agent's choices read off the screen, and the keys that pick one. Pure:
// rows of text in, choices and keystrokes out, so the reading and the typing
// can be checked without a terminal.

/** One choice. `n` is typed to pick it; `steps`, where given, means it is
 * picked by moving that many places down (negative: up) and pressing Enter. */
export type Choice = { n: string; label: string; steps?: number };

/** The choices on screen, with an identity: the same question, options,
 * selection and place on screen give the same `id`, anything else another. */
export type ChoiceSet = { id: string; choices: Choice[] };

/** One write to the pane, `delay` ms after the one before. */
export type Keystroke = { data: string; delay: number };

/** Between keys an agent must read one at a time (the desktop composer's
 * KEY_GAP, src/terminal/composer.rs). */
export const KEY_GAP_MS = 15;
/** Before the Enter that ends them (the composer's SETTLE): in the same read
 * as the keys before it, a TUI can take it as part of them. */
export const SETTLE_MS = 50;

/** The keys that pick `choice`, each its own write. Arrows follow the
 * pane's cursor-key mode (DECCKM): `ESC O A` when it asked for application
 * mode, `ESC [ A` otherwise. */
export function choiceKeys(choice: Choice, appCursor: boolean): Keystroke[] {
  if (choice.steps === undefined) return [{ data: choice.n, delay: 0 }];
  const arrow = `\x1b${appCursor ? "O" : "["}${choice.steps > 0 ? "B" : "A"}`;
  const keys: Keystroke[] = [];
  for (let i = 0; i < Math.abs(choice.steps); i++) keys.push({ data: arrow, delay: i === 0 ? 0 : KEY_GAP_MS });
  keys.push({ data: "\r", delay: keys.length ? SETTLE_MS : 0 });
  return keys;
}

/** FNV-1a, 32 bits, in base 36: enough to tell two choice sets apart. */
function hash(text: string) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(36);
}

/** The nearest non-blank row above `row`: the question, as near as the
 * screen says it. */
function above(rows: string[], row: number) {
  for (let y = row - 1; y >= 0; y--) if (rows[y].trim()) return rows[y].trim();
  return "";
}

function identify(kind: string, rows: string[], base: number, first: number, choices: Choice[]): ChoiceSet {
  const at = choices.findIndex((c) => c.steps === 0);
  const id = hash([kind, base + first, at, above(rows, first), ...choices.map((c) => `${c.n} ${c.label}`)].join("\n"));
  return { id, choices };
}

/** A menu picked with the arrows, no numbers — an agent's "trust this
 * folder?" — said by the hint under it. Its options are the lines above, at
 * the selected one's indent. */
function arrowMenu(rows: string[], base: number): ChoiceSet | null {
  let hint = -1;
  for (let y = rows.length - 1; y >= 0 && hint < 0; y--) if (/Enter to (confirm|select)/i.test(rows[y])) hint = y;
  if (hint <= 0) return null;
  let y = hint - 1;
  while (y >= 0 && !rows[y].trim()) y--;
  const end = y;
  for (; y >= 0 && rows[y].trim(); y--);
  const block = rows.slice(y + 1, end + 1);
  const sel = block.findIndex((l) => /^\s*❯\s/.test(l));
  if (sel < 0 || /^\s*❯\s+\d{1,2}[.)]\s/.test(block[sel])) return null;
  const col = block[sel].indexOf("❯");
  const options = block
    .map((l, i) => ({ l, i }))
    .filter(({ l, i }) => i === sel || (l.slice(0, col + 2).trim() === "" && l[col + 2] !== " "));
  if (options.length < 2 || options.length > 9) return null;
  const at = options.findIndex((o) => o.i === sel);
  const choices = options.map((o, k) => ({ n: String(k + 1), label: o.l.replace(/^\s*❯?\s*/, ""), steps: k - at }));
  return identify("arrows", rows, base, y + 1 + options[0].i, choices);
}

/** The last run of consecutively numbered lines ("❯ 1. Yes", "2. No, and
 * tell Claude…"), box edges and the cursor mark stripped. A run from 1, or
 * one from further down whose first lines have scrolled off, if the
 * selection mark is in it: an agent's own cursor, not a numbered list in its
 * output. */
function numberedMenu(rows: string[], base: number): ChoiceSet | null {
  let found: { first: number; choices: Choice[] } | null = null;
  // From 1: lines between that are not the next number (a label wrapped, a
  // description under it) are passed over, as they always were.
  let run: Choice[] = [];
  let runFirst = 0;
  // From anywhere: broken by any number out of turn, kept only if marked.
  let loose: Choice[] = [];
  let looseFirst = 0;
  let marked = false;
  for (let y = 0; y < rows.length; y++) {
    const edged = rows[y].replace(/^[\s│┃|]+|[\s│┃|]+$/g, "");
    const mark = /^[❯›]/.test(edged);
    const m = /^(\d{1,2})[.)]\s+(.+)$/.exec(edged.replace(/^[❯›>▸●◉○]\s*/, ""));
    if (!m) continue;
    const n = Number(m[1]);
    const choice = { n: m[1], label: m[2] };
    if (n === 1) {
      run = [];
      runFirst = y;
    }
    if (n === run.length + 1) run.push(choice);
    if (run.length >= 2 && n === run.length) found = { first: runFirst, choices: [...run] };
    if (loose.length && n === Number(loose[loose.length - 1].n) + 1) {
      loose.push(choice);
      marked ||= mark;
    } else {
      loose = [choice];
      looseFirst = y;
      marked = mark;
    }
    if (loose.length >= 2 && marked && loose[0].n !== "1") found = { first: looseFirst, choices: [...loose] };
  }
  return found && identify("numbers", rows, base, found.first, found.choices);
}

/** The choices on screen. `rows` is the pane's screen, top to bottom, and
 * `base` the buffer line of its first row; numbered lines count only while
 * the agent is waiting. */
export function findChoices(rows: string[], base: number, waiting: boolean): ChoiceSet | null {
  return arrowMenu(rows, base) ?? (waiting ? numberedMenu(rows, base) : null);
}
