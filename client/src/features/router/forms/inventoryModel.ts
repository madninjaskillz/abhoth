// The router inventory (D152) as data for the forms: servers -> cards -> engines allowed per
// model type, the engine catalog and capabilities, models with their computed engine list, and
// Hugging Face results. Pure functions only (tested with plain node): the edits each form sends.
import type { FormEdit } from "./formModel";

export type ModelKind = "llm" | "tts" | "stt" | "image" | "music" | "embedding";

export interface KindInfo { id: ModelKind; label: string }

export interface CatalogKind {
  id: string;
  title: string;
  kinds: ModelKind[];
  formats: string[];
  archs: string[];
  quants: string[];
  probe: string;
  driver: string;
  notes: string;
}

export interface InvCard {
  id: string;
  label?: string | null;
  gpuUuid: string;
  vramGib?: number | null;
  reserveGib: number;
  managed: boolean;
  onDemand: boolean;
  /** Set explicitly (else the legacy rule: LLMs on the engines with a port here). */
  explicit: boolean;
  engines: Record<string, string[]>;
}

export interface InvServer {
  id: string;
  title?: string | null;
  host?: string | null;
  exec: "local" | "ssh";
  modelDir?: string | null;
  cards: InvCard[];
}

export interface Caps {
  kinds: ModelKind[];
  formats: string[];
  archs: string[];
  quants: string[];
  maxContext?: number;
  base?: string;
  sources: string[];
  version?: string;
}

export interface Probe { stamp: string; version?: string; archsSeen?: string[]; kindsSeen?: ModelKind[]; ctx?: number; at: number }

export interface InvEngine {
  id: string;
  title?: string | null;
  kinds: ModelKind[];
  driver: string;
  image?: string | null;
  cards: string[];
  profiles: { id: string; title?: string | null; lanes?: number | null; ctx?: number | null; vramGib?: number | null }[];
  caps: Caps;
  overrides?: Record<string, unknown> | null;
  probe?: Probe | null;
  probeHow: string;
  usedBy: string[];
}

export interface EngineMatch { engine: string; ok: boolean; reason: string; via: string; fileOf?: string }

export interface CardOption {
  card: string;
  server: string;
  engine: string;
  profile: string;
  rank: number;
  ok: boolean;
  needGib?: number;
  roomGib?: number;
  restore?: boolean;
  preset?: string;
  /** Why not (absent when there is nothing to say). */
  why?: string[];
}

export interface InvModel {
  id: string;
  title?: string | null;
  description?: string | null;
  kind: ModelKind;
  vision?: boolean | null;
  primary?: string | null;
  primaryChoice?: string | null;
  prefer: string[];
  allow: string[];
  deny: string[];
  format?: string | null;
  arch?: string | null;
  quant?: string | null;
  order: string[];
  supported: EngineMatch[];
  files: { key: string; file?: string | null; servedId?: string | null; vramGib?: number | null; format?: string | null; arch?: string | null; quant?: string | null; restore: boolean }[];
  servers?: string[] | null;
  context?: number | null;
  sizeGib?: number | null;
  licence?: string | null;
  source?: { hf?: string | null; revision?: string | null; files?: string[]; gated?: boolean } | null;
  routes: { name: string; onDemand: boolean; auto: boolean; pin?: { engine: string; profile?: string } | null }[];
  placement: CardOption[];
}

export interface Inventory {
  migrated: boolean;
  restartNeeded?: boolean;
  hf?: { set: boolean; from: string };
  catalog: CatalogKind[];
  kinds: KindInfo[];
  servers: InvServer[];
  engines: InvEngine[];
  models: InvModel[];
}

export interface DetectedGpu { index: number; uuid: string; name: string; vramGib?: number | null; card?: string | null }

export interface HfRow { repo: string; kind?: ModelKind | null; pipeline?: string | null; downloads: number; likes: number; gated: boolean; licence?: string | null; gguf: boolean; updated?: string | null }

export interface HfFile { path: string; size: number; sha256?: string | null }
export interface HfChoice { id: string; label: string; format: string; quant?: string | null; bytes: number; sizeGib: number; needGib: number; files: HfFile[] }
export interface CardFit { card: string; server: string; label?: string | null; roomGib?: number | null; fits?: boolean | null }

export interface HfDetails {
  repo: string;
  revision?: string | null;
  kind?: ModelKind | null;
  pipeline?: string | null;
  arch?: string | null;
  context?: number | null;
  vision: boolean;
  licence?: string | null;
  gated: boolean;
  gatedHow?: string | null;
  title: string;
  choices: HfChoice[];
  fit?: Record<string, CardFit[]>;
}

export interface DownloadJob {
  id: string;
  server: string;
  repo: string;
  revision: string;
  model?: string;
  dir: string;
  files: { path: string; size: number; dest: string; bytes: number; done: boolean }[];
  state: "queued" | "running" | "done" | "failed" | "cancelled";
  error?: string;
  total: number;
  bytes: number;
  started: number;
  finished?: number;
  by: string;
}

export const KIND_LABELS: Record<ModelKind, string> = {
  llm: "Language models",
  tts: "Text to speech",
  stt: "Speech to text",
  image: "Images",
  music: "Music",
  embedding: "Embeddings",
};

/** Engines that can run a type at all (by their capabilities). */
export function enginesForKind(inv: Inventory, kind: ModelKind): InvEngine[] {
  return inv.engines.filter((e) => e.caps.kinds.includes(kind));
}

/** A card's engines for one type, after ticking or unticking one. */
export function toggled(list: string[], engine: string, on: boolean): string[] {
  const s = list.filter((x) => x !== engine && x !== "*");
  return on ? [...s, engine] : s;
}

/** The edit that sets `cards.<id>.engines` (all types at once: the table is one value). */
export function cardEnginesEdit(card: string, engines: Record<string, string[]>): FormEdit {
  const clean: Record<string, string[]> = {};
  for (const [k, v] of Object.entries(engines)) if (v.length || k === "llm") clean[k] = [...v];
  return { path: ["cards", card, "engines"], value: clean };
}

/** A new card from a detected GPU (`A`, `B`, … the first free letter). */
export function nextCardId(taken: string[]): string {
  for (let i = 0; i < 26; i++) {
    const id = String.fromCharCode(65 + i);
    if (!taken.includes(id)) return id;
  }
  return `card${taken.length + 1}`;
}

export function newCardEdits(id: string, server: string, gpu: DetectedGpu, engines: Record<string, string[]> = { llm: [] }): FormEdit[] {
  const v: Record<string, unknown> = { server, gpu_uuid: gpu.uuid, label: gpu.name, managed: true, on_demand: true, engines };
  if (gpu.vramGib) v.vram_gib = gpu.vramGib;
  return [{ path: ["cards", id], value: v as FormEdit["value"] }];
}

/** A new server (step one of the guided flow). */
export function newServerEdits(id: string, f: { title: string; host: string; exec: "ssh" | "local"; user: string; key: string; hostKey: string; modelDir: string }): FormEdit[] {
  const v: Record<string, unknown> = { title: f.title || id, exec: f.exec };
  if (f.exec === "ssh") {
    v.host = f.host;
    v.ssh = { user: f.user || "abhoth-exec", key: f.key, host_key: f.hostKey };
  }
  if (f.modelDir) v.model_dir = f.modelDir;
  return [{ path: ["servers", id], value: v as FormEdit["value"] }];
}

export function validateServerId(id: string, taken: string[]): string | null {
  if (!/^[a-z0-9][a-z0-9_-]{0,31}$/.test(id)) return "lower-case letters, digits, - and _ (up to 32)";
  if (taken.includes(id)) return "there is a server with that name";
  return null;
}

export function validModelDir(p: string): string | null {
  if (!p) return null;
  if (!p.startsWith("/")) return "an absolute path on the server, e.g. /srv/models";
  if (p.split("/").includes("..")) return "no .. in the path";
  return null;
}

/** The model's engine choices: primary, preference order, and allow/deny exceptions. */
export function modelEngineEdits(model: string, f: { primary: string; prefer: string[]; allow: string[]; deny: string[] }): FormEdit[] {
  const n = (v: string[]) => (v.length ? v : null);
  return [
    { path: ["models", model, "primary"], value: f.primary || null },
    { path: ["models", model, "prefer"], value: n(f.prefer.filter((x) => x !== f.primary)) },
    { path: ["models", model, "allow"], value: n(f.allow) },
    { path: ["models", model, "deny"], value: n(f.deny) },
  ];
}

/** Move an engine up or down a preference list. */
export function moved(list: string[], engine: string, by: -1 | 1): string[] {
  const i = list.indexOf(engine);
  const j = i + by;
  if (i < 0 || j < 0 || j >= list.length) return list;
  const out = [...list];
  [out[i], out[j]] = [out[j], out[i]];
  return out;
}

/** A catalog-friendly model id from a repo (`Qwen/Qwen3-8B-GGUF` -> `qwen3-8b`). */
export function modelIdFromRepo(repo: string, taken: string[] = []): string {
  const base = (repo.split("/").pop() ?? repo)
    .toLowerCase()
    .replace(/[-_.](gguf|exl2|awq|gptq|safetensors|mlx)$/i, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40) || "model";
  let id = base;
  for (let i = 2; taken.includes(id); i++) id = `${base}-${i}`;
  return id;
}

/** The engines a new model's file could run on (by the catalog: format, type, arch). */
export function enginesFor(inv: Inventory, kind: ModelKind, format: string, arch?: string | null): InvEngine[] {
  const a = (arch ?? "").toLowerCase().replace(/[^a-z0-9]/g, "");
  return inv.engines.filter((e) => {
    if (!e.caps.kinds.includes(kind)) return false;
    if (e.caps.formats.length && !e.caps.formats.includes(format)) return false;
    if (a && e.caps.archs.length && !e.caps.archs.some((x) => x.toLowerCase().replace(/[^a-z0-9]/g, "") === a)) return false;
    return true;
  });
}

/** The model a Hugging Face choice becomes (catalog entry + its file for the chosen engine). */
/**
 * The file an engine loads from a choice: the first shard of a GGUF quant, the one weight file,
 * or (several safetensors shards and the like) null for the folder itself.
 */
export function mainFile(c: HfChoice): string | null {
  const ext = c.format === "pytorch" ? "bin" : c.format;
  const weights = c.files
    .filter((f) => !f.path.toLowerCase().includes("mmproj") && f.path.toLowerCase().endsWith(`.${ext}`))
    .map((f) => f.path)
    .sort();
  if (c.format === "gguf" && weights.length) return weights[0];
  if (weights.length === 1) return weights[0];
  if (weights.length > 1) return null;
  const big = [...c.files].sort((a, b) => b.size - a.size)[0];
  return c.files.length === 1 && big ? big.path : null;
}

/** `dir` is where the files land on the server (`<model folder>/<owner>/<name>`). */
export function newModelEdits(id: string, d: HfDetails, c: HfChoice, engine: string, servers: string[], dir: string): FormEdit[] {
  const mmproj = c.files.find((f) => f.path.toLowerCase().includes("mmproj"));
  const main = mainFile(c);
  const v: Record<string, unknown> = {
    title: d.title,
    kind: d.kind ?? "llm",
    format: c.format,
    primary: engine,
    servers,
    size_gib: c.sizeGib,
    source: { hf: d.repo, ...(d.revision ? { revision: d.revision } : {}), files: c.files.map((f) => f.path), ...(d.gated ? { gated: true } : {}) },
  };
  if (d.arch) v.arch = d.arch;
  if (c.quant) v.quant = c.quant;
  if (d.context) v.context = d.context;
  if (d.vision) v.vision = true;
  if (d.licence) v.licence = d.licence;
  const base = dir.replace(/\/+$/, "");
  const art: Record<string, unknown> = { file: main ? `${base}/${main}` : base, vram_gib: c.needGib };
  if (mmproj) art.options = { mmproj: `${base}/${mmproj.path}` };
  v.artifacts = { [engine]: art };
  return [{ path: ["models", id], value: v as FormEdit["value"] }];
}

/** Where a repo lands on a server (as the router computes it). */
export function repoDir(modelDir: string, repo: string): string {
  return `${modelDir.replace(/\/+$/, "")}/${repo}`;
}

export function fmtGib(n?: number | null): string {
  if (n == null) return "?";
  if (n > 0 && n < 1) return `${Math.round(n * 1024)} MiB`;
  return n >= 100 ? `${Math.round(n)} GiB` : `${n.toFixed(1)} GiB`;
}

export function pct(bytes: number, total: number): number {
  if (!total) return 0;
  return Math.max(0, Math.min(100, Math.round((bytes / total) * 100)));
}

/** "fits on A, B" / "too big for every card" for a choice. */
export function fitSummary(fits: CardFit[] | undefined): { ok: boolean; text: string } {
  if (!fits || fits.length === 0) return { ok: false, text: "no managed cards" };
  const ok = fits.filter((f) => f.fits);
  if (ok.length === 0) return { ok: false, text: "too big for every card" };
  return { ok: true, text: `fits on ${ok.map((f) => f.card).join(", ")}` };
}

/** How the router picked a card for a model, in a line. */
export function placementLine(m: InvModel): string {
  const ok = m.placement.filter((p) => p.ok);
  if (ok.length === 0 && Array.isArray(m.servers) && m.servers.length === 0) return "files not on a server yet";
  if (ok.length === 0) return m.placement.length ? "nowhere to run it yet" : "no cards";
  const best = ok[0];
  return `${best.engine}${best.profile ? ` (${best.profile})` : ""} on ${ok.filter((p) => p.rank === best.rank).map((p) => p.card).join(", ")}`;
}

/** Where a step of the guided setup stands: what to do next. */
export function nextStep(inv: Inventory): "server" | "cards" | "engines" | "models" | null {
  if (inv.servers.length === 0) return "server";
  if (inv.servers.every((s) => s.cards.length === 0)) return "cards";
  if (inv.engines.length === 0) return "engines";
  if (inv.models.length === 0) return "models";
  return null;
}

// ── folder browsing (the server's filesystem, through the router) ─────────────────────────────

export interface DirEntry { name: string; path: string; dir: boolean; link: boolean; outside: boolean; size?: number | null; model: boolean }
export interface DirListing {
  path: string;
  real: string;
  parent?: string | null;
  entries: DirEntry[];
  offset: number;
  total: number;
  truncated: boolean;
  more: boolean;
  free?: number | null;
  writable: boolean;
  roots: string[];
}
export interface PathStat {
  path: string;
  allowed: boolean;
  exists: boolean;
  dir?: boolean;
  size?: number | null;
  writable?: boolean;
  free?: number | null;
  parentExists?: string | null;
  parentWritable?: boolean;
}

/** The client's copy of the router's path rule: absolute, no `..`, normalised. */
export function cleanPath(p: string): { ok: true; path: string } | { ok: false; error: string } {
  const t = p.trim();
  if (!t) return { ok: false, error: "a path is needed" };
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(t)) return { ok: false, error: "not a plain path" };
  if (!t.startsWith("/")) return { ok: false, error: "an absolute path, starting with /" };
  if (t.includes("\\")) return { ok: false, error: "no backslashes" };
  const parts = t.split("/").filter((x) => x && x !== ".");
  if (parts.includes("..")) return { ok: false, error: "no .. in a path" };
  return { ok: true, path: `/${parts.join("/")}` };
}

/** Breadcrumbs for a clean path: `/`, `/srv`, `/srv/models`. */
export function crumbs(path: string): { label: string; path: string }[] {
  const parts = path.split("/").filter(Boolean);
  const out = [{ label: "/", path: "/" }];
  parts.forEach((p, i) => out.push({ label: p, path: `/${parts.slice(0, i + 1).join("/")}` }));
  return out;
}

/** A new folder's name: one plain component. */
export function validFolderName(n: string): string | null {
  const t = n.trim();
  if (!t) return "a name is needed";
  if (t.includes("/") || t.includes("\\")) return "one folder at a time (no /)";
  if (t === "." || t === ".." || t.startsWith(".")) return "not starting with a dot";
  if (t.length > 255) return "too long";
  return null;
}

export function joinPath(dir: string, name: string): string {
  return `${dir.replace(/\/+$/, "")}/${name.trim()}`;
}

export function fmtBytes(n?: number | null): string {
  if (n == null) return "";
  if (n < 1024) return `${n} B`;
  const u = ["KiB", "MiB", "GiB", "TiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}

/** What a typed path's check says, in a line (and whether "use it" is fine). */
export function statLine(s: PathStat, mode: "folder" | "file"): { ok: boolean; text: string } {
  if (!s.allowed) return { ok: false, text: "outside the folders this server allows" };
  if (!s.exists) {
    if (mode === "file") return { ok: false, text: "no such file" };
    return s.parentWritable ? { ok: true, text: "does not exist yet (it can be created)" } : { ok: false, text: "does not exist, and its parent is not writable" };
  }
  if (mode === "folder" && !s.dir) return { ok: false, text: "a file, not a folder" };
  if (mode === "file" && s.dir) return { ok: false, text: "a folder, not a file" };
  if (mode === "folder" && !s.writable) return { ok: true, text: "exists, read only for the engine user" };
  return { ok: true, text: `${mode === "file" ? `file${s.size != null ? `, ${fmtBytes(s.size)}` : ""}` : "folder, writable"}${s.free != null ? ` · ${fmtBytes(s.free)} free` : ""}` };
}
