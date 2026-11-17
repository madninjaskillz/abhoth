import { createContext, useContext } from "react";
import { create, createStore, useStore, type StoreApi } from "zustand";

// Wire types: the server's Core/Live records, camelCased by SignalR's JSON protocol.

export interface LiveEvent {
  seq: number;
  type: string;
  at: string;
  data: any; // eslint-disable-line @typescript-eslint/no-explicit-any
}

export interface LiveAgent {
  id: string;
  letter: string;
  label: string;
  type: string;
  status: string;
  progress?: number | null;
  note?: string | null;
  activity?: string | null;
  slot?: string | null;
  model?: string | null;
  isLead: boolean;
}

export interface LiveSlotLease {
  owner: string;
  kind: string;
  label: string;
  slot?: string | null;
  copy?: string | null;
}

export interface LiveSlots {
  total: number;
  leases: LiveSlotLease[];
}

export interface LivePermission {
  id: string;
  tool: string;
  risk: string;
}

export interface LiveTranscriptItem {
  kind: "user" | "assistant" | "tool" | "system";
  text?: string | null;
  toolId?: string | null;
  toolName?: string | null;
  args?: string | null;
  result?: string | null;
  isError: boolean;
}

export interface LiveSnapshot {
  sessionId?: string | null;
  title?: string | null;
  workingDirectory: string;
  mode: "tui" | "headless" | "worker";
  model?: string | null;
  busy: boolean;
  queued: number;
  lastSeq: number;
  history: LiveTranscriptItem[];
  currentTurn: LiveEvent[];
  agents: { agents: LiveAgent[] };
  slots?: LiveSlots | null;
  pendingPermissions: LivePermission[];
}

// The chat transcript as rendered.

export type ChatItem =
  | { kind: "user"; key: number; text: string; source: string }
  | { kind: "assistant"; key: number; text: string; streaming: boolean }
  | { kind: "thinking"; key: number; text: string; streaming: boolean }
  | {
      kind: "tool";
      key: number;
      toolId?: string | null;
      name: string;
      args: string;
      result?: string;
      isError?: boolean;
      elapsedMs?: number | null;
      done: boolean;
    }
  | { kind: "system"; key: number; text: string }
  | { kind: "notice"; key: number; level: string; text: string }
  | { kind: "turnEnd"; key: number; elapsedMs: number; tokens: number; outcome: string };

export type ConnectionState = "connecting" | "connected" | "reconnecting" | "disconnected" | "unauthorized";

export interface SessionState {
  /** True once a snapshot has been loaded for this project. */
  loaded: boolean;
  sessionId?: string | null;
  workingDirectory: string;
  mode: "tui" | "headless" | "worker" | "";
  model?: string | null;
  busy: boolean;
  queued: number;
  lastSeq: number;
  items: ChatItem[];
  agents: LiveAgent[];
  slots?: LiveSlots | null;
  permissions: LivePermission[];
  /** Bumped when the agent may have changed files (git panel refreshes). */
  fileEpoch: number;

  loadSnapshot: (s: LiveSnapshot) => void;
  applyEvents: (events: LiveEvent[]) => void;
}

let nextKey = 1;

function closeStreaming(items: ChatItem[]): ChatItem[] {
  const last = items[items.length - 1];
  if (last && (last.kind === "assistant" || last.kind === "thinking") && last.streaming) {
    return [...items.slice(0, -1), { ...last, streaming: false }];
  }
  return items;
}

/** Fold one event into the transcript (pure; also used for a snapshot's current turn). */
function reduce(state: SessionState, ev: LiveEvent): Partial<SessionState> {
  const d = ev.data ?? {};
  switch (ev.type) {
    case "turnStart":
      return { busy: true };
    case "user":
      return { items: [...closeStreaming(state.items), { kind: "user", key: nextKey++, text: d.text ?? "", source: d.source ?? "" }] };
    case "assistantDelta":
    case "thinkingDelta": {
      const kind = ev.type === "assistantDelta" ? "assistant" : "thinking";
      const items = state.items;
      const last = items[items.length - 1];
      if (last && last.kind === kind && last.streaming) {
        return { items: [...items.slice(0, -1), { ...last, text: last.text + (d.text ?? "") }] };
      }
      return { items: [...closeStreaming(items), { kind, key: nextKey++, text: d.text ?? "", streaming: true }] };
    }
    case "toolCall":
      return {
        items: [
          ...closeStreaming(state.items),
          { kind: "tool", key: nextKey++, toolId: d.id, name: d.name ?? "tool", args: d.args ?? "", done: false },
        ],
      };
    case "toolResult": {
      const items = closeStreaming(state.items);
      let at = -1;
      for (let i = items.length - 1; i >= 0; i--) {
        const it = items[i];
        if (it.kind === "tool" && !it.done && (d.id ? it.toolId === d.id : it.name === d.name)) {
          at = i;
          break;
        }
      }
      const patch = { result: d.result ?? "", isError: !!d.isError, elapsedMs: d.elapsedMs, done: true };
      if (at < 0) {
        return {
          items: [...items, { kind: "tool", key: nextKey++, toolId: d.id, name: d.name ?? "tool", args: "", ...patch }],
          fileEpoch: state.fileEpoch + 1,
        };
      }
      const copy = items.slice();
      copy[at] = { ...(copy[at] as Extract<ChatItem, { kind: "tool" }>), ...patch };
      return { items: copy, fileEpoch: state.fileEpoch + 1 };
    }
    case "notice":
      return { items: [...closeStreaming(state.items), { kind: "notice", key: nextKey++, level: d.level ?? "info", text: d.text ?? "" }] };
    case "turnEnd":
      return {
        busy: false,
        items: [
          ...closeStreaming(state.items),
          { kind: "turnEnd", key: nextKey++, elapsedMs: d.elapsedMs ?? 0, tokens: d.tokens ?? 0, outcome: d.outcome ?? "done" },
        ],
        fileEpoch: state.fileEpoch + 1,
      };
    case "busy":
      return { busy: !!d.busy, queued: d.queued ?? 0 };
    case "agents":
      return { agents: d.agents ?? [] };
    case "slots":
      return { slots: d };
    case "permission":
      return state.permissions.some((p) => p.id === d.id) ? {} : { permissions: [...state.permissions, d] };
    case "permissionResolved":
      return { permissions: state.permissions.filter((p) => p.id !== d.id) };
    default:
      return {};
  }
}

function fromHistory(h: LiveTranscriptItem): ChatItem | null {
  switch (h.kind) {
    case "user":
      return { kind: "user", key: nextKey++, text: h.text ?? "", source: "" };
    case "assistant":
      return { kind: "assistant", key: nextKey++, text: h.text ?? "", streaming: false };
    case "system":
      return { kind: "system", key: nextKey++, text: h.text ?? "" };
    case "tool":
      return {
        kind: "tool",
        key: nextKey++,
        toolId: h.toolId,
        name: h.toolName ?? "tool",
        args: h.args ?? "",
        result: h.result ?? undefined,
        isError: h.isError,
        done: h.result != null,
      };
    default:
      return null;
  }
}

const makeSessionStore = () =>
  createStore<SessionState>((set, get) => ({
  loaded: false,
  workingDirectory: "",
  mode: "",
  busy: false,
  queued: 0,
  lastSeq: 0,
  items: [],
  agents: [],
  permissions: [],
  fileEpoch: 0,

  loadSnapshot: (s) => {
    let state: SessionState = {
      ...get(),
      loaded: true,
      sessionId: s.sessionId,
      workingDirectory: s.workingDirectory,
      mode: s.mode,
      model: s.model,
      busy: s.busy,
      queued: s.queued,
      lastSeq: s.lastSeq,
      items: s.history.map(fromHistory).filter((x): x is ChatItem => x !== null),
      agents: s.agents?.agents ?? [],
      slots: s.slots,
      permissions: s.pendingPermissions ?? [],
    };
    for (const ev of s.currentTurn ?? []) state = { ...state, ...reduce(state, ev) };
    // Still streaming if the turn is running.
    if (!s.busy) state = { ...state, items: closeStreaming(state.items) };
    state = { ...state, busy: s.busy, lastSeq: s.lastSeq, fileEpoch: get().fileEpoch + 1 };
    set(state);
  },

  applyEvents: (events) => {
    let state = get();
    let changed: Partial<SessionState> = {};
    for (const ev of events) {
      if (ev.seq <= state.lastSeq) continue; // already in the snapshot
      const patch = reduce(state, ev);
      state = { ...state, ...patch, lastSeq: ev.seq };
      changed = { ...changed, ...patch, lastSeq: ev.seq };
    }
    if (Object.keys(changed).length > 0) set(changed);
  },
}));

// One store per project (tab), created when the tab is first shown.
const stores = new Map<string, StoreApi<SessionState>>();

export function sessionStore(projectId: string): StoreApi<SessionState> {
  let s = stores.get(projectId);
  if (!s) {
    s = makeSessionStore();
    stores.set(projectId, s);
  }
  return s;
}

export function existingSessionStore(projectId: string): StoreApi<SessionState> | undefined {
  return stores.get(projectId);
}

export function dropSessionStore(projectId: string) {
  stores.delete(projectId);
}

export interface ProjectContextValue {
  id: string;
  root: string;
  /** A bench run: no composer, no cancel. */
  readOnly?: boolean;
}

export const ProjectContext = createContext<ProjectContextValue | null>(null);

export function useProject(): ProjectContextValue {
  const p = useContext(ProjectContext);
  if (!p) throw new Error("useProject outside a project");
  return p;
}

/** The current project's session state (inside a ProjectContext). */
export function useSession<T>(selector: (s: SessionState) => T): T {
  const { id } = useProject();
  return useStore(sessionStore(id), selector);
}

// The one hub connection, shared by every project.
export const useConnection = create<{ connection: ConnectionState; setConnection: (c: ConnectionState) => void }>((set) => ({
  connection: "connecting",
  setConnection: (connection) => set({ connection }),
}));
