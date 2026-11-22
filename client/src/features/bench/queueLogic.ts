// Setups: per card ENGINE → ENGINE PROFILE → MODEL (only compatible ones), a MODE for the whole setup; saved
// profiles are named setups. The bench queue's "All" expansion and result matching. Pure (type imports only), so
// plain node tests it (test/queue.test.ts); the profile builder (Profiles.tsx) uses the same rules.
import type { BenchResult, BenchSuite, CardPick, ModelArtifact, ProfileList } from "../../lib/api";

export const ALL = "*";
export const OFF = "off";
export const MODES = ["auto", "hands-on", "orchestrator", "peer"];
export const CARDS = ["A", "B"] as const;
/** Card VRAM (GiB) and what stays free; the check is the server's rough estimate, mirrored. */
const CARD_VRAM = 32;
const RESERVE = 1.5;

export type Pick = { engine: string; engineProfile: string; model: string };
/** A dimension filter: a value, or ALL. engine may also be OFF (the card is off). */
export interface CardFilter {
  engine: string;
  engineProfile: string;
  model: string;
}

export interface Selection {
  suite: string;
  /** "saved": pick saved profiles; "build": per-dimension pickers. */
  source: "saved" | "build";
  profile: string;
  A: CardFilter;
  B: CardFilter;
  /** Build: card B has its own pickers (else it runs the same as A). */
  perCard: boolean;
  /** A mode, ALL, or "" = each profile's own default. */
  mode: string;
}

export interface Setup {
  suite: string;
  profile?: string | null;
  cards?: Record<string, CardPick> | null;
  mode: string | null;
}

export const MAX_ADD = 500;

export function artifactOf(d: ProfileList | undefined, model: string, engine: string): ModelArtifact | null {
  const a = d?.models?.[model]?.artifacts?.[engine];
  if (a == null) return null;
  return typeof a === "string" ? { file: a } : a;
}

/** The engine profiles a model may use on an engine; null = any of that engine's (no list, or an empty one, as the server reads it). */
export function allowedEngineProfiles(d: ProfileList | undefined, model: string, engine: string): string[] | null {
  const only = artifactOf(d, model, engine)?.engineProfiles;
  return Array.isArray(only) && only.length ? only : null;
}

/** Why a pick can't run on a card (null = fine). */
export function pickProblem(d: ProfileList | undefined, card: string, p: Pick): string | null {
  const ep = d?.engineProfiles?.[p.engineProfile];
  const m = d?.models?.[p.model];
  if (!d?.engines?.[p.engine]) return `card ${card}: unknown engine ${p.engine}`;
  if (!ep) return `card ${card}: unknown engine profile ${p.engineProfile}`;
  if (ep.engine !== p.engine) return `card ${card}: ${p.engineProfile} is for ${ep.engine}, not ${p.engine}`;
  if (!m) return `card ${card}: unknown model ${p.model}`;
  const art = artifactOf(d, p.model, p.engine);
  if (!art) return `card ${card}: ${p.model} has no ${p.engine} build (runs on: ${Object.keys(m.artifacts ?? {}).join(", ") || "nothing"})`;
  const only = allowedEngineProfiles(d, p.model, p.engine);
  if (only && !only.includes(p.engineProfile)) return `card ${card}: ${p.model} runs on ${p.engine} only with ${only.join(", ")}, not ${p.engineProfile}`;
  const need = (art.vramGiB ?? m.vramGiB ?? 0) + (ep.vramGiB ?? 0);
  if (need > CARD_VRAM - RESERVE) return `card ${card}: needs about ${need.toFixed(1)} GiB by our rough estimate, more than the card has to spare`;
  return null;
}

/** Every valid pick a card filter allows (ALL = any); [] when engine is OFF. */
export function picksFor(d: ProfileList | undefined, card: string, f: CardFilter): Pick[] {
  if (f.engine === OFF) return [];
  const out: Pick[] = [];
  for (const [epName, ep] of Object.entries(d?.engineProfiles ?? {})) {
    if (f.engine !== ALL && ep.engine !== f.engine) continue;
    if (f.engineProfile !== ALL && epName !== f.engineProfile) continue;
    for (const model of Object.keys(d?.models ?? {})) {
      if (f.model !== ALL && model !== f.model) continue;
      const p = { engine: ep.engine, engineProfile: epName, model };
      if (!pickProblem(d, card, p)) out.push(p);
    }
  }
  return out;
}

const anyOf = (v: string) => (!v || v === ALL ? ALL : v);

/** Engine profiles of the engine (ALL = any engine) that some allowed model can use (just `model` when one is picked; "" or ALL = any). */
export function engineProfileOptions(d: ProfileList | undefined, card: string, engine: string, model: string): string[] {
  if (!engine || engine === OFF) return [];
  return [...new Set(picksFor(d, card, { engine, engineProfile: ALL, model: anyOf(model) }).map((p) => p.engineProfile))];
}

/** Models that can run on the engine (on `engineProfile` when one is picked; "" or ALL = any). */
export function modelOptions(d: ProfileList | undefined, card: string, engine: string, engineProfile: string): string[] {
  if (!engine || engine === OFF) return [];
  if (engine === ALL) return Object.keys(d?.models ?? {});
  return [...new Set(picksFor(d, card, { engine, engineProfile: anyOf(engineProfile), model: ALL }).map((p) => p.model))];
}

/** Models with a build for the engine that are hidden under this engine profile, each with the reason. */
export function hiddenModels(d: ProfileList | undefined, card: string, engine: string, engineProfile: string): { model: string; reason: string }[] {
  if (!engine || engine === ALL || engine === OFF || !engineProfile || engineProfile === ALL) return [];
  const out: { model: string; reason: string }[] = [];
  for (const model of Object.keys(d?.models ?? {})) {
    if (!artifactOf(d, model, engine)) continue;
    const why = pickProblem(d, card, { engine, engineProfile, model });
    if (why) out.push({ model, reason: why.replace(/^card \w+: /, "") });
  }
  return out;
}

/** A profile's cards as {A, B} picks (null = off). */
export function cardsOf(d: ProfileList | undefined, profile: string | null | undefined): Record<string, Pick | null> | null {
  const cards = profile ? d?.profiles?.[profile]?.cards : null;
  if (!cards) return null;
  const norm = (v: CardPick | null | undefined): Pick | null => (!v || v === OFF || v === "same" ? null : v);
  const a = norm(cards.A);
  return { A: a, B: cards.B === "same" ? a : norm(cards.B) };
}

function filterMatches(f: CardFilter, p: Pick | null): boolean {
  if (f.engine === OFF) return p === null;
  if (!p) return false;
  return (f.engine === ALL || f.engine === p.engine) && (f.engineProfile === ALL || f.engineProfile === p.engineProfile) && (f.model === ALL || f.model === p.model);
}

const modesOf = (mode: string) => (mode === ALL ? MODES : [mode || null]);

/** Every valid setup the selection names ("All" expands; bridges get no mode). */
export function expand(sel: Selection, suites: BenchSuite[], d: ProfileList | undefined): Setup[] {
  const suiteNames = (sel.suite === ALL ? suites.filter((s) => !s.targets?.length).map((s) => s.name) : [sel.suite]).filter(Boolean);
  const out: Setup[] = [];
  if (sel.source === "saved") {
    const profiles = d?.profiles ?? {};
    const names = sel.profile === ALL ? Object.keys(profiles).filter((p) => !profiles[p].hidden) : [sel.profile];
    for (const suite of suiteNames)
      for (const p of names) {
        if (p && !profiles[p]) continue;
        if (p && profiles[p].bridge) {
          out.push({ suite, profile: p, mode: null });
          continue;
        }
        for (const m of modesOf(sel.mode)) out.push({ suite, profile: p || null, mode: m });
      }
    return out;
  }
  const aPicks: (Pick | null)[] = sel.A.engine === OFF ? [null] : picksFor(d, "A", sel.A);
  const combos: Record<string, CardPick>[] = [];
  for (const a of aPicks) {
    if (!sel.perCard) {
      if (a) combos.push({ A: a, B: "same" });
      continue;
    }
    const bPicks: (Pick | null)[] = sel.B.engine === OFF ? [null] : picksFor(d, "B", sel.B);
    for (const b of bPicks) if (a || b) combos.push({ A: a ?? OFF, B: b ?? OFF });
  }
  for (const suite of suiteNames) for (const cards of combos) for (const m of modesOf(sel.mode)) out.push({ suite, cards, mode: m });
  return out;
}

/** A result's mode: peer when its settings turned peer mode on, else its lead mode. */
export function modeOfResult(r: BenchResult): string {
  if (r.sets?.some((s) => s.replace(/\s/g, "").toLowerCase() === "agent.peermode=true")) return "peer";
  return r.lead_mode || "auto";
}

/** A result's profile under its current name (old names go through the aliases). */
export function profileOfResult(r: BenchResult, d: ProfileList | undefined): string | null {
  let p = r.profile ?? null;
  for (let i = 0; p && i < 4 && d?.aliases?.[p]; i++) p = d.aliases[p];
  return p;
}

/** Whether a result belongs to the selection (ALL matches anything). */
export function matches(r: BenchResult, sel: Selection, d: ProfileList | undefined): boolean {
  if (sel.suite !== ALL && sel.suite && r.suite !== sel.suite) return false;
  const p = profileOfResult(r, d);
  if (sel.source === "saved") {
    if (sel.profile !== ALL && (sel.profile || null) !== p) return false;
  } else {
    const cards = cardsOf(d, p);
    if (!cards) return false;
    if (!filterMatches(sel.A, cards.A)) return false;
    if (sel.perCard ? !filterMatches(sel.B, cards.B) : JSON.stringify(cards.B) !== JSON.stringify(cards.A)) return false;
  }
  if (sel.mode !== ALL && sel.mode && d?.profiles?.[p ?? ""]?.bridge !== true && modeOfResult(r) !== sel.mode) return false;
  return true;
}
