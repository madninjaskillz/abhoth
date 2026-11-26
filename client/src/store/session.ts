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
  /** What it served on when known, else the model it asks for. */
  model?: string | null;
  /** The model id it sends (a gateway alias); null for a bridge. */
  requestedModel?: string | null;
  /** What answered its last request; null = not known. */
  servedModel?: string | null;
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

export interface LiveQuestion {
  id: string;
  question: string;
  options: string[];
}

export interface EngineAuth {
  engine: string;
  message: string;
  loginCommand: string;
}

export interface EngineResult {
  ok: boolean;
  engine: string;
  message?: string | null;
  auth?: EngineAuth | null;
}

export interface LiveSnapshot {
  engine?: string | null;
  uncensored?: boolean | null;
  pendingQuestions?: LiveQuestion[] | null;
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
  | ({ kind: "turnEnd"; key: number; elapsedMs: number; tokens: number } & TurnEndSignals);

/**
 * How a turn ended, from its events (the mascot's mood: lib/moods.ts). `outcome` and `reason`
 * are the server's; the counts are the turn's permission answers and notice levels.
 */
export interface TurnEndSignals {
  outcome: string;
  reason?: string | null;
  denied: number;
  errors: number;
  warnings: number;
  pending: number;
}

/** What the running turn has seen so far (reset when a turn starts). */
export interface TurnTally {
  denied: number;
  errors: number;
  warnings: number;
}

const emptyTally: TurnTally = { denied: 0, errors: 0, warnings: 0 };

export type ConnectionState = "connecting" | "connected" | "reconnecting" | "disconnected" | "unauthorized";

export interface SessionState {
  /** True once a snapshot has been loaded for this project. */
  loaded: boolean;
  sessionId?: string | null;
  workingDirectory: string;
  mode: "tui" | "headless" | "worker" | "";
  model?: string | null;
  /** "native", "claude" or "codex". */
  engine: string;
  /** A bridge that needs a login (from the session, or from asking for the engine). */
  engineAuth?: EngineAuth | null;
  /** ask_user questions waiting for an answer. */
  questions: LiveQuestion[];
  /** The session's /uncensored switch (chat requests and the image model). */
  uncensored: boolean;
  busy: boolean;
  queued: number;
  lastSeq: number;
  items: ChatItem[];
  agents: LiveAgent[];
  slots?: LiveSlots | null;
  permissions: LivePermission[];
  /** Bumped when the agent may have changed files (git panel refreshes). */
  fileEpoch: number;
  /** The running turn's permission answers and notice levels (for its turn-end mood). */
  tally: TurnTally;
  /** When the running turn started here (ms since epoch; 0 when not known). */
  turnStartedAt: number;

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
      // A prompt from the queue (browser, bench) started: one fewer waiting.
      return {
        busy: true,
        queued: d.source && d.source !== "tui" ? Math.max(0, state.queued - 1) : state.queued,
        tally: emptyTally,
        turnStartedAt: Date.parse(ev.at) || Date.now(),
      };
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
    case "notice": {
      const level = d.level ?? "info";
      const t = state.tally;
      return {
        items: [...closeStreaming(state.items), { kind: "notice", key: nextKey++, level, text: d.text ?? "" }],
        tally: { ...t, errors: t.errors + (level === "error" ? 1 : 0), warnings: t.warnings + (level === "warning" ? 1 : 0) },
      };
    }
    case "turnEnd":
      return {
        busy: false,
        items: [
          ...closeStreaming(state.items),
          {
            kind: "turnEnd",
            key: nextKey++,
            elapsedMs: d.elapsedMs ?? 0,
            tokens: d.tokens ?? 0,
            outcome: d.outcome ?? "done",
            reason: d.reason ?? null,
            ...state.tally,
            pending: state.permissions.length + state.questions.length,
          },
        ],
        fileEpoch: state.fileEpoch + 1,
        tally: emptyTally,
        turnStartedAt: 0,
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
      return {
        permissions: state.permissions.filter((p) => p.id !== d.id),
        tally: d.decision === "no" ? { ...state.tally, denied: state.tally.denied + 1 } : state.tally,
      };
    case "question":
      return state.questions.some((q) => q.id === d.id) ? {} : { questions: [...state.questions, d] };
    case "questionResolved":
      return { questions: state.questions.filter((q) => q.id !== d.id) };
    case "engine":
      return { engine: d.engine ?? "native", engineAuth: null };
    case "uncensored":
      return { uncensored: !!d.on };
    case "engineAuth":
      return { engineAuth: { engine: d.engine, message: d.message ?? "", loginCommand: d.loginCommand ?? "" } };
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
  engine: "native",
  questions: [],
  uncensored: false,
  permissions: [],
  fileEpoch: 0,
  tally: emptyTally,
  turnStartedAt: 0,

  loadSnapshot: (s) => {
    let state: SessionState = {
      ...get(),
      loaded: true,
      sessionId: s.sessionId,
      workingDirectory: s.workingDirectory,
      mode: s.mode,
      model: s.model,
      engine: s.engine ?? "native",
      questions: s.pendingQuestions ?? [],
      uncensored: !!s.uncensored,
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
