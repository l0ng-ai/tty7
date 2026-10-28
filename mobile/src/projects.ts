import * as api from "./api";
import type { Folder, Host, ProjectOpened, Tree } from "./api";
import { agentLook, icon } from "./icons";
import "./projects.css";

type Agent = "claude" | "codex";
type Target = { host: Host; machine: string | null; name: string };

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text?: string) {
  const node = document.createElement(tag);
  node.className = cls;
  if (text) node.textContent = text;
  return node;
}

function glyph(name: keyof typeof icon) {
  const node = el("span", "icon");
  node.innerHTML = icon[name];
  return node;
}

function button(label: string, run: () => void, cls = "project-control") {
  const node = el("button", cls, label);
  node.type = "button";
  node.onclick = run;
  return node;
}

function read<T>(key: string, fallback: T): T {
  try { return JSON.parse(localStorage.getItem(key) ?? "null") ?? fallback; }
  catch { return fallback; }
}

function write(key: string, data: unknown) {
  try { localStorage.setItem(key, JSON.stringify(data)); } catch { /* Storage may be unavailable. */ }
}

function savedFolders(key: string): Folder[] {
  const value = read<unknown>(key, []);
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is Folder => !!item && typeof item.name === "string" && typeof item.path === "string").slice(0, 40);
}

export function createProjects(options: {
  host: Host;
  machine: string | null;
  name: string;
  grid: () => { cols: number; rows: number };
  target: (target: Target) => void;
  machines: () => void;
  sessions: () => void;
  opened: (project: ProjectOpened, agent: Agent) => void;
}) {
  const { host, machine } = options;
  const storage = `projects.${host.id}.${machine ?? "local"}`;
  let favorites = savedFolders(`${storage}.favorites`);
  let recent = savedFolders(`${storage}.recent`);
  let folders: api.Folders | null = null;
  let initialDir: string | null = null;
  let selected: Folder | null = null;
  let selectionVersion = 0;
  let agent: Agent = read<Agent>(`${storage}.agent`, "codex") === "claude" ? "claude" : "codex";
  let alive = true;
  let loading = false;
  let launching = false;
  let generation = 0;
  let watch: number | null = null;
  let tree: Tree | null = null;
  let targets: Target[] = [{ host, machine, name: options.name }];
  let paired: Host[] = [host];
  let launch: { path: string; agent: Agent; id: string } | null = null;

  const root = el("div", "screen projects-screen");
  const heading = el("header", "projects-heading");
  const back = button("", options.machines, "project-icon-button");
  back.ariaLabel = "Choose a paired computer";
  back.append(glyph("back"));
  const title = el("h1", "projects-title", "Projects");
  const computer = el("select", "project-computer");
  computer.ariaLabel = "Computer";
  const sessions = button("", options.sessions, "project-icon-button");
  sessions.ariaLabel = "Open existing terminals and agents";
  sessions.append(glyph("terminal"));
  heading.append(back, title, computer, sessions);

  const searchBox = el("label", "project-search");
  searchBox.append(glyph("search"));
  const search = el("input", "project-search-input");
  search.type = "search";
  search.placeholder = "Search projects or enter a path…";
  search.ariaLabel = "Search projects or enter a directory path";
  search.autocapitalize = "off";
  search.spellcheck = false;
  search.setAttribute("autocorrect", "off");
  searchBox.append(search);
  const main = el("main", "projects-main");
  const favoriteArea = el("section", "project-favorites");
  const favoriteButtons = el("div", "project-shortcuts");
  favoriteArea.append(el("h2", "project-section-title", "Favorites"), favoriteButtons);
  const listArea = el("section", "project-list-area");
  const listHead = el("div", "project-list-heading");
  const listTitle = el("h2", "project-section-title", "Project folders");
  const location = el("button", "project-location");
  location.ariaLabel = "Choose the current folder as a project";
  location.onclick = () => folders && choose({ name: leaf(folders.dir), path: folders.dir });
  listHead.append(listTitle, location);
  const navigation = el("div", "project-navigation");
  const home = button("Home", () => folders && load(folders.home));
  const up = button("Up", () => folders?.parent && load(folders.parent));
  const refresh = button("Refresh", () => load(folders?.dir ?? null));
  navigation.append(home, up, refresh);
  const list = el("div", "project-folder-list");
  const status = el("p", "project-status");
  status.setAttribute("role", "status");
  const error = el("p", "project-error");
  error.setAttribute("role", "alert");
  listArea.append(listHead, navigation, list, status);
  main.append(favoriteArea, listArea);

  const footer = el("footer", "project-launch");
  const selectedTitle = el("h2", "project-selected-title", "Choose a project");
  const selectedPath = el("p", "project-selected-path", "Pick a folder above, or browse your computer.");
  const workspace = el("p", "project-workspace");
  const agents = el("div", "project-agents");
  agents.setAttribute("role", "group");
  agents.ariaLabel = "Start with";
  const agentButtons = (["claude", "codex"] as const).map((kind) => {
    const choice = button("", () => { agent = kind; write(`${storage}.agent`, kind); launch = null; drawSelection(); }, "project-agent");
    const mark = el("span", "project-agent-mark");
    const look = agentLook(kind);
    if (look.mark) {
      mark.style.maskImage = `url(${JSON.stringify(look.mark)})`;
      mark.style.webkitMaskImage = `url(${JSON.stringify(look.mark)})`;
    }
    mark.style.backgroundColor = kind === "claude" ? look.field : "var(--ink)";
    const words = el("span", "project-agent-words");
    words.append(el("b", "", kind === "claude" ? "Claude" : "Codex"), el("span", "", kind === "claude" ? "Anthropic" : "OpenAI"));
    choice.append(mark, words, el("span", "project-radio"));
    choice.ariaLabel = `Start with ${kind === "claude" ? "Claude" : "Codex"}`;
    agents.append(choice);
    return choice;
  });
  const start = button("Choose a project", () => void open(), "project-start");
  footer.append(selectedTitle, selectedPath, workspace, el("h3", "project-section-title", "Start with"), agents, start);
  root.append(heading, searchBox, error, main, footer);

  function leaf(path: string) { return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path; }
  function display(path: string) {
    const home = folders?.home;
    return home && (path === home || path.startsWith(`${home}/`) || path.startsWith(`${home}\\`)) ? `~${path.slice(home.length)}` : path;
  }
  function choose(folder: Folder) {
    if (launching) return;
    selected = folder;
    selectionVersion++;
    launch = null;
    search.blur();
    draw();
  }
  function drawSelection() {
    selectedTitle.textContent = selected?.name ?? "Choose a project";
    selectedPath.textContent = selected ? display(selected.path) : "Pick a folder above, or browse your computer.";
    selectedPath.title = selected?.path ?? "";
    workspace.textContent = selected ? (selected.workspace_id ? "Uses this project's existing workspace" : `Creates a workspace for ${selected.name} if needed`) : "";
    agentButtons.forEach((button, i) => {
      const active = (["claude", "codex"] as const)[i] === agent;
      button.classList.toggle("selected", active);
      button.setAttribute("aria-pressed", String(active));
    });
    start.disabled = !selected || launching;
    start.textContent = launching ? "Opening…" : selected ? `Start ${agent === "claude" ? "Claude" : "Codex"} in ${selected.name}` : "Choose a project";
    for (const button of root.querySelectorAll<HTMLButtonElement>("button:not(.project-start)")) button.disabled = launching;
    computer.disabled = launching;
    search.disabled = launching;
  }
  function folderRow(folder: Folder) {
    const row = el("div", `project-folder-row${selected?.path === folder.path ? " selected" : ""}`);
    const pick = button("", () => choose(folder), "project-folder-pick");
    pick.append(glyph("folder"), el("span", "project-folder-name", folder.name));
    pick.ariaLabel = `Choose ${folder.name}, ${folder.path}`;
    pick.setAttribute("aria-pressed", String(selected?.path === folder.path));
    const saved = favorites.some((item) => item.path === folder.path);
    const star = button("", () => {
      favorites = saved ? favorites.filter((item) => item.path !== folder.path) : [...favorites, { name: folder.name, path: folder.path }].slice(-40);
      write(`${storage}.favorites`, favorites);
      draw();
    }, `project-icon-button project-favorite${saved ? " saved" : ""}`);
    star.ariaLabel = `${saved ? "Unfavorite" : "Favorite"} ${folder.name}`;
    star.setAttribute("aria-pressed", String(saved));
    star.append(glyph("star"));
    const browse = button("", () => load(folder.path), "project-icon-button project-browse");
    browse.ariaLabel = `Browse inside ${folder.name}`;
    browse.append(glyph("chevron"));
    row.append(pick, star, browse);
    return row;
  }
  function draw() {
    favoriteButtons.replaceChildren(...favorites.map((folder) => {
      const shortcut = button(folder.name, () => choose(folders?.folders.find((f) => f.path === folder.path) ?? folder), "project-shortcut");
      shortcut.prepend(glyph("star"));
      shortcut.title = folder.path;
      return shortcut;
    }));
    favoriteArea.hidden = favorites.length === 0;
    const q = search.value.trim().toLowerCase();
    const merged = new Map<string, Folder>();
    for (const folder of [...favorites, ...recent, ...(folders?.folders ?? [])]) merged.set(folder.path, folder);
    const shown = (q ? [...merged.values()] : folders?.folders ?? []).filter((folder) => `${folder.name} ${folder.path}`.toLowerCase().includes(q));
    list.replaceChildren(...shown.map(folderRow));
    if (/^(\/|~[\\/]|[a-z]:[\\/]|\\\\)/i.test(search.value.trim())) {
      list.prepend(button(`Use folder: ${search.value.trim()}`, () => void usePath(), "project-path-action"));
    }
    list.append(button("Browse another folder", () => {
      search.value = "";
      void load(folders?.home ?? null);
    }, "project-browse-home"));
    location.textContent = folders ? display(folders.dir) : "";
    location.title = folders?.dir ?? "";
    navigation.hidden = !folders || folders.dir === initialDir;
    up.disabled = !folders?.parent;
    status.textContent = loading ? "Loading folders…" : folders?.truncated ? "Showing the first 256 folders. Browse a narrower directory to see the rest." : shown.length ? "" : q ? "No project matches. Enter a full path to open another folder." : "No folders here. Select this folder using its path above, or go up.";
    root.setAttribute("aria-busy", String(loading || launching));
    drawSelection();
    up.disabled = launching || !folders?.parent;
  }
  async function load(path: string | null) {
    if (launching) return;
    const mine = ++generation;
    loading = true;
    error.textContent = "";
    draw();
    try {
      const result = await api.folders(host.id, machine, path);
      if (!alive || mine !== generation) return;
      folders = result;
      initialDir ??= result.dir;
      if (selected) selected = result.folders.find((f) => f.path === selected?.path) ?? selected;
      return result;
    } catch (e) {
      if (!alive || mine !== generation) return;
      error.textContent = String(e instanceof Error ? e.message : e);
    } finally {
      if (alive && mine === generation) { loading = false; draw(); }
    }
  }
  async function usePath() {
    const path = search.value.trim();
    selected = null;
    launch = null;
    const selection = ++selectionVersion;
    const result = await load(path);
    if (alive && result && selection === selectionVersion && search.value.trim() === path) {
      search.value = "";
      choose({ name: leaf(result.dir), path: result.dir });
    }
  }
  async function open() {
    if (!selected || launching) return;
    const folder = selected;
    const chosenAgent = agent;
    if (!launch || launch.path !== folder.path || launch.agent !== chosenAgent) launch = { path: folder.path, agent: chosenAgent, id: crypto.randomUUID() };
    launching = true;
    let opened = false;
    error.textContent = "";
    draw();
    try {
      const project = await api.projectOpen(host.id, machine, folder.path, launch.id, options.grid());
      if (!alive) return;
      recent = [{ name: leaf(project.cwd), path: project.cwd }, ...recent.filter((item) => item.path !== project.cwd)].slice(0, 20);
      write(`${storage}.recent`, recent);
      options.opened(project, chosenAgent);
      opened = true;
    } catch (e) {
      if (alive) error.textContent = String(e instanceof Error ? e.message : e);
    } finally { if (alive && !opened) { launching = false; draw(); } }
  }
  function drawTargets() {
    targets = paired.map((host) => ({ host, machine: null, name: host.name }));
    for (const remote of tree?.remotes ?? []) {
      if (remote.connected && !remote.pending && !remote.error) targets.push({ host, machine: remote.key, name: `${host.name} / ${remote.name}` });
    }
    if (!targets.some((t) => t.host.id === host.id && t.machine === machine)) targets.push({ host, machine, name: options.name });
    computer.replaceChildren(...targets.map((target, index) => {
      const option = document.createElement("option");
      option.value = String(index);
      option.textContent = target.name;
      option.selected = target.host.id === host.id && target.machine === machine;
      return option;
    }));
  }
  computer.onchange = () => { const target = targets[Number(computer.value)]; if (target) options.target(target); };
  search.oninput = draw;
  search.onkeydown = (event) => {
    if (event.key === "Enter" && /^(\/|~[\\/]|[a-z]:[\\/]|\\\\)/i.test(search.value.trim())) { event.preventDefault(); void usePath(); }
  };
  api.hosts().then((hosts) => { if (alive) { paired = hosts; drawTargets(); } }, () => {});
  api.watch(host.id, (message) => {
    if (!alive) return;
    if (message.type === "tree") { tree = message.tree; drawTargets(); }
  }).then((id) => { if (alive) watch = id; else void api.unwatch(id); }, () => {});
  drawTargets();
  void load(null);
  return {
    element: root,
    resume: async () => { await api.alive(host.id).catch(() => false); if (alive) await load(folders?.dir ?? null); },
    dispose: () => { alive = false; generation++; if (watch !== null) void api.unwatch(watch).catch(() => {}); },
  };
}
