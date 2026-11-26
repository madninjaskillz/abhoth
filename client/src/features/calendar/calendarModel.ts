// The Supervisor calendar's admin page model (SUPERVISOR.md 8.3): the API's shapes, and the pure
// helpers the page draws with (week grid placement, lanes for overlaps, times in the calendar's
// zone, the effect editor's rows). No DOM, so plain node tests it (test/calendarModel.test.ts).

export type Priority = "low" | "normal" | "high" | "critical";
export type KeyScope = "chat" | "use_reserved" | "lease" | "switch" | "card_state" | "hold_clocks" | "fan_profile" | "on_demand";

export const KEY_SCOPES: { id: KeyScope; label: string; hint: string }[] = [
  { id: "chat", label: "Chat", hint: "chat/images/music on the event's routes" },
  { id: "use_reserved", label: "Use reserved", hint: "placement on the reserved cards and lanes" },
  { id: "lease", label: "Lease", hint: "slot leases on the reserved cards" },
  { id: "switch", label: "Switch", hint: "layout switches, only to the listed layouts" },
  { id: "card_state", label: "Card state", hint: "online/offline for the reserved cards" },
  { id: "hold_clocks", label: "Hold clocks", hint: "clock holds until the occurrence ends" },
  { id: "fan_profile", label: "Fan profile", hint: "a configured fan profile (never below safety)" },
  { id: "on_demand", label: "On demand", hint: "on-demand switches within the event's cards" },
];

export interface KeyPolicy {
  scopes: KeyScope[];
  routes?: string[];
  layouts?: string[];
  all_layouts?: boolean;
  grace_before: string;
  grace_after: string;
  overrides_lower?: boolean;
}

export interface Detect {
  process?: { argv_any: string; label_arg?: string; label?: string; label_default?: string };
  lock_file?: string;
  json_file?: string;
  http?: string;
  json?: string;
  mode?: "extend" | "start" | "both";
  grace?: string;
}

export interface EntryConfig {
  title: string;
  owner?: string;
  at?: string;
  rrule?: string;
  start?: string;
  cron?: string;
  ics?: string;
  duration?: string;
  tz?: string;
  lead?: string;
  tail?: string;
  detect?: Detect;
  after?: string;
  effects?: unknown[];
  priority?: Priority;
  key?: KeyPolicy;
  enabled?: boolean;
  until?: string;
  exdates?: string[];
  source?: string;
}

export interface KeyRecord {
  id: number;
  event: string;
  prefix: string;
  owner: string;
  created: number;
  revoked?: number;
  retire_at?: number;
  last_used?: number;
}

export interface EntryView extends EntryConfig {
  id: string;
  schedule: string;
  effects_text: string[];
  origin: string;
  frequent: boolean;
  broken: boolean;
  keys: KeyRecord[];
}

export interface Run {
  entry: string;
  start: number;
  last_seen: number;
  ongoing: boolean;
  detail?: string;
}

export interface Occurrence {
  entry: string;
  id: string;
  title: string;
  priority: Priority;
  nominal: number;
  start: number;
  end: number;
  block_start: number;
  block_end: number;
  tail_end: number;
  unplanned: boolean;
  extended: boolean;
  run?: Run;
  skipped: boolean;
  ended_at?: number;
}

export interface Holder {
  entry: string;
  occurrence: string;
  priority: Priority;
  since: number;
  until: number;
  on_reserve: string;
  contenders: string[];
}

export interface NowState {
  at: number;
  active: Occurrence[];
  holders: Record<string, Holder>;
}

export interface Issue {
  path: string;
  message: string;
  severity: "error" | "warning";
}

export interface Overlap {
  a: string;
  b: string;
  card: string;
  at: number;
  until: number;
  equal_priority: boolean;
  message: string;
}

export interface CalendarDoc {
  rev: string;
  tz: string;
  server: string;
  entries: EntryView[];
  issues: Issue[];
  now: NowState;
  overlaps: Overlap[];
  at: number;
}

export interface Preview {
  id: string;
  issues: Issue[];
  next: Occurrence[];
  overlaps: Overlap[];
  effects: string[];
  schedule: string;
}

// ── time in the calendar's zone ───────────────────────────────────────────────────────────────

/** Wall-clock parts of an epoch in `tz`. */
export function localParts(epoch: number, tz: string): { y: number; m: number; d: number; h: number; mi: number; wd: number } {
  const f = new Intl.DateTimeFormat("en-GB", {
    timeZone: tz, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", weekday: "short", hourCycle: "h23",
  });
  const p: Record<string, string> = {};
  for (const x of f.formatToParts(new Date(epoch * 1000))) p[x.type] = x.value;
  const wd = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].indexOf(p.weekday);
  return { y: +p.year, m: +p.month, d: +p.day, h: +p.hour % 24, mi: +p.minute, wd };
}

export function hhmm(epoch: number, tz: string): string {
  const p = localParts(epoch, tz);
  return `${String(p.h).padStart(2, "0")}:${String(p.mi).padStart(2, "0")}`;
}

export function dayLabel(epoch: number, tz: string): string {
  return new Intl.DateTimeFormat("en-GB", { timeZone: tz, weekday: "short", day: "numeric", month: "short" }).format(new Date(epoch * 1000));
}

/** The epoch of local midnight on the day of `epoch` in `tz` (DST-safe: corrected until it lands). */
export function localMidnight(epoch: number, tz: string): number {
  const want = localParts(epoch, tz);
  let t = epoch - (want.h * 3600 + want.mi * 60) - (((epoch % 60) + 60) % 60);
  for (let i = 0; i < 3; i++) {
    const p = localParts(t, tz);
    const sod = p.h * 3600 + p.mi * 60;
    if (sod === 0 && p.d === want.d) return t;
    t = p.d === want.d ? t - sod : t + 86400 - sod;
  }
  return t;
}

/** The Monday 00:00 (local) of the week holding `epoch`, and the 8 day starts after it (DST-safe). */
export function weekDays(epoch: number, tz: string): number[] {
  const p = localParts(epoch, tz);
  let start = localMidnight(epoch, tz);
  for (let i = 0; i < p.wd; i++) start = localMidnight(start - 3600, tz);
  const days = [start];
  for (let i = 0; i < 7; i++) days.push(localMidnight(days[days.length - 1] + 26 * 3600, tz));
  return days;
}

// ── the week grid ─────────────────────────────────────────────────────────────────────────────

export interface Placed {
  occ: Occurrence;
  day: number;
  /** 0..1 of the day column */
  top: number;
  bottom: number;
  /** the planned core inside the block (lead and tail shown lighter) */
  coreTop: number;
  coreBottom: number;
  lane: number;
  lanes: number;
  /** continues from the day before / into the next day */
  clippedTop: boolean;
  clippedBottom: boolean;
}

/**
 * Occurrences placed in day columns (`days` = 8 day starts), split at midnight, with lanes so
 * overlapping blocks of one day sit side by side.
 */
export function placeWeek(occ: Occurrence[], days: number[]): Placed[] {
  const out: Placed[] = [];
  for (let d = 0; d < days.length - 1; d++) {
    const a = days[d];
    const b = days[d + 1];
    const len = b - a;
    const inDay = occ
      .filter((o) => o.block_end > a && o.block_start < b)
      .sort((x, y) => x.block_start - y.block_start || x.entry.localeCompare(y.entry));
    const lanesEnd: number[] = [];
    const placed: Placed[] = [];
    for (const o of inDay) {
      const s = Math.max(o.block_start, a);
      const e = Math.min(o.block_end, b);
      let lane = lanesEnd.findIndex((end) => end <= s);
      if (lane < 0) {
        lane = lanesEnd.length;
        lanesEnd.push(e);
      } else lanesEnd[lane] = e;
      const cs = Math.min(Math.max(o.start, a), b);
      const ce = Math.min(Math.max(o.end, a), b);
      placed.push({
        occ: o, day: d,
        top: (s - a) / len, bottom: (e - a) / len,
        coreTop: (cs - a) / len, coreBottom: (ce - a) / len,
        lane, lanes: 1,
        clippedTop: o.block_start < a, clippedBottom: o.block_end > b,
      });
    }
    // Lanes per overlapping cluster.
    let cluster: Placed[] = [];
    let clusterEnd = -1;
    const flush = () => {
      const n = Math.max(1, ...cluster.map((p) => p.lane + 1));
      for (const p of cluster) p.lanes = n;
      cluster = [];
    };
    for (const p of placed) {
      if (cluster.length && p.top >= clusterEnd) flush();
      cluster.push(p);
      clusterEnd = Math.max(clusterEnd, p.bottom);
    }
    flush();
    out.push(...placed);
  }
  return out;
}

/** A stable colour index per entry id (0..7). */
export function colourOf(entry: string): number {
  let h = 0;
  for (const c of entry) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 8;
}

export function phaseOf(o: Occurrence, now: number): "scheduled" | "lead" | "active" | "tail" | "extended" | "done" | "skipped" {
  if (o.skipped) return "skipped";
  if (now < o.block_start) return "scheduled";
  if (now >= o.block_end) return "done";
  if (now < o.start) return "lead";
  if (now < o.end) return "active";
  if (now < o.tail_end) return "tail";
  return "extended";
}

// ── the editor ────────────────────────────────────────────────────────────────────────────────

export type EffectKind =
  | "block_switch" | "block_on_demand" | "block_ensure" | "reserve_card" | "reserve_slots"
  | "hold_clocks" | "clock_profile" | "fan_profile" | "mute_alerts" | "info";

export interface EffectRow {
  kind: EffectKind;
  /** the target (`server:gpu`, `B`, `card:B`, `route:x`, `all`), the profile, or the rule selector */
  value: string;
  onReserve?: "wait" | "drain" | "preempt";
  n?: number;
  /** clock_profile's target / fan_profile's fan */
  extra?: string;
}

export const EFFECT_KINDS: { id: EffectKind; label: string; placeholder: string }[] = [
  { id: "block_switch", label: "Block switches", placeholder: "server:deepthought, card:B or all" },
  { id: "block_on_demand", label: "Block on-demand", placeholder: "server:…, card:…, route:… or all" },
  { id: "block_ensure", label: "Block engine starts", placeholder: "card:B" },
  { id: "reserve_card", label: "Reserve card", placeholder: "B" },
  { id: "reserve_slots", label: "Reserve slots", placeholder: "card:B" },
  { id: "hold_clocks", label: "Hold clocks", placeholder: "B" },
  { id: "clock_profile", label: "Clock profile", placeholder: "quiet" },
  { id: "fan_profile", label: "Fan profile", placeholder: "quiet" },
  { id: "mute_alerts", label: "Mute alerts", placeholder: "hot_card or hot_* (never critical)" },
  { id: "info", label: "Info only", placeholder: "" },
];

export function effectRow(v: unknown): EffectRow {
  if (v === "info" || v == null) return { kind: "info", value: "" };
  const o = v as Record<string, unknown>;
  const kind = (EFFECT_KINDS.find((k) => k.id in o)?.id ?? "info") as EffectKind;
  const row: EffectRow = { kind, value: typeof o[kind] === "string" ? (o[kind] as string) : "" };
  if (kind === "reserve_card") row.onReserve = (o.on_reserve as EffectRow["onReserve"]) ?? "wait";
  if (kind === "reserve_slots") row.n = typeof o.n === "number" ? o.n : 1;
  if (kind === "clock_profile") row.extra = (o.target as string) ?? (o.card as string) ?? "";
  if (kind === "fan_profile") row.extra = (o.fan as string) ?? "";
  return row;
}

export function effectValue(r: EffectRow): unknown {
  switch (r.kind) {
    case "info":
      return "info";
    case "reserve_card":
      return { reserve_card: r.value, on_reserve: r.onReserve ?? "wait" };
    case "reserve_slots":
      return { reserve_slots: r.value, n: r.n ?? 1 };
    case "clock_profile":
      return r.extra ? { clock_profile: r.value, target: r.extra } : { clock_profile: r.value };
    case "fan_profile":
      return r.extra ? { fan_profile: r.value, fan: r.extra } : { fan_profile: r.value };
    default:
      return { [r.kind]: r.value };
  }
}

export type WhenKind = "at" | "rrule" | "cron";

export function whenKind(e: EntryConfig): WhenKind {
  return e.cron ? "cron" : e.rrule || e.after ? "rrule" : "at";
}

/** Strip empty optional fields so the saved TOML stays tidy. */
export function tidy(e: EntryConfig): EntryConfig {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(e)) {
    if (v === undefined || v === null || v === "") continue;
    if (Array.isArray(v) && v.length === 0 && k !== "effects") continue;
    out[k] = v;
  }
  return out as unknown as EntryConfig;
}

export function keyState(k: KeyRecord, now: number): "live" | "retiring" | "revoked" | "retired" {
  if (k.revoked) return "revoked";
  if (k.retire_at && k.retire_at <= now) return "retired";
  if (k.retire_at) return "retiring";
  return "live";
}
