import * as signalR from "@microsoft/signalr";
import { csrfHeaders } from "@/lib/auth";
import type { QueryClient } from "@tanstack/react-query";
import { existingSessionStore, sessionStore, useConnection, type EngineResult, type LiveEvent, type LiveSnapshot } from "@/store/session";
import { normPath, useWorkspace } from "@/store/workspace";
import type { EditMarkers } from "@/lib/api";

// One SignalR connection to /hub/session for every project. A project's events that arrive while
// its snapshot loads are held and applied after it (those already in the snapshot drop by Seq).

let connection: signalR.HubConnection | null = null;
const loading = new Set<string>();
const held = new Map<string, LiveEvent[]>();
const watched = new Set<string>();

async function loadSnapshot(conn: signalR.HubConnection, pid: string) {
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
  if (connection?.state === signalR.HubConnectionState.Connected) void loadSnapshot(connection, pid);
}

export function unwatchProject(pid: string) {
  watched.delete(pid);
}

export function startHub(queryClient: QueryClient): signalR.HubConnection {
  if (connection) return connection;
  const setConnection = useConnection.getState().setConnection;
  const conn = new signalR.HubConnectionBuilder()
    .withUrl("/hub/session", { withCredentials: true, headers: csrfHeaders() })
    .withAutomaticReconnect({ nextRetryDelayInMilliseconds: (ctx) => Math.min(10000, 500 * 2 ** ctx.previousRetryCount) })
    .configureLogging(signalR.LogLevel.Warning)
    .build();
  conn.serverTimeoutInMilliseconds = 60000;

  conn.on("events", (pid: string, events: LiveEvent[]) => {
    if (loading.has(pid)) held.get(pid)?.push(...events);
    else existingSessionStore(pid)?.getState().applyEvents(events);
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
    void queryClient.invalidateQueries();
  });
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
  void start();
  connection = conn;
  return conn;
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
