// Calls to the router's admin API through the web server (/api/admin/router/* -> /admin/api/*,
// signed as the acting admin), the service keys, and the live stream hook shared by the admin
// pages, the supervisor dashboard and the app's status areas.
import { useEffect, useRef, useState } from "react";
import { csrfHeaders, sessionEnded } from "@/lib/auth";
import {
  applyEvent, type AuditRow, type CheckResult, type ConfigDoc, type Job, type LeasesViewT, type LiveEvent, type Plan,
  type RouteEdit, type RouteView, type StateView,
} from "./routerModel";

export class RouterApiError extends Error {
  constructor(message: string, public status: number, public data?: Record<string, unknown>) {
    super(message);
  }
}

async function call<T>(method: string, url: string, body?: unknown, headers: Record<string, string> = {}): Promise<T> {
  const res = await fetch(url, {
    method,
    credentials: "same-origin",
    headers: {
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...(method === "GET" ? {} : csrfHeaders()),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!res.ok) {
    if (res.status === 401 && url.startsWith("/api/") && !url.startsWith("/api/admin/router")) sessionEnded();
    const d = (data ?? {}) as Record<string, unknown>;
    throw new RouterApiError((d.error as string) || res.statusText, res.status, d);
  }
  return data as T;
}

const R = "/api/admin/router";

export const routerApi = {
  state: () => call<StateView>("GET", `${R}/state`),
  config: () => call<ConfigDoc>("GET", `${R}/config`),
  check: (text: string) => call<CheckResult>("POST", `${R}/config/check`, { text }),
  save: (text: string, rev: string, reason: string) => call<CheckResult>("PUT", `${R}/config`, { text, reason }, { "If-Match": `"${rev}"` }),
  plan: (server: string, layout: string) => call<Plan>("GET", `${R}/plan?server=${encodeURIComponent(server)}&layout=${encodeURIComponent(layout)}`),
  switchTo: (server: string, layout: string) => call<Job>("POST", `${R}/switch`, { server, layout }),
  jobs: (limit = 50) => call<Job[]>("GET", `${R}/jobs?limit=${limit}`),
  job: (id: string) => call<Job>("GET", `${R}/jobs/${encodeURIComponent(id)}`),
  cancelJob: (id: string) => call<Job>("POST", `${R}/jobs/${encodeURIComponent(id)}/cancel`),
  card: (card: string, action: "offline" | "online" | "exclude" | "include", reason: string) =>
    call<Job>("POST", `${R}/cards/${encodeURIComponent(card)}/${action}`, { reason }),
  audit: (limit = 100) => call<AuditRow[]>("GET", `${R}/audit?limit=${limit}`),
  leases: () => call<LeasesViewT>("GET", `${R}/leases`),
  release: (key: string) => call<{ ok: boolean }>("DELETE", `${R}/leases/${encodeURIComponent(key)}`),
  routes: () => call<RouteView[]>("GET", `${R}/routes`),
  editRoute: (name: string, edit: RouteEdit) => call<RouteView>("PATCH", `${R}/routes/${encodeURIComponent(name)}`, edit),
};

export interface ServiceKey {
  id: number;
  name: string;
  prefix: string;
  created: number;
  lastUsed: number | null;
  kind: string;
  scopes: string[];
  note?: string;
  owner?: string;
  /** Only in the answer that created it. */
  key?: string;
}

export const serviceKeyApi = {
  list: () => call<{ keys: ServiceKey[]; scopes: string[]; router: boolean }>("GET", "/api/admin/service-keys"),
  create: (name: string, scopes: string[], note: string) => call<ServiceKey>("POST", "/api/admin/service-keys", { name, scopes, note }),
  revoke: (id: number) => call<{ ok: boolean }>("DELETE", `/api/admin/service-keys/${id}`),
};

export type StreamStatus = "connecting" | "live" | "retrying";

/**
 * Follow an SSE stream (EventSource; the cookie authenticates, it reconnects by itself) and call
 * `on` for each named event. Returns the close function.
 */
export function followEvents(url: string, kinds: string[], on: (e: LiveEvent) => void, status?: (s: StreamStatus) => void): () => void {
  const es = new EventSource(url, { withCredentials: true });
  status?.("connecting");
  es.onopen = () => status?.("live");
  es.onerror = () => status?.("retrying");
  for (const k of kinds)
    es.addEventListener(k, (m) => {
      try {
        on({ kind: k, data: JSON.parse((m as MessageEvent).data) });
      } catch {
        /* a frame that isn't JSON: ignored */
      }
    });
  return () => es.close();
}

/**
 * The control plane's live state: the stream's first `state`, then telemetry, job steps and
 * reloads applied as they come (the admin Overview, the dashboard, the Models tab). `url`
 * defaults to the admin stream; `enabled` false keeps it closed.
 */
export function useLiveState(enabled = true, url = `${R}/events`) {
  const [state, setState] = useState<StateView | undefined>();
  const [status, setStatus] = useState<StreamStatus>("connecting");
  const [lastJob, setLastJob] = useState<Job | undefined>();
  const ref = useRef<StateView | undefined>();
  useEffect(() => {
    if (!enabled) return;
    return followEvents(
      url,
      ["state", "telemetry", "job", "config", "lease", "audit"],
      (e) => {
        if (e.kind === "job" && e.data) setLastJob(e.data as Job);
        const next = applyEvent(ref.current, e);
        if (next !== ref.current) {
          ref.current = next;
          setState(next);
        }
      },
      setStatus,
    );
  }, [enabled, url]);
  return { state, status, lastJob };
}
