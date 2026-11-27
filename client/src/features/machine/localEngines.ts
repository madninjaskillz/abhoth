// Local Engines' calls to /api/machine (admins; LOCAL-ENGINES.md).
import { csrfHeaders } from "@/lib/auth";
import type { CheckResult, Endpoint, LocalEngines } from "./localEnginesModel";

export * from "./localEnginesModel";

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`/api/machine/${path}`, {
    method,
    credentials: "same-origin",
    headers: { ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...(method === "GET" ? {} : csrfHeaders()) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data: unknown;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = text;
  }
  if (!res.ok) {
    const d = (data ?? {}) as { error?: string };
    throw new Error(typeof d.error === "string" ? d.error : res.statusText);
  }
  return data as T;
}

export const localEnginesApi = {
  get: () => call<LocalEngines>("GET", "local-engines"),
  /** `key`: undefined keeps the stored key, "" removes it. */
  save: (id: string, endpoint: Endpoint, rev: string, key?: string) =>
    call<{ ok: boolean; rev: string }>("PUT", `endpoints/${encodeURIComponent(id)}`, key === undefined ? { endpoint, rev } : { endpoint, rev, key }),
  remove: (id: string) => call<{ ok: boolean }>("DELETE", `endpoints/${encodeURIComponent(id)}`),
  check: (endpoint: Endpoint, id?: string, key?: string) => call<CheckResult>("POST", "endpoints/check", { endpoint, id, key }),
  tools: (t: { exposed?: Record<string, boolean>; defaults?: Record<string, string> }) => call<unknown>("PUT", "tools", t),
  share: (s: { engines?: Record<string, boolean>; endpoints?: Record<string, boolean> }) => call<unknown>("PUT", "share", s),
};
