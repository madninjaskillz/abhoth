import { create } from "zustand";
import type { EditMarkers } from "@/lib/api";

// Per-project UI state: which worktree the tree shows, the tree's filters, the editor tabs,
// and the agent's edit markers (pushed by the server).

export interface EditorTab {
  /** `${wt}|${path}` — one tab per file per worktree. */
  key: string;
  wt: string;
  path: string;
  name: string;
  dirty: boolean;
  diff: boolean;
}

export interface ProjectUi {
  /** The worktree folder the tree shows (the project's own folder by default). */
  wt: string;
  changedOnly: boolean;
  showIgnored: boolean;
  markers: EditMarkers;
  /** "chat", a terminal ("term:<n>") or an editor tab key. */
  center: string;
  tabs: EditorTab[];
  /** The terminal tabs in the centre (each its own shell, alive while the tab is open). */
  terminals: number[];
}

export const termKey = (n: number) => `term:${n}`;

const empty = (root: string): ProjectUi => ({
  wt: root,
  changedOnly: false,
  showIgnored: false,
  markers: { editing: [], edited: [] },
  center: "chat",
  tabs: [],
  terminals: [1],
});

interface WorkspaceState {
  projects: Record<string, ProjectUi>;
  /** The project tab in front. */
  active: string | null;
  /** Projects whose tab has been shown (their panels stay mounted, so terminals live on). */
  mounted: string[];

  ensure: (pid: string, root: string) => void;
  setActive: (pid: string) => void;
  forget: (pid: string) => void;
  patch: (pid: string, p: Partial<ProjectUi>) => void;
  setMarkers: (pid: string, m: EditMarkers) => void;
  openFile: (pid: string, wt: string, path: string, diff?: boolean) => void;
  closeTab: (pid: string, key: string) => void;
  setDirty: (pid: string, key: string, dirty: boolean) => void;
  setDiff: (pid: string, key: string, diff: boolean) => void;
  /** Opens another shell tab and shows it. */
  addTerminal: (pid: string) => void;
  closeTerminal: (pid: string, n: number) => void;
}

export const tabKey = (wt: string, path: string) => `${wt}|${path}`;

function loadPrefs(): Record<string, Partial<ProjectUi>> {
  try {
    return JSON.parse(localStorage.getItem("openorca.ui") ?? "{}");
  } catch {
    return {};
  }
}

function savePrefs(projects: Record<string, ProjectUi>) {
  try {
    const prefs: Record<string, Partial<ProjectUi>> = {};
    for (const [id, p] of Object.entries(projects)) prefs[id] = { changedOnly: p.changedOnly, showIgnored: p.showIgnored };
    localStorage.setItem("openorca.ui", JSON.stringify(prefs));
  } catch {
    /* private window */
  }
}

export const useWorkspace = create<WorkspaceState>((set, get) => ({
  projects: {},
  active: null,
  mounted: [],

  ensure: (pid, root) => {
    if (get().projects[pid]) return;
    const prefs = loadPrefs()[pid] ?? {};
    set((s) => ({ projects: { ...s.projects, [pid]: { ...empty(root), ...prefs } } }));
  },
  setActive: (pid) =>
    set((s) => ({ active: pid, mounted: s.mounted.includes(pid) ? s.mounted : [...s.mounted, pid] })),
  forget: (pid) =>
    set((s) => {
      const projects = { ...s.projects };
      delete projects[pid];
      return { projects, mounted: s.mounted.filter((m) => m !== pid), active: s.active === pid ? null : s.active };
    }),
  patch: (pid, p) =>
    set((s) => {
      const cur = s.projects[pid];
      if (!cur) return {};
      const projects = { ...s.projects, [pid]: { ...cur, ...p } };
      if ("changedOnly" in p || "showIgnored" in p) savePrefs(projects);
      return { projects };
    }),
  setMarkers: (pid, markers) => get().patch(pid, { markers }),
  openFile: (pid, wt, path, diff = false) => {
    const cur = get().projects[pid];
    if (!cur) return;
    const key = tabKey(wt, path);
    const exists = cur.tabs.find((t) => t.key === key);
    const name = path.split("/").pop() || path;
    const tabs = exists
      ? cur.tabs.map((t) => (t.key === key && diff ? { ...t, diff } : t))
      : [...cur.tabs, { key, wt, path, name, dirty: false, diff }];
    get().patch(pid, { tabs, center: key });
  },
  closeTab: (pid, key) => {
    const cur = get().projects[pid];
    if (!cur) return;
    const i = cur.tabs.findIndex((t) => t.key === key);
    const tabs = cur.tabs.filter((t) => t.key !== key);
    const center = cur.center === key ? (tabs[Math.min(i, tabs.length - 1)]?.key ?? "chat") : cur.center;
    get().patch(pid, { tabs, center });
  },
  setDirty: (pid, key, dirty) => {
    const cur = get().projects[pid];
    if (!cur || cur.tabs.find((t) => t.key === key)?.dirty === dirty) return;
    get().patch(pid, { tabs: cur.tabs.map((t) => (t.key === key ? { ...t, dirty } : t)) });
  },
  setDiff: (pid, key, diff) => {
    const cur = get().projects[pid];
    if (!cur) return;
    get().patch(pid, { tabs: cur.tabs.map((t) => (t.key === key ? { ...t, diff } : t)) });
  },
  addTerminal: (pid) => {
    const cur = get().projects[pid];
    if (!cur) return;
    const n = Math.max(0, ...cur.terminals) + 1;
    get().patch(pid, { terminals: [...cur.terminals, n], center: termKey(n) });
  },
  closeTerminal: (pid, n) => {
    const cur = get().projects[pid];
    if (!cur) return;
    const terminals = cur.terminals.filter((t) => t !== n);
    get().patch(pid, { terminals, center: cur.center === termKey(n) ? "chat" : cur.center });
  },
}));

/** This project's UI state (inside a ProjectContext, via the id). */
export function useProjectUi<T>(pid: string, selector: (p: ProjectUi) => T): T | undefined {
  return useWorkspace((s) => (s.projects[pid] ? selector(s.projects[pid]) : undefined));
}

/** Normalise a full path for comparisons with the server's (forward slashes, maybe lower case). */
export function normPath(p: string, caseInsensitive: boolean): string {
  let s = p.replace(/\\/g, "/").replace(/\/+$/, "");
  if (caseInsensitive) s = s.toLowerCase();
  return s;
}
