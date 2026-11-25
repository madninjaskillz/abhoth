import * as signalR from "@microsoft/signalr";
import { csrfHeaders } from "@/lib/auth";
import { WsHubConnection, hubSocketUrl, transportOf, type HubLike, type HubState } from "@/lib/hubConnection";
import type { QueryClient } from "@tanstack/react-query";
import { existingSessionStore, sessionStore, useConnection, type EngineResult, type LiveEvent, type LiveSnapshot } from "@/store/session";
import { normPath, useWorkspace } from "@/store/workspace";
import type { EditMarkers } from "@/lib/api";
import { beginLoad, finishLoad, openedAgents, routeAgentEvents } from "@/features/agents/agentStore";
import type { AgentTranscriptWire } from "@/features/agents/agentTabs";

// One hub connection for every project: SignalR at /hub/session (the C# host) or the plain
// websocket at /hub/ws (the Rust host; see hubConnection.ts). A project's events that arrive while
// its snapshot loads are held and applied after it (those already in the snapshot drop by Seq).

let connection: HubLike | null = null;
let starting = false;
const loading = new Set<string>();
const held = new Map<string, LiveEvent[]>();
const watched = new Set<string>();

async function loadSnapshot(conn: HubLike, pid: string) {
  loading.add(pid);
  held.set(pid, []);
  try {
    const snap = await conn.invoke<LiveSnapshot>("GetSnapshot", pid);
    const store = sessionStore(pid);
    store.getState().loadSnapshot(snap);
    const pending = held.get(pid) ?? [];
    store.getState().applyEvents(pending);
  } catch (e) {
    sessionStore(pid).getState().applyEvents([
      { seq: Number.MAX_SAFE_INTEGER - 1, type: "notice", at: "", data: { level: "error", text: `Could not load this project's session: ${hubMessage(e)}` } },
    ]);
  } finally {
    loading.delete(pid);
    held.delete(pid);
  }
}

/** Start following a project's session (its tab was shown). */
export function watchProject(pid: string) {
  if (watched.has(pid)) return;
  watched.add(pid);
  if (connection?.state === "Connected") void loadSnapshot(connection, pid);
}

export function unwatchProject(pid: string) {
  watched.delete(pid);
}

/** SignalR's connection seen through the small interface the app uses. */
function fromSignalR(c: signalR.HubConnection): HubLike {
  return {
    get state() {
      return c.state as unknown as HubState;
    },
    invoke: (method, ...args) => c.invoke(method, ...args),
    on: (method, handler) => c.on(method, handler),
    off: (method) => c.off(method),
    start: () => c.start(),
    stop: () => c.stop(),
    onreconnecting: (cb) => c.onreconnecting(cb),
    onreconnected: (cb) => c.onreconnected(cb),
    onclose: (cb) => c.onclose(cb),
  };
}

/** The transport the server offers (/api/info says `hub: "ws"` on the Rust host). */
async function detectTransport(): Promise<"ws" | "signalr" | "unauthorized"> {
  try {
    const res = await fetch("/api/info", { credentials: "same-origin", cache: "no-store" });
    if (res.status === 401) return "unauthorized";
    return res.ok ? transportOf(await res.json()) : "signalr";
  } catch {
    return "signalr";
  }
}

export function startHub(queryClient: QueryClient): void {
  if (connection || starting) return;
  starting = true;
  const setConnection = useConnection.getState().setConnection;
  setConnection("connecting");
  void detectTransport().then((transport) => {
    if (transport === "unauthorized") {
      starting = false;
      setConnection("unauthorized");
      return;
    }
    connect(queryClient, transport);
  });
}

function connect(queryClient: QueryClient, transport: "ws" | "signalr") {
  const setConnection = useConnection.getState().setConnection;
  let conn: HubLike;
  if (transport === "ws") {
    conn = new WsHubConnection({ url: hubSocketUrl(window.location) });
  } else {
    const sr = new signalR.HubConnectionBuilder()
      .withUrl("/hub/session", { withCredentials: true, headers: csrfHeaders() })
      .withAutomaticReconnect({ nextRetryDelayInMilliseconds: (ctx) => Math.min(10000, 500 * 2 ** ctx.previousRetryCount) })
      .configureLogging(signalR.LogLevel.Warning)
      .build();
    sr.serverTimeoutInMilliseconds = 60000;
    conn = fromSignalR(sr);
  }

  conn.on("events", (pid: string, events: LiveEvent[]) => {
    if (loading.has(pid)) held.get(pid)?.push(...events);
    else existingSessionStore(pid)?.getState().applyEvents(events);
    routeAgentEvents(pid, events);
  });
  conn.on("markers", (pid: string, markers: EditMarkers) => useWorkspace.getState().setMarkers(pid, markers));
  conn.on("files", (pid: string, folder: string, dirs: string[] | null) => {
    const ci = true; // compare loosely: the server sends its own spelling
    const wt = normPath(folder, ci);
    void queryClient.invalidateQueries({
      predicate: (q) => {
        const [kind, p, w] = q.queryKey as [string, string, string];
        if (p !== pid || typeof w !== "string" || normPath(w, ci) !== wt) return false;
        if (kind === "changes" || kind === "worktrees") return true;
        if (kind === "tree") return dirs === null || dirs.includes((q.queryKey as string[])[3] ?? "");
        if (kind === "git") return true;
        return false;
      },
    });
    void queryClient.invalidateQueries({ queryKey: ["git", pid] });
    window.dispatchEvent(new CustomEvent("openorca:files", { detail: { pid, folder, dirs } }));
  });
  conn.on("projects", () => void queryClient.invalidateQueries({ queryKey: ["projects"] }));

  conn.onreconnecting(() => setConnection("reconnecting"));
  conn.onreconnected(async () => {
    setConnection("connected");
    await Promise.all([...watched].map((pid) => loadSnapshot(conn, pid)));
    // Agent tabs that were open missed events while away: fetch them again.
    await Promise.all([...watched].flatMap((pid) => openedAgents(pid).map((a) => loadAgentTranscript(pid, a))));
    void queryClient.invalidateQueries();
  });
  // Resumed within the server's grace period (websocket only): nothing was missed.
  conn.onresumed?.(() => setConnection("connected"));
  conn.onclose(() => setConnection("disconnected"));

  const start = async () => {
    setConnection("connecting");
    try {
      await conn.start();
      setConnection("connected");
      await Promise.all([...watched].map((pid) => loadSnapshot(conn, pid)));
    } catch (e) {
      const msg = String(e);
      setConnection(msg.includes("401") ? "unauthorized" : "disconnected");
      if (!msg.includes("401")) setTimeout(start, 3000);
    }
  };
  connection = conn;
  starting = false;
  void start();
}

export async function sendPrompt(pid: string, text: string): Promise<string | null> {
  if (!connection) return "Not connected.";
  return (await connection.invoke<string | null>("SendPrompt", pid, text)) ?? null;
}

export async function sendPromptWithAttachments(pid: string, text: string, attachments: string[]): Promise<string | null> {
  if (!connection) return "Not connected.";
  return (await connection.invoke<string | null>("SendPromptWithAttachments", pid, text, attachments)) ?? null;
}

export async function cancelTurn(pid: string): Promise<boolean> {
  if (!connection) return false;
  return connection.invoke<boolean>("Cancel", pid);
}

export async function resolvePermission(pid: string, id: string, decision: "yes" | "all" | "no"): Promise<boolean> {
  if (!connection) return false;
  return connection.invoke<boolean>("ResolvePermission", pid, id, decision);
}

/** The server's message from a hub error, without SignalR's "An unexpected error occurred invoking…" wrapper. */
function hubMessage(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  const i = m.indexOf("HubException: ");
  return i >= 0 ? m.slice(i + "HubException: ".length) : m;
}

/** Ask for another engine for the project's next turns ("native", "claude", "codex"). */
export async function setEngine(pid: string, engine: string): Promise<EngineResult> {
  if (!connection) return { ok: false, engine: "native", message: "Not connected." };
  try {
    return await connection.invoke<EngineResult>("SetEngine", pid, engine);
  } catch (e) {
    return { ok: false, engine: "native", message: hubMessage(e) };
  }
}

/** Answer an ask_user question (one of its options, or free text). */
export async function answerQuestion(pid: string, id: string, answer: string): Promise<boolean> {
  if (!connection) return false;
  return await connection.invoke<boolean>("AnswerQuestion", pid, id, answer);
}

// ── sub-agent tabs ───────────────────────────────────────────────────────────

/** Fetch a sub-agent's transcript (its tab opened, or a refresh); live events keep it current after. */
export async function loadAgentTranscript(pid: string, agentId: string): Promise<void> {
  beginLoad(pid, agentId);
  if (!connection || connection.state !== "Connected") {
    finishLoad(pid, agentId, null, "Not connected.");
    return;
  }
  try {
    const t = await connection.invoke<AgentTranscriptWire | null>("GetAgentTranscript", pid, agentId);
    finishLoad(pid, agentId, t ?? null);
  } catch (e) {
    finishLoad(pid, agentId, null, hubMessage(e));
  }
}

export interface AgentMessageResult {
  ok: boolean;
  outcome: string;
  message?: string | null;
}

/** Message a sub-agent: queued while it runs, or continues it once finished. */
export async function sendAgentMessage(pid: string, agentId: string, text: string): Promise<AgentMessageResult> {
  if (!connection) return { ok: false, outcome: "error", message: "Not connected." };
  try {
    return await connection.invoke<AgentMessageResult>("SendAgentMessage", pid, agentId, text);
  } catch (e) {
    return { ok: false, outcome: "error", message: hubMessage(e) };
  }
}

/** Stop one running sub-agent. */
export async function cancelAgent(pid: string, agentId: string): Promise<boolean> {
  if (!connection) return false;
  try {
    return await connection.invoke<boolean>("CancelAgent", pid, agentId);
  } catch {
    return false;
  }
}
