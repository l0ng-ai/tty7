// The frontend's view of the Rust side: typed wrappers over its commands, and
// the shapes that cross the bridge. The wire types mirror tty7-mobile-proto;
// its tests pin the tags these match on.

import { Channel, invoke } from "@tauri-apps/api/core";

export interface Host {
  id: string;
  name: string;
  relay?: string | null;
  addrs: string[];
  /** Which computer it runs on, the same for every tty7 there whatever its
   * key; absent for pairings made with an older desktop. */
  machine?: string | null;
  /** When this phone paired it, in Unix seconds. */
  paired_at?: number | null;
}

export type AgentStatus = "idle" | "working" | "waiting" | "done";

export interface AgentView {
  kind: string;
  status: AgentStatus;
  message?: string | null;
}

export interface PaneView {
  id: number;
  title: string;
  cwd?: string | null;
  agent?: AgentView | null;
  /** Nothing runs in it: the machine's server restarted since. */
  stopped?: boolean;
}

export interface TabView {
  id: string;
  name: string;
  hibernated: boolean;
  panes: PaneView[];
}

export interface WorkspaceView {
  id: string;
  name: string;
  tabs: TabView[];
  /** The desktop sidebar's groups, in its order; absent from an older desktop. */
  groups?: GroupView[];
  /** The tab the desktop last had in front here. */
  active_tab?: string;
}

/** One of the desktop sidebar's groups: pinned, per repository or SSH host,
 * or the tabs in none (unnamed when it is the only group). */
export interface GroupView {
  name?: string;
  pinned?: boolean;
  collapsed?: boolean;
  tabs: string[];
}

export interface Tree {
  host: string;
  workspaces: WorkspaceView[];
  /** Machines the desktop is linked to over SSH. Absent from older gateways. */
  remotes?: RemoteView[];
}

export interface RemoteView {
  key: string;
  name: string;
  connected: boolean;
  error?: string | null;
  /** Up, but its first read has not come back: workspaces unknown, not absent. */
  pending?: boolean;
  workspaces: WorkspaceView[];
}

export interface LinkInfo {
  path: "direct" | "relay" | "connecting";
  rtt_ms: number;
}

export type TreeMsg =
  | { type: "tree"; tree: Tree }
  | { type: "link"; link: LinkInfo }
  | { type: "error"; message: string }
  | { type: "closed" };

export type PaneEvent =
  | { type: "size"; cols: number; rows: number }
  | { type: "cwd"; path: string }
  | { type: "agent"; agent: AgentView | null }
  | { type: "exited"; code: number | null }
  | { type: "error"; message: string }
  /** Whether the pane runs at this phone's size; `refused` says why not. */
  | { type: "lease"; held: boolean; refused?: string | null };

export const hosts = () => invoke<Host[]>("hosts");

export const pair = (code: string, deviceName: string) =>
  invoke<Host>("pair", { code, deviceName });

export const forget = (hostId: string) => invoke<void>("forget", { hostId });

/** Sets the style of what the page does not draw: status bar, keyboard. */
export const appearance = (style: "system" | "light" | "dark") => invoke<void>("appearance", { style });

export interface Insets {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/** What the system bars cover, in CSS pixels. Android only: zero elsewhere. */
export const insets = () => invoke<Insets>("insets");

/** The `tty7pair:` link the app was opened with and has not handled, once. */
export const openedLink = () => invoke<string | null>("opened_link");

/** Sends the app to the background, as Back from the first screen does. */
export const toBackground = () => invoke<void>("to_background");

export function watch(hostId: string, onMsg: (msg: TreeMsg) => void) {
  const onEvent = new Channel<TreeMsg>();
  onEvent.onmessage = onMsg;
  return invoke<number>("watch", { hostId, onEvent });
}

/** Ends a watch by the id `watch` resolved with. */
export const unwatch = (watch: number) => invoke<void>("unwatch", { watch });

export const refresh = (watch: number) => invoke<void>("refresh", { watch });

/** Output arrives as ArrayBuffers of raw terminal bytes, events as objects. */
/** `machine` is a remote's key, or null for the paired machine itself. */
export function paneOpen(
  hostId: string,
  machine: string | null,
  paneId: number,
  onOutput: (bytes: Uint8Array) => void,
  onEvent: (event: PaneEvent) => void,
) {
  const channel = new Channel<ArrayBuffer | PaneEvent>();
  channel.onmessage = (msg) => {
    if (msg instanceof ArrayBuffer) onOutput(new Uint8Array(msg));
    else onEvent(msg);
  };
  return invoke<number>("pane_open", { hostId, machine, paneId, onOutput: channel });
}

export interface TabCreated {
  tab_id: string;
  pane_id: number;
}

export const tabNew = (
  hostId: string,
  machine: string | null,
  workspaceId: string,
  cwd: string | null,
  size: { cols: number; rows: number } | null,
) => invoke<TabCreated>("tab_new", { hostId, machine, workspaceId, cwd, size });

/** Closes a tab and its panes; the machine keeps it to reopen where it can. */
export const tabClose = (hostId: string, machine: string | null, workspaceId: string, tabId: string) =>
  invoke<void>("tab_close", { hostId, machine, workspaceId, tabId });

/** Closes a pane on the machine, ending what runs in it. Not `paneClose`,
 * which only stops this phone watching one. */
export const paneKill = (hostId: string, machine: string | null, paneId: number) =>
  invoke<void>("pane_kill", { hostId, machine, paneId });

export const paneInput = (handle: number, data: string) =>
  invoke<void>("pane_input", { handle, data });

/** Run the pane at `size`, the phone's grid; `null` gives it back. */
export const paneLease = (handle: number, size: { cols: number; rows: number } | null) =>
  invoke<void>("pane_lease", { handle, size });

/** The largest file the gateway takes (`tty7_mobile_proto::MAX_UPLOAD`),
 * checked here too so a big video is refused before it is read in. */
export const MAX_UPLOAD = 20 * 2 ** 20;

/** Sends a file to the machine a pane runs on; resolves with its path there.
 * The bytes go as the raw body; the rest rides in headers, encoded, since a
 * header is ASCII and a file name need not be. */
export const upload = (hostId: string, machine: string | null, name: string, bytes: Uint8Array) =>
  invoke<string>("upload", bytes, {
    headers: {
      "x-host": encodeURIComponent(hostId),
      "x-name": encodeURIComponent(name),
      "x-machine": encodeURIComponent(machine ?? ""),
    },
  });

export interface Diff {
  root: string;
  patch: string;
  untracked: string[];
  truncated: boolean;
}

/** What has changed in the repository `cwd` is in. */
export const diff = (hostId: string, machine: string | null, cwd: string) =>
  invoke<Diff>("diff", { hostId, machine, cwd });

export const paneClose = (handle: number) => invoke<void>("pane_close", { handle });
