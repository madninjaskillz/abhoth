// The router control plane's shapes (crates/orca-router/src/control_api/model.rs and
// user_view.rs) and the pure helpers the widgets use. Pure (no imports, no DOM), so plain node
// tests it (test/routerModel.test.ts). The widgets built on it are shared by the admin pages,
// the supervisor dashboard and the app's Models tab.

export interface Telemetry {
  name: string;
  memUsedMib: number;
  memTotalMib: number;
  utilPct: number;
  tempC: number;
  memTempC?: number;
  powerW: number;
  powerLimitW?: number;
  smMhz: number;
  smMaxMhz: number;
  appClockMhz?: number;
  clockReasons: string[];
  clockCheck: string;
  ageMs: number;
  stale: boolean;
}

export interface EngineView {
  engine: string;
  profile: string;
  model: string;
  servedId: string;
  state: string;
  since: number;
  healthy: boolean;
}

export interface CardView {
  id: string;
  server: string;
  gpuUuid: string;
  label: string;
  managed: boolean;
  adminState: string;
  adminNote?: string;
  engine?: EngineView;
  telemetry?: Telemetry;
  lanes: number;
  inflight: number;
  queued: number;
  leases: number;
  onDemand: boolean;
  foreign: string[];
}

export interface ServerView {
  id: string;
  title: string;
  reachability: string;
  exec: string;
  activeLayout: string | null;
  drift: boolean;
  hostctlOk: boolean;
  cards: CardView[];
}

export interface GuardView {
  id: string;
  kind: string;
  active: boolean;
  stale: boolean;
  blocks: string[];
  reason?: string;
  retrySeconds: number;
}

export interface LayoutView {
  id: string;
  title: string;
  server: string;
  mode: string;
  cards: Record<string, string>;
  active: boolean;
  source: string;
}

export interface JobSummary {
  id: string;
  server: string;
  kind: string;
  target: string;
  state: string;
  step: number;
  steps: number;
  title: string;
  started: number;
  estimateSeconds: number;
}

export interface StateView {
  rev: string;
  configError?: string;
  mode: string;
  servers: ServerView[];
  guards: GuardView[];
  job?: JobSummary;
  layouts: LayoutView[];
  updated: number;
}

export interface PlanCard { card: string; before: string; after: string; action: string; reason: string }
export interface PlanStep { n: number; title: string; detail: string; estimateSeconds: number }
export interface Check { name: string; status: string; reason?: string }
export interface Blocker { kind: string; id: string; reason: string; holder?: string; retrySeconds: number }

export interface Plan {
  server: string;
  from: string | null;
  to: string;
  cards: PlanCard[];
  steps: PlanStep[];
  preconditions: Check[];
  blockers: Blocker[];
  restores: { file: string; bytes: number; seconds: number }[];
  drains: { card: string; inflight: number }[];
  routes: { route: string; before: string[]; after: string[] }[];
  slotsBefore: string[];
  slotsAfter: string[];
  estimateSeconds: number;
  runnable: boolean;
  note?: string;
}

export interface JobStep { n: number; title: string; state: string; seconds: number; output?: string }

export interface Job {
  id: string;
  server: string;
  kind: string;
  from?: string;
  target: string;
  requestedBy: string;
  state: string;
  started: number;
  ended?: number;
  error?: string;
  steps: JobStep[];
}

export interface AuditRow {
  id: number;
  time: number;
  who: string;
  action: string;
  target: string;
  revFrom?: string;
  revTo?: string;
  diff?: string;
  reason?: string;
}

export interface LeaseView {
  key: string;
  slot: string;
  card: string;
  kind: string;
  holder: string;
  principal: string;
  expiresInSeconds: number;
  reason?: string;
}

export interface RouteView {
  name: string;
  targets: string[];
  cold: string;
  fallback: string[];
  access: string;
  adapters: string[];
  hidden: boolean;
  sameAs?: string;
  servingNow: string[];
  allowUserSwitch: boolean;
}

export interface RouteEdit {
  cold?: string;
  fallback?: string[];
  access?: string;
  hidden?: boolean;
  allowUserSwitch?: boolean;
}

export interface ConfigDoc { rev: string; path: string; text: string; error?: string }
export interface Problem { path: string; line?: number; message: string; severity: string }
export interface CheckResult { ok: boolean; problems: Problem[]; diff: string; consequences: string[]; plan?: Plan; rev?: string }

/** The filtered view for everyone (GET /v1/abhoth/view; the app's supervisor mode). */
export interface CatalogModel {
  name: string;
  kind: string;
  status: "live" | "cold" | "switching" | string;
  cold: string;
  fallback?: string[];
  canSwitchOnDemand: boolean;
  serving?: string[];
}

export interface Actions {
  chat: boolean;
  models: boolean;
  switch: boolean;
  onDemand: boolean;
  leases: boolean;
  readControl: boolean;
  editControl: boolean;
}

export interface UserView {
  principal: string;
  admin: boolean;
  actions: Actions;
  models: CatalogModel[];
  layouts?: LayoutView[];
  job?: JobSummary;
  cards?: CardView[];
  rev: string;
}

/** One event of the live stream (`event:` name and parsed `data:`). */
export interface LiveEvent {
  kind: string;
  data: unknown;
}

/** The live state after an event: `state` replaces, `telemetry` merges cards, `job` moves the summary. */
export function applyEvent(s: StateView | undefined, e: LiveEvent): StateView | undefined {
  if (e.kind === "state") return e.data as StateView;
  if (!s) return s;
  if (e.kind === "telemetry") {
    const cards = ((e.data as { cards?: CardView[] }).cards ?? []) as CardView[];
    const by = new Map(cards.map((c) => [`${c.server}/${c.id}`, c]));
    return {
      ...s,
      updated: (e.data as { updated?: number }).updated ?? s.updated,
      servers: s.servers.map((srv) => ({ ...srv, cards: srv.cards.map((c) => by.get(`${c.server}/${c.id}`) ?? c) })),
    };
  }
  if (e.kind === "job") {
    const j = e.data as Job | null;
    if (!j) return s;
    if (j.state !== "running" && j.state !== "queued") return s.job?.id === j.id ? { ...s, job: undefined } : s;
    const p = jobProgress(j);
    return {
      ...s,
      job: {
        id: j.id,
        server: j.server,
        kind: j.kind,
        target: j.target,
        state: j.state,
        step: p.current?.n ?? p.done,
        steps: p.total,
        title: p.current?.title ?? "",
        started: j.started,
        estimateSeconds: s.job?.id === j.id ? s.job.estimateSeconds : 0,
      },
    };
  }
  if (e.kind === "config") return { ...s, rev: (e.data as { rev: string }).rev ?? s.rev };
  return s;
}

export function jobProgress(j: Pick<Job, "steps">): { done: number; total: number; pct: number; current?: JobStep; failed?: JobStep } {
  const total = j.steps.length;
  const done = j.steps.filter((x) => x.state === "ok" || x.state === "skipped").length;
  return {
    done,
    total,
    pct: total ? Math.round((done / total) * 100) : 0,
    current: j.steps.find((x) => x.state === "running"),
    failed: j.steps.find((x) => x.state === "failed"),
  };
}

export type Level = "ok" | "warn" | "bad" | "idle";

/** A card's health at a glance, with the reasons (the tile's colour and tooltip). */
export function cardHealth(c: CardView): { level: Level; reasons: string[] } {
  const reasons: string[] = [];
  let level: Level = "ok";
  const up = (l: Level) => {
    const order: Level[] = ["idle", "ok", "warn", "bad"];
    if (order.indexOf(l) > order.indexOf(level)) level = l;
  };
  if (!c.managed) level = "idle";
  if (c.adminState === "offline") { up("warn"); reasons.push("offline"); }
  if (c.adminState === "missing") { up("bad"); reasons.push("GPU missing"); }
  const e = c.engine;
  if (e) {
    if (e.state === "failed") { up("bad"); reasons.push("engine failed"); }
    else if (e.state === "unknown") { up("warn"); reasons.push("unknown process on the card"); }
    else if (e.state !== "ready") { up("warn"); reasons.push(`engine ${e.state}`); }
    else if (!e.healthy) { up("warn"); reasons.push("health checks failing"); }
  }
  const t = c.telemetry;
  if (t) {
    if (t.stale) { up("warn"); reasons.push("telemetry stale"); }
    if (t.tempC >= 88) { up("bad"); reasons.push(`${t.tempC} °C`); }
    else if (t.tempC >= 80) { up("warn"); reasons.push(`${t.tempC} °C`); }
    if (t.clockCheck && t.clockCheck !== "ok") { up("warn"); reasons.push(t.clockCheck); }
  }
  if (c.foreign.length && c.managed) { up("warn"); reasons.push(`foreign: ${c.foreign.join(", ")}`); }
  return { level, reasons };
}

export function pct(used: number, total: number): number {
  return total > 0 ? Math.min(100, Math.round((used / total) * 100)) : 0;
}

export function fmtGib(mib: number): string {
  return `${(mib / 1024).toFixed(mib >= 10240 ? 0 : 1)} GiB`;
}

export function fmtBytes(b: number): string {
  if (b >= 1e9) return `${(b / 1e9).toFixed(1)} GB`;
  if (b >= 1e6) return `${(b / 1e6).toFixed(0)} MB`;
  return `${b} B`;
}

export function fmtSeconds(s: number): string {
  if (s < 90) return `${Math.round(s)} s`;
  const m = Math.floor(s / 60);
  const r = Math.round(s % 60);
  return r ? `${m} min ${r} s` : `${m} min`;
}

/** "3 h ago" from a unix-ms time. */
export function ago(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return `${s} s ago`;
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

/** Problems keyed by line (the config editor's gutter marks). */
export function problemsByLine(ps: Problem[]): Map<number, Problem[]> {
  const m = new Map<number, Problem[]>();
  for (const p of ps) if (p.line) m.set(p.line, [...(m.get(p.line) ?? []), p]);
  return m;
}

/** Which models a principal can pick right now, live first (the app's model picker). */
export function pickable(v: Pick<UserView, "models">): CatalogModel[] {
  const rank = (m: CatalogModel) => (m.status === "live" ? 0 : m.cold === "fallback" || m.canSwitchOnDemand ? 1 : 2);
  return [...v.models].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}

/** The words of a cold policy for people. */
export function coldLabel(cold: string, fallback: string[] = []): string {
  if (cold === "fallback") return fallback.length ? `falls back to ${fallback.join(", ")}` : "falls back";
  if (cold === "on_demand") return "switches a card on demand";
  return "refused while cold";
}

/** Route access in words. */
export function accessLabel(a: string): string {
  if (a === "users" || !a) return "every user";
  if (a === "admins") return "admins only";
  if (a.startsWith("keys:")) return `keys: ${a.slice(5).split(",").map((x) => x.trim()).join(", ")}`;
  return a;
}

/** Parse one SSE frame block ("event: x\ndata: {...}"); null for comments and keepalives. */
export function parseSse(block: string): LiveEvent | null {
  let kind = "message";
  const data: string[] = [];
  for (const line of block.split("\n")) {
    if (line.startsWith(":")) continue;
    if (line.startsWith("event:")) kind = line.slice(6).trim();
    else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
  }
  if (!data.length) return null;
  try {
    return { kind, data: JSON.parse(data.join("\n")) };
  } catch {
    return { kind, data: data.join("\n") };
  }
}
