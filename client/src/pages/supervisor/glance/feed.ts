// Glance's live feed (SUPERVISOR.md 2.5, the supervisor's TelemetryHub): every SSE frame is a
// FeedMessage `{type, seq, ts, ...}`: a `snapshot` (`state`: the hub's sections) on connect,
// then `patch` (an RFC 7386 merge patch of the sections) and `event` messages, all numbered by
// one `seq` (also the SSE id, so a reconnect resumes with Last-Event-ID). The
// store applies them with structural sharing (unchanged parts keep their identity, so memoised
// cards skip re-rendering) and keeps the chart history in fixed-size rings: memory stays flat
// however long the page is open.
// Type imports only: plain node runs this file in the tests.
import type { GlanceState, Gpu, Instance, Point } from "./model";

// ------------------------------------------------------------------ merge patch

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** RFC 7386: objects merge, `null` deletes, anything else replaces. Returns `target` itself when nothing changed. */
export function mergePatch<T>(target: T, patch: unknown): T {
  if (!isObj(patch)) return patch as T;
  const base: Record<string, unknown> = isObj(target) ? target : {};
  let out: Record<string, unknown> | null = isObj(target) ? null : {};
  for (const [k, pv] of Object.entries(patch)) {
    if (pv === null) {
      if (k in base) {
        out ??= { ...base };
        delete out[k];
      }
      continue;
    }
    const cur = base[k];
    const next = mergePatch(cur, pv);
    if (next !== cur) {
      out ??= { ...base };
      out[k] = next;
    }
  }
  return (out ?? base) as T;
}

/** The merge patch that turns `a` into `b` (the server does this; here for tests and fakes). */
export function diff(a: unknown, b: unknown): unknown {
  if (!isObj(a) || !isObj(b)) return JSON.stringify(a) === JSON.stringify(b) ? undefined : b;
  const out: Record<string, unknown> = {};
  for (const k of Object.keys(a)) if (!(k in b)) out[k] = null;
  for (const [k, v] of Object.entries(b)) {
    if (v === null) {
      if (k in a && a[k] !== null) out[k] = null;
      continue;
    }
    const d = diff(a[k], v);
    if (d !== undefined) out[k] = d;
  }
  return Object.keys(out).length ? out : undefined;
}

// ------------------------------------------------------------------ history rings

/** One series: a ring of (time, value) pairs in typed arrays (no garbage per sample). */
export class Ring {
  readonly cap: number;
  private t: Float64Array;
  private v: Float64Array;
  private head = 0; // next write
  len = 0;
  touched = 0; // wall time of the last push (pruning)

  constructor(cap: number) {
    this.cap = cap;
    this.t = new Float64Array(cap);
    this.v = new Float64Array(cap);
  }

  get lastT(): number {
    return this.len ? this.t[(this.head - 1 + this.cap) % this.cap] : -Infinity;
  }

  /** Adds a sample newer than the last one (older or equal times are ignored). */
  push(t: number, v: number | null | undefined): boolean {
    if (!(t > this.lastT)) return false;
    this.t[this.head] = t;
    this.v[this.head] = v == null || !Number.isFinite(v) ? NaN : v;
    this.head = (this.head + 1) % this.cap;
    if (this.len < this.cap) this.len++;
    return true;
  }

  /** Samples with t ≥ from, oldest first. */
  points(from = -Infinity): Point[] {
    const out: Point[] = [];
    const start = (this.head - this.len + this.cap) % this.cap;
    for (let n = 0; n < this.len; n++) {
      const i = (start + n) % this.cap;
      if (this.t[i] < from) continue;
      out.push([this.t[i], Number.isNaN(this.v[i]) ? null : this.v[i]]);
    }
    return out;
  }

  /**
   * At most 2 × `buckets` points over [from, to]: each bucket keeps its min and max in time
   * order, so a spike survives however narrow the chart is.
   */
  decimated(from: number, to: number, buckets: number): Point[] {
    const pts = this.points(from).filter((p) => p[1] != null && p[0] <= to);
    if (pts.length <= buckets * 2) return pts;
    const out: Point[] = [];
    const span = (to - from) / buckets || 1;
    let b = -1;
    let lo: Point | null = null;
    let hi: Point | null = null;
    const flush = () => {
      if (!lo || !hi) return;
      if (lo === hi) out.push(lo);
      else if (lo[0] < hi[0]) out.push(lo, hi);
      else out.push(hi, lo);
    };
    for (const p of pts) {
      const nb = Math.min(buckets - 1, Math.floor((p[0] - from) / span));
      if (nb !== b) {
        flush();
        b = nb;
        lo = hi = p;
        continue;
      }
      if ((p[1] as number) < (lo![1] as number)) lo = p;
      if ((p[1] as number) > (hi![1] as number)) hi = p;
    }
    flush();
    return out;
  }
}

export class History {
  readonly cap: number;
  readonly rings = new Map<string, Ring>();
  constructor(cap = 720) {
    this.cap = cap;
  }

  push(id: string, t: number, v: number | null | undefined, wall: number): void {
    let r = this.rings.get(id);
    if (!r) {
      if (v == null) return; // a series starts with a value
      r = new Ring(this.cap);
      this.rings.set(id, r);
    }
    if (r.push(t, v)) r.touched = wall;
  }

  get(id: string): Ring | undefined {
    return this.rings.get(id);
  }

  /** Drops series nothing has fed for `idleS` (a layout switch replaces engines and slots). */
  prune(wall: number, idleS: number): void {
    for (const [id, r] of this.rings) if (wall - r.touched > idleS) this.rings.delete(id);
  }
}

// Series ids: the legacy dash's `hist` keys for host-wide series, plus
// gpu:<uuid>:<metric>, llm:<key>, slot:<key>:<slot id>.
export const GPU_SERIES = ["temperature_gpu", "temperature_memory", "power_draw_watts", "utilization_gpu_ratio", "fan_duty", "fan_rpm"] as const;
export const gpuSeries = (g: Pick<Gpu, "uuid">, m: (typeof GPU_SERIES)[number]) => `gpu:${g.uuid}:${m}`;
export const llmSeries = (i: Pick<Instance, "key">) => `llm:${i.key}`;
export const slotSeries = (i: Pick<Instance, "key">, id: number | string) => `slot:${i.key}:${id}`;

const loudest = (noise: Record<string, number> | undefined): number | null => {
  const v = Object.values(noise || {}).filter((x) => typeof x === "number");
  return v.length ? Math.max(...v) : null;
};

/** The current value of every series, read from the live state (what each patch appends). */
export function samplesOf(s: GlanceState): [string, number | null][] {
  const out: [string, number | null][] = [];
  const insts = Object.values(s.llm || {});
  out.push(["total_tok_s", insts.reduce((a, i) => a + (i.tok_s || 0), 0)]);
  const h = s.host;
  if (h) {
    out.push(["cpu", h.cpu ?? null]);
    if (h.mem_total && h.mem_avail != null) out.push(["mem_used", h.mem_total - h.mem_avail]);
  }
  const noise = loudest(s.noise);
  if (noise != null) out.push(["noise", noise]);
  if (s.ecoflow) out.push(["ecoflow", s.ecoflow.watts ?? null]);
  if (s.bmc && s.bmc.live) for (const k of ["cpu1", "cpu2", "nic"] as const) out.push([`bmc:${k}`, s.bmc[k] ?? null]);
  if (s.esp && s.esp.live)
    for (const f of s.esp.fans || []) {
      out.push([`fan:${f.fan}:duty`, f.duty ?? null]);
      out.push([`fan:${f.fan}:rpm`, f.rpm ?? null]);
    }
  for (const g of s.gpus || []) {
    out.push([gpuSeries(g, "temperature_gpu"), g.temperature_gpu ?? null]);
    out.push([gpuSeries(g, "temperature_memory"), g.temperature_memory ?? null]);
    out.push([gpuSeries(g, "power_draw_watts"), g.power_draw_watts ?? null]);
    out.push([gpuSeries(g, "utilization_gpu_ratio"), g.utilization_gpu_ratio ?? null]);
    const duty = g.duct && s.esp?.live ? g.duct.duty ?? null : g.fan_speed_ratio != null ? g.fan_speed_ratio * 100 : null;
    out.push([gpuSeries(g, "fan_duty"), duty]);
    out.push([gpuSeries(g, "fan_rpm"), g.duct && s.esp?.live ? g.duct.rpm ?? null : null]);
  }
  for (const i of insts) {
    out.push([llmSeries(i), i.tok_s ?? 0]);
    for (const sl of i.slots || []) out.push([slotSeries(i, sl.id), sl.tok_s ?? 0]);
  }
  return out;
}

/** The history a snapshot (or a patch that has one) carries, as series id → points. */
export function histOf(s: GlanceState, slotStepS = 2.5): [string, Point[]][] {
  const out: [string, Point[]][] = [];
  for (const [k, pts] of Object.entries(s.hist || {})) out.push([k, pts]);
  for (const g of s.gpus || []) for (const [m, pts] of Object.entries(g.hist || {})) out.push([`gpu:${g.uuid}:${m}`, pts]);
  for (const i of Object.values(s.llm || {})) {
    if (i.hist) out.push([llmSeries(i), i.hist]);
    // slot_hist has values only, one per collector tick, the newest last.
    for (const [id, vals] of Object.entries(i.slot_hist || {}))
      out.push([slotSeries(i, id), vals.map((v, n): Point => [s.time - (vals.length - 1 - n) * slotStepS, v])]);
  }
  return out;
}

/** The state without its history (the store keeps that in rings). */
export function withoutHist(s: GlanceState): GlanceState {
  let out = s;
  if (out.hist !== undefined) {
    out = { ...out };
    delete out.hist;
  }
  if (Array.isArray(out.gpus) && out.gpus.some((g) => g.hist !== undefined)) {
    out = { ...out, gpus: out.gpus!.map((g) => (g.hist === undefined ? g : (({ hist: _h, ...rest }) => rest)(g))) };
  }
  if (out.llm && Object.values(out.llm).some((i) => i.hist !== undefined || i.slot_hist !== undefined)) {
    const llm: Record<string, Instance> = {};
    for (const [k, i] of Object.entries(out.llm)) {
      if (i.hist === undefined && i.slot_hist === undefined) llm[k] = i;
      else {
        const { hist: _h, slot_hist: _s, ...rest } = i;
        llm[k] = rest;
      }
    }
    out = { ...out, llm };
  }
  return out;
}

// ------------------------------------------------------------------ store

export type FeedStatus = "connecting" | "live" | "stale" | "offline";

export interface FeedEvent {
  kind: string;
  severity?: "info" | "warning" | "critical";
  text?: string;
  /** Wall time (ms) it arrived. */
  at: number;
  data: unknown;
}

/** One SSE frame's data (`orca_sensors::feed::FeedMessage`). */
export type FeedMessage =
  | { type: "snapshot"; seq: number; ts: number; state: Record<string, unknown> }
  | { type: "patch"; seq: number; ts: number; patch: unknown }
  | { type: "event"; seq: number; ts: number; event: { kind: string; severity?: FeedEvent["severity"]; text?: string; data?: unknown } };

/**
 * The hub's `history` section (the seed for the charts, set once by the source) in the shape
 * histOf reads.
 */
export function historyState(h: unknown): GlanceState | null {
  if (!isObj(h)) return null;
  const gpus = isObj(h.gpus) ? Object.entries(h.gpus).map(([uuid, hist]) => ({ uuid, hist }) as unknown as Gpu) : [];
  const llm: Record<string, Instance> = {};
  if (isObj(h.llm)) for (const [key, v] of Object.entries(h.llm)) if (isObj(v)) llm[key] = { ...v, key } as unknown as Instance;
  return { time: Number(h.time) || 0, hist: (isObj(h.hist) ? h.hist : {}) as GlanceState["hist"], gpus, llm };
}

/** The raw state without history (the `history` section, and the legacy dash's inline `hist`). */
function stripRaw(raw: Record<string, unknown>): Record<string, unknown> {
  let out = raw;
  if ("history" in out) {
    out = { ...out };
    delete out.history;
  }
  return withoutHist(out as unknown as GlanceState) as unknown as Record<string, unknown>;
}

export interface StoreView {
  state: GlanceState | null;
  status: FeedStatus;
  /** Wall time (ms) of the last message. */
  lastAt: number;
  error: string | null;
  version: number;
}

const MAX_EVENTS = 50;

export class GlanceStore {
  readonly history: History;
  private view: StoreView = { state: null, status: "connecting", lastAt: 0, error: null, version: 0 };
  /** The feed's state as it arrives (sections), without history. */
  private raw: Record<string, unknown> | null = null;
  /** Sections → the page's state (model.ts `fromSections`); the default reads a legacy state. */
  private derive: (raw: Record<string, unknown>, tsMs: number) => GlanceState;
  private seq = -1;
  private listeners = new Set<() => void>();
  readonly events: FeedEvent[] = [];
  /** Series idle this long are dropped (seconds). */
  pruneAfterS = 900;
  private lastPrune = 0;
  now: () => number;

  constructor(opts: { cap?: number; now?: () => number; derive?: (raw: Record<string, unknown>, tsMs: number) => GlanceState } = {}) {
    this.history = new History(opts.cap ?? 720);
    this.now = opts.now ?? (() => Date.now());
    this.derive = opts.derive ?? ((raw) => raw as unknown as GlanceState);
  }

  get current(): StoreView {
    return this.view;
  }
  get lastSeq(): number {
    return this.seq;
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };
  getSnapshot = (): StoreView => this.view;

  private set(p: Partial<StoreView>) {
    this.view = { ...this.view, ...p, version: this.view.version + 1 };
    for (const l of [...this.listeners]) l();
  }

  /** History a message carries: the `history` section, or a legacy state's inline `hist`. */
  private seed(raw: Record<string, unknown>, wall: number) {
    const h = "history" in raw ? historyState(raw.history) : (raw as unknown as GlanceState);
    if (h) for (const [id, pts] of histOf(h)) for (const [t, v] of pts) this.history.push(id, t, v, wall);
  }

  private apply(raw: Record<string, unknown>, tsMs: number, carriesHist: boolean) {
    const wall = this.now() / 1000;
    if (carriesHist) this.seed(raw, wall);
    this.raw = carriesHist ? stripRaw(raw) : raw;
    const next = this.derive(this.raw, tsMs);
    for (const [id, v] of samplesOf(next)) this.history.push(id, next.time, v, wall);
    if (wall - this.lastPrune > 60) {
      this.lastPrune = wall;
      this.history.prune(wall, this.pruneAfterS);
    }
    this.set({ state: next, status: "live", lastAt: this.now(), error: null });
  }

  /** A snapshot: the state (sections) at `seq`. */
  snapshot(state: Record<string, unknown> | GlanceState, seq: number, tsMs = this.now()): void {
    this.seq = seq;
    this.apply(state as Record<string, unknown>, tsMs, true);
  }

  /** Applies patch `seq`; false when a message was missed (the caller re-syncs). */
  patch(p: unknown, seq: number, tsMs = this.now()): boolean {
    if (!this.raw || seq !== this.seq + 1) return false;
    this.seq = seq;
    const carries = isObj(p) && ("history" in p || JSON.stringify(p).includes('"hist"'));
    this.apply(mergePatch(this.raw, p), tsMs, carries);
    return true;
  }

  /** An immediate event; with a `seq`, false when a message was missed. */
  event(e: Omit<FeedEvent, "at">, seq?: number): boolean {
    if (seq != null) {
      if (seq !== this.seq + 1) return false;
      this.seq = seq;
    }
    this.events.push({ ...e, at: this.now() });
    if (this.events.length > MAX_EVENTS) this.events.splice(0, this.events.length - MAX_EVENTS);
    this.set({ lastAt: this.now() });
    return true;
  }

  status(status: FeedStatus, error: string | null = null): void {
    if (status === this.view.status && error === this.view.error) return;
    this.set({ status, error });
  }

  /** Keep-alives count as traffic (the server sends one when nothing changed). */
  alive(): void {
    this.view = { ...this.view, lastAt: this.now() };
  }
}

// ------------------------------------------------------------------ SSE connection

/** What the feed needs of an EventSource (the browser's, or a fake in tests). */
export interface SourceLike {
  addEventListener(type: string, fn: (ev: { data?: string; lastEventId?: string }) => void): void;
  close(): void;
  onerror: ((ev: unknown) => void) | null;
  onopen: ((ev: unknown) => void) | null;
}

export interface FeedOptions {
  url: string;
  open?: (url: string) => SourceLike;
  /** No message for this long: stale, reconnect (ms). */
  staleMs?: number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (t: unknown) => void;
}

/**
 * Connects the store to the stream; returns a stop function. A gap in the patch numbers or a
 * silent stream reconnects, and the server's first message on any connection is a snapshot.
 */
export function connectFeed(store: GlanceStore, o: FeedOptions): () => void {
  const open = o.open ?? ((u: string) => new EventSource(u) as unknown as SourceLike);
  const staleMs = o.staleMs ?? 15000;
  const setT = o.setTimer ?? ((fn, ms) => setInterval(fn, ms));
  const clearT = o.clearTimer ?? ((t) => clearInterval(t as ReturnType<typeof setInterval>));
  let src: SourceLike | null = null;
  let stopped = false;

  const connect = () => {
    if (stopped) return;
    src?.close();
    store.status("connecting");
    const s = open(o.url);
    src = s;
    s.onopen = () => undefined;
    s.onerror = () => {
      if (src === s) store.status("offline", "connection lost; retrying");
    };
    // Every frame's data is a FeedMessage; a message out of order (a missed seq) reconnects,
    // and the server answers a reconnect with a snapshot (or the replay since Last-Event-ID).
    const onMessage = (ev: { data?: string; lastEventId?: string }) => {
      if (src !== s) return;
      let m: FeedMessage;
      try {
        m = JSON.parse(ev.data ?? "null") as FeedMessage;
      } catch {
        return connect();
      }
      if (!m || typeof m !== "object") return;
      const seq = typeof m.seq === "number" ? m.seq : Number(ev.lastEventId);
      let ok = true;
      if (m.type === "snapshot") store.snapshot(m.state ?? {}, seq, m.ts);
      else if (m.type === "patch") ok = store.patch(m.patch, seq, m.ts);
      else if (m.type === "event") ok = store.event({ kind: m.event?.kind ?? "event", severity: m.event?.severity, text: m.event?.text, data: m.event?.data }, seq);
      if (!ok) connect();
    };
    for (const t of ["snapshot", "patch", "event"]) s.addEventListener(t, onMessage);
    s.addEventListener("ping", () => src === s && store.alive());
  };

  const watchdog = setT(() => {
    const v = store.current;
    if (v.status === "live" && store.now() - v.lastAt > staleMs) {
      store.status("stale", "no data for " + Math.round((store.now() - v.lastAt) / 1000) + " s; reconnecting");
      connect();
    }
  }, Math.max(1000, Math.floor(staleMs / 3)));

  connect();
  return () => {
    stopped = true;
    clearT(watchdog);
    src?.close();
    src = null;
  };
}
