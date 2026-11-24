// The agent strip's two views (bars, tabs), the tab model, and a sub-agent's transcript as chat
// items. Only type imports, so plain node runs the tests.
import type { ChatItem, LiveAgent } from "@/store/session";

// ── the view toggle ──────────────────────────────────────────────────────────

export type AgentView = "bars" | "tabs";

export const VIEW_KEY = "openorca.agentView";

/** Where the choice is kept (localStorage in the browser; tests pass their own). */
export interface ViewStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

function browserStorage(): ViewStorage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null; // blocked site data
  }
}

/** The remembered view; bars when nothing (or something unknown) is stored, or storage is blocked. */
export function loadView(storage: ViewStorage | null = browserStorage()): AgentView {
  try {
    return storage?.getItem(VIEW_KEY) === "tabs" ? "tabs" : "bars";
  } catch {
    return "bars";
  }
}

export function saveView(view: AgentView, storage: ViewStorage | null = browserStorage()): void {
  try {
    storage?.setItem(VIEW_KEY, view);
  } catch {
    /* private window: the choice lasts for this page only */
  }
}

// ── the tab model ────────────────────────────────────────────────────────────

export type TabState = "running" | "waiting" | "done" | "failed" | "idle";

export interface AgentTab {
  id: string;
  letter: string;
  label: string;
  isLead: boolean;
  state: TabState;
  /** 0..100 while running with a known progress; null otherwise. */
  pct: number | null;
}

/** An agent's status word as a tab state. */
export function tabState(status: string): TabState {
  switch (status.toLowerCase()) {
    case "running":
      return "running";
    case "pending":
    case "waiting":
      return "waiting";
    case "completed":
    case "completedpartial":
      return "done";
    case "failed":
    case "cancelled":
      return "failed";
    default:
      return "idle";
  }
}

/** One tab per agent, the Lead first (running while the session is busy). */
export function buildTabs(agents: LiveAgent[], busy: boolean): AgentTab[] {
  const lead = agents.find((a) => a.isLead);
  // Helpers by letter, so a tab stays put when its agent finishes (the bars list running ones first).
  const helpers = agents.filter((a) => !a.isLead).sort((a, b) => (a.letter || "").localeCompare(b.letter || ""));
  const ordered = [...(lead ? [lead] : []), ...helpers];
  return ordered.map((a) => {
    const state = a.isLead ? (busy ? "running" : "idle") : tabState(a.status);
    const pct = state === "running" && a.progress != null ? Math.round(Math.max(0, Math.min(1, a.progress)) * 100) : null;
    return { id: a.id, letter: a.letter || "·", label: a.isLead ? "Lead" : a.label || `Agent ${a.letter}`, isLead: a.isLead, state, pct };
  });
}

/**
 * The tab to show: the chosen one while it is still listed, else the Lead (null = the main
 * chat). A finished agent stays listed with the recent ones, so its tab outlives the job.
 */
export function selectedTab(tabs: AgentTab[], chosen: string | null | undefined): string | null {
  if (!chosen) return null;
  const t = tabs.find((x) => x.id === chosen);
  return t && !t.isLead ? t.id : null;
}

// ── a sub-agent's transcript ─────────────────────────────────────────────────

/** Wire types: the server's LiveAgentEntry / LiveAgentDelta / LiveAgentTranscript. */
export interface AgentEntry {
  agentId: string;
  n: number;
  at: string;
  kind: "task" | "assistant" | "tool" | "toolResult" | "message" | "status";
  text: string;
  isError: boolean;
  tool?: string | null;
  args?: string | null;
  from?: string | null;
}

export interface AgentDelta {
  agentId: string;
  text: string;
  end: number;
}

export interface AgentTranscriptWire {
  agentId: string;
  letter: string;
  label: string;
  type: string;
  status: string;
  task: string;
  entries: AgentEntry[];
  dropped: number;
  streaming: string;
  streamEnd: number;
  canMessage: boolean;
  canCancel: boolean;
  result?: string | null;
}

export interface AgentTranscriptState {
  loaded: boolean;
  error?: string | null;
  status: string;
  canMessage: boolean;
  canCancel: boolean;
  dropped: number;
  /** The number the next entry must have (entries below it are already in). */
  nextN: number;
  streamEnd: number;
  streaming: string;
  items: ChatItem[];
  /** Keys for items, per transcript (so React keys stay stable). */
  nextKey: number;
}

export const emptyTranscript = (): AgentTranscriptState => ({
  loaded: false,
  status: "",
  canMessage: false,
  canCancel: false,
  dropped: 0,
  nextN: 0,
  streamEnd: 0,
  streaming: "",
  items: [],
  nextKey: 1,
});

/** Fold one entry in (entries already in are skipped). */
export function applyEntry(s: AgentTranscriptState, e: AgentEntry): AgentTranscriptState {
  if (e.n < s.nextN) return s;
  let items = s.items;
  let key = s.nextKey;
  let streaming = s.streaming;
  switch (e.kind) {
    case "task":
      items = [...items, { kind: "user", key: key++, text: e.text, source: "lead" }];
      break;
    case "message":
      // The user's own messages read like the main chat's; the Lead's follow-ups are marked.
      items = [...items, { kind: "user", key: key++, text: e.text, source: e.from === "lead" ? "lead" : "web" }];
      break;
    case "assistant":
      items = [...items, { kind: "assistant", key: key++, text: stripProgress(e.text), streaming: false }];
      streaming = "";
      break;
    case "tool":
      items = [...items, { kind: "tool", key: key++, toolId: `n${e.n}`, name: e.tool || "tool", args: e.args ?? e.text, done: false }];
      streaming = "";
      break;
    case "toolResult": {
      let at = -1;
      for (let i = items.length - 1; i >= 0; i--) {
        const it = items[i];
        if (it.kind === "tool" && !it.done) {
          at = i;
          break;
        }
      }
      if (at < 0) {
        items = [...items, { kind: "tool", key: key++, name: "tool", args: "", result: e.text, isError: e.isError, done: true }];
      } else {
        items = items.slice();
        items[at] = { ...(items[at] as Extract<ChatItem, { kind: "tool" }>), result: e.text, isError: e.isError, done: true };
      }
      break;
    }
    default:
      items = [...items, { kind: "notice", key: key++, level: e.isError ? "error" : "info", text: e.text }];
      streaming = "";
  }
  return { ...s, items, nextKey: key, streaming, nextN: e.n + 1 };
}

/** Fold streamed text in: only the part past what is already there (by the stream's total). */
export function applyDelta(s: AgentTranscriptState, d: AgentDelta): AgentTranscriptState {
  if (d.end <= s.streamEnd) return s;
  const take = Math.min(d.text.length, d.end - s.streamEnd);
  return { ...s, streaming: s.streaming + d.text.slice(d.text.length - take), streamEnd: d.end };
}

/** A fetched transcript as state (live entries and deltas fold in after it). */
export function fromWire(t: AgentTranscriptWire): AgentTranscriptState {
  let s: AgentTranscriptState = {
    ...emptyTranscript(),
    loaded: true,
    status: t.status,
    canMessage: t.canMessage,
    canCancel: t.canCancel,
    dropped: t.dropped,
  };
  if (t.dropped > 0) {
    s = { ...s, items: [{ kind: "notice", key: s.nextKey, level: "info", text: `${t.dropped} earlier entries are no longer kept.` }], nextKey: s.nextKey + 1 };
  }
  for (const e of t.entries) s = applyEntry(s, e);
  // The first entry may have been dropped: start where the fetched ones start.
  if (t.entries.length === 0) s = { ...s, nextN: t.dropped };
  return { ...s, streaming: t.streaming ?? "", streamEnd: t.streamEnd ?? 0 };
}

/** "[progress 40%]" markers are for the bars (the tab shows the percentage); drop them from the text. */
export function stripProgress(text: string): string {
  return text.replace(/\[progress\s+\d{1,3}\s*%\]\s*/gi, "");
}

/** The items to draw: the entries, then the reply still streaming. */
export function transcriptItems(s: AgentTranscriptState): ChatItem[] {
  const live = s.streaming ? stripProgress(s.streaming) : "";
  return live ? [...s.items, { kind: "assistant", key: -1, text: live, streaming: true }] : s.items;
}
