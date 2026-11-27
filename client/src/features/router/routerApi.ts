// Calls to the router's admin API, the service keys, and the live stream hook shared by the admin
// pages, the supervisor dashboard and the app's status areas. In the app they go through the web
// server (/api/admin/router/* -> /admin/api/*, signed as the acting admin); the router's own admin
// pages (D151, router-admin-main.tsx) point the same calls at the router with setRouterTransport.
import { useEffect, useRef, useState } from "react";
import { csrfHeaders, sessionEnded } from "@/lib/auth";
import {
  applyEvent, type AuditRow, type CheckResult, type ConfigDoc, type Job, type LeasesViewT, type LiveEvent, type Plan,
  type RouteEdit, type RouteView, type StateView,
} from "./routerModel";
import type { AdminKey, ConfigData, FormEdit, GatewayCheck, GatewayDoc, NewAdminKey } from "./forms/formModel";
import type { CardOption, CatalogKind, DetectedCpu, DetectedGpu, DirListing, DownloadJob, HfDetails, HfFile, HfRow, Inventory, PathStat } from "./forms/inventoryModel";
import type {
  Approvals, Draft, DraftSource, FitAnswer, InstalledPkg, PackageDetail, PackageSummary, Report, StoreInfo, StoreJob, TestRun,
} from "./forms/engineStoreModel";

/** Where the admin API is and how calls authenticate. */
export interface RouterTransport {
  /** The admin API's base (no trailing slash): "/api/admin/router" in the app, "<page>/api" on the router. */
  base: string;
  /** Headers for state-changing calls (the CSRF token of whichever session the page has). */
  headers: () => Record<string, string>;
  /** A 401 from the admin API (the app: nothing, its own session is fine; the router page: sign in again). */
  unauthorized: () => void;
}

const appTransport: RouterTransport = { base: "/api/admin/router", headers: csrfHeaders, unauthorized: () => {} };
let transport: RouterTransport = appTransport;

/** The router's own admin pages call the router directly (D151). */
export function setRouterTransport(t: Partial<RouterTransport>) {
  transport = { ...transport, ...t };
}

// Bridges (ROUTER-MEDIA-PLAN.md 11): machines that dial /bridge/ws.
export interface BridgeCard {
  id: string; global: string; device: "gpu" | "cpu"; label: string; gpu_uuid?: string;
  vram_gib?: number; cores?: number; ram_gib?: number; shared?: boolean; engines: Record<string, string[]>;
}
export interface BridgeEngine {
  id: string; package?: string | null; method?: string; kinds: string[]; api?: string;
  ports: Record<string, number>; lifecycle: string; idle_stop_seconds?: number;
}
export interface BridgeView {
  id: string; enabled: boolean; priority: number; token: { created: number } | null;
  online: boolean; seen: boolean; version?: string; since?: number; lastSeen?: number; peer?: string;
  title?: string; os?: string; arch?: string; modelDir?: string | null; cards?: BridgeCard[];
  engines?: BridgeEngine[]; files?: number; issues?: string[]; streams?: number;
}
export interface BridgeEngineState {
  engine: string; card: string; state: "stopped" | "starting" | "running" | "failed"; error?: string | null;
  active: number; idle_seconds?: number | null; up_seconds?: number | null; port?: number | null;
}

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
      ...(method === "GET" ? {} : url.startsWith(transport.base) ? transport.headers() : csrfHeaders()),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!res.ok) {
    if (res.status === 401 && url.startsWith(transport.base)) transport.unauthorized();
    else if (res.status === 401 && url.startsWith("/api/") && !url.startsWith("/api/admin/router")) sessionEnded();
    const d = (data ?? {}) as Record<string, unknown>;
    throw new RouterApiError((d.error as string) || res.statusText, res.status, d);
  }
  return data as T;
}

const R = () => transport.base;

export const routerApi = {
  state: () => call<StateView>("GET", `${R()}/state`),
  config: () => call<ConfigDoc>("GET", `${R()}/config`),
  check: (text: string) => call<CheckResult>("POST", `${R()}/config/check`, { text }),
  save: (text: string, rev: string, reason: string) => call<CheckResult>("PUT", `${R()}/config`, { text, reason }, { "If-Match": `"${rev}"` }),
  plan: (server: string, layout: string) => call<Plan>("GET", `${R()}/plan?server=${encodeURIComponent(server)}&layout=${encodeURIComponent(layout)}`),
  switchTo: (server: string, layout: string) => call<Job>("POST", `${R()}/switch`, { server, layout }),
  jobs: (limit = 50) => call<Job[]>("GET", `${R()}/jobs?limit=${limit}`),
  job: (id: string) => call<Job>("GET", `${R()}/jobs/${encodeURIComponent(id)}`),
  cancelJob: (id: string) => call<Job>("POST", `${R()}/jobs/${encodeURIComponent(id)}/cancel`),
  card: (card: string, action: "offline" | "online" | "exclude" | "include", reason: string) =>
    call<Job>("POST", `${R()}/cards/${encodeURIComponent(card)}/${action}`, { reason }),
  audit: (limit = 100) => call<AuditRow[]>("GET", `${R()}/audit?limit=${limit}`),
  leases: () => call<LeasesViewT>("GET", `${R()}/leases`),
  release: (key: string) => call<{ ok: boolean }>("DELETE", `${R()}/leases/${encodeURIComponent(key)}`),
  routes: () => call<RouteView[]>("GET", `${R()}/routes`),
  editRoute: (name: string, edit: RouteEdit) => call<RouteView>("PATCH", `${R()}/routes/${encodeURIComponent(name)}`, edit),
  // D151: the config as data for the forms, and form edits turned into checked text.
  configJson: () => call<ConfigData>("GET", `${R()}/config/json`),
  configEdit: (edits: FormEdit[], text?: string) => call<{ text: string; check: CheckResult }>("POST", `${R()}/config/edit`, text === undefined ? { edits } : { edits, text }),
  // router.json, the gateway's model list.
  gateway: () => call<GatewayDoc>("GET", `${R()}/gateway`),
  gatewayCheck: (text: string) => call<GatewayCheck>("POST", `${R()}/gateway/check`, { text }),
  gatewaySave: (text: string, rev: string, reason: string) =>
    call<GatewayCheck & { reloaded: boolean; reloadError?: string }>("PUT", `${R()}/gateway`, { text, reason }, { "If-Match": `"${rev}"` }),
  // The router's own admin keys.
  adminKeys: () => call<{ keys: AdminKey[]; local: boolean }>("GET", `${R()}/admin-keys`),
  createAdminKey: (name: string) => call<NewAdminKey>("POST", `${R()}/admin-keys`, { name }),
  revokeAdminKey: (id: number) => call<{ ok: boolean }>("DELETE", `${R()}/admin-keys/${id}`),
  // Bridges: tokens (shown once), settings, commands.
  bridges: () => call<{ bridges: BridgeView[]; path: string }>("GET", `${R()}/bridges`),
  mintBridgeToken: (id: string, rotate = false) => call<{ id: string; token: string }>("POST", `${R()}/bridges`, { id, rotate }),
  revokeBridge: (id: string) => call<{ ok: boolean }>("DELETE", `${R()}/bridges/${encodeURIComponent(id)}`),
  bridgeSettings: (id: string, s: { enabled?: boolean; priority?: number }) => call<{ id: string; enabled: boolean; priority: number }>("PUT", `${R()}/bridges/${encodeURIComponent(id)}/settings`, s),
  bridgeCmd: <T = unknown>(id: string, op: string, args: Record<string, unknown> = {}) => call<T>("POST", `${R()}/bridges/${encodeURIComponent(id)}/cmd`, { op, args }),
  // D152: the inventory, engine capabilities, Hugging Face and downloads.
  inventory: () => call<Inventory>("GET", `${R()}/inventory`),
  catalog: () => call<CatalogKind[]>("GET", `${R()}/catalog`),
  detect: (server: string) => call<{ server: string; gpus: DetectedGpu[]; cpu?: DetectedCpu | null }>("POST", `${R()}/inventory/detect`, { server }),
  ggufMeta: (server: string, path: string) => call<Record<string, string>>("POST", `${R()}/inventory/gguf`, { server, path }),
  browse: (server: string, path: string, offset = 0) =>
    call<DirListing>("GET", `${R()}/inventory/browse?server=${encodeURIComponent(server)}&path=${encodeURIComponent(path)}&offset=${offset}&limit=200`),
  pathStat: (server: string, path: string) => call<PathStat>("GET", `${R()}/inventory/stat?server=${encodeURIComponent(server)}&path=${encodeURIComponent(path)}`),
  mkdir: (server: string, path: string) => call<PathStat>("POST", `${R()}/inventory/mkdir`, { server, path }),
  place: (model: string) => call<{ placement: CardOption[] }>("POST", `${R()}/inventory/place`, { model }),
  hfSearch: (q: string, kind?: string) => call<HfRow[]>("GET", `${R()}/hf/search?q=${encodeURIComponent(q)}${kind ? `&kind=${encodeURIComponent(kind)}` : ""}`),
  hfModel: (repo: string) => call<HfDetails>("GET", `${R()}/hf/model?repo=${encodeURIComponent(repo)}`),
  hfToken: () => call<{ set: boolean; from: string }>("GET", `${R()}/hf/token`),
  hfSetToken: (token: string) => call<{ set: boolean; from: string }>("PUT", `${R()}/hf/token`, { token }),
  hfRemoveToken: () => call<{ set: boolean; from: string }>("DELETE", `${R()}/hf/token`),
  downloads: () => call<DownloadJob[]>("GET", `${R()}/downloads`),
  startDownload: (r: { server: string; repo: string; revision?: string | null; files: HfFile[]; model?: string; gated?: boolean; dir?: string }) =>
    call<DownloadJob>("POST", `${R()}/downloads`, r),
  cancelDownload: (id: string) => call<DownloadJob>("POST", `${R()}/downloads/${encodeURIComponent(id)}/cancel`),
  resumeDownload: (id: string) => call<DownloadJob>("POST", `${R()}/downloads/${encodeURIComponent(id)}/resume`),
  removeDownload: (id: string) => call<{ ok: boolean }>("DELETE", `${R()}/downloads/${encodeURIComponent(id)}`),
};

const ES = () => `${R()}/engine-store`;
const q = encodeURIComponent;

/** Engine packages: the stores, installs (jobs), My packages and the Create package wizard. */
export const engineStoreApi = {
  stores: () => call<{ stores: StoreInfo[] }>("GET", `${ES()}/stores`),
  addStore: (url: string, trusted: boolean, name?: string) => call<StoreInfo>("POST", `${ES()}/stores`, name ? { url, trusted, name } : { url, trusted }),
  removeStore: (id: string) => call<{ ok: boolean }>("DELETE", `${ES()}/stores/${q(id)}`),
  packages: () => call<{ packages: PackageSummary[] }>("GET", `${ES()}/packages`),
  package: (store: string, id: string) => call<PackageDetail>("GET", `${ES()}/package?store=${q(store)}&id=${q(id)}`),
  fit: (store: string, id: string, server: string) => call<FitAnswer>("POST", `${ES()}/fit`, { store, id, server }),
  validate: (manifest: string, files: Record<string, string>, mode: "private" | "store") => call<Report>("POST", `${ES()}/validate`, { manifest, files, mode }),
  install: (r: { store: string; id: string; server: string; card: string; approvals: Approvals; engine?: string; params?: Record<string, unknown> }) =>
    call<{ job: StoreJob }>("POST", `${ES()}/install`, r),
  update: (engine: string) => call<{ job: StoreJob }>("POST", `${ES()}/update`, { engine }),
  remove: (engine: string, uninstall: boolean) => call<{ job: StoreJob }>("POST", `${ES()}/remove`, { engine, uninstall }),
  jobs: () => call<{ jobs: StoreJob[] }>("GET", `${ES()}/jobs`),
  installed: () => call<{ installed: InstalledPkg[] }>("GET", `${ES()}/installed`),
  draft: (source: DraftSource, value: string) => call<Draft>("POST", `${ES()}/wizard/draft`, { source, value }),
  testRun: (image: string, server: string, card: string) => call<TestRun>("POST", `${ES()}/wizard/test-run`, { image, server, card, confirm: true }),
  my: () => call<{ packages: PackageSummary[] }>("GET", `${ES()}/my`),
  saveMy: (id: string, manifest: string, files: Record<string, string>) => call<{ report: Report }>("PUT", `${ES()}/my/${q(id)}`, { manifest, files }),
  removeMy: (id: string) => call<{ ok: boolean }>("DELETE", `${ES()}/my/${q(id)}`),
  /** A plain link (the session cookie authenticates it): the package as a zip. */
  exportUrl: (id: string) => `${ES()}/my/${q(id)}/export`,
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

/** A key request waiting for an admin (D149): someone's app asked the router for a key. */
export interface KeyRequest {
  id: string;
  /** What the person reads out ("ABCD-2345"). */
  code: string;
  client: string;
  ip: string;
  created: number;
  expiresIn: number;
  suggestedName: string;
}

export const keyRequestApi = {
  list: () => call<{ requests: KeyRequest[]; router: boolean; claudeGroup: string; scopes: string[] }>("GET", "/api/admin/key-requests"),
  approve: (id: string, name: string, claude: boolean) =>
    call<{ ok: boolean; name: string; scopes: string[] }>("POST", `/api/admin/key-requests/${encodeURIComponent(id)}/approve`, { name, claude }),
  deny: (id: string) => call<{ ok: boolean }>("POST", `/api/admin/key-requests/${encodeURIComponent(id)}/deny`),
};

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
export function useLiveState(enabled = true, url = `${R()}/events`) {
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
