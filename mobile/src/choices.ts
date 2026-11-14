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

/** `steps` arrows down (negative: up), each its own write, no Enter. */
export function arrowKeys(steps: number, appCursor: boolean): Keystroke[] {
  const arrow = `\x1b${appCursor ? "O" : "["}${steps > 0 ? "B" : "A"}`;
  return Array.from({ length: Math.abs(steps) }, (_, i) => ({ data: arrow, delay: i === 0 ? 0 : KEY_GAP_MS }));
}

/** Keys run one after another: each run after the first waits SETTLE for the
 * agent to redraw from the one before. */
export function chain(...runs: Keystroke[][]): Keystroke[] {
  const keys: Keystroke[] = [];
  for (const run of runs) {
    if (!run.length) continue;
    keys.push(...run.map((k, i) => (i === 0 && keys.length ? { ...k, delay: SETTLE_MS } : k)));
  }
  return keys;
}

/** Enter, SETTLE after whatever came before it. */
export const ENTER: Keystroke[] = [{ data: "\r", delay: 0 }];

/** Answer text typed into an agent's own one-line field: one write, line
 * breaks flattened, since a newline there is Enter. */
export function textKeys(text: string): Keystroke[] {
  const flat = text.replace(/\s*\r?\n\s*/g, " ").trim();
  return flat ? [{ data: flat, delay: 0 }] : [];
}

/** The number of the option the agent's cursor (❯ or ›) is on, in the
 * numbered menu on screen; null when none is marked. */
export function markedNumber(rows: string[]): number | null {
  let found: number | null = null;
  for (const row of rows) {
    const edged = row.replace(/^[\s│┃|]+/, "");
    const m = /^[❯›]\s*(\d{1,2})[.)]\s/.exec(edged);
    if (m) found = Number(m[1]);
  }
  return found;
}

/** A label as a menu draws it, its checkbox taken off. */
function bare(label: string) {
  return label.replace(/^(\[[ xX✔✓]?\]|[☐☑☒◯◉○●✔✓])\s*/, "").trim().toLowerCase();
}

/** The choice on screen that `label` names: equal once checkboxes are off,
 * or one the start of the other where the screen cut it short. */
export function matchChoice(set: ChoiceSet | null, label: string): Choice | null {
  const want = label.trim().toLowerCase();
  if (!set || !want) return null;
  return (
    set.choices.find((c) => bare(c.label) === want) ??
    set.choices.find((c) => {
      const got = bare(c.label).replace(/…$/, "");
      return got.length >= 4 && (want.startsWith(got) || got.startsWith(want));
    }) ??
    null
  );
}

// The keys that answer a prompt the agent's hook described (AgentView.prompt).
// The hook gives the options but not where the agent's cursor is or how its
// menu takes keys, so the screen still has a say: an option drawn on it is
// picked by the keys the scraped buttons would send for it, and the cursor
// mark says where arrows start. Claude Code and Codex both open a menu on its
// first option; Claude Code takes an option's digit alone (pick and confirm),
// and draws "Type something" after the options, then, for a multi-select,
// Submit.

/** Where the agent's cursor is, counting the options from 1. */
const cursorAt = (rows: string[]) => markedNumber(rows) ?? 1;

/** Picking option `i` (from 0) of a single-select question. */
export function pickKeys(
  options: { label: string }[],
  i: number,
  agent: string,
  rows: string[],
  set: ChoiceSet | null,
  appCursor: boolean,
): Keystroke[] {
  const label = options[i].label;
  const shown = matchChoice(set, label);
  if (shown) return choiceKeys(shown, appCursor);
  if (agent === "claude" && i < 9) return [{ data: String(i + 1), delay: 0 }];
  return choiceKeys({ n: String(i + 1), label, steps: i + 1 - cursorAt(rows) }, appCursor);
}

/** A multi-select answered: Enter on each picked option (from 0), the
 * user's own words typed on the row after the options, then Submit, the row
 * after that. */
export function multiKeys(
  optionCount: number,
  allowOther: boolean,
  picked: number[],
  other: string | null,
  rows: string[],
  appCursor: boolean,
): Keystroke[] {
  let at = cursorAt(rows);
  const runs: Keystroke[][] = [];
  for (const i of [...picked].sort((a, b) => a - b)) {
    runs.push(arrowKeys(i + 1 - at, appCursor), ENTER);
    at = i + 1;
  }
  const otherRow = optionCount + 1;
  if (allowOther && other) {
    runs.push(arrowKeys(otherRow - at, appCursor), textKeys(other));
    at = otherRow;
  }
  const submit = optionCount + (allowOther ? 1 : 0) + 1;
  runs.push(arrowKeys(submit - at, appCursor), ENTER);
  return chain(...runs);
}

/** An answer in the user's own words: on the row after the options, or, for
 * a question without options, straight into its field. */
export function otherKeys(optionCount: number, text: string, rows: string[], appCursor: boolean): Keystroke[] {
  if (!optionCount) return chain(textKeys(text), ENTER);
  return chain(arrowKeys(optionCount + 1 - cursorAt(rows), appCursor), textKeys(text), ENTER);
}

/** A permission granted once or refused: the "Yes" or "No" option on screen
 * as its button would pick it; failing that, 1 for yes (both agents put it
 * first) and Esc for no (both take it as no). */
export function permissionKeys(allow: boolean, set: ChoiceSet | null, appCursor: boolean): Keystroke[] {
  const shown = set?.choices.find((c) => (allow ? /^yes\b/ : /^no\b/).test(bare(c.label)));
  if (shown) return choiceKeys(shown, appCursor);
  return [{ data: allow ? "1" : "\x1b", delay: 0 }];
}

/** Sending a set of answers from the agent's review step: its Submit
 * option where the screen shows one, else Enter on what is highlighted. */
export function submitAllKeys(set: ChoiceSet | null, appCursor: boolean): Keystroke[] {
  const shown = set?.choices.find((c) => /^submit\b/.test(bare(c.label)));
  return shown ? choiceKeys(shown, appCursor) : ENTER;
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
