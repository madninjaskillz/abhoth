// This machine (MACHINE.md, D155): the desktop app's own inventory and engines. The inventory
// forms are the router admin's own, pointed at /api/admin/machine (an in-process router, no
// port); these are the calls that are the machine's alone.
import { csrfHeaders } from "@/lib/auth";

export interface MachineEngineState {
  engine: string; card: string; method?: string; lifecycle?: string; port?: number | null;
  state: "stopped" | "starting" | "running" | "failed"; error?: string | null; active?: number;
  idle_seconds?: number | null; up_seconds?: number | null; starts?: number;
  prepare?: { state: "ready" | "fetching" | "failed"; detail?: string; error?: string } | null;
}
export interface Pending { id: string; at: number; who: string; op: string; reasons: string[] }
export interface RemoteChange { id: string; at: number; who: string; what: string; undone?: boolean; backup?: string | null }
export interface MachineStatus {
  server: string; title: string; hostname: string;
  bridgeMode: boolean; remoteConfig: boolean; bridgeRouter?: string | null; advancedFile?: string | null;
  bridge?: { connected: boolean; server?: string | null; enabled?: boolean; router?: string; lastError?: string | null; connects?: number } | null;
  engines: { engines?: MachineEngineState[] };
  problems: string[]; error?: string | null;
  share: { engines: Record<string, boolean>; models: Record<string, boolean> };
  rev: string;
  pending: Pending[];
  remoteChanges: RemoteChange[];
  at: number;
}
export interface ManualEngines { rev: string; engines: Record<string, unknown>; error?: string }

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`/api/machine/${path}`, {
    method,
    credentials: "same-origin",
    headers: { ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...(method === "GET" ? {} : csrfHeaders()) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = text; }
  if (!res.ok) {
    const d = (data ?? {}) as { error?: string };
    const e = new Error(d.error || res.statusText) as Error & { status?: number };
    e.status = res.status;
    throw e;
  }
  return data as T;
}

export const machineApi = {
  status: () => call<MachineStatus>("GET", "status"),
  engines: () => call<ManualEngines>("GET", "engines"),
  saveEngines: (engines: unknown, rev: string) => call<ManualEngines>("PUT", "engines", { engines, rev }),
  share: (s: { engines?: Record<string, boolean>; models?: Record<string, boolean> }) => call<unknown>("PUT", "share", s),
  decide: (id: string, approve: boolean) => call<unknown>("POST", `pending/${encodeURIComponent(id)}`, { approve }),
  undo: (id: string) => call<unknown>("POST", `changes/${encodeURIComponent(id)}/undo`),
  start: (engine: string, card: string) => call<unknown>("POST", "engines/start", { engine, card }),
  stop: (engine: string, card: string) => call<unknown>("POST", "engines/stop", { engine, card }),
};
