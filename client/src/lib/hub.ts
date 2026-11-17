import * as signalR from "@microsoft/signalr";
import { useSession, type LiveEvent, type LiveSnapshot } from "@/store/session";

// One SignalR connection to /hub/session. Events that arrive while a snapshot is loading are
// held and applied after it (those already in the snapshot are dropped by Seq).

let connection: signalR.HubConnection | null = null;
let loading = false;
let held: LiveEvent[] = [];

async function loadSnapshot(conn: signalR.HubConnection) {
  loading = true;
  held = [];
  try {
    const snap = await conn.invoke<LiveSnapshot>("GetSnapshot");
    useSession.getState().loadSnapshot(snap);
    const pending = held;
    held = [];
    useSession.getState().applyEvents(pending);
  } finally {
    loading = false;
  }
}

export function startHub(): signalR.HubConnection {
  if (connection) return connection;
  const conn = new signalR.HubConnectionBuilder()
    .withUrl("/hub/session", { withCredentials: true })
    .withAutomaticReconnect({ nextRetryDelayInMilliseconds: (ctx) => Math.min(10000, 500 * 2 ** ctx.previousRetryCount) })
    .configureLogging(signalR.LogLevel.Warning)
    .build();
  conn.serverTimeoutInMilliseconds = 60000;

  conn.on("events", (events: LiveEvent[]) => {
    if (loading) held.push(...events);
    else useSession.getState().applyEvents(events);
  });
  conn.onreconnecting(() => useSession.getState().setConnection("reconnecting"));
  conn.onreconnected(async () => {
    useSession.getState().setConnection("connected");
    await loadSnapshot(conn);
  });
  conn.onclose(() => useSession.getState().setConnection("disconnected"));

  const start = async () => {
    useSession.getState().setConnection("connecting");
    try {
      await conn.start();
      useSession.getState().setConnection("connected");
      await loadSnapshot(conn);
    } catch (e) {
      const msg = String(e);
      useSession.getState().setConnection(msg.includes("401") ? "unauthorized" : "disconnected");
      if (!msg.includes("401")) setTimeout(start, 3000);
    }
  };
  void start();
  connection = conn;
  return conn;
}

export async function sendPrompt(text: string): Promise<string | null> {
  if (!connection) return "Not connected.";
  return (await connection.invoke<string | null>("SendPrompt", text)) ?? null;
}

export async function cancelTurn(): Promise<boolean> {
  if (!connection) return false;
  return connection.invoke<boolean>("Cancel");
}

export async function resolvePermission(id: string, decision: "yes" | "all" | "no"): Promise<boolean> {
  if (!connection) return false;
  return connection.invoke<boolean>("ResolvePermission", id, decision);
}
