// The Engine store and the Create package wizard as data (section 10 and 13.3 of the router media
// plan): the engine package manifest (abhoth-engine.json, schema 1) mirrored from
// stores/schema/abhoth-engine.schema.json, the router's engine-store API shapes, and the pure
// helpers the screens use (filtering, fit summaries, the approvals an install needs, and the
// wizard's form <-> manifest conversion). Plain node tests, no DOM.

// ── the manifest ─────────────────────────────────────────────────────────────────────────────

export const KINDS = ["llm", "tts", "stt", "image", "music", "embedding", "video"] as const;
export const GPU_KINDS = ["cuda", "rocm", "metal", "cpu"] as const;
export const METHOD_TYPES = ["docker", "compose", "pip", "uv", "conda", "binary", "git", "existing"] as const;
/** API types a package may declare (orca-engines APIS; others are a warning, not an error). */
export const APIS = [
  "openai-chat", "openai-responses", "anthropic-messages", "openai-speech", "openai-transcriptions", "openai-images", "openai-embeddings", "comfyui", "wyoming", "sa3", "http",
] as const;
export const PROBES = ["props", "models", "comfyui", "sa3", "wyoming", "http", "none"] as const;
export const LAYOUTS = ["flat", "hf", "comfyui", "repo"] as const;
export const PARAM_TYPES = ["string", "int", "float", "bool", "enum", "path"] as const;
export const UNPACK = ["zip", "tar.gz", "tar.xz", "none"] as const;
/** Methods that run code on the host (not sandboxed): they always need an explicit approval. */
export const HOST_RUN_TYPES = ["pip", "uv", "conda", "binary", "git", "existing"];

export type GpuKind = (typeof GPU_KINDS)[number];
export type MethodType = (typeof METHOD_TYPES)[number];

export interface CapSet { kinds?: string[]; formats?: string[]; archs?: string[]; quants?: string[] }
export interface ManifestCaps extends CapSet { inherits?: string; add?: CapSet; remove?: CapSet; max_context?: number }
export interface Hardware { gpu?: GpuKind[]; min_vram_gib?: number; cpu_ok?: boolean; min_ram_gib?: number; min_cores?: number; simd?: string[] }
export interface Param {
  type: (typeof PARAM_TYPES)[number];
  default?: unknown;
  description?: string;
  per_model?: boolean;
  required?: boolean;
  secret?: boolean;
  choices?: string[];
  min?: number;
  max?: number;
}
export interface ModelsLayout { mount?: string; layout: (typeof LAYOUTS)[number]; folders?: string[]; env?: string; read_only?: boolean }
export interface PortDecl { id: string; container?: number; protocol?: "http" | "tcp" }
export interface Health { http?: string; tcp?: boolean; start_seconds?: number }
export interface Mount { source: string; target: string; read_only?: boolean }
export interface Step { run: string[]; cwd?: string; env?: Record<string, string>; platforms?: string[] }
export interface Asset { platform: string; gpu?: GpuKind[]; url: string; sha256: string; unpack?: (typeof UNPACK)[number]; strip?: number; size?: number }
export interface Detect { command?: string; paths?: string[]; version_args?: string[]; version_regex?: string; image?: string; url?: string }
export interface Method {
  type: MethodType;
  label?: string;
  platforms?: string[];
  gpu?: GpuKind[];
  requires?: { commands?: string[]; simd?: string[]; min_vram_gib?: number; min_ram_gib?: number };
  run?: string[];
  env?: Record<string, string>;
  workdir?: string;
  ports?: PortDecl[];
  health?: Health;
  mounts?: Mount[];
  update?: Step[];
  uninstall?: Step[];
  image?: string;
  entrypoint?: string;
  docker_args?: string[];
  compose?: string;
  compose_file?: string;
  python?: string;
  package?: string;
  requirements?: string[];
  requirements_file?: string;
  extra_index_urls?: string[];
  environment_file?: string;
  files?: string[];
  assets?: Asset[];
  repo?: string;
  commit?: string;
  build?: Step[];
  detect?: Detect;
}
export interface Security {
  privileged?: boolean;
  host_network?: boolean;
  host_mounts?: { source: string; target: string; read_only?: boolean; why: string }[];
  devices?: string[];
  cap_add?: string[];
}
export interface Manifest {
  $schema?: string;
  schema: 1;
  id: string;
  name: string;
  version: string;
  description?: string;
  kinds: string[];
  caps?: ManifestCaps;
  hardware?: Hardware;
  params?: Record<string, Param>;
  api: string;
  also_speaks?: string[];
  probe?: (typeof PROBES)[number];
  endpoints?: Record<string, string>;
  models?: ModelsLayout;
  methods: Method[];
  security?: Security;
  licence?: string;
  source?: string;
  homepage?: string;
  notes?: string[];
  private?: boolean;
}

// ── the API ──────────────────────────────────────────────────────────────────────────────────

export interface StoreInfo { id: string; name: string; url: string | null; trusted: boolean; builtin: boolean; packages: number; error?: string }
export interface MethodSummary { type: string; label: string; platforms: string[]; gpu: string[] }
export interface PackageSummary {
  store: string;
  trusted: boolean;
  id: string;
  name: string;
  version: string;
  kinds: string[];
  summary: string;
  api: string;
  licence: string;
  source: string;
  private: boolean;
  methods: MethodSummary[];
  needsApproval: string[];
  installed: { server: string; version: string }[];
  /** A newer version in the store, when there is one. */
  update: string | null;
}
export interface Issue { level: "error" | "warning"; at: string; message: string }
export interface Report { ok: boolean; mode: string; manifest?: Manifest | null; issues: Issue[]; files: { path: string; size: number }[]; total_bytes: number }
export interface PackageDetail { package: { store: string; trusted: boolean; manifest: Manifest; files: Record<string, string> }; report: Report }
export interface Rejection { method: number; label: string; reasons: string[] }
export interface Fit {
  ok: boolean;
  method: number | null;
  method_label: string | null;
  method_type: string | null;
  rejections: Rejection[];
  warnings: string[];
  approvals: string[];
}
export interface CardFit { card: string; device: "gpu" | "cpu"; label: string; vramGib?: number | null; fit: Fit }
export type HostFacts = Record<string, unknown>;
export interface FitAnswer { server: string; host: HostFacts; cards: CardFit[] }
export interface Approvals { host_run: boolean; privileged: boolean; host_network: boolean; host_mounts: string[]; devices: string[]; cap_add: string[]; untrusted: boolean }
export interface StoreJob {
  id: string;
  op: "install" | "update" | "remove";
  package: string;
  server: string;
  card: string;
  engine: string;
  state: "running" | "done" | "failed";
  log: string[];
  error?: string;
  started: number;
  finished?: number;
}
export interface InstalledPkg { server: string; id: string; version: string; method: number; method_type: string; pins: string[]; at: number; store: string; engines: string[] }
export type DraftSource = "scratch" | "docker" | "compose" | "pip" | "release" | "engine";
/** Registry metadata for an image (read only, nothing runs). The router decides the keys. */
export interface ImageProbe {
  image?: string;
  digest?: string;
  ports?: unknown;
  env?: unknown;
  labels?: Record<string, string>;
  licence?: string | null;
  [k: string]: unknown;
}
export interface Draft { manifest: Manifest; files: Record<string, string>; notes: string[]; probe?: ImageProbe | null }
export interface TestRun { ok: boolean; probes: { path: string; status: number | string; snippet: string }[]; api_guess: string | null; health_guess: string | null; logs: string }

// ── the store list ───────────────────────────────────────────────────────────────────────────

/** Packages matching a kind ("" for all) and a free-text search over id, name, summary, kinds and store. */
export function filterPackages(pkgs: PackageSummary[], kind: string, text: string): PackageSummary[] {
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  return pkgs.filter((p) => {
    if (kind && !p.kinds.includes(kind)) return false;
    if (words.length === 0) return true;
    const hay = [p.id, p.name, p.summary, p.store, p.api, ...p.kinds, ...p.methods.map((m) => m.type)].join(" ").toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

/** The method types a package offers, once each, in order. */
export function methodTypes(p: { methods: { type: string }[] }): string[] {
  return [...new Set(p.methods.map((m) => m.type))];
}

export const isHostRun = (type: string | null | undefined) => !!type && HOST_RUN_TYPES.includes(type);

/** A store URL the admin may add: http(s) only. */
export function validStoreUrl(url: string): string | null {
  const u = url.trim();
  if (!u) return "the store's URL (an index.json or a GitHub repo)";
  if (!/^https?:\/\/[^\s/]+/i.test(u)) return "a URL starting with https://";
  return null;
}

// ── fit ──────────────────────────────────────────────────────────────────────────────────────

/** One line for a card: fits with which method, or why not. */
export function fitLine(c: CardFit): string {
  const f = c.fit;
  if (f.ok) {
    const how = f.method_label ? `${f.method_label} (${f.method_type ?? "?"})` : (f.method_type ?? "a method");
    const w = f.warnings.length ? ` · ${f.warnings.length} warning${f.warnings.length === 1 ? "" : "s"}` : "";
    return `Fits: ${how}${w}`;
  }
  const first = f.rejections.flatMap((r) => r.reasons)[0];
  return first ? `Does not fit: ${first}` : "Does not fit";
}

/** Why each other method was refused: "2. pip (host): needs python 3.12". */
export function rejectionLines(f: Fit): string[] {
  return f.rejections.map((r) => `${r.method + 1}. ${r.label || "method"}: ${r.reasons.join("; ") || "refused"}`);
}

// ── approvals ────────────────────────────────────────────────────────────────────────────────

export type ApprovalKind = "host_run" | "privileged" | "host_network" | "host_mounts" | "devices" | "cap_add" | "untrusted" | "other";
export interface ApprovalItem { key: string; kind: ApprovalKind; value?: string; label: string; warning?: string }

const ALIASES: Record<string, ApprovalKind> = {
  host_run: "host_run", "host-run": "host_run", hostrun: "host_run",
  privileged: "privileged",
  host_network: "host_network", "host-network": "host_network",
  host_mounts: "host_mounts", host_mount: "host_mounts", "host-mount": "host_mounts", mount: "host_mounts",
  devices: "devices", device: "devices",
  cap_add: "cap_add", "cap-add": "cap_add", cap: "cap_add",
  untrusted: "untrusted",
};

/** Read one approval string ("host_run", "host_mounts:/srv/x", "device=/dev/kfd", "cap_add:SYS_NICE"). */
export function parseApproval(s: string): ApprovalItem {
  const t = s.trim();
  const m = /^([A-Za-z_-]+)\s*[:=]\s*(.+)$/.exec(t);
  const name = (m ? m[1] : t).toLowerCase();
  const kind = ALIASES[name];
  const value = m ? m[2].trim() : undefined;
  if (!kind || ((kind === "host_mounts" || kind === "devices" || kind === "cap_add") && !value)) return { key: `other:${t}`, kind: "other", label: t };
  return describe(kind, value);
}

function describe(kind: ApprovalKind, value?: string): ApprovalItem {
  const key = value ? `${kind}:${value}` : kind;
  switch (kind) {
    case "host_run": return { key, kind, label: "Run programs on the host", warning: "Runs on the host, not sandboxed: it can do anything the router's (or bridge's) account can." };
    case "privileged": return { key, kind, label: "Privileged container", warning: "A privileged container has full access to the host." };
    case "host_network": return { key, kind, label: "Host network", warning: "The container shares the host's network: no isolation." };
    case "host_mounts": return { key, kind, value, label: `Mount ${value} from the host` };
    case "devices": return { key, kind, value, label: `Pass the device ${value} through` };
    case "cap_add": return { key, kind, value, label: `Add the capability ${value}` };
    case "untrusted": return { key, kind, label: "Install an untrusted package", warning: "This package comes from a store you have not marked trusted." };
    default: return { key, kind, value, label: value ?? kind };
  }
}

/**
 * Everything the admin must tick, one by one: the chosen method's approvals from fit, the
 * package's own list, host-run for a host method, and "untrusted" for an untrusted store. Once each.
 */
export function approvalItems(a: { fit?: Fit | null; needsApproval?: string[]; trusted: boolean; methodType?: string | null }): ApprovalItem[] {
  const out: ApprovalItem[] = [];
  const add = (i: ApprovalItem) => {
    if (!out.some((o) => o.key === i.key)) out.push(i);
  };
  for (const s of a.fit?.approvals ?? []) add(parseApproval(s));
  for (const s of a.needsApproval ?? []) add(parseApproval(s));
  if (isHostRun(a.methodType ?? a.fit?.method_type)) add(describe("host_run"));
  if (!a.trusted) add(describe("untrusted"));
  // Host-run and untrusted first: they are the ones to read.
  const rank = (k: ApprovalKind) => (k === "untrusted" ? 0 : k === "host_run" ? 1 : 2);
  return out.sort((x, y) => rank(x.kind) - rank(y.kind));
}

export const allTicked = (items: ApprovalItem[], ticked: ReadonlySet<string>) => items.every((i) => ticked.has(i.key));

/** The install call's approvals from the ticked items (unticked ones stay false / out). */
export function approvalsFrom(items: ApprovalItem[], ticked: ReadonlySet<string>): Approvals {
  const a: Approvals = { host_run: false, privileged: false, host_network: false, host_mounts: [], devices: [], cap_add: [], untrusted: false };
  for (const i of items) {
    if (!ticked.has(i.key)) continue;
    if (i.kind === "host_run" || i.kind === "privileged" || i.kind === "host_network" || i.kind === "untrusted") a[i.kind] = true;
    else if ((i.kind === "host_mounts" || i.kind === "devices" || i.kind === "cap_add") && i.value) a[i.kind].push(i.value);
  }
  return a;
}

// ── install params ───────────────────────────────────────────────────────────────────────────

/** A param's default as text for an input. */
export function paramText(v: unknown): string {
  if (v === undefined || v === null) return "";
  return typeof v === "string" ? v : JSON.stringify(v);
}

/** A typed value from an input, by the param's type (text stays text when it does not parse). */
export function paramValue(type: string, text: string): unknown {
  const t = text.trim();
  if (type === "int" && /^-?\d+$/.test(t)) return Number(t);
  if (type === "float" && t !== "" && !Number.isNaN(Number(t))) return Number(t);
  if (type === "bool" && (t === "true" || t === "false")) return t === "true";
  return text;
}

/** Only the params the admin changed from the default, typed. */
export function changedParams(params: Record<string, Param> | undefined, texts: Record<string, string>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [id, p] of Object.entries(params ?? {})) {
    const t = texts[id];
    if (t === undefined || t === paramText(p.default)) continue;
    out[id] = paramValue(p.type, t);
  }
  return out;
}

// ── installed ────────────────────────────────────────────────────────────────────────────────

/** Installed packages per server, in server order. */
export function installedByServer(rows: InstalledPkg[]): [string, InstalledPkg[]][] {
  const m = new Map<string, InstalledPkg[]>();
  for (const r of rows) m.set(r.server, [...(m.get(r.server) ?? []), r]);
  return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
}

/** Engines in the inventory that no installed package made: manual engines, shown as they are. */
export function manualEngines<E extends { id: string }>(engines: E[], installed: InstalledPkg[]): E[] {
  const fromPkgs = new Set(installed.flatMap((r) => r.engines));
  return engines.filter((e) => !fromPkgs.has(e.id));
}

export const anyRunning = (jobs: StoreJob[] | undefined) => !!jobs?.some((j) => j.state === "running");

// ── the wizard's form ────────────────────────────────────────────────────────────────────────

export interface ParamRow { id: string; type: Param["type"]; def: string; description: string; per_model: boolean; raw: Partial<Param> }
export interface AssetRow { platform: string; url: string; sha256: string; gpu: string[]; unpack: string; strip: string }
export interface MethodRow {
  type: MethodType;
  label: string;
  platforms: string;
  gpu: string[];
  /** One argument per line. */
  run: string;
  /** KEY=value per line. */
  env: string;
  /** id:container[/tcp], comma or line separated (container optional). */
  ports: string;
  /** source:target[:ro] per line. */
  mounts: string;
  healthHttp: string;
  healthStart: string;
  image: string;
  compose: string;
  python: string;
  pkg: string;
  /** One requirement per line. */
  requirements: string;
  assets: AssetRow[];
  repo: string;
  commit: string;
  detectCommand: string;
  detectPaths: string;
  detectVersionArgs: string;
  detectVersionRegex: string;
  detectUrl: string;
  /** The method as drafted or loaded: fields the form does not show are kept. */
  raw: Partial<Method>;
}
export interface WizardForm {
  id: string;
  name: string;
  version: string;
  description: string;
  kinds: string[];
  inherits: string;
  gpu: string[];
  minVram: string;
  cpuOk: boolean;
  minRam: string;
  simd: string;
  params: ParamRow[];
  api: string;
  alsoSpeaks: string;
  probe: string;
  modelsLayout: string;
  modelsMount: string;
  modelsFolders: string;
  modelsEnv: string;
  modelsReadOnly: boolean;
  licence: string;
  source: string;
  homepage: string;
  notes: string;
  private: boolean;
  methods: MethodRow[];
  /** The manifest the form came from: keys the form does not show (security, endpoints…) are kept. */
  base: Partial<Manifest>;
}

/** Split on commas and new lines; trimmed, empty ones out. */
export const splitList = (s: string) => s.split(/[,\n]/).map((x) => x.trim()).filter(Boolean);
/** Split on new lines only (run args, requirements: commas may be part of them). */
export const splitLines = (s: string) => s.split("\n").map((x) => x.trim()).filter(Boolean);
const num = (s: string): number | undefined => {
  const t = s.trim();
  if (!t) return undefined;
  const n = Number(t);
  return Number.isFinite(n) ? n : undefined;
};
const str = (v: unknown) => (v === undefined || v === null ? "" : String(v));

/** Fields that belong to one method type; the others are dropped when the type changes. */
const TYPE_FIELDS: Record<string, (keyof Method)[]> = {
  docker: ["image", "entrypoint", "docker_args"],
  compose: ["compose", "compose_file"],
  pip: ["python", "package", "requirements", "requirements_file", "extra_index_urls"],
  uv: ["python", "package", "requirements", "requirements_file", "extra_index_urls"],
  conda: ["python", "package", "requirements", "environment_file"],
  binary: ["assets"],
  git: ["repo", "commit", "build"],
  existing: ["detect"],
};
const ALL_TYPE_FIELDS = [...new Set(Object.values(TYPE_FIELDS).flat())];

export function emptyMethod(type: MethodType = "docker"): MethodRow {
  return {
    type, label: "", platforms: type === "docker" || type === "compose" ? "linux/amd64" : "*", gpu: ["cuda"], run: "", env: "", ports: "http", mounts: "",
    healthHttp: "/health", healthStart: "", image: "", compose: "", python: "3.12", pkg: "", requirements: "", assets: [], repo: "", commit: "",
    detectCommand: "", detectPaths: "", detectVersionArgs: "--version", detectVersionRegex: "", detectUrl: "", raw: {},
  };
}

export const emptyAsset = (): AssetRow => ({ platform: "linux/amd64", url: "", sha256: "", gpu: [], unpack: "", strip: "" });

export function emptyForm(): WizardForm {
  return manifestToForm({ schema: 1, id: "", name: "", version: "", kinds: [], api: "", methods: [] });
}

function portsText(ports: PortDecl[] | undefined) {
  return (ports ?? []).map((p) => `${p.id}${p.container ? `:${p.container}` : ""}${p.protocol === "tcp" ? "/tcp" : ""}`).join(", ");
}

/** "http:8880, ws:9000/tcp" -> port declarations. */
export function parsePorts(s: string): PortDecl[] {
  return splitList(s).map((p) => {
    const m = /^([^:/]+)(?::(\d+))?(?:\/(tcp|http))?$/.exec(p);
    if (!m) return { id: p };
    const d: PortDecl = { id: m[1] };
    if (m[2]) d.container = Number(m[2]);
    if (m[3]) d.protocol = m[3] as "http" | "tcp";
    return d;
  });
}

/** "KEY=value" lines -> env (values may contain "="). */
export function parseEnv(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const l of splitLines(s)) {
    const i = l.indexOf("=");
    if (i > 0) out[l.slice(0, i).trim()] = l.slice(i + 1);
  }
  return out;
}

/** "source:target[:ro]" lines -> mounts (a Windows "C:\x" source keeps its drive colon). */
export function parseMounts(s: string): Mount[] {
  return splitLines(s).map((l) => {
    let ro = false;
    let t = l;
    if (/:ro$/i.test(t)) {
      ro = true;
      t = t.slice(0, -3);
    }
    const drive = /^[A-Za-z]:[\\/]/.test(t) ? 2 : 0;
    const i = t.indexOf(":", drive);
    const m: Mount = i < 0 ? { source: t, target: t } : { source: t.slice(0, i), target: t.slice(i + 1) };
    if (ro) m.read_only = true;
    return m;
  });
}

function methodToRow(m: Method): MethodRow {
  const e = emptyMethod(m.type);
  return {
    ...e,
    type: m.type,
    label: m.label ?? "",
    platforms: (m.platforms ?? []).join(", "),
    gpu: [...(m.gpu ?? [])],
    run: (m.run ?? []).join("\n"),
    env: Object.entries(m.env ?? {}).map(([k, v]) => `${k}=${v}`).join("\n"),
    ports: portsText(m.ports),
    mounts: (m.mounts ?? []).map((x) => `${x.source}:${x.target}${x.read_only ? ":ro" : ""}`).join("\n"),
    healthHttp: m.health?.http ?? "",
    healthStart: str(m.health?.start_seconds),
    image: m.image ?? "",
    compose: m.compose ?? "",
    python: m.python ?? "",
    pkg: m.package ?? "",
    requirements: (m.requirements ?? []).join("\n"),
    assets: (m.assets ?? []).map((a) => ({ platform: a.platform, url: a.url, sha256: a.sha256, gpu: [...(a.gpu ?? [])], unpack: a.unpack ?? "", strip: str(a.strip) })),
    repo: m.repo ?? "",
    commit: m.commit ?? "",
    detectCommand: m.detect?.command ?? "",
    detectPaths: (m.detect?.paths ?? []).join("\n"),
    detectVersionArgs: (m.detect?.version_args ?? []).join(" "),
    detectVersionRegex: m.detect?.version_regex ?? "",
    detectUrl: m.detect?.url ?? "",
    raw: m,
  };
}

/** Set a key, or delete it when the value is empty (blank text, empty list or object). */
function put<T extends object>(o: T, k: keyof T, v: unknown) {
  const empty = v === undefined || v === "" || (Array.isArray(v) && v.length === 0) || (typeof v === "object" && v !== null && !Array.isArray(v) && Object.keys(v).length === 0);
  if (empty) delete o[k];
  else (o as Record<keyof T, unknown>)[k] = v;
}

export function rowToMethod(r: MethodRow): Method {
  const m: Method = { ...(r.raw as Method), type: r.type };
  // Keep only the type-specific fields of this type.
  const own = TYPE_FIELDS[r.type] ?? [];
  for (const f of ALL_TYPE_FIELDS) if (!own.includes(f)) delete m[f];
  put(m, "label", r.label.trim());
  put(m, "platforms", splitList(r.platforms));
  put(m, "gpu", r.gpu as GpuKind[]);
  put(m, "run", splitLines(r.run));
  put(m, "env", parseEnv(r.env));
  put(m, "ports", parsePorts(r.ports));
  put(m, "mounts", parseMounts(r.mounts));
  const health: Health = { ...(r.raw.health ?? {}) };
  put(health, "http", r.healthHttp.trim());
  put(health, "start_seconds", num(r.healthStart));
  put(m, "health", health);
  if (r.type === "docker") put(m, "image", r.image.trim());
  if (r.type === "compose") put(m, "compose", r.compose);
  if (r.type === "pip" || r.type === "uv" || r.type === "conda") {
    put(m, "python", r.python.trim());
    put(m, "package", r.pkg.trim());
    put(m, "requirements", splitLines(r.requirements));
  }
  if (r.type === "binary") {
    put(m, "assets", r.assets.map((a) => {
      const x: Asset = { platform: a.platform.trim(), url: a.url.trim(), sha256: a.sha256.trim().toLowerCase() };
      put(x, "gpu", a.gpu as GpuKind[]);
      put(x, "unpack", a.unpack);
      put(x, "strip", num(a.strip));
      const old = (r.raw.assets ?? []).find((o) => o.url === x.url);
      if (old?.size !== undefined) x.size = old.size;
      return x;
    }));
  }
  if (r.type === "git") {
    put(m, "repo", r.repo.trim());
    put(m, "commit", r.commit.trim());
  }
  if (r.type === "existing") {
    const d: Detect = { ...(r.raw.detect ?? {}) };
    put(d, "command", r.detectCommand.trim());
    put(d, "paths", splitLines(r.detectPaths));
    put(d, "version_args", r.detectVersionArgs.trim() ? r.detectVersionArgs.trim().split(/\s+/) : []);
    put(d, "version_regex", r.detectVersionRegex.trim());
    put(d, "url", r.detectUrl.trim());
    put(m, "detect", d);
  }
  return m;
}

export function manifestToForm(m: Manifest): WizardForm {
  return {
    id: m.id ?? "",
    name: m.name ?? "",
    version: m.version ?? "",
    description: m.description ?? "",
    kinds: [...(m.kinds ?? [])],
    inherits: m.caps?.inherits ?? "",
    gpu: [...(m.hardware?.gpu ?? [])],
    minVram: str(m.hardware?.min_vram_gib),
    cpuOk: !!m.hardware?.cpu_ok,
    minRam: str(m.hardware?.min_ram_gib),
    simd: (m.hardware?.simd ?? []).join(", "),
    params: Object.entries(m.params ?? {}).map(([id, p]) => ({ id, type: p.type, def: paramText(p.default), description: p.description ?? "", per_model: !!p.per_model, raw: p })),
    api: m.api ?? "",
    alsoSpeaks: (m.also_speaks ?? []).join(", "),
    probe: m.probe ?? "",
    modelsLayout: m.models?.layout ?? "",
    modelsMount: m.models?.mount ?? "",
    modelsFolders: (m.models?.folders ?? []).join(", "),
    modelsEnv: m.models?.env ?? "",
    modelsReadOnly: !!m.models?.read_only,
    licence: m.licence ?? "",
    source: m.source ?? "",
    homepage: m.homepage ?? "",
    notes: (m.notes ?? []).join("\n"),
    private: !!m.private,
    methods: (m.methods ?? []).map(methodToRow),
    base: m,
  };
}

/** The manifest from the form, keeping whatever the form does not show from where it came. */
export function formToManifest(f: WizardForm): Manifest {
  const m = { ...(f.base as Manifest), schema: 1 } as Manifest;
  m.id = f.id.trim();
  m.name = f.name.trim();
  m.version = f.version.trim();
  put(m, "description", f.description.trim());
  m.kinds = KINDS.filter((k) => f.kinds.includes(k));
  const caps: ManifestCaps = { ...(f.base.caps ?? {}) };
  put(caps, "inherits", f.inherits.trim());
  put(m, "caps", caps);
  const hw: Hardware = { ...(f.base.hardware ?? {}) };
  put(hw, "gpu", GPU_KINDS.filter((g) => f.gpu.includes(g)));
  put(hw, "min_vram_gib", num(f.minVram));
  if (f.cpuOk) hw.cpu_ok = true;
  else if (f.base.hardware?.cpu_ok !== undefined) hw.cpu_ok = false;
  else delete hw.cpu_ok;
  put(hw, "min_ram_gib", num(f.minRam));
  put(hw, "simd", splitList(f.simd));
  put(m, "hardware", hw);
  const params: Record<string, Param> = {};
  for (const p of f.params) {
    const id = p.id.trim();
    if (!id) continue;
    const x: Param = { ...(p.raw as Param), type: p.type };
    put(x, "default", p.def.trim() === "" ? undefined : paramValue(p.type, p.def));
    put(x, "description", p.description.trim());
    if (p.per_model) x.per_model = true;
    else delete x.per_model;
    params[id] = x;
  }
  put(m, "params", params);
  m.api = f.api.trim();
  put(m, "also_speaks", splitList(f.alsoSpeaks));
  put(m, "probe", f.probe);
  if (f.modelsLayout) {
    const md: ModelsLayout = { ...(f.base.models ?? {}), layout: f.modelsLayout as ModelsLayout["layout"] };
    put(md, "mount", f.modelsMount.trim());
    put(md, "folders", splitList(f.modelsFolders));
    put(md, "env", f.modelsEnv.trim());
    if (f.modelsReadOnly) md.read_only = true;
    else if (f.base.models?.read_only !== undefined) md.read_only = false;
    else delete md.read_only;
    m.models = md;
  } else delete m.models;
  put(m, "licence", f.licence.trim());
  put(m, "source", f.source.trim());
  put(m, "homepage", f.homepage.trim());
  put(m, "notes", splitLines(f.notes));
  if (f.private) m.private = true;
  else delete m.private;
  m.methods = f.methods.map(rowToMethod);
  return ordered(m);
}

const ORDER: (keyof Manifest)[] = [
  "$schema", "schema", "id", "name", "version", "description", "kinds", "caps", "hardware", "params", "api", "also_speaks", "probe", "endpoints", "models", "methods", "security", "licence", "source", "homepage", "notes", "private",
];

/** The manifest's keys in the schema's order (stable JSON for the review step and diffs). */
export function ordered(m: Manifest): Manifest {
  const out: Record<string, unknown> = {};
  for (const k of ORDER) if (m[k] !== undefined) out[k] = m[k];
  for (const [k, v] of Object.entries(m)) if (!(k in out) && v !== undefined) out[k] = v;
  return out as unknown as Manifest;
}

export const manifestJson = (m: Manifest) => JSON.stringify(ordered(m), null, 2) + "\n";

/** The review step's text back to a manifest, or the parse error. */
export function parseManifest(text: string): { manifest?: Manifest; error?: string } {
  try {
    const v = JSON.parse(text) as unknown;
    if (!v || typeof v !== "object" || Array.isArray(v)) return { error: "The manifest must be a JSON object." };
    return { manifest: v as Manifest };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
}

/** The wizard's own quick checks before the router's (the router's report is the authority). */
export function formProblems(f: WizardForm): string[] {
  const out: string[] = [];
  if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(f.id.trim())) out.push("Id: lower case letters, digits, . _ - (up to 64).");
  if (!f.name.trim()) out.push("Name is needed.");
  if (!f.version.trim()) out.push("Version is needed.");
  if (f.kinds.length === 0) out.push("Tick at least one kind.");
  if (!f.api.trim()) out.push("Pick the API it speaks.");
  if (f.methods.length === 0) out.push("Add at least one method.");
  const ids = f.params.map((p) => p.id.trim()).filter(Boolean);
  for (const id of ids) if (!/^[a-z][a-z0-9_]{0,40}$/.test(id)) out.push(`Param "${id}": lower case letters, digits and _.`);
  if (new Set(ids).size !== ids.length) out.push("Two params have the same id.");
  return out;
}

/** Issues grouped by level, errors first, for the review list. */
export function sortIssues(issues: Issue[]): Issue[] {
  return [...issues].sort((a, b) => (a.level === b.level ? a.at.localeCompare(b.at) : a.level === "error" ? -1 : 1));
}

/** The first docker image in a manifest (for the test run). */
export function firstImage(m: Partial<Manifest> | undefined): string {
  return m?.methods?.find((x) => x.type === "docker" && x.image)?.image ?? "";
}

/** Apply the test run's guesses: the API, and the health path on every docker method without one. */
export function applyGuesses(f: WizardForm, api: string | null, health: string | null): WizardForm {
  return {
    ...f,
    api: api ?? f.api,
    methods: health ? f.methods.map((m) => (m.type === "docker" || m.type === "compose" ? { ...m, healthHttp: health } : m)) : f.methods,
  };
}

/** What the source box asks for, per source. */
export const SOURCES: { id: DraftSource; label: string; hint: string; placeholder: string; multiline?: boolean }[] = [
  { id: "scratch", label: "From scratch", hint: "An empty package to fill in.", placeholder: "" },
  { id: "docker", label: "Docker image", hint: "The router reads the registry's metadata only: nothing runs.", placeholder: "ghcr.io/owner/image:tag" },
  { id: "compose", label: "Compose file", hint: "Paste a docker-compose.yml.", placeholder: "services:\n  app:\n    image: …", multiline: true },
  { id: "pip", label: "pip package", hint: "A pinned spec, e.g. name==1.2.3.", placeholder: "kokoro==0.9.4" },
  { id: "release", label: "GitHub release", hint: "A release page URL: its assets become a binary method.", placeholder: "https://github.com/owner/repo/releases/tag/v1.0" },
  { id: "engine", label: "Existing engine", hint: "One of the inventory's manual engines: it pre-fills the rest.", placeholder: "" },
];
