import "@xterm/xterm/css/xterm.css";
import "./style.css";

import { Terminal } from "@xterm/xterm";
import type { ITheme } from "@xterm/xterm";
import * as scanner from "@tauri-apps/plugin-barcode-scanner";

import * as api from "./api";
import type {
  AgentStatus,
  AgentView,
  Host,
  LinkInfo,
  PaneView,
  TabView,
  Tree,
  WorkspaceView,
} from "./api";
import { agentLook, icon } from "./icons";
import logoUrl from "./assets/logo.svg?url";

const app = document.getElementById("app")!;

// ---------------------------------------------------------------------------
// A tiny DOM helper. Four screens do not justify a framework, and the
// terminal — the one heavy view — is xterm.js either way.

type Child = Node | string | null | undefined | false;

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<HTMLElementTagNameMap[K]> & { class?: string } = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  const { class: cls, ...rest } = props;
  if (cls) el.className = cls;
  Object.assign(el, rest);
  for (const c of children) if (c) el.append(c);
  return el;
}

/** One of the drawn icons, as an element. */
function ico(name: keyof typeof icon, cls = "icon") {
  const el = h("span", { class: cls });
  el.innerHTML = icon[name];
  return el;
}

/** An error from the Rust side, read as a sentence. */
function sentence(text: string) {
  const t = text.trim();
  if (!t) return "";
  return `${t[0].toUpperCase()}${t.slice(1)}${/[.!?]$/.test(t) ? "" : "."}`;
}

function errorText(e: unknown) {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

// ---------------------------------------------------------------------------
// Navigation. Every screen is pushed or popped: the new one slides in over the
// old, the way a phone's own apps move, and back undoes it.

// Each screen registers what to do when the app comes back from the
// background, and how to let go of its streams when it is left.
let onResume: (() => void) | null = null;
let onLeave: (() => void) | null = null;

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") onResume?.();
});

const still = matchMedia("(prefers-reduced-motion: reduce)");

function go(direction: "push" | "pop", render: () => HTMLElement) {
  const swap = () => {
    onLeave?.();
    onLeave = null;
    onResume = null;
    app.replaceChildren(render());
  };
  if (!document.startViewTransition || still.matches || !app.firstChild) {
    swap();
    return;
  }
  document.documentElement.dataset.nav = direction;
  document
    .startViewTransition(swap)
    .finished.finally(() => delete document.documentElement.dataset.nav);
}

interface ScreenParts {
  title: string;
  back?: { label: string; onclick: () => void };
  trailing?: Child[];
  /** Under the large title: the connection line on a machine. */
  subtitle?: Child;
  body: Child[];
  footer?: Child;
  /** Floats over the bottom of the list: search and the one "+". */
  dock?: Child;
}

/** A screen with a large title that folds into the bar as it scrolls away. */
function screen(parts: ScreenParts) {
  const bar = h(
    "header",
    { class: "nav" },
    h(
      "div",
      { class: "nav-lead" },
      parts.back &&
        h(
          "button",
          { class: "nav-back", onclick: parts.back.onclick, ariaLabel: `Back to ${parts.back.label}` },
          ico("back"),
          h("span", {}, parts.back.label),
        ),
    ),
    h("div", { class: "nav-title" }, parts.title),
    h("div", { class: "nav-trail" }, ...(parts.trailing ?? [])),
  );
  const large = h("h1", { class: "large-title" }, parts.title);
  const scroll = h(
    "main",
    { class: "scroll" },
    h("div", { class: "title-block" }, large, parts.subtitle),
    ...parts.body,
  );
  new IntersectionObserver(
    ([entry]) => bar.classList.toggle("folded", !entry.isIntersecting),
    { root: scroll, threshold: 0, rootMargin: "-8px 0px 0px 0px" },
  ).observe(large);
  const view = h("div", { class: "screen" }, bar, scroll, parts.footer, parts.dock);
  if (parts.dock) view.classList.add("docked");
  if (!parts.back && !parts.trailing?.length) view.classList.add("root");
  return view;
}

/** The bar floating at the bottom of a list: a search field, and the screen's
 * one way to add something, within the thumb's reach. */
function floatingBar(placeholder: string, onSearch: (query: string) => void, add: { label: string; run: () => void }) {
  const field = h("input", {
    type: "search",
    class: "search-input",
    placeholder,
    enterKeyHint: "search",
    autocapitalize: "off",
    spellcheck: false,
    ariaLabel: placeholder,
  });
  field.setAttribute("autocorrect", "off");
  field.oninput = () => onSearch(field.value.trim().toLowerCase());
  field.onkeydown = (e) => {
    if (e.key === "Enter") field.blur();
  };
  return h(
    "div",
    { class: "float-bar" },
    h("label", { class: "search" }, ico("search"), field),
    h("button", { class: "fab", ariaLabel: add.label, onclick: add.run }, ico("plus")),
  );
}

function section(title: Child, ...rows: Child[]) {
  return h(
    "section",
    { class: "group" },
    title && h("h2", { class: "group-title" }, title),
    h("div", { class: "card" }, ...rows),
  );
}

// ---------------------------------------------------------------------------
// Machines

/** What the last visit to each machine saw: its link and how many tabs it
 * had. The list shows it rather than holding a stream open per machine. */
const seen = new Map<string, { link: LinkInfo | null; tabs: number | null }>();

function hostMeta(hostId: string): { tone: string; text: string } | null {
  const last = seen.get(hostId);
  if (!last) return null;
  const tabs = last.tabs === null ? "" : ` · ${last.tabs} ${last.tabs === 1 ? "tab" : "tabs"}`;
  if (last.link === null) return { tone: "offline", text: "Offline" };
  if (last.link.path === "connecting") return { tone: "connecting", text: `Connecting…${tabs}` };
  const path = last.link.path === "direct" ? "Direct" : "Relay";
  return { tone: last.link.path, text: `${path} · ${last.link.rtt_ms} ms${tabs}` };
}

function hostsScreen(direction: "push" | "pop" = "pop") {
  go(direction, () => {
    const body = h("div", { class: "stack" });
    let all: Host[] = [];
    let query = "";
    const dock = floatingBar("Search machines", (q) => {
      query = q;
      render();
    }, { label: "Pair a machine", run: () => pairScreen() });
    dock.hidden = true;
    const view = screen({ title: "Machines", body: [body], dock });

    const render = () => {
      const shown = all.filter((host) => host.name.toLowerCase().includes(query));
      body.replaceChildren(
        shown.length
          ? section(null, ...shown.map(hostRow))
          : h("p", { class: "search-empty" }, `No machine matches “${query}”.`),
      );
    };
    api.hosts().then((hosts) => {
      all = hosts;
      if (hosts.length === 0) {
        body.replaceChildren(
          h(
            "div",
            { class: "empty" },
            h("img", { class: "empty-mark", src: logoUrl, alt: "" }),
            h("h2", { class: "empty-title" }, "Pair your computer"),
            h(
              "p",
              { class: "empty-body" },
              "Reach the panes open in tty7 on your computer, and type into them from here.",
            ),
            h("button", { class: "button primary", onclick: () => pairScreen() }, "Pair a machine"),
          ),
        );
        return;
      }
      dock.hidden = false;
      render();
    });
    return view;
  });
}

function hostRow(host: Host) {
  const meta = hostMeta(host.id);
  return h(
    "button",
    { class: meta?.tone === "offline" ? "row machine dim" : "row machine", onclick: () => hostScreen(host, "push") },
    h("span", { class: "tile" }, ico("machine")),
    h(
      "span",
      { class: "row-text" },
      h("span", { class: "row-title" }, host.name),
      meta && h("span", { class: `row-sub link ${meta.tone}` }, h("span", { class: "link-dot" }), meta.text),
    ),
    ico("chevron", "icon row-chevron"),
  );
}

// ---------------------------------------------------------------------------
// Pairing

/** Only a phone has a camera to point: the desktop dev build pastes. */
const canScan = /iPhone|iPad|Android/.test(navigator.userAgent);

class CameraDenied extends Error {}

/** Reads a QR code with the camera. The camera runs behind the WebView, so
 * the page goes transparent except for a viewfinder and a Cancel button (the
 * plugin's own full-screen view has no way out). Null when cancelled. */
async function scanCode(): Promise<string | null> {
  let state = await scanner.checkPermissions();
  if (state !== "granted" && state !== "denied") state = await scanner.requestPermissions();
  if (state !== "granted") throw new CameraDenied();

  const overlay = h(
    "div",
    { class: "scanner" },
    h("div", { class: "scanner-frame" }),
    h("p", { class: "scanner-hint" }, "Point at the QR code in tty7 on your computer"),
    h("button", { class: "button scanner-cancel", onclick: () => void scanner.cancel() }, "Cancel"),
  );
  document.documentElement.classList.add("scanning");
  document.body.append(overlay);
  try {
    return (await scanner.scan({ windowed: true, formats: [scanner.Format.QRCode] })).content;
  } catch (e) {
    if (/cancel/i.test(errorText(e))) return null;
    throw e;
  } finally {
    overlay.remove();
    document.documentElement.classList.remove("scanning");
  }
}

function pairScreen() {
  go("push", () => {
    const code = h("textarea", {
      class: "field-input code",
      placeholder: "tty7pair:…",
      rows: 4,
      autocapitalize: "off",
      spellcheck: false,
      ariaLabel: "Pairing code",
    });
    const name = h("input", {
      class: "field-input",
      value: guessDeviceName(),
      placeholder: "This phone",
      ariaLabel: "This phone's name",
      autocapitalize: "words",
    });
    const error = h("p", { class: "field-error", role: "alert" });
    const submit = h("button", { class: "button primary wide" }, "Pair");

    const sync = () => {
      submit.disabled = code.value.trim() === "";
      error.textContent = "";
    };

    const canPaste = typeof navigator.clipboard?.readText === "function";
    const paste = h("button", { class: "chip", hidden: !canPaste }, ico("paste"), "Paste");
    paste.onclick = async () => {
      try {
        code.value = (await navigator.clipboard.readText()).trim();
        sync();
      } catch {
        paste.hidden = true;
        code.focus();
      }
    };

    const scan = h("button", { class: "chip", hidden: !canScan }, ico("scan"), "Scan");
    scan.onclick = async () => {
      error.textContent = "";
      try {
        const got = (await scanCode())?.trim();
        if (got == null) return;
        if (!got.startsWith("tty7pair:")) {
          error.textContent = "That QR code isn't a tty7 pairing code.";
          return;
        }
        code.value = got;
        sync();
        // Scanning is the whole gesture: pair straight away.
        submit.click();
      } catch (e) {
        error.textContent =
          e instanceof CameraDenied
            ? "tty7 can't use the camera. Allow it in Settings, or paste the code instead."
            : sentence(errorText(e));
      }
    };

    code.oninput = sync;
    sync();

    submit.onclick = async () => {
      submit.disabled = true;
      submit.classList.add("busy");
      submit.textContent = "Pairing…";
      try {
        const host = await api.pair(code.value.trim(), name.value.trim() || "phone");
        hostScreen(host, "push");
      } catch (e) {
        error.textContent = errorText(e);
        submit.classList.remove("busy");
        submit.textContent = "Pair";
        submit.disabled = false;
      }
    };

    return screen({
      title: "Pair a machine",
      back: { label: "Machines", onclick: () => hostsScreen() },
      body: [
        h(
          "ol",
          { class: "steps" },
          // Named as the desktop's Settings names them: most people pair
          // from there, not from the command line.
          h(
            "li",
            {},
            h(
              "span",
              {},
              "In tty7 on your computer, open ",
              h("strong", {}, "Settings → Mobile"),
              " and turn on ",
              h("strong", {}, "Allow phone access"),
              ".",
            ),
          ),
          h(
            "li",
            {},
            h(
              "span",
              {},
              "Click ",
              h("strong", {}, "Show code"),
              canScan ? ", then scan it here, or copy the code and paste it below." : ", copy the code and paste it below.",
            ),
          ),
        ),
        h(
          "section",
          { class: "group" },
          h(
            "div",
            { class: "group-head" },
            h("h2", { class: "group-title" }, "Pairing code"),
            h("div", { class: "chips" }, scan, paste),
          ),
          h("div", { class: "card field" }, code),
          error,
        ),
        h(
          "section",
          { class: "group" },
          h("h2", { class: "group-title" }, "Name"),
          h("div", { class: "card field" }, name),
          h(
            "p",
            { class: "group-note" },
            "How this phone shows up under ",
            h("strong", {}, "Paired phones"),
            " on your computer.",
          ),
        ),
      ],
      footer: h("div", { class: "dock" }, submit),
    });
  });
}

function guessDeviceName() {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android";
  return "tty7 mobile";
}

// ---------------------------------------------------------------------------
// One machine: what needs you first, then every workspace as the desktop
// groups it.

/** Tries again after a dropped connection, sooner at first: 1s, 2s, 4s … up
 * to 15s between tries, for as long as the screen is up. */
function retrier(run: () => void) {
  let attempt = 0;
  let timer: number | undefined;
  return {
    schedule() {
      clearTimeout(timer);
      timer = window.setTimeout(run, Math.min(15_000, 1000 * 2 ** attempt++));
    },
    /** It worked: the next drop starts from the shortest wait again. */
    reset() {
      attempt = 0;
      clearTimeout(timer);
    },
    cancel() {
      clearTimeout(timer);
    },
  };
}

/** How long a machine may stay silent before the screen says so. */
const SLOW_CONNECT_MS = 10_000;

function hostScreen(host: Host, direction: "push" | "pop" = "pop") {
  go(direction, () => {
    const link = h("p", { class: "link" });
    // Shown here and remembered for the machine list.
    const showLink = (info: LinkInfo | null) => {
      renderLink(link, info);
      seen.set(host.id, { link: info, tabs: seen.get(host.id)?.tabs ?? null });
    };
    showLink({ path: "connecting", rtt_ms: 0 });
    const notice = h("div", { class: "notice-slot" });
    const body = h("div", { class: "stack" }, skeleton());

    let alive = true;
    let lastTree: Tree | null = null;
    let slow: number | undefined;
    let query = "";
    const draw = () => {
      if (lastTree) body.replaceChildren(...renderTree(host, lastTree, query));
    };
    const dock = floatingBar("Search tabs", (q) => {
      query = q;
      draw();
    }, { label: "New tab", run: () => lastTree && newTabSheet(host, lastTree, failed) });

    const menu = menuButton([
      { label: "Refresh", icon: "refresh", run: () => api.refresh(host.id).catch(() => start()) },
      {
        label: "Forget this machine",
        icon: "trash",
        danger: true,
        run: async () => {
          if (confirm(`Forget ${host.name}? You'll need a new pairing code to reach it again.`)) {
            await api.forget(host.id);
            hostsScreen();
          }
        },
      },
    ]);

    const view = screen({
      title: host.name,
      back: { label: "Machines", onclick: () => hostsScreen() },
      trailing: [menu],
      subtitle: link,
      body: [notice, body],
      dock,
    });

    const retry = retrier(() => start());
    let offlineNow = false;
    // The notice on screen is the dropped connection's, for a tree to clear.
    let dropped = false;
    // A network that comes back is the moment to try, not the next tick.
    const online = () => {
      if (offlineNow) start();
    };
    window.addEventListener("online", online);

    onLeave = () => {
      alive = false;
      clearTimeout(slow);
      retry.cancel();
      window.removeEventListener("online", online);
    };

    const failed = (message: string) =>
      notice.replaceChildren(noticeCard({ title: "Couldn't open a tab", body: [sentence(message)] }));

    const offline = (message: string) => {
      offlineNow = true;
      dropped = true;
      retry.schedule();
      showLink(null);
      notice.replaceChildren(
        noticeCard({
          title: `Can't reach ${host.name}`,
          body: [
            sentence(message),
            ` Check that tty7 is running on ${host.name} with phone access on.`,
          ],
          actions: [
            { label: "Try now", run: start },
            { label: "Pair again", run: () => pairScreen() },
          ],
        }),
      );
      if (!lastTree) body.replaceChildren();
    };

    const start = async () => {
      offlineNow = false;
      retry.cancel();
      clearTimeout(slow);
      // Retrying under a tree, the dropped connection's notice stays up to
      // say why the tree may be stale, until a new one replaces it.
      if (!(lastTree && dropped)) notice.replaceChildren();
      showLink({ path: "connecting", rtt_ms: 0 });
      if (!lastTree) body.replaceChildren(skeleton());
      // A machine that never answers leaves nothing to show but a spinner;
      // say what is likely wrong while still trying.
      slow = window.setTimeout(() => {
        if (alive && !lastTree)
          notice.replaceChildren(
            noticeCard({
              title: `Still looking for ${host.name}`,
              body: [
                "Check that tty7 is running there with phone access on (",
                h("strong", {}, "Settings → Mobile"),
                "). If the network changed since you paired, pairing again gives this phone its new address.",
              ],
              tone: "warn",
              actions: [{ label: "Pair again", run: () => pairScreen() }],
            }),
          );
      }, SLOW_CONNECT_MS);
      try {
        await api.watch(host.id, (msg) => {
          if (!alive) return;
          switch (msg.type) {
            case "tree":
              clearTimeout(slow);
              retry.reset();
              if (!lastTree || dropped) notice.replaceChildren();
              dropped = false;
              lastTree = msg.tree;
              seen.set(host.id, { link: seen.get(host.id)?.link ?? null, tabs: tabCount(msg.tree) });
              draw();
              break;
            case "link":
              showLink(msg.link);
              break;
            case "error":
              clearTimeout(slow);
              notice.replaceChildren(noticeCard({ title: msg.message, tone: "warn" }));
              if (!lastTree) body.replaceChildren();
              break;
            case "closed":
              offline("The connection closed.");
              break;
          }
        });
      } catch (e) {
        if (alive) {
          clearTimeout(slow);
          offline(errorText(e));
        }
      }
    };
    // Back from the background the stream may be dead, or fine and merely
    // behind: watching again covers both, since the gateway sends the whole
    // tree on every new watch.
    onResume = start;
    start();
    return view;
  });
}

function renderLink(el: HTMLElement, link: LinkInfo | null) {
  el.className = `link ${link?.path ?? "offline"}`;
  const text =
    link === null
      ? "Offline"
      : link.path === "connecting"
        ? "Connecting…"
        : `${link.path === "direct" ? "Direct" : "Relay"} · ${link.rtt_ms} ms`;
  el.replaceChildren(h("span", { class: "link-dot" }), text);
}

function skeleton() {
  const row = () =>
    h(
      "div",
      { class: "row skeleton" },
      h("span", { class: "avatar" }),
      h("span", { class: "row-text" }, h("span", { class: "bone" }), h("span", { class: "bone short" })),
    );
  return h("div", { class: "group", ariaHidden: "true" }, h("div", { class: "card" }, row(), row(), row()));
}

const STATUS_WORD: Record<AgentStatus, string> = {
  working: "Working",
  waiting: "Needs input",
  done: "Done",
  idle: "Idle",
};

/** Which machine a pane or workspace is on: a remote the desktop is linked
 * to, or null for the paired machine itself. */
type Place = { key: string; name: string } | null;

function tabCount(tree: Tree) {
  const count = (list: WorkspaceView[]) => list.reduce((n, ws) => n + ws.tabs.length, 0);
  return count(tree.workspaces) + (tree.remotes ?? []).reduce((n, r) => n + count(r.workspaces), 0);
}

/** The panes a search keeps: by tab name, pane title, directory or agent. */
function matching(ws: WorkspaceView, query: string): WorkspaceView {
  if (!query) return ws;
  const hit = (tab: TabView, pane: PaneView) =>
    [tab.name, pane.title, pane.cwd, pane.agent && agentLook(pane.agent.kind).name, ws.name].some((text) =>
      text?.toLowerCase().includes(query),
    );
  return {
    ...ws,
    tabs: ws.tabs
      .map((tab) => ({ ...tab, panes: tab.panes.filter((pane) => hit(tab, pane)) }))
      .filter((tab) => tab.panes.length > 0),
  };
}

function renderTree(host: Host, tree: Tree, query = ""): Node[] {
  if (query) {
    const groups = [
      ...tree.workspaces.map((ws) => [null, matching(ws, query)] as const),
      ...(tree.remotes ?? []).flatMap((r) => r.workspaces.map((ws) => [r, matching(ws, query)] as const)),
    ].filter(([, ws]) => ws.tabs.length > 0);
    return groups.length
      ? groups.map(([place, ws]) => workspaceGroup(host, place, ws, place?.name))
      : [h("p", { class: "search-empty" }, `No tab matches “${query}”.`)];
  }
  const out: Node[] = [];
  const remotes = tree.remotes ?? [];

  out.push(...tree.workspaces.map((ws) => workspaceGroup(host, null, ws)));
  if (tree.workspaces.length === 0) {
    out.push(
      h(
        "div",
        { class: "empty" },
        h("span", { class: "tile large" }, ico("terminal")),
        h("h2", { class: "empty-title" }, "Nothing open"),
        h("p", { class: "empty-body" }, `Open a tab in tty7 on ${host.name} and it appears here.`),
      ),
    );
  }

  // Then every machine the desktop reaches over SSH, as the desktop's
  // sidebar lists them: its own heading, its workspaces under it.
  for (const remote of remotes) {
    const state = !remote.connected
      ? "Link down"
      : remote.error
        ? "Not answering"
        : remote.pending
          ? "Reading…"
          : "Connected";
    const tone = !remote.connected || remote.error ? "offline" : remote.pending ? "connecting" : "direct";
    out.push(
      h(
        "section",
        { class: "remote" },
        h(
          "div",
          { class: "remote-head" },
          h("span", { class: "tile" }, ico("server")),
          h(
            "div",
            { class: "remote-titles" },
            h("h2", { class: "remote-name" }, remote.name),
            h(
              "p",
              { class: `link ${tone}` },
              h("span", { class: "link-dot" }),
              `SSH · ${state}`,
            ),
          ),
        ),
        !remote.connected &&
          h(
            "p",
            { class: "remote-note" },
            `tty7 on ${host.name} lost its link to ${remote.name}. Reconnect it there to reach its workspaces.`,
          ),
        remote.error && h("p", { class: "remote-note" }, sentence(remote.error)),
        remote.pending && skeleton(),
        remote.connected &&
          !remote.error &&
          !remote.pending &&
          remote.workspaces.length === 0 &&
          h("p", { class: "remote-note" }, `No workspaces on ${remote.name}.`),
        ...remote.workspaces.map((ws) => workspaceGroup(host, remote, ws)),
      ),
    );
  }
  return out;
}

/** A workspace is one card of its tabs, named as the desktop's sidebar names
 * them; a split tab gives each of its panes a row. `where` names the machine
 * when a search mixes them. */
function workspaceGroup(host: Host, place: Place, ws: WorkspaceView, where?: string) {
  const rows = ws.tabs.flatMap((tab) => tab.panes.map((pane) => paneRow(host, place, tab, pane)));
  return h(
    "section",
    { class: "group" },
    h(
      "div",
      { class: "group-head" },
      h("h2", { class: "group-title" }, where ? `${where} · ${workspaceName(ws.name)}` : workspaceName(ws.name)),
      h("span", { class: "group-count" }, String(ws.tabs.length)),
    ),
    rows.length ? h("div", { class: "card" }, ...rows) : h("p", { class: "group-empty" }, "No tabs open."),
  );
}

/** The agents a new tab can start in, and the command that starts each. */
const STARTERS = [
  { kind: "claude", command: "claude" },
  { kind: "codex", command: "codex" },
  { kind: null, command: null },
] as const;

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
    // Private storage off: the choice is just not kept.
  }
}

/** The sheet "+" opens on a machine: pick what runs, pick the workspace, Open.
 * The tab starts in the directory its workspace's last tab is in, sized to
 * this screen, and the agent's command is typed into it once it is live. */
function newTabSheet(host: Host, tree: Tree, failed: (message: string) => void) {
  type Target = { place: Place; ws: WorkspaceView };
  const targets: Target[] = [
    ...tree.workspaces.map((ws) => ({ place: null, ws })),
    ...(tree.remotes ?? [])
      .filter((r) => r.connected && !r.error && !r.pending)
      .flatMap((r) => r.workspaces.map((ws) => ({ place: { key: r.key, name: r.name }, ws }))),
  ];
  let starter = Math.max(0, STARTERS.findIndex((s) => (s.kind ?? "shell") === remembered("newtab.agent")));
  let target = 0;

  const agents = h("div", { class: "agent-grid" });
  const places = h("div", { class: "card" });
  const open = h("button", { class: "button primary wide sheet-open" }, "Open");
  const error = h("p", { class: "field-error", role: "alert" });

  const draw = () => {
    agents.replaceChildren(
      ...STARTERS.map((s, i) =>
        h(
          "button",
          { class: i === starter ? "agent-choice on" : "agent-choice", onclick: () => ((starter = i), draw()) },
          avatar(s.kind ? { kind: s.kind, status: "idle" } : null),
          h("span", {}, s.kind ? agentLook(s.kind).name : "Shell"),
        ),
      ),
    );
    places.replaceChildren(
      ...targets.map((t, i) =>
        h(
          "button",
          { class: "row choice", onclick: () => ((target = i), draw()) },
          h("span", { class: "row-title" }, workspaceName(t.ws.name)),
          h("span", { class: "row-meta" }, t.place?.name ?? `${t.ws.tabs.length} ${t.ws.tabs.length === 1 ? "tab" : "tabs"}`),
          i === target ? ico("check", "icon choice-check") : h("span", { class: "choice-check" }),
        ),
      ),
    );
    open.disabled = targets.length === 0;
  };
  draw();

  const sheet = h(
    "div",
    { class: "sheet", role: "dialog", ariaLabel: "New tab" },
    h("span", { class: "sheet-grip" }),
    h(
      "div",
      { class: "sheet-head" },
      h("h2", { class: "sheet-title" }, "New tab"),
      h("button", { class: "round", ariaLabel: "Close", onclick: () => close() }, ico("close")),
    ),
    h("div", { class: "sheet-body" },
      h("section", { class: "sheet-group" }, h("h3", { class: "group-title" }, "Agent"), agents),
      h(
        "section",
        { class: "sheet-group" },
        h("h3", { class: "group-title" }, "Workspace"),
        targets.length ? places : h("p", { class: "group-empty" }, `Open a workspace in tty7 on ${host.name} first.`),
      ),
      error,
    ),
    open,
  );
  const scrim = h("div", { class: "scrim", onclick: (e: Event) => e.target === scrim && close() }, sheet);
  const close = () => {
    scrim.classList.add("leaving");
    setTimeout(() => scrim.remove(), still.matches ? 0 : 220);
  };
  open.onclick = async () => {
    const { place, ws } = targets[target];
    const s = STARTERS[starter];
    remember("newtab.agent", s.kind ?? "shell");
    open.disabled = true;
    error.textContent = "";
    const cwd = ws.tabs.at(-1)?.panes[0]?.cwd ?? null;
    try {
      const created = await api.tabNew(host.id, place?.key ?? null, ws.id, cwd, phoneGrid());
      scrim.remove();
      const title = s.kind ? agentLook(s.kind).name : "shell";
      terminalScreen(host, place, { id: created.pane_id, title, cwd }, title, s.command ?? undefined);
    } catch (e) {
      error.textContent = sentence(errorText(e));
      failed(errorText(e));
      open.disabled = false;
    }
  };
  document.body.append(scrim);
}

/** The grid that fills this screen at the readable size: what a tab started
 * here is spawned at, since no desktop window is showing it yet. */
function phoneGrid() {
  const cellW = READABLE_PX * CELL_EM;
  const cellH = READABLE_PX * 1.18;
  // The terminal screen's bar, its dock (keys, page dots, message box, the
  // home indicator's gap) and the xterm padding.
  const chrome = 56 + 132 + 16;
  return {
    cols: Math.max(20, Math.floor((window.innerWidth - 12) / cellW)),
    rows: Math.max(5, Math.floor((window.innerHeight - chrome) / cellH)),
  };
}

/** The desktop leaves a workspace unnamed as "-". */
function workspaceName(name: string) {
  return name && name !== "-" ? name : "Untitled workspace";
}

/** A pane's row: its tab's name first, as on the desktop, then what the pane
 * is doing — its agent's state, or where its shell is. */
function paneRow(host: Host, place: Place, tab: TabView, pane: PaneView) {
  const agent = pane.agent;
  const sub: Child[] = [];
  if (agent && agent.status !== "idle")
    sub.push(h("span", { class: `status-word ${agent.status}` }, STATUS_WORD[agent.status]));
  // A tab named after its directory says the cwd already; what tells its
  // panes apart then is what runs in them.
  const namedByPath = /^[~/]/.test(tab.name);
  const split = tab.panes.length > 1 || namedByPath ? pane.title : null;
  const dir = agent || namedByPath ? null : shortPath(pane.cwd);
  const detail = [split, agent?.message ?? dir].filter(Boolean).join(" · ");
  if (detail) sub.push(sub.length ? ` · ${detail}` : detail);
  return h(
    "button",
    {
      class: tab.hibernated ? "row asleep" : "row",
      onclick: () => terminalScreen(host, place, pane, tab.name),
    },
    avatar(agent),
    h(
      "span",
      { class: "row-text" },
      h(
        "span",
        { class: "row-title" },
        tab.name,
        tab.hibernated && h("span", { class: "tag" }, "Asleep"),
      ),
      sub.length > 0 && h("span", { class: "row-sub" }, ...sub),
    ),
    agent && agent.status !== "idle" && agent.status !== "done" && h("span", { class: `status-dot ${agent.status}` }),
    ico("chevron", "icon row-chevron"),
  );
}

/** A pane's avatar, as the desktop's tab strip draws it: the agent's mark on
 * its brand colour, or a terminal, with the status dot as a badge. Waiting is
 * hollow, so it differs from Done in shape and not only in hue. */
function avatar(agent: AgentView | null | undefined, cls = "avatar") {
  const el = h("span", { class: cls });
  if (agent) {
    const look = agentLook(agent.kind);
    el.title = `${look.name}: ${STATUS_WORD[agent.status]}`;
    if (look.mark) {
      const mark = h("span", { class: "mark" });
      mark.style.setProperty("--mark", `url("${look.mark}")`);
      el.append(mark);
    } else el.append(h("span", { class: "glyph" }, look.name.slice(0, 2)));
  } else el.append(h("span", { class: "glyph" }, ">_"));
  return el;
}

/** The last two segments of a path: the part that tells panes apart. */
function shortPath(path: string | null | undefined) {
  if (!path) return "";
  const parts = path.split("/").filter(Boolean);
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join("/")}`;
}

interface NoticeParts {
  title: string;
  body?: Child[];
  tone?: "warn" | "bad";
  actions?: { label: string; run: () => void }[];
}

function noticeCard({ title, body, tone = "bad", actions = [] }: NoticeParts) {
  return h(
    "div",
    { class: `notice ${tone}`, role: "status" },
    ico("alert", "icon notice-icon"),
    h(
      "div",
      { class: "notice-text" },
      h("p", { class: "notice-title" }, title),
      body && h("p", { class: "notice-body" }, ...body),
      actions.length > 0 &&
        h(
          "div",
          { class: "notice-actions" },
          ...actions.map((a) => h("button", { class: "button tinted small", onclick: a.run }, a.label)),
        ),
    ),
  );
}

interface MenuItem {
  label: string;
  icon: keyof typeof icon;
  danger?: boolean;
  run: () => void;
}

/** A trailing ⋯ button with a small menu that drops from it. Items can be
 * given as a function, to be read afresh each time it opens. */
function menuButton(items: MenuItem[] | (() => MenuItem[]), cls = "nav-icon") {
  const wrap = h("div", { class: "menu-wrap" });
  const list = h("div", { class: "menu", role: "menu", hidden: true });
  const outside = (e: Event) => {
    if (!wrap.contains(e.target as Node)) close();
  };
  const close = () => {
    list.hidden = true;
    document.removeEventListener("pointerdown", outside, true);
  };
  const fill = () => {
    list.replaceChildren();
    for (const item of typeof items === "function" ? items() : items)
      list.append(
      h(
        "button",
        {
          class: item.danger ? "menu-item danger" : "menu-item",
          role: "menuitem",
          onclick: () => {
            close();
            item.run();
          },
        },
          h("span", {}, item.label),
          ico(item.icon),
        ),
      );
  };
  const button = h("button", { class: cls, ariaLabel: "More" }, ico("more"));
  button.onclick = () => {
    if (!list.hidden) return close();
    fill();
    list.hidden = false;
    document.addEventListener("pointerdown", outside, true);
  };
  wrap.append(button, list);
  return wrap;
}

// ---------------------------------------------------------------------------
// A terminal. The pane keeps the size its desktop window gave it — the phone
// watches rather than attaches — so the font shrinks to fit the width.

// The desktop's Light and Dark ANSI palettes (src/ui/presets.rs), so a pane
// reads the same on the phone as in the window it lives in, on the app's own
// neutral surface.
const ANSI = {
  light: ["#24292e", "#d1242f", "#1a7f37", "#9a6700", "#0969da", "#8250df", "#1b7c83", "#6e7781", "#57606a", "#cf222e", "#1f883d", "#bf8700", "#218bff", "#a475f9", "#3192aa", "#8c959f"],
  dark: ["#616161", "#ff8272", "#b4fa72", "#fefdc2", "#a5d5fe", "#ff8ffd", "#d0d1fe", "#f1f1f1", "#8e8e8e", "#ffc4bd", "#d6fcb9", "#fefdd5", "#c1e3fe", "#ffb1fe", "#e5e6fe", "#feffff"],
};
const NAMES = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"] as const;

const darkScheme = matchMedia("(prefers-color-scheme: dark)");

function terminalTheme(): ITheme {
  const dark = darkScheme.matches;
  const ansi = dark ? ANSI.dark : ANSI.light;
  const theme: Record<string, string> = dark
    ? { background: "#1e1e20", foreground: "#ececed", cursor: "#ececed", cursorAccent: "#1e1e20", selectionBackground: "#ffffff2e" }
    : { background: "#fcfcfb", foreground: "#1c1c1e", cursor: "#1c1c1e", cursorAccent: "#fcfcfb", selectionBackground: "#0000001f" };
  // xterm's scrollbar, drawn to match the system indicator WebKit gives the
  // pan (style.css shapes it).
  const bar = dark ? "#ffffff" : "#000000";
  theme.scrollbarSliderBackground = `${bar}59`;
  theme.scrollbarSliderHoverBackground = `${bar}59`;
  theme.scrollbarSliderActiveBackground = `${bar}73`;
  NAMES.forEach((n, i) => {
    theme[n] = ansi[i];
    theme[`bright${n[0].toUpperCase()}${n.slice(1)}`] = ansi[i + 8];
  });
  return theme as ITheme;
}

/** A key on the bar: a sequence to send, the ctrl latch, or the clipboard.
 * `text` is what it shows when that is not its label; `flex` its share of
 * the row. */
type Key = {
  label: string;
  seq: string;
  text?: string;
  icon?: keyof typeof icon;
  latch?: boolean;
  paste?: boolean;
  flex?: number;
};

/** The key row, a page at a time: what an agent and a shell need most first,
 * a swipe away the rest. */
const KEY_PAGES: Key[][] = [
  [
    { label: "esc", seq: "\x1b", flex: 1.2 },
    { label: "tab", seq: "\t", flex: 1.2 },
    // Claude Code's mode switch, among others.
    { label: "Shift Tab", text: "⇧tab", seq: "\x1b[Z", flex: 1.4 },
    { label: "ctrl", seq: "", latch: true, flex: 1.3 },
    { label: "^C", seq: "\x03" },
    { label: "Up", seq: "\x1b[A", icon: "up", flex: 0.9 },
    { label: "Down", seq: "\x1b[B", icon: "down", flex: 0.9 },
    // Enter on its own, without opening the keyboard: confirming an agent's
    // highlighted choice, or a prompt's default.
    { label: "Enter", seq: "\r", icon: "enter" },
  ],
  [
    { label: "Left", seq: "\x1b[D", icon: "left" },
    { label: "Right", seq: "\x1b[C", icon: "right" },
    // Quick answers: an agent's numbered choices (Claude Code takes the digit
    // alone) and y/n prompts. Each is just the keystroke; ⏎ is there for the
    // prompts that also want Enter.
    { label: "1", seq: "1" },
    { label: "2", seq: "2" },
    { label: "3", seq: "3" },
    { label: "y", seq: "y" },
    { label: "n", seq: "n" },
    { label: "Paste", seq: "", icon: "paste", paste: true, flex: 1.2 },
  ],
  [
    { label: "|", seq: "|" },
    { label: "/", seq: "/" },
    { label: "~", seq: "~" },
    { label: "-", seq: "-" },
    { label: "_", seq: "_" },
    { label: "Home", seq: "\x1b[H", flex: 1.4 },
    { label: "End", seq: "\x1b[F", flex: 1.4 },
  ],
];

/** Hack's advance width, in ems — what the fit divides by. */
const CELL_EM = 0.602;
/** The smallest font a pane is read at before it pans instead of shrinking. */
const READABLE_PX = 11;

/** What was typed into a pane's compose box and not sent yet, by pane. Kept
 * across leaving the pane, and across a send that did not get through. */
const drafts = new Map<string, string>();

/** `run` is typed into the pane, then Enter, once it is first live: the
 * agent a new tab was opened for. */
function terminalScreen(host: Host, place: Place, pane: PaneView, title: string, run?: string) {
  go("push", () => {
    // What the pane is doing and whether keystrokes will land, in words: the
    // one line under the title.
    const stateWord = h("span", {}, "Connecting…");
    const state = h("span", { class: "term-state connecting" }, h("span", { class: "link-dot" }), stateWord);
    // On a remote, the path says which machine it is on, the way a prompt does.
    const where = (path: string | null | undefined) =>
      place && path ? `${place.name}:${shortPath(path)}` : shortPath(path);
    const cwd = h("span", { class: "term-cwd" }, where(pane.cwd));
    const sub = h("span", { class: "term-sub" }, state, cwd);
    // Copying, the fit and the phone's size live in the ⋯ menu; the bar keeps
    // only the way back and what this is.
    const menu = menuButton(
      () => [
        { label: "Select text", icon: "copy", run: () => selecting(copyView.hidden) },
        ...(cramped && !leased
          ? [
              {
                label: readable ? "Fit the whole width" : "Make the text readable",
                icon: readable ? ("fit" as const) : ("zoom" as const),
                run: toggleZoom,
              },
            ]
          : []),
        {
          label: wanted ? "Give back the desktop's size" : "Use this phone's size",
          icon: "phone" as const,
          run: toggleTake,
        },
      ],
      "round",
    );
    const bar = h(
      "header",
      { class: "term-nav" },
      h("button", { class: "round", ariaLabel: `Back to ${host.name}`, onclick: () => hostScreen(host) }, ico("back")),
      h("div", { class: "term-titles" }, h("span", { class: "term-title" }, title), sub),
      menu,
    );
    const screenEl = h("div", { class: "term" });
    const banner = h("div", { class: "term-banner-slot" });
    // Selecting in xterm itself does not work by touch. The pane's text is
    // laid out here instead, as a plain page the phone selects in its own
    // way — handles, magnifier, Copy.
    const copyText = h("pre", { class: "term-copy-text" });
    const copyDone = h("button", { class: "button tinted small" }, "Done");
    const copyView = h(
      "div",
      { class: "term-copy", hidden: true },
      h("div", { class: "term-copy-bar" }, h("span", {}, "Select text to copy"), copyDone),
      copyText,
    );
    const pages = h("div", { class: "key-pages" });
    const dots = h("div", { class: "key-dots", ariaHidden: "true" });

    // The message box: a real text field, so the phone's own keyboard works
    // in full — autocorrect, dictation, an IME's candidates, moving the
    // caret — and what is written goes in whole, then Enter.
    const draftKey = `${host.id}/${place?.key ?? ""}/${pane.id}`;
    const field = h("textarea", {
      class: "compose-input",
      rows: 1,
      value: drafts.get(draftKey) ?? "",
      placeholder: pane.agent ? `Message ${agentLook(pane.agent.kind).name}…` : "Type a command…",
      enterKeyHint: "send",
      ariaLabel: "Message",
    });
    const sendKey = h("button", { class: "round send", ariaLabel: "Send" }, ico("send"));
    // Typing straight into the terminal, key by key, for what a message box
    // cannot do: a full-screen program, a password prompt.
    const keyboard = h("button", { class: "round", ariaLabel: "Type into the terminal" }, ico("keyboard"));
    const compose = h("div", { class: "compose" }, field, sendKey, keyboard);
    // What typing straight into the terminal goes through. xterm's own hidden
    // textarea is not: iOS input methods never commit into it (a pinyin
    // candidate stays unwritten), so this one, a plain field the keyboard
    // treats like any other, takes the keys and hands them on as they are
    // committed. Nothing is sent mid-composition.
    const typing = h("textarea", {
      class: "term-typing",
      rows: 1,
      autocapitalize: "off",
      spellcheck: false,
      ariaLabel: "Type into the terminal",
    });
    typing.setAttribute("autocorrect", "off");
    typing.setAttribute("autocomplete", "off");

    const view = h(
      "div",
      { class: "screen term-screen" },
      bar,
      h("div", { class: "term-wrap" }, screenEl, typing, copyView, banner),
      h("div", { class: "term-dock" }, pages, dots, compose),
    );

    const term = new Terminal({
      cols: 80,
      rows: 24,
      fontSize: 11,
      fontFamily: "Hack, Menlo, ui-monospace, monospace",
      scrollback: 5000,
      cursorBlink: false,
      theme: terminalTheme(),
      // Nothing reaches the pane from xterm itself. Keys come through `typing`
      // and the key bar (iOS input methods never commit into xterm's own
      // textarea). And xterm's answers to a program's queries — colours,
      // device attributes, the cursor's position — stay unsent: the desktop,
      // which owns the pane, answers those already, and a second answer from
      // here arrives late, on a replay, and lands in the shell as typed text.
      disableStdin: true,
    });

    let handle: number | null = null;
    // Whether keystrokes land: set by a successful open, cleared by anything
    // that says they no longer do.
    let live = false;
    let ctrl = false;
    let cols = 80;
    let alive = true;
    // Whether the pane is wider than the phone can show legibly.
    let cramped = false;
    // The pane exited: nothing to reconnect to.
    let ended = false;
    // The phone's size: `wanted` is what the user asked for, `leased` what
    // the daemon confirmed, `sent` the grid last asked for.
    let wanted = false;
    let leased = false;
    let releasing = false;
    let sent = "";
    let ctrlKey: HTMLButtonElement | null = null;

    // Typing that does not get through is said, never dropped quietly: the
    // pane goes offline with a way back, rather than looking live.
    // A drop is retried on its own, sooner at first; the pane stays on
    // screen as it was until the new stream replaces it.
    const retry = retrier(() => reopen());
    const offline = (message: string) => {
      live = false;
      setState("connecting", "Reconnecting");
      showBanner(`${sentence(message)} Reconnecting…`, { label: "Try now", run: reopen });
      retry.schedule();
    };
    // Only the first refusal speaks: a key typed just before it fails on its
    // own, with a vaguer reason.
    const refused = (message: string) => {
      if (alive && live) offline(message);
    };
    const input = (data: string): Promise<boolean> => {
      if (handle === null || !live) return Promise.resolve(false);
      return api.paneInput(handle, data).then(
        () => true,
        (e) => {
          refused(errorText(e));
          return false;
        },
      );
    };
    const send = (data: string) => {
      if (ctrl && data.length === 1) {
        const c = data.toUpperCase().charCodeAt(0);
        if (c >= 64 && c <= 95) data = String.fromCharCode(c - 64);
        ctrl = false;
        ctrlKey?.classList.remove("on");
      }
      // Typing brings a pane scrolled back into its history to the prompt.
      term.scrollToBottom();
      void input(data);
    };

    const canPaste = typeof navigator.clipboard?.readText === "function";
    const paste = async () => {
      let text: string;
      try {
        text = await navigator.clipboard.readText();
      } catch {
        return; // Declined at the system's paste prompt.
      }
      if (!text) return;
      if (document.activeElement === field) {
        field.setRangeText(text, field.selectionStart, field.selectionEnd, "end");
        edited();
      } else {
        const body = text.replace(/\r?\n/g, "\r");
        send(term.modes.bracketedPasteMode ? `\x1b[200~${body}\x1b[201~` : body);
      }
    };

    for (const page of KEY_PAGES) {
      const row = h("div", { class: "key-page" });
      for (const k of page) {
        if (k.paste && !canPaste) continue;
        const key = h("button", { class: "key", ariaLabel: k.label }, k.icon ? ico(k.icon) : (k.text ?? k.label));
        key.style.flex = String(k.flex ?? 1);
        if (k.latch) ctrlKey = key;
        // Keep focus where it is — the terminal, the message box or neither —
        // so the soft keyboard stays as it is.
        key.onpointerdown = (e) => e.preventDefault();
        key.onclick = () => {
          if (k.latch) {
            ctrl = !ctrl;
            key.classList.toggle("on", ctrl);
          } else if (k.paste) void paste();
          else send(k.seq);
        };
        row.append(key);
      }
      pages.append(row);
      dots.append(h("span", {}));
    }
    const showPage = () => {
      const at = Math.round(pages.scrollLeft / Math.max(1, pages.clientWidth));
      [...dots.children].forEach((dot, i) => dot.classList.toggle("on", i === at));
    };
    pages.addEventListener("scroll", showPage, { passive: true });
    showPage();

    const grow = () => {
      // Off the page it measures nothing; the first fit comes once it is on.
      if (!field.isConnected) return;
      field.style.height = "auto";
      field.style.height = `${field.scrollHeight}px`;
    };
    const edited = () => {
      if (field.value) drafts.set(draftKey, field.value);
      else drafts.delete(draftKey);
      // Written, the round button sends; empty, it is the keyboard's.
      sendKey.hidden = !field.value;
      keyboard.hidden = !!field.value;
      grow();
    };
    field.addEventListener("input", edited);
    const submit = async () => {
      const body = field.value.replace(/\r?\n/g, "\r");
      // Several lines go in as one paste where the program asked for that,
      // so an agent takes them as one message and a shell does not run each.
      const data = body.includes("\r") && term.modes.bracketedPasteMode ? `\x1b[200~${body}\x1b[201~` : body;
      if (data && !(await input(data))) return;
      // Enter as its own write: a program that tells pasting from typing by
      // how the bytes arrive would otherwise take it as part of the text.
      // An empty box sends Enter alone.
      if (!(await input("\r"))) return;
      field.value = "";
      edited();
    };
    field.addEventListener("keydown", (e) => {
      // Enter that confirms an IME's candidate is the IME's, not a send.
      if (e.key !== "Enter" || e.shiftKey || e.isComposing || e.keyCode === 229) return;
      e.preventDefault();
      void submit();
    });
    sendKey.onpointerdown = (e) => e.preventDefault();
    sendKey.onclick = () => void submit();

    keyboard.onpointerdown = (e) => e.preventDefault();
    keyboard.onclick = () => {
      if (document.activeElement === typing) typing.blur();
      else typing.focus({ preventScroll: true });
    };
    typing.addEventListener("focus", () => keyboard.classList.add("on"));
    typing.addEventListener("blur", () => keyboard.classList.remove("on"));

    // Committed text goes to the pane and the field is emptied again; while
    // an input method is composing, the field holds the candidate.
    let imeOpen = false;
    const flush = () => {
      if (imeOpen || !typing.value) return;
      const text = typing.value.replace(/\r?\n/g, "\r");
      typing.value = "";
      send(text);
    };
    typing.addEventListener("compositionstart", () => (imeOpen = true));
    typing.addEventListener("compositionend", () => {
      imeOpen = false;
      // The committed text is in the field after this event, not during it.
      setTimeout(flush);
    });
    typing.addEventListener("input", (e) => {
      if (!(e as InputEvent).isComposing) flush();
    });
    // The keys a field would keep to itself, as the terminal's: a Backspace on
    // the empty field is the pane's, and so on. An input method's own Enter
    // and Backspace (keyCode 229) stay with it.
    const TYPING_KEYS: Record<string, string> = {
      Backspace: "\x7f",
      Enter: "\r",
      Tab: "\t",
      Escape: "\x1b",
      ArrowUp: "\x1b[A",
      ArrowDown: "\x1b[B",
      ArrowRight: "\x1b[C",
      ArrowLeft: "\x1b[D",
    };
    typing.addEventListener("keydown", (e) => {
      if (imeOpen || e.isComposing || e.keyCode === 229) return;
      const seq = TYPING_KEYS[e.key];
      if (!seq || (e.key === "Backspace" && typing.value)) return;
      e.preventDefault();
      send(e.shiftKey && e.key === "Tab" ? "\x1b[Z" : seq);
    });

    // Two ways to show a pane wider than the phone. Fitted, the font shrinks
    // until every column shows; readable, the font stays legible and the view
    // pans sideways, following the cursor. A pane that fits legibly is simply
    // fitted, and the toggle has nothing to offer.
    let readable = true;
    const fittedSize = () => {
      const width = screenEl.clientWidth - 12;
      return width <= 0 ? READABLE_PX : Math.min(14, Math.floor((width / cols / CELL_EM) * 10) / 10);
    };
    const fit = () => {
      // Taken over, the pane is exactly the phone's width at the readable
      // size: nothing to shrink, nothing to pan.
      if (leased) {
        cramped = false;
        term.options.fontSize = READABLE_PX;
        screenEl.classList.remove("panning");
        return;
      }
      const fitted = fittedSize();
      cramped = fitted < READABLE_PX;
      term.options.fontSize = cramped && readable ? READABLE_PX : Math.max(4, fitted);
      screenEl.classList.toggle("panning", cramped && readable);
      follow();
      // The new font size is laid out on the next frame.
      requestAnimationFrame(pinScrollbar);
    };
    // xterm puts its scrollbar at the right of its own box, which pans with
    // the text; shift it so it stays at the right of what is on screen. The
    // `translate` property, not `transform`, so it never fights xterm's own
    // styling of the bar.
    const pinScrollbar = () => {
      const box = screenEl.querySelector<HTMLElement>(".xterm");
      const bar = box?.querySelector<HTMLElement>(".xterm-scrollable-element > .scrollbar.vertical");
      if (!box || !bar) return;
      if (!screenEl.classList.contains("panning")) {
        bar.style.translate = "";
        return;
      }
      const edge = screenEl.scrollLeft + screenEl.clientWidth - parseFloat(getComputedStyle(screenEl).paddingRight);
      bar.style.translate = `${Math.min(0, edge - (box.offsetLeft + box.offsetWidth))}px 0`;
    };
    screenEl.addEventListener("scroll", pinScrollbar, { passive: true });

    // Scrolling the scrollback by touch. xterm 6 carries a gesture recogniser
    // but registers nothing with it, so a swipe over the terminal never
    // reaches the scrollback. A swipe is claimed by its first few points:
    // mostly vertical, it scrolls the buffer here, a row at a time, and
    // coasts after the finger lifts; mostly sideways, it is left to the
    // native pan of a pane wider than the phone.
    let touch: { x: number; y: number; axis: "x" | "y" | null; carry: number; samples: [number, number][] } | null =
      null;
    let coast = 0;
    const rowHeight = () => {
      const drawn = screenEl.querySelector<HTMLElement>(".xterm-screen");
      return drawn && term.rows ? drawn.clientHeight / term.rows : 14;
    };
    // Moves the buffer by a distance in pixels, keeping what is short of a
    // row for the next move. Dragging down goes back in the scrollback.
    const scrollBy = (dy: number, carry: number) => {
      const total = carry + dy;
      const rows = Math.trunc(total / rowHeight());
      if (rows) term.scrollLines(-rows);
      return total - rows * rowHeight();
    };
    screenEl.addEventListener(
      "touchstart",
      (e) => {
        cancelAnimationFrame(coast);
        if (e.touches.length !== 1) return (touch = null);
        const t = e.touches[0];
        touch = { x: t.clientX, y: t.clientY, axis: null, carry: 0, samples: [[t.clientY, e.timeStamp]] };
      },
      { passive: true },
    );
    screenEl.addEventListener(
      "touchmove",
      (e) => {
        if (!touch || e.touches.length !== 1) return;
        const t = e.touches[0];
        if (!touch.axis) {
          const dx = Math.abs(t.clientX - touch.x);
          const dy = Math.abs(t.clientY - touch.y);
          if (Math.max(dx, dy) < 6) return;
          touch.axis = dy > dx ? "y" : "x";
        }
        if (touch.axis !== "y") return;
        e.preventDefault();
        const last = touch.samples[touch.samples.length - 1][0];
        touch.carry = scrollBy(t.clientY - last, touch.carry);
        touch.samples.push([t.clientY, e.timeStamp]);
        if (touch.samples.length > 5) touch.samples.shift();
      },
      { passive: false },
    );
    screenEl.addEventListener(
      "touchend",
      (e) => {
        const lifted = touch;
        touch = null;
        // A tap: the keyboard comes up for typing into the pane. The mouse
        // events the tap would turn into are cancelled, or xterm would move
        // focus to its own textarea.
        if (lifted && !lifted.axis && e.cancelable) {
          e.preventDefault();
          typing.focus({ preventScroll: true });
          return;
        }
        if (lifted?.axis === "y") glide(lifted);
      },
      { passive: false },
    );
    const glide = (lifted: NonNullable<typeof touch>) => {
      if (lifted.samples.length < 2) return;
      const [y0, t0] = lifted.samples[0];
      const [y1, t1] = lifted.samples[lifted.samples.length - 1];
      // Pixels per millisecond at the lift, eased out over about a second.
      let velocity = (y1 - y0) / Math.max(1, t1 - t0);
      let carry = lifted.carry;
      let at = performance.now();
      const step = (now: number) => {
        if (!alive) return;
        const dt = now - at;
        at = now;
        velocity *= Math.pow(0.995, dt);
        if (Math.abs(velocity) < 0.02) return;
        carry = scrollBy(velocity * dt, carry);
        coast = requestAnimationFrame(step);
      };
      coast = requestAnimationFrame(step);
    };
    const follow = () => {
      if (!screenEl.classList.contains("panning")) return;
      const cell = screenEl.scrollWidth / cols;
      const x = term.buffer.active.cursorX * cell;
      const view = screenEl.clientWidth;
      if (x < screenEl.scrollLeft + cell * 4 || x > screenEl.scrollLeft + view - cell * 4)
        screenEl.scrollLeft = Math.max(0, x - view / 2);
    };
    const toggleZoom = () => {
      readable = !readable;
      fit();
    };

    // Taking the pane over (the lets are with the others above): it runs at
    // the phone's grid while this screen is open, and the desktop keeps its
    // window and can take it back.
    const takeGrid = () => {
      const width = screenEl.clientWidth - 12;
      const height = screenEl.clientHeight - 16;
      // A row's height, measured off what xterm drew and scaled to the size
      // the pane will be read at; the metrics guess only before the first draw.
      const drawn = screenEl.querySelector<HTMLElement>(".xterm-screen");
      const now = term.options.fontSize ?? READABLE_PX;
      const rowH =
        drawn && term.rows && drawn.clientHeight
          ? ((drawn.clientHeight / term.rows) * READABLE_PX) / now
          : READABLE_PX * 1.18;
      return {
        cols: Math.max(20, Math.floor(width / (READABLE_PX * CELL_EM))),
        rows: Math.max(5, Math.floor(height / rowH)),
      };
    };
    const askLease = () => {
      if (handle === null || !live || !wanted) return;
      const grid = takeGrid();
      const key = `${grid.cols}x${grid.rows}`;
      if (leased && key === sent) return;
      sent = key;
      api.paneLease(handle, grid).catch(() => {});
    };
    // Taken over is said under the title, beside Live.
    const liveLabel = () => (leased ? "Live · phone size" : "Live");
    const toggleTake = () => {
      wanted = !wanted;
      if (wanted) {
        sent = "";
        askLease();
      } else if (handle !== null && leased) {
        releasing = true;
        api.paneLease(handle, null).catch(() => {});
      }
    };
    // The keyboard coming up or the phone turning changes the grid; asked
    // for once it settles, not on every step of the animation.
    let regrid: number | undefined;
    const regridSoon = () => {
      clearTimeout(regrid);
      if (wanted) regrid = window.setTimeout(askLease, 250);
    };
    const leaseEvent = (held: boolean, refused: string | null | undefined) => {
      const was = leased;
      leased = held;
      if (held) {
        banner.replaceChildren();
      } else {
        sent = "";
        const ours = releasing;
        releasing = false;
        if (refused) {
          wanted = false;
          showBanner(sentence(refused));
        } else if (was && !ours) {
          // Nobody here let go: the desktop took it back, or another device
          // took it over. Asking again is one tap, not automatic.
          wanted = false;
          showBanner("The desktop took this pane back.", {
            label: "Take over again",
            run: () => {
              banner.replaceChildren();
              toggleTake();
            },
          });
        }
      }
      if (live) setState("live", liveLabel());
      fit();
    };
    term.onCursorMove(follow);
    // The whole buffer, scrollback included, as lines of text. A row the
    // terminal wrapped joins the one before it, so a long URL or path comes
    // out whole; its trailing blanks are real and kept.
    const bufferText = () => {
      const buf = term.buffer.active;
      const lines: string[] = [];
      for (let y = 0; y < buf.length; y++) {
        const line = buf.getLine(y);
        if (!line) continue;
        const text = line.translateToString(!buf.getLine(y + 1)?.isWrapped);
        if (line.isWrapped && lines.length) lines[lines.length - 1] += text;
        else lines.push(text);
      }
      while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
      return lines.join("\n");
    };
    const selecting = (on: boolean) => {
      copyView.hidden = !on;
      if (!on) {
        getSelection()?.removeAllRanges();
        copyText.textContent = "";
        return;
      }
      typing.blur();
      field.blur();
      copyText.textContent = bufferText();
      copyText.scrollTop = copyText.scrollHeight;
    };
    copyDone.onclick = () => selecting(false);

    const retheme = () => {
      term.options.theme = terminalTheme();
    };
    window.addEventListener("resize", fit);
    window.addEventListener("resize", regridSoon);
    darkScheme.addEventListener("change", retheme);

    const setState = (cls: string, label: string) => {
      state.className = `term-state ${cls}`;
      stateWord.textContent = label;
    };
    const showBanner = (text: string, action?: { label: string; run: () => void }) =>
      banner.replaceChildren(
        h(
          "div",
          { class: "term-banner" },
          h("span", {}, text),
          action && h("button", { class: "button tinted small", onclick: action.run }, action.label),
        ),
      );

    // Which open is current. A stream that was replaced may still deliver
    // its last bytes or its error; those are not this pane's any more.
    let generation = 0;
    const open = async () => {
      const mine = ++generation;
      const current = () => alive && mine === generation;
      // The screen is cleared for the replay when it starts, not before, so a
      // retry that fails leaves the last good screen up.
      let replayed = false;
      const replay = () => {
        if (replayed) return;
        replayed = true;
        term.reset();
      };
      live = false;
      retry.cancel();
      if (!banner.hasChildNodes()) setState("connecting", "Connecting");
      try {
        const opened = await api.paneOpen(
          host.id,
          place?.key ?? null,
          pane.id,
          (bytes) => {
            if (!current()) return;
            replay();
            term.write(bytes);
          },
          (event) => {
            if (!current()) return;
            if (event.type === "size") replay();
            switch (event.type) {
              case "size":
                cols = event.cols;
                term.resize(event.cols, event.rows);
                fit();
                break;
              case "agent":
                field.placeholder = event.agent
                  ? `Message ${agentLook(event.agent.kind).name}…`
                  : "Type a command…";
                break;
              case "cwd":
                cwd.textContent = where(event.path);
                break;
              case "exited":
                ended = true;
                live = false;
                handle = null;
                retry.cancel();
                setState("offline", "Closed");
                showBanner(
                  event.code === null ? "This pane closed." : `This pane exited with code ${event.code}.`,
                );
                break;
              case "error":
                offline(event.message);
                break;
              case "lease":
                leaseEvent(event.held, event.refused);
                break;
            }
          },
        );
        if (!current()) {
          api.paneClose(opened);
          return;
        }
        handle = opened;
        live = true;
        // A new stream starts at the desktop's size; the lease the old one
        // held ended with it. Asked for again if it is still wanted.
        leased = false;
        sent = "";
        askLease();
        retry.reset();
        banner.replaceChildren();
        setState("live", liveLabel());
        // A new tab's agent, started once: not again on a reconnect.
        if (run) {
          const command = run;
          run = undefined;
          void input(`${command}\r`);
        }
      } catch (e) {
        if (current()) offline(errorText(e));
      }
    };
    const reopen = () => {
      if (handle !== null) api.paneClose(handle);
      handle = null;
      open();
    };
    const online = () => {
      if (!live && !ended) reopen();
    };
    window.addEventListener("online", online);

    onLeave = () => {
      alive = false;
      retry.cancel();
      window.removeEventListener("online", online);
      window.removeEventListener("resize", fit);
      window.removeEventListener("resize", regridSoon);
      clearTimeout(regrid);
      darkScheme.removeEventListener("change", retheme);
      if (handle !== null) api.paneClose(handle);
      term.dispose();
    };
    // The pane replays its screen on every open, so coming back from the
    // background is a fresh open onto a reset terminal — never a gap.
    onResume = reopen;

    // Hack has to be loaded before xterm measures a cell, or the first fit is
    // taken with the fallback face's metrics.
    requestAnimationFrame(() => {
      document.fonts.load("12px Hack").finally(() => {
        if (!alive) return;
        term.open(screenEl);
        fit();
        edited();
        open();
      });
    });
    return view;
  });
}

hostsScreen("push");
