// The phone's own notifications when an agent on a machine starts waiting on
// you or finishes, with its answers as buttons where there are few enough.
//
// Local only: they are posted by this app, from the tree it watches, so they
// come only while it holds a connection to that machine — in front, or on
// Android in the background under the connection's foreground service. There
// is no push server; once iOS suspends the app (about half a minute after it
// leaves, `lib.rs` `background_grace`) nothing more arrives.

import { invoke } from "@tauri-apps/api/core";
import {
  createChannel,
  Importance,
  onAction,
  registerActionTypes,
  removeActive,
  requestPermission,
  sendNotification,
  Visibility,
} from "@tauri-apps/plugin-notification";
import type { ActionType, Options } from "@tauri-apps/plugin-notification";
import { Terminal } from "@xterm/xterm";

import * as api from "./api";
import type { AgentStatus, AgentView, Host, PromptView, TabView, Tree, WorkspaceView } from "./api";
import { choiceKeys, findChoices } from "./choices";
import type { Choice } from "./choices";
import { agentLook } from "./icons";

/** Which machine a pane is on: a remote the desktop is linked to, or null
 * for the paired machine itself (main.ts `Place`). */
export type Place = { key: string; name: string } | null;

/** Where a notification leads: one pane on one machine. */
export interface Target {
  hostId: string;
  place: Place;
  paneId: number;
  /** The tab's name as the list shows it. */
  tab: string;
}

/** What a button on a notification answers. */
type Answer = { kind: "allow" } | { kind: "deny" } | { kind: "option"; label: string };

/** A notification on show, by its id. `prompt` is the prompt it asks while
 * it is a "needs you" one: what an answer must still find before it is sent. */
interface Posted {
  target: Target;
  agent: string;
  needsYou: boolean;
  prompt: string | null;
  answers: Map<string, Answer>;
}

interface Seen {
  status: AgentStatus;
  prompt: string | null;
}

const android = /Android/.test(navigator.userAgent);
const CHANNEL = "agents";
const PERMISSION_TYPE = "agent.permission";
/** Android's connection notification is 1 (KeepAliveService.kt); these start
 * well clear of it. */
let nextId = 100;
/** One notification per pane: a new one replaces the last, so the shade
 * holds the latest word on each. */
const ids = new Map<string, number>();
const posted = new Map<number, Posted>();
/** Where each id leads, kept after its answer is done with: a tap on it
 * still opens the pane. */
const targets = new Map<number, Target>();
/** Per machine, what each pane's agent was last seen doing; absent until a
 * machine's first tree, which is the baseline and says nothing. */
const known = new Map<string, Map<string, Seen>>();
/** The prompt each pane was last notified for, so the same one coming back
 * is not news again. */
const notifiedPrompt = new Map<string, string>();
/** Each pane's agent as last heard: what a button checks before it answers. */
const latest = new Map<string, AgentView>();
/** The action types of question notifications on show, by type id. iOS keeps
 * only the set registered last, so every live one goes in each time. */
const questionTypes = new Map<string, ActionType>();
const answering = new Set<number>();

let ready = false;
let granted = false;
let config: {
  enabled: () => boolean;
  /** The app lock is on: nothing of a pane's content on the lock screen,
   * and no answering without opening the app. */
  private: () => boolean;
  open: (target: Target) => void;
} | null = null;

/** The same key main.ts gives a pane on screen (`ownKey`). */
export function paneKey(place: Place, paneId: number) {
  return `${place?.key ?? ""}/${paneId}`;
}

const fullKey = (hostId: string, key: string) => `${hostId}|${key}`;

function tabTitle(tab: TabView) {
  return /^[~/]/.test(tab.name) ? (tab.name.split("/").filter(Boolean).pop() ?? tab.name) : tab.name;
}

/** Fresh from the system, not the plugin's cached answer: notifications can
 * be turned on in the phone's settings while the app runs. */
async function checkGranted() {
  try {
    granted = (await invoke<boolean | null>("plugin:notification|is_permission_granted")) === true;
  } catch {
    granted = false;
  }
  return granted;
}

/** Starts listening for taps on notifications. Without the plugin (the
 * desktop dev build) nothing is ever posted. */
export async function setup(options: NonNullable<typeof config>) {
  config = options;
  try {
    await checkGranted();
    if (android)
      await createChannel({
        id: CHANNEL,
        name: "Agents",
        description: "When an agent on your computer needs you or finishes",
        importance: Importance.High,
        visibility: Visibility.Private,
        vibration: true,
      });
    await registerTypes();
    await onAction((raw) => void acted(raw));
    ready = true;
  } catch {
    ready = false;
  }
  document.addEventListener("visibilitychange", () => {
    if (ready && document.visibilityState === "visible") void checkGranted();
  });
}

/** Whether notifications may be posted: unknown until asked. */
export async function permission(): Promise<"granted" | "denied" | "unasked"> {
  if (!ready) return "denied";
  if (await checkGranted()) return "granted";
  return remembered("notify.asked") ? "denied" : "unasked";
}

/** Asks the phone for leave to notify, once: at a moment the user can see
 * why (opening an agent's pane, or turning the setting on), never on launch. */
export async function ask(again = false) {
  if (!ready || granted) return granted;
  if (!again && remembered("notify.asked")) return false;
  remember("notify.asked", "1");
  try {
    await requestPermission();
  } catch {
    // Refused or unsupported: the check below says which.
  }
  return checkGranted();
}

/** A machine's tree, as a watch reports it. `onScreen` is the pane open on
 * the screen (`paneKey`), which says for itself what its agent does. */
export function heard(host: Host, tree: Tree, onScreen: string | null) {
  const before = known.get(host.id);
  const now = new Map<string, Seen>();
  for (const key of latest.keys()) if (key.startsWith(`${host.id}|`)) latest.delete(key);
  const spaces: [Place, WorkspaceView][] = [
    ...tree.workspaces.map((ws): [Place, WorkspaceView] => [null, ws]),
    ...(tree.remotes ?? []).flatMap((r) => r.workspaces.map((ws): [Place, WorkspaceView] => [{ key: r.key, name: r.name }, ws])),
  ];
  // On screen and in front, a pane is being read; behind, it is not.
  const watched = document.visibilityState === "visible" ? onScreen : null;
  for (const [place, ws] of spaces)
    for (const tab of ws.tabs)
      for (const pane of tab.panes) {
        const agent = pane.agent;
        if (!agent || pane.stopped) continue;
        const key = paneKey(place, pane.id);
        const full = fullKey(host.id, key);
        const prompt = agent.status === "waiting" ? (agent.prompt?.id ?? null) : null;
        now.set(key, { status: agent.status, prompt });
        latest.set(full, agent);
        if (!before) continue;
        const was = before.get(key);
        const target: Target = { hostId: host.id, place, paneId: pane.id, tab: tabTitle(tab) };
        if (agent.status === "waiting") {
          const fresh = was?.status !== "waiting" || prompt !== was.prompt;
          const repeat = prompt !== null && notifiedPrompt.get(full) === prompt;
          if (fresh && !repeat && key !== watched) {
            if (prompt !== null) notifiedPrompt.set(full, prompt);
            void needsYou(full, target, agent);
          } else if (fresh) withdraw(full);
        } else {
          if (was?.status === "waiting") withdraw(full);
          if (agent.status === "done" && was?.status === "working" && key !== watched) void finished(full, target, agent);
        }
      }
  if (before) for (const key of before.keys()) if (!now.has(key)) withdraw(fullKey(host.id, key));
  known.set(host.id, now);
}

function idFor(full: string) {
  let id = ids.get(full);
  if (id === undefined) {
    id = nextId++;
    ids.set(full, id);
  }
  return id;
}

/** Takes down a pane's "needs you" once it is answered or gone. */
function withdraw(full: string) {
  const id = ids.get(full);
  const was = id === undefined ? undefined : posted.get(id);
  if (id === undefined || !was?.needsYou) return;
  posted.delete(id);
  if (questionTypes.delete(`agent.q.${id}`)) void registerTypes();
  removeActive([{ id }]).catch(() => {});
}

function allowed() {
  return ready && granted && !!config?.enabled();
}

/** The prompt in a line: the question, or the tool and what it would run. */
function promptText(prompt: PromptView | null | undefined) {
  if (!prompt) return null;
  if (prompt.kind === "permission") return prompt.summary ? `${prompt.tool}: ${prompt.summary}` : `Wants to use ${prompt.tool}`;
  const [first, ...rest] = prompt.questions;
  if (!first) return null;
  return rest.length ? `${first.text} (+${rest.length} more)` : first.text;
}

/** The buttons a prompt gets: Allow once / Deny for a permission, and one
 * per option for a lone single-choice question of up to three. */
function answersFor(prompt: PromptView | null | undefined) {
  const answers = new Map<string, Answer>();
  if (prompt?.kind === "permission") {
    answers.set("allow", { kind: "allow" });
    answers.set("deny", { kind: "deny" });
  } else if (prompt?.kind === "question" && prompt.questions.length === 1) {
    const [q] = prompt.questions;
    if (!q.multi_select && q.options.length >= 1 && q.options.length <= 3)
      q.options.forEach((o, i) => answers.set(`opt.${i}`, { kind: "option", label: o.label }));
  }
  return answers;
}

function clip(text: string, max: number) {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

async function registerTypes() {
  const permission: ActionType = {
    id: PERMISSION_TYPE,
    actions: [
      // In front, so the connection is there to answer on; unlocked, since
      // it runs a command on the computer.
      { id: "allow", title: "Allow once", foreground: true, requiresAuthentication: true },
      { id: "deny", title: "Deny", foreground: true, requiresAuthentication: true, destructive: true },
    ],
  };
  await registerActionTypes([permission, ...questionTypes.values()]);
}

function show(id: number, target: Target, options: Omit<Options, "id" | "extra" | "group">) {
  targets.set(id, target);
  sendNotification({
    group: target.hostId,
    extra: extra(target),
    ...options,
    id,
    ...(android ? { channelId: CHANNEL } : {}),
    autoCancel: true,
  });
}

/** Where a notification leads, carried in it too: Android hands it back with
 * a tap even if this page has been loaded again since. Strings only: iOS
 * takes no other kind. */
function extra(target: Target): Record<string, string> {
  return {
    host: target.hostId,
    machine: target.place?.key ?? "",
    machineName: target.place?.name ?? "",
    pane: String(target.paneId),
    tab: target.tab,
  };
}

async function needsYou(full: string, target: Target, agent: AgentView) {
  if (!allowed()) return;
  const id = idFor(full);
  const name = agentLook(agent.kind).name;
  const hidden = !!config?.private();
  const answers = hidden ? new Map<string, Answer>() : answersFor(agent.prompt);
  let actionTypeId: string | undefined;
  if (agent.prompt?.kind === "permission" && answers.size) actionTypeId = PERMISSION_TYPE;
  else if (answers.size) {
    actionTypeId = `agent.q.${id}`;
    questionTypes.set(actionTypeId, {
      id: actionTypeId,
      actions: [...answers].map(([actionId, a]) => ({
        id: actionId,
        title: clip(a.kind === "option" ? a.label : actionId, 40),
        foreground: true,
        requiresAuthentication: true,
      })),
    });
  } else questionTypes.delete(`agent.q.${id}`);
  posted.set(id, { target, agent: agent.kind, needsYou: true, prompt: agent.prompt?.id ?? null, answers });
  try {
    await registerTypes();
  } catch {
    actionTypeId = undefined;
  }
  const text = promptText(agent.prompt) ?? agent.message ?? "Waiting for your answer";
  show(id, target, {
    title: `${name} needs you · ${target.tab}`,
    body: hidden ? undefined : clip(text, 180),
    largeBody: hidden || text.length <= 60 ? undefined : clip(text, 600),
    actionTypeId,
  });
}

async function finished(full: string, target: Target, agent: AgentView) {
  if (!allowed()) return;
  const id = idFor(full);
  posted.set(id, { target, agent: agent.kind, needsYou: false, prompt: null, answers: new Map() });
  show(id, target, {
    title: `${agentLook(agent.kind).name} finished ${target.tab}`,
    body: config?.private() ? undefined : (agent.message ?? undefined),
  });
}

/** Said when a button's prompt is no longer the one the agent asks: nothing
 * was sent. */
function changed(id: number, record: Posted, title: string, body: string) {
  posted.set(id, { ...record, needsYou: false, prompt: null, answers: new Map() });
  if (!allowed()) return;
  show(id, record.target, { title, body });
}

/** The tap's own shape: the plugin's types call it `Options`, the payload is
 * `{ actionId, notification }` on both phones. */
function readAction(raw: unknown): { actionId: string; id: number | null; extra: Record<string, unknown> | null } | null {
  if (!raw || typeof raw !== "object") return null;
  const { actionId, notification } = raw as { actionId?: unknown; notification?: unknown };
  if (typeof actionId !== "string") return null;
  const n = notification && typeof notification === "object" ? (notification as { id?: unknown; extra?: unknown }) : null;
  return {
    actionId,
    id: typeof n?.id === "number" ? n.id : null,
    extra: n?.extra && typeof n.extra === "object" ? (n.extra as Record<string, unknown>) : null,
  };
}

function targetFrom(extra: Record<string, unknown> | null): Target | null {
  if (!extra || typeof extra.host !== "string" || typeof extra.pane !== "string") return null;
  const paneId = Number(extra.pane);
  if (!Number.isInteger(paneId)) return null;
  const machine = typeof extra.machine === "string" && extra.machine ? extra.machine : null;
  return {
    hostId: extra.host,
    place: machine ? { key: machine, name: typeof extra.machineName === "string" ? extra.machineName : machine } : null,
    paneId,
    tab: typeof extra.tab === "string" ? extra.tab : "",
  };
}

async function acted(raw: unknown) {
  const action = readAction(raw);
  if (!action || action.actionId === "dismiss") return;
  const record = action.id === null ? undefined : posted.get(action.id);
  const target = record?.target ?? (action.id === null ? null : targets.get(action.id)) ?? targetFrom(action.extra);
  if (!target) return;
  const answer = record?.answers.get(action.actionId);
  if (action.actionId === "tap" || !answer || !record || action.id === null) {
    config?.open(target);
    return;
  }
  if (answering.has(action.id)) return;
  answering.add(action.id);
  try {
    await answerWith(action.id, record, answer);
  } finally {
    answering.delete(action.id);
  }
}

/** Whether the pane's agent still asks `prompt`. */
function stillAsks(agent: AgentView | null | undefined, prompt: string | null) {
  return agent?.status === "waiting" && prompt !== null && agent.prompt?.id === prompt;
}

const sleep = (ms: number) => new Promise((r) => window.setTimeout(r, ms));

const clean = (text: string) => text.toLowerCase().replace(/\s+/g, " ").trim();

/** The choice on screen an answer means: the first "Yes" for Allow once, the
 * option by its label (the screen may cut a long one short). */
function choiceFor(choices: Choice[], answer: Answer): Choice | null {
  if (answer.kind === "allow") return choices.find((c) => /^yes\b/i.test(c.label.trim())) ?? null;
  if (answer.kind === "option") {
    const want = clean(answer.label);
    return (
      choices.find((c) => clean(c.label) === want) ??
      choices.find((c) => {
        const got = clean(c.label).replace(/…$/, "");
        return got.length >= 4 && (got.startsWith(want) || want.startsWith(got));
      }) ??
      null
    );
  }
  return null;
}

/** Answers from a notification's button: opens the pane's stream without a
 * screen, reads the agent's menu off the replay as a tap on it would, checks
 * the prompt is the one the notification asked, and types the same keys,
 * one write each. Anything else and nothing is sent. */
async function answerWith(id: number, record: Posted, answer: Answer) {
  const { target } = record;
  const full = fullKey(target.hostId, paneKey(target.place, target.paneId));
  const name = agentLook(record.agent).name;
  const moved = () =>
    changed(id, record, "That question changed", `${name} in ${target.tab} asks something else now. Nothing was sent; open it to answer.`);
  if (!stillAsks(latest.get(full), record.prompt)) return moved();

  const term = new Terminal({ cols: 80, rows: 24, scrollback: 0, allowProposedApi: true });
  let handle: number | null = null;
  let streamAgent: AgentView | null | undefined;
  let lastOutput = 0;
  let broken = false;
  const writes: Promise<void>[] = [];
  try {
    handle = await api.paneOpen(
      target.hostId,
      target.place?.key ?? null,
      target.paneId,
      (bytes) => {
        lastOutput = performance.now();
        writes.push(new Promise((done) => term.write(bytes, done)));
      },
      (event) => {
        if (event.type === "size") term.resize(event.cols, event.rows);
        else if (event.type === "agent") streamAgent = event.agent;
        else if (event.type === "exited" || event.type === "error") broken = true;
      },
    );
    // The replay is in when the output has gone quiet for a moment.
    const start = performance.now();
    while (!broken && performance.now() - start < 5000) {
      await sleep(100);
      if (lastOutput && performance.now() - lastOutput > 300) break;
    }
    await Promise.all(writes);
    if (broken) throw new Error("the pane's stream ended");
    if (streamAgent !== undefined && !stillAsks(streamAgent, record.prompt)) return moved();

    const buf = term.buffer.active;
    const rows: string[] = [];
    for (let y = buf.baseY; y < buf.baseY + term.rows; y++) rows.push(buf.getLine(y)?.translateToString(true) ?? "");
    const set = findChoices(rows, buf.baseY, true);
    const choice = set && choiceFor(set.choices, answer);
    // Deny is Esc, which an agent takes as no; still only with its menu up.
    const keys =
      answer.kind === "deny"
        ? set && [{ data: "\x1b", delay: 0 }]
        : choice && choiceKeys(choice, term.modes.applicationCursorKeysMode);
    if (!keys) {
      return changed(id, record, `Open ${target.tab} to answer`, `${name}'s question isn't on its screen the way tty7 can answer it from here.`);
    }
    // Once more, as late as it can be: the tree may have moved meanwhile.
    if (!stillAsks(latest.get(full), record.prompt)) return moved();
    for (const key of keys) {
      if (key.delay) await sleep(key.delay);
      await api.paneInput(handle, key.data);
    }
    posted.delete(id);
  } catch {
    changed(id, record, `Couldn't answer ${name}`, `Nothing reached ${target.tab}. Open it to answer.`);
  } finally {
    if (handle !== null) api.paneClose(handle).catch(() => {});
    term.dispose();
  }
}

function remembered(key: string) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function remember(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Private storage off: asked again next time.
  }
}
