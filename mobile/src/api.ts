// The frontend's view of the Rust side: typed wrappers over its commands, and
// the shapes that cross the bridge. The wire types mirror tty7-mobile-proto;
// its tests pin the tags these match on.

import { Channel, invoke } from "@tauri-apps/api/core";

export interface Host {
  id: string;
  name: string;
  relay?: string | null;
  addrs: string[];
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

export function watch(hostId: string, onMsg: (msg: TreeMsg) => void) {
  const onEvent = new Channel<TreeMsg>();
  onEvent.onmessage = onMsg;
  return invoke<void>("watch", { hostId, onEvent });
}

export const refresh = (hostId: string) => invoke<void>("refresh", { hostId });

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

export const paneInput = (handle: number, data: string) =>
  invoke<void>("pane_input", { handle, data });

/** Run the pane at `size`, the phone's grid; `null` gives it back. */
export const paneLease = (handle: number, size: { cols: number; rows: number } | null) =>
  invoke<void>("pane_lease", { handle, size });

export const paneClose = (handle: number) => invoke<void>("pane_close", { handle });
