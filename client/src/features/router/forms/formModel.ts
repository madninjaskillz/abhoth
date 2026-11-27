// The router admin forms' logic (D151), plain TypeScript (no DOM, no imports) so node --test can
// run it: router.toml sections <-> form fields, the edits a form sends (the router applies them to
// the text with comments kept and checks the result), and router.json's model list.

// ── API shapes ───────────────────────────────────────────────────────────────────────────────

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };
export type Obj = { [k: string]: Json };

export interface ConfigData {
  rev: string;
  path: string;
  error?: string | null;
  data: Obj | null;
  includes: string[];
  parseError?: string | null;
}

/** One change for POST config/edit: the value at `path` (null removes it). */
export interface FormEdit {
  path: string[];
  value: Json;
}

export interface GatewayDoc {
  rev: string;
  path: string;
  text: string;
  data: Obj | null;
  error?: string | null;
  writable: boolean;
  /** The control plane changes the loaded config (upstreams, on-demand) on top of the file. */
  overlay: boolean;
}

export interface GatewayProblem { path: string; message: string; severity: string; line?: number }
export interface GatewayCheck { ok: boolean; problems: GatewayProblem[]; diff: string; consequences: string[]; rev: string }

export interface AdminKey { id: number; name: string; prefix: string; created: number; createdBy: string; lastUsed: number | null; revoked: number | null }
export interface NewAdminKey extends AdminKey { key: string }

export type Errors = Record<string, string>;

// ── small helpers ────────────────────────────────────────────────────────────────────────────

export const ID_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;

export function obj(v: Json | undefined): Obj {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : {};
}

export function str(v: Json | undefined): string {
  return typeof v === "string" ? v : typeof v === "number" ? String(v) : "";
}

export function numText(v: Json | undefined): string {
  return typeof v === "number" ? String(v) : "";
}

/** "" -> undefined (leave the key out); a number; or NaN for text that is not one. */
export function parseNum(s: string): number | undefined {
  const t = s.trim();
  if (!t) return undefined;
  const n = Number(t);
  return Number.isFinite(n) ? n : NaN;
}

/** Set `k` on `o` to `v`, or remove it when `v` is undefined / "" (keeps key order for edits). */
export function put(o: Obj, k: string, v: Json | undefined): Obj {
  if (v === undefined || v === "") delete o[k];
  else o[k] = v;
  return o;
}

/** `a = b` lines <-> a string map. */
export function mapToLines(m: Json | undefined): string {
  return Object.entries(obj(m)).map(([k, v]) => `${k} = ${str(v)}`).join("\n");
}

export function linesToMap(text: string): { map: Obj; bad: number[] } {
  const map: Obj = {};
  const bad: number[] = [];
  text.split("\n").forEach((line, i) => {
    const t = line.trim();
    if (!t) return;
    const at = t.indexOf("=");
    if (at <= 0) return bad.push(i + 1);
    map[t.slice(0, at).trim()] = t.slice(at + 1).trim();
  });
  return { map, bad };
}

export function listToText(v: Json | undefined): string {
  return Array.isArray(v) ? v.map((x) => str(x)).join("\n") : "";
}

export function textToList(t: string): string[] {
  return t.split(/[\n,]/).map((s) => s.trim()).filter(Boolean);
}

function idErrors(id: string, existing: string[], original: string | null, what: string): Errors {
  if (!id.trim()) return { id: `A ${what} needs an id.` };
  if (!ID_RE.test(id)) return { id: "Letters, digits, - _ . only (64 at most), starting with a letter or digit." };
  if (id !== original && existing.includes(id)) return { id: `There is already a ${what} called ${id}.` };
  return {};
}

/** The edits that save `value` under `[section].id`, renaming from `original` when it changed. */
export function sectionEdits(section: string, id: string, original: string | null, value: Obj): FormEdit[] {
  const edits: FormEdit[] = [];
  if (original && original !== id) edits.push({ path: [section, original], value: null });
  edits.push({ path: [section, id], value });
  return edits;
}

export function removeEdits(section: string, id: string): FormEdit[] {
  return [{ path: [section, id], value: null }];
}

// ── servers ──────────────────────────────────────────────────────────────────────────────────

export interface ServerForm {
  id: string;
  title: string;
  host: string;
  exec: string;
  sshUser: string;
  sshPort: string;
  sshKey: string;
  sshHostKey: string;
  hostctl: string;
  vars: string;
  locks: string;
}

export const EXEC_KINDS = ["ssh", "local"];

export function serverForm(id: string, s: Json | undefined): ServerForm {
  const o = obj(s);
  const ssh = obj(o.ssh);
  return {
    id,
    title: str(o.title),
    host: str(o.host),
    exec: str(o.exec) || "ssh",
    sshUser: str(ssh.user),
    sshPort: numText(ssh.port),
    sshKey: str(ssh.key),
    sshHostKey: str(ssh.host_key),
    hostctl: str(o.hostctl),
    vars: mapToLines(o.vars),
    locks: listToText(o.locks),
  };
}

export function validateServer(f: ServerForm, existing: string[], original: string | null): Errors {
  const e: Errors = { ...idErrors(f.id, existing, original, "server") };
  if (!f.host.trim()) e.host = "Where the router reaches it: a host name or address.";
  if (f.exec === "ssh") {
    if (!f.sshUser.trim()) e.sshUser = "The SSH user (the forced-command account, e.g. abhoth-exec).";
    if (!f.sshKey.startsWith("secret:")) e.sshKey = "A secret reference (secret:<id> from [secrets]), never the key itself.";
    if (!f.sshHostKey.trim()) e.sshHostKey = "The host key to pin: SHA256:… or an ssh-ed25519 line.";
    const p = parseNum(f.sshPort);
    if (p !== undefined && (!Number.isInteger(p) || p < 1 || p > 65535)) e.sshPort = "A port, 1-65535.";
  }
  if (linesToMap(f.vars).bad.length) e.vars = `Lines ${linesToMap(f.vars).bad.join(", ")} are not name = value.`;
  return e;
}

/** The server table: the form's fields over the original (keys the form doesn't show are kept). */
export function serverValue(f: ServerForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  put(o, "title", f.title.trim() || undefined);
  put(o, "host", f.host.trim());
  put(o, "exec", f.exec);
  if (f.exec === "ssh") {
    const ssh: Obj = { ...obj(o.ssh) };
    put(ssh, "user", f.sshUser.trim() || undefined);
    put(ssh, "port", parseNum(f.sshPort));
    put(ssh, "key", f.sshKey.trim() || undefined);
    put(ssh, "host_key", f.sshHostKey.trim() || undefined);
    if (Object.keys(ssh).length) o.ssh = ssh;
    else delete o.ssh;
  } else delete o.ssh;
  put(o, "hostctl", f.hostctl || undefined);
  const vars = linesToMap(f.vars).map;
  put(o, "vars", Object.keys(vars).length ? vars : undefined);
  const locks = textToList(f.locks);
  put(o, "locks", locks.length ? locks : undefined);
  return o;
}

// ── cards ────────────────────────────────────────────────────────────────────────────────────

export interface CardForm {
  id: string;
  server: string;
  /** D153: a GPU, or the server's CPU (cores and a RAM budget; no UUID, never switched). */
  device: "gpu" | "cpu";
  gpuUuid: string;
  ramGib: string;
  cores: string;
  label: string;
  vramGib: string;
  reserveGib: string;
  cpus: string;
  managed: boolean;
  onDemand: boolean;
}

export function cardForm(id: string, c: Json | undefined): CardForm {
  const o = obj(c);
  const cpu = o.device === "cpu";
  return {
    id,
    server: str(o.server),
    device: cpu ? "cpu" : "gpu",
    gpuUuid: cpu ? "" : str(o.gpu_uuid),
    ramGib: numText(o.ram_gib),
    cores: numText(o.cores),
    label: str(o.label),
    vramGib: cpu ? "" : numText(o.vram_gib),
    reserveGib: numText(o.reserve_gib),
    cpus: str(o.cpus),
    managed: cpu ? false : o.managed !== false,
    onDemand: cpu ? false : o.on_demand === true,
  };
}

export function validateCard(f: CardForm, existing: string[], original: string | null, servers: string[]): Errors {
  const e: Errors = { ...idErrors(f.id, existing, original, "card") };
  if (!servers.includes(f.server)) e.server = servers.length ? "Pick the server the card is in." : "Add a server first.";
  if (f.device === "cpu") {
    const r = parseNum(f.ramGib);
    if (r === undefined || Number.isNaN(r) || r <= 0) e.ramGib = "The RAM engines on this CPU may use, in GiB (leave the host some).";
    const n = parseNum(f.cores);
    if (n !== undefined && (Number.isNaN(n) || n < 1 || !Number.isInteger(n))) e.cores = "A whole number of cores.";
  } else if (f.managed && !/^GPU-[0-9a-fA-F-]{8,}$/.test(f.gpuUuid.trim())) e.gpuUuid = "The GPU's UUID as nvidia-smi -L prints it (GPU-…).";
  for (const [k, v] of [["vramGib", f.vramGib], ["reserveGib", f.reserveGib]] as const) {
    const n = parseNum(v);
    if (n !== undefined && (Number.isNaN(n) || n < 0)) e[k] = "GiB, a number ≥ 0.";
  }
  if (f.cpus && !/^\d+(-\d+)?(,\d+(-\d+)?)*$/.test(f.cpus.trim())) e.cpus = "A CPU list such as 0-23 or 0-11,24-35.";
  return e;
}

export function cardValue(f: CardForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  put(o, "server", f.server);
  put(o, "label", f.label.trim() || undefined);
  put(o, "reserve_gib", parseNum(f.reserveGib));
  put(o, "cpus", f.cpus.trim() || undefined);
  if (f.device === "cpu") {
    // never GPU fields; never managed or switched in this release (the router refuses on_demand)
    put(o, "device", "cpu");
    delete o.gpu_uuid;
    delete o.vram_gib;
    put(o, "ram_gib", parseNum(f.ramGib));
    put(o, "cores", parseNum(f.cores));
    o.managed = false;
    if (o.on_demand !== undefined) o.on_demand = false;
    return o;
  }
  delete o.device;
  delete o.ram_gib;
  delete o.cores;
  put(o, "gpu_uuid", f.gpuUuid.trim() || undefined);
  put(o, "vram_gib", parseNum(f.vramGib));
  put(o, "managed", f.managed ? undefined : false);
  put(o, "on_demand", f.onDemand ? true : undefined);
  return o;
}

/** What each layout runs on a card ("same" resolved to the card before it in the layout). */
export interface CardRun { layout: string; title: string; engine: string; profile: string; model: string; lanes: number | null }

export function cardRuns(data: Obj | null, card: string): CardRun[] {
  const layouts = obj(data?.layouts);
  const engines = obj(data?.engines);
  const out: CardRun[] = [];
  for (const [lid, l] of Object.entries(layouts)) {
    const cards = obj(obj(l).cards);
    let prev: Obj | null = null;
    for (const [cid, spec] of Object.entries(cards)) {
      let s: Obj | null = typeof spec === "object" ? obj(spec) : spec === "same" ? prev : null;
      if (typeof spec === "object") prev = obj(spec);
      if (cid !== card || !s) continue;
      const lanes = obj(obj(obj(engines[str(s.engine)]).profiles)[str(s.profile)]).lanes;
      out.push({ layout: lid, title: str(obj(l).title), engine: str(s.engine), profile: str(s.profile), model: str(s.model), lanes: typeof lanes === "number" ? lanes : null });
      s = null;
    }
  }
  return out;
}

// ── engines and their profiles ───────────────────────────────────────────────────────────────

export interface ProfileForm { id: string; title: string; lanes: string; ctx: string; vramGib: string; build: string }
export interface EngineForm {
  id: string;
  title: string;
  driver: string;
  image: string;
  container: string;
  servedId: string;
  maxContext: string;
  /** One port for every card, or `A = 8094` lines per card. */
  port: string;
  profiles: ProfileForm[];
}

export const DRIVERS = ["docker", "compose", "systemd", "process", "templates"];

export function engineForm(id: string, e: Json | undefined): EngineForm {
  const o = obj(e);
  const port = typeof o.port === "number" ? String(o.port) : mapToLines(o.port);
  return {
    id,
    title: str(o.title),
    driver: str(o.driver) || "docker",
    image: str(o.image),
    container: str(o.container),
    servedId: str(o.served_id),
    maxContext: numText(o.max_context),
    port,
    profiles: Object.entries(obj(o.profiles)).map(([pid, p]) => {
      const po = obj(p);
      return { id: pid, title: str(po.title), lanes: numText(po.lanes), ctx: numText(po.ctx), vramGib: numText(po.vram_gib), build: str(po.build) };
    }),
  };
}

function portValue(text: string): Json | undefined | null {
  const t = text.trim();
  if (!t) return undefined;
  if (/^\d+$/.test(t)) return Number(t);
  const { map, bad } = linesToMap(t);
  if (bad.length) return null;
  const out: Obj = {};
  for (const [k, v] of Object.entries(map)) {
    if (!/^\d+$/.test(str(v))) return null;
    out[k] = Number(v);
  }
  return out;
}

export function validateEngine(f: EngineForm, existing: string[], original: string | null): Errors {
  const e: Errors = { ...idErrors(f.id, existing, original, "engine") };
  if (portValue(f.port) === null) e.port = "A port, or one `CARD = port` line per card.";
  const mc = parseNum(f.maxContext);
  if (mc !== undefined && (!Number.isInteger(mc) || mc <= 0)) e.maxContext = "Tokens, a whole number.";
  const seen = new Set<string>();
  f.profiles.forEach((p, i) => {
    if (!ID_RE.test(p.id)) e[`profiles.${i}.id`] = "A profile needs an id (letters, digits, - _ .).";
    else if (seen.has(p.id)) e[`profiles.${i}.id`] = `Profile ${p.id} is listed twice.`;
    seen.add(p.id);
    const lanes = parseNum(p.lanes);
    if (lanes === undefined || !Number.isInteger(lanes) || lanes < 1) e[`profiles.${i}.lanes`] = "Lanes (slots): 1 or more.";
    const v = parseNum(p.vramGib);
    if (v !== undefined && (Number.isNaN(v) || v < 0)) e[`profiles.${i}.vramGib`] = "GiB, a number ≥ 0.";
    const c = parseNum(p.ctx);
    if (c !== undefined && (!Number.isInteger(c) || c <= 0)) e[`profiles.${i}.ctx`] = "Tokens, a whole number.";
  });
  return e;
}

export function engineValue(f: EngineForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  put(o, "title", f.title.trim() || undefined);
  put(o, "driver", f.driver);
  put(o, "image", f.image.trim() || undefined);
  put(o, "container", f.container.trim() || undefined);
  put(o, "served_id", f.servedId.trim() || undefined);
  put(o, "max_context", parseNum(f.maxContext));
  const port = portValue(f.port);
  put(o, "port", port === null ? undefined : port);
  const old = obj(o.profiles);
  const profiles: Obj = {};
  for (const p of f.profiles) {
    const po: Obj = { ...obj(old[p.id]) };
    put(po, "title", p.title.trim() || undefined);
    put(po, "lanes", parseNum(p.lanes));
    put(po, "ctx", parseNum(p.ctx));
    put(po, "vram_gib", parseNum(p.vramGib));
    put(po, "build", p.build.trim() || undefined);
    profiles[p.id] = po;
  }
  put(o, "profiles", f.profiles.length ? profiles : undefined);
  return o;
}

// ── layouts ──────────────────────────────────────────────────────────────────────────────────

export type SlotKind = "unset" | "off" | "same" | "spec";
export interface LayoutSlot { kind: SlotKind; engine: string; profile: string; model: string }
export interface LayoutForm { id: string; title: string; description: string; mode: string; hidden: boolean; cards: Record<string, LayoutSlot> }

export const AGENT_MODES = ["", "auto", "hands-on", "orchestrator", "peer"];

export function layoutForm(id: string, l: Json | undefined, cardIds: string[]): LayoutForm {
  const o = obj(l);
  const cards: Record<string, LayoutSlot> = {};
  const given = obj(o.cards);
  for (const c of [...cardIds, ...Object.keys(given).filter((k) => !cardIds.includes(k))]) {
    const v = given[c];
    if (v === undefined) cards[c] = { kind: "unset", engine: "", profile: "", model: "" };
    else if (typeof v === "string") cards[c] = { kind: v === "same" ? "same" : "off", engine: "", profile: "", model: "" };
    else {
      const s = obj(v);
      cards[c] = { kind: "spec", engine: str(s.engine), profile: str(s.profile), model: str(s.model) };
    }
  }
  return { id, title: str(o.title), description: str(o.description), mode: str(o.mode), hidden: o.hidden === true, cards };
}

export function validateLayout(f: LayoutForm, existing: string[], original: string | null, data: Obj | null): Errors {
  const e: Errors = { ...idErrors(f.id, existing, original, "layout") };
  const engines = obj(data?.engines);
  const models = obj(data?.models);
  let first = true;
  for (const [c, s] of Object.entries(f.cards)) {
    if (s.kind === "unset") continue;
    if (s.kind === "same" && first) e[`cards.${c}`] = "\"Same as the card before\" needs a card before it.";
    if (s.kind === "spec") {
      if (!engines[s.engine]) e[`cards.${c}`] = "Pick an engine.";
      else if (!obj(obj(engines[s.engine]).profiles)[s.profile]) e[`cards.${c}`] = `Pick one of ${s.engine}'s profiles.`;
      else if (!s.model.trim()) e[`cards.${c}`] = "Pick a model.";
      else if (Object.keys(models).length && !models[s.model]) e[`cards.${c}`] = `No model ${s.model} in [models].`;
    }
    first = false;
  }
  if (!Object.values(f.cards).some((s) => s.kind !== "unset")) e.cards = "A layout says what at least one card runs.";
  return e;
}

export function layoutValue(f: LayoutForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  put(o, "title", f.title.trim() || undefined);
  put(o, "description", f.description.trim() || undefined);
  put(o, "mode", f.mode || undefined);
  put(o, "hidden", f.hidden ? true : undefined);
  const cards: Obj = {};
  for (const [c, s] of Object.entries(f.cards)) {
    if (s.kind === "off" || s.kind === "same") cards[c] = s.kind;
    else if (s.kind === "spec") cards[c] = { engine: s.engine, profile: s.profile, model: s.model.trim() };
  }
  o.cards = cards;
  return o;
}

/** The ids in a section of the config, in file order. */
export function ids(data: Obj | null, section: string): string[] {
  return Object.keys(obj(data?.[section]));
}

/** Who uses a server / card / engine (to refuse removing what is still referenced). */
export function usedBy(data: Obj | null, section: "servers" | "cards" | "engines", id: string): string[] {
  const out: string[] = [];
  if (section === "servers") for (const [c, v] of Object.entries(obj(data?.cards))) if (str(obj(v).server) === id) out.push(`card ${c}`);
  if (section === "cards" || section === "engines")
    for (const [l, v] of Object.entries(obj(data?.layouts))) {
      const cards = obj(obj(v).cards);
      const hit = section === "cards" ? id in cards : Object.values(cards).some((s) => typeof s === "object" && str(obj(s).engine) === id);
      if (hit) out.push(`layout ${l}`);
    }
  return out;
}

// ── router.json: the gateway's model list ────────────────────────────────────────────────────

export interface ModelForm {
  name: string;
  /** The cards (upstreams) this name balances over; empty with relay. */
  cards: string[];
  relay: boolean;
  servedId: string;
  weight: string;
  contextTokens: string;
  outputTokens: string;
  vision: boolean;
  tools: boolean;
  reasoning: boolean;
  effortLevels: string;
  thinkingOffEffort: string;
  /** `params` (defaults added to requests) as JSON text. */
  params: string;
  dropParams: string;
}

export function deployments(data: Obj | null): Obj[] {
  return Array.isArray(data?.deployments) ? (data!.deployments as Json[]).map(obj) : [];
}

export function modelForm(d: Obj | undefined): ModelForm {
  const o = d ?? {};
  const mi = obj(o.modelInfo);
  return {
    name: str(o.name),
    cards: Array.isArray(o.cards) ? o.cards.map((c) => str(c)) : [],
    relay: o.relay === true,
    servedId: str(o.servedId),
    weight: numText(o.weight),
    contextTokens: numText(mi.max_input_tokens),
    outputTokens: numText(mi.max_output_tokens),
    vision: mi.supports_vision === true,
    tools: mi.supports_function_calling === true,
    reasoning: mi.supports_reasoning === true,
    effortLevels: listToText(mi.effort_levels).replace(/\n/g, ", "),
    thinkingOffEffort: str(mi.thinking_off_effort),
    params: o.params && Object.keys(obj(o.params)).length ? JSON.stringify(o.params, null, 2) : "",
    dropParams: listToText(o.dropParams).replace(/\n/g, ", "),
  };
}

export function validateModel(f: ModelForm, upstreams: string[], hasRelay: boolean): Errors {
  const e: Errors = {};
  if (!f.name.trim()) e.name = "The name clients ask for.";
  if (!f.servedId.trim()) e.servedId = "The model id the engine answers to.";
  if (f.relay) {
    if (!hasRelay) e.relay = "There is no on-demand relay in this router.json.";
  } else if (!f.cards.length) e.cards = "Pick at least one card (upstream).";
  else for (const c of f.cards) if (!upstreams.includes(c)) e.cards = `No upstream ${c}.`;
  for (const [k, v] of [["contextTokens", f.contextTokens], ["outputTokens", f.outputTokens], ["weight", f.weight]] as const) {
    const n = parseNum(v);
    if (n !== undefined && (!Number.isInteger(n) || n <= 0)) e[k] = "A whole number above 0.";
  }
  if (f.params.trim()) {
    try {
      const p = JSON.parse(f.params);
      if (!p || typeof p !== "object" || Array.isArray(p)) e.params = "A JSON object, e.g. {\"reasoning_effort\": \"low\"}.";
    } catch {
      e.params = "Not valid JSON.";
    }
  }
  const levels = textToList(f.effortLevels);
  if (f.thinkingOffEffort && f.thinkingOffEffort !== "drop" && f.thinkingOffEffort !== "none" && !levels.includes(f.thinkingOffEffort))
    e.thinkingOffEffort = "drop, none, or one of the effort levels.";
  return e;
}

export function modelValue(f: ModelForm, original: Obj | undefined): Obj {
  const o: Obj = { ...(original ?? {}) };
  o.name = f.name.trim();
  if (f.relay) {
    o.relay = true;
    delete o.cards;
  } else {
    delete o.relay;
    o.cards = f.cards;
  }
  o.servedId = f.servedId.trim();
  put(o, "weight", parseNum(f.weight));
  const mi: Obj = { ...obj(o.modelInfo) };
  put(mi, "max_input_tokens", parseNum(f.contextTokens));
  put(mi, "max_output_tokens", parseNum(f.outputTokens));
  put(mi, "supports_vision", f.vision ? true : undefined);
  put(mi, "supports_function_calling", f.tools ? true : undefined);
  put(mi, "supports_reasoning", f.reasoning ? true : undefined);
  const levels = textToList(f.effortLevels);
  put(mi, "effort_levels", levels.length ? levels : undefined);
  put(mi, "thinking_off_effort", f.thinkingOffEffort || undefined);
  o.modelInfo = mi;
  put(o, "params", f.params.trim() ? (JSON.parse(f.params) as Json) : undefined);
  const drop = textToList(f.dropParams);
  put(o, "dropParams", drop.length ? drop : undefined);
  return o;
}

export interface UpstreamForm { card: string; url: string; engine: string; servedId: string; lanes: string; keyFile: string; online: boolean }

export function upstreamForm(card: string, u: Json | undefined): UpstreamForm {
  const o = obj(u);
  return { card, url: str(o.url), engine: str(o.engine), servedId: str(o.servedId), lanes: numText(o.lanes) || "1", keyFile: str(o.keyFile), online: o.online !== false };
}

export function validateUpstream(f: UpstreamForm, existing: string[], original: string | null): Errors {
  const e: Errors = {};
  if (!f.card.trim()) e.card = "The card id (A, B, …).";
  else if (f.card !== original && existing.includes(f.card)) e.card = `There is already an upstream ${f.card}.`;
  if (!/^https?:\/\/[^\s/]+/.test(f.url.trim())) e.url = "The engine's root URL, e.g. http://gpu-host:8094 (no /v1).";
  const l = parseNum(f.lanes);
  if (l === undefined || !Number.isInteger(l) || l < 1) e.lanes = "Lanes: 1 or more.";
  if (/^(sk-|abk_|Bearer )/.test(f.keyFile.trim())) e.keyFile = "A path to a file holding the key, never the key.";
  return e;
}

export function upstreamValue(f: UpstreamForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  o.url = f.url.trim().replace(/\/+$/, "");
  put(o, "engine", f.engine.trim() || undefined);
  put(o, "servedId", f.servedId.trim() || undefined);
  o.lanes = parseNum(f.lanes) ?? 1;
  put(o, "keyFile", f.keyFile.trim() || undefined);
  put(o, "online", f.online ? undefined : false);
  return o;
}

export interface ClaudeForm { enabled: boolean; maxSessions: string; access: string; routes: string; contextTokens: string; maxOutputTokens: string }

export function claudeForm(c: Json | undefined): ClaudeForm {
  const o = obj(c);
  return {
    enabled: o.enabled === true,
    maxSessions: numText(o.maxSessions),
    access: str(o.access) || "admins",
    routes: mapToLines(o.routes),
    contextTokens: numText(o.contextTokens),
    maxOutputTokens: numText(o.maxOutputTokens),
  };
}

export function validateClaude(f: ClaudeForm): Errors {
  const e: Errors = {};
  if (!/^(admins|users|anonymous|keys:.+)$/.test(f.access.trim())) e.access = "admins, users, anonymous or keys:<names>.";
  if (linesToMap(f.routes).bad.length) e.routes = "One `route-name = model` per line.";
  const n = parseNum(f.maxSessions);
  if (n !== undefined && (!Number.isInteger(n) || n < 1)) e.maxSessions = "1 or more.";
  return e;
}

export function claudeValue(f: ClaudeForm, original: Json | undefined): Obj {
  const o: Obj = { ...obj(original) };
  o.enabled = f.enabled;
  put(o, "maxSessions", parseNum(f.maxSessions));
  o.access = f.access.trim();
  const routes = linesToMap(f.routes).map;
  put(o, "routes", Object.keys(routes).length ? routes : undefined);
  put(o, "contextTokens", parseNum(f.contextTokens));
  put(o, "maxOutputTokens", parseNum(f.maxOutputTokens));
  return o;
}

/** router.json as the page writes it: the document with `change` applied, two-space JSON. */
export function gatewayText(data: Obj, change: (d: Obj) => void): string {
  const d = JSON.parse(JSON.stringify(data)) as Obj;
  change(d);
  return JSON.stringify(d, null, 2) + "\n";
}

/** Replace (or add, at `index` -1) one deployment. */
export function withDeployment(data: Obj, index: number, value: Obj | null): string {
  return gatewayText(data, (d) => {
    const list = deployments(d);
    if (value === null) list.splice(index, 1);
    else if (index < 0) list.push(value);
    else list[index] = value;
    d.deployments = list;
  });
}

/** The capability badges a model row shows. */
export function modelBadges(d: Obj): string[] {
  const mi = obj(d.modelInfo);
  const out: string[] = [];
  const ctx = mi.max_input_tokens;
  if (typeof ctx === "number") out.push(ctx >= 1000 ? `${Math.round(ctx / 1024)}k ctx` : `${ctx} ctx`);
  if (mi.supports_vision === true) out.push("vision");
  if (mi.supports_function_calling === true) out.push("tools");
  if (mi.supports_reasoning === true || Array.isArray(mi.effort_levels)) out.push("reasoning");
  return out;
}
