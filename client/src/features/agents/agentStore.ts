import { create } from "zustand";
import type { LiveEvent } from "@/store/session";
import {
  applyDelta, applyEntry, emptyTranscript, fromWire, loadView, saveView,
  type AgentDelta, type AgentEntry, type AgentTranscriptState, type AgentTranscriptWire, type AgentView,
} from "./agentTabs";

const TAB_KEY = "openorca.agentTab";

/** The chosen tabs survive a reload of this browser tab (sessionStorage; nothing when it is blocked). */
function loadSelected(): Record<string, string | null> {
  try {
    const raw = typeof sessionStorage === "undefined" ? null : sessionStorage.getItem(TAB_KEY);
    const v = raw ? JSON.parse(raw) : null;
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
}

function saveSelected(v: Record<string, string | null>) {
  try {
    sessionStorage.setItem(TAB_KEY, JSON.stringify(v));
  } catch {
    /* blocked: the choice lasts for this page */
  }
}

/** The strip's view (remembered per browser) and each project's chosen tab (null = the main chat). */
export const useAgentUi = create<{
  view: AgentView;
  setView: (v: AgentView) => void;
  selected: Record<string, string | null>;
  select: (pid: string, agentId: string | null) => void;
}>((set) => ({
  view: loadView(),
  setView: (view) => {
    saveView(view);
    set({ view });
  },
  selected: loadSelected(),
  select: (pid, agentId) =>
    set((s) => {
      const selected = { ...s.selected, [pid]: agentId };
      saveSelected(selected);
      return { selected };
    }),
}));

const key = (pid: string, agentId: string) => `${pid}|${agentId}`;

/**
 * Transcripts of the sub-agents whose tabs were opened, kept live from the project's events.
 * Events for agents nobody opened are dropped; ones that arrive while a transcript loads are
 * held and folded in after it (repeats drop by entry number and stream position).
 */
export const useAgentTranscripts = create<{ map: Record<string, AgentTranscriptState> }>(() => ({ map: {} }));

const loading = new Map<string, LiveEvent[]>();

export function agentTranscriptKey(pid: string, agentId: string) {
  return key(pid, agentId);
}

/** Opened transcripts of a project (to reload after a reconnect). */
export function openedAgents(pid: string): string[] {
  const prefix = pid + "|";
  return Object.keys(useAgentTranscripts.getState().map)
    .filter((k) => k.startsWith(prefix))
    .map((k) => k.slice(prefix.length));
}

function put(k: string, s: AgentTranscriptState) {
  useAgentTranscripts.setState((st) => ({ map: { ...st.map, [k]: s } }));
}

function fold(s: AgentTranscriptState, ev: LiveEvent): AgentTranscriptState {
  if (ev.type === "agentEntry") return applyEntry(s, ev.data as AgentEntry);
  if (ev.type === "agentDelta") return applyDelta(s, ev.data as AgentDelta);
  return s;
}

export function beginLoad(pid: string, agentId: string) {
  const k = key(pid, agentId);
  loading.set(k, []);
  if (!useAgentTranscripts.getState().map[k]) put(k, emptyTranscript());
}

export function finishLoad(pid: string, agentId: string, wire: AgentTranscriptWire | null, error?: string) {
  const k = key(pid, agentId);
  const held = loading.get(k) ?? [];
  loading.delete(k);
  if (!wire) {
    put(k, { ...emptyTranscript(), loaded: true, error: error ?? "This agent's transcript is not available (it may belong to a session that has since stopped)." });
    return;
  }
  let s = fromWire(wire);
  for (const ev of held) s = fold(s, ev);
  put(k, s);
}

/** A project's live events: the agent ones go to the transcripts that are open. */
export function routeAgentEvents(pid: string, events: LiveEvent[]) {
  let map: Record<string, AgentTranscriptState> | null = null;
  for (const ev of events) {
    if (ev.type !== "agentEntry" && ev.type !== "agentDelta") continue;
    const k = key(pid, (ev.data as { agentId?: string })?.agentId ?? "");
    const held = loading.get(k);
    if (held) {
      held.push(ev);
      continue;
    }
    const cur: AgentTranscriptState | undefined = (map ?? useAgentTranscripts.getState().map)[k];
    if (!cur || !cur.loaded || cur.error) continue;
    const next = fold(cur, ev);
    if (next !== cur) map = { ...(map ?? useAgentTranscripts.getState().map), [k]: next };
  }
  if (map) useAgentTranscripts.setState({ map });
}

/** Forget a project's transcripts (its session restarted or the tab closed). */
export function dropAgentTranscripts(pid: string) {
  const prefix = pid + "|";
  useAgentTranscripts.setState((st) => ({ map: Object.fromEntries(Object.entries(st.map).filter(([k]) => !k.startsWith(prefix))) }));
}
