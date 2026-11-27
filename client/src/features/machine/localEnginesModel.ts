// Local Engines' model (plain TS, no DOM; docs/design/LOCAL-ENGINES.md, D156): the engines this machine uses. Managed
// engines come from This Machine's inventory; unmanaged engines and Cloud providers are
// endpoints (a kind, a name and a URL). Served at /api/machine/local-engines (admins).

export type EngineKind = "llm" | "tts" | "stt" | "image" | "music" | "embedding" | "video";
export type EndpointGroup = "unmanaged" | "cloud";

export interface Endpoint {
  title: string;
  group: EndpointGroup;
  kind: EngineKind;
  url: string;
  api: string;
  /** Empty: Engine default. */
  models: string[];
  timeoutSeconds?: number | null;
  uncensoredModel?: string | null;
}

export interface EndpointView extends Endpoint {
  id: string;
  keySet: boolean;
  shared: boolean;
  tool: boolean;
}

export interface ManagedEngine {
  id: string;
  title: string;
  kinds: EngineKind[];
  driver: string | null;
  models: string[];
  cards: string[];
  states: { card: string; state: string; error?: string | null }[];
  shared: boolean;
  tool: boolean;
}

export interface LocalEngines {
  managed: ManagedEngine[];
  endpoints: EndpointView[];
  defaults: Partial<Record<EngineKind, string>>;
  kinds: EngineKind[];
  apis: Record<EngineKind, string[]>;
  tools: Partial<Record<EngineKind, string>>;
  rev: string;
  clashes: string[];
  bridgeMode: boolean;
  error?: string;
}

export interface CheckResult { ok: boolean; models?: string[]; status?: number; error?: string }

export const KIND_LABELS: Record<EngineKind, string> = {
  llm: "LLM",
  tts: "Text to speech",
  stt: "Speech to text",
  image: "Images",
  music: "Music",
  embedding: "Embeddings",
  video: "Video",
};

export const API_LABELS: Record<string, string> = {
  openai: "OpenAI-compatible",
  anthropic: "Anthropic",
  ollama: "Ollama",
  "openai-images": "OpenAI images API",
  comfyui: "ComfyUI",
  "openai-speech": "OpenAI speech API",
  "openai-stt": "OpenAI transcription API",
  wyoming: "Wyoming",
  synth: "Synth",
  sa3: "Stable Audio 3",
};

/** Cloud presets: a name, base URL and API style to start from. */
export const CLOUD_PRESETS: { title: string; url: string; api: string }[] = [
  { title: "Anthropic", url: "https://api.anthropic.com/v1", api: "anthropic" },
  { title: "OpenAI", url: "https://api.openai.com/v1", api: "openai" },
  { title: "OpenRouter", url: "https://openrouter.ai/api/v1", api: "openai" },
];

/** A URL on this PC or a private network address (an engine "here"), else one elsewhere. */
export function isLocalUrl(url: string): boolean {
  let host = "";
  try {
    host = new URL(url.replace(/^tcp:\/\//, "http://")).hostname.toLowerCase();
  } catch {
    return false;
  }
  if (host === "localhost" || host === "127.0.0.1" || host === "[::1]" || host === "::1" || host === "0.0.0.0") return true;
  return false;
}

/** The engines a tool kind may use (exposed, of that kind), managed and unmanaged, in order. */
export function toolChoices(le: LocalEngines, kind: EngineKind): { id: string; title: string }[] {
  const m = le.managed.filter((e) => e.tool && e.kinds.includes(kind)).map((e) => ({ id: e.id, title: e.title }));
  const u = le.endpoints.filter((e) => e.tool && e.kind === kind).map((e) => ({ id: e.id, title: e.title }));
  return [...m, ...u];
}

/** An id from a title (as the server makes them): `LM Studio` → `lm-studio`, not one of `taken`. */
export function idFrom(title: string, taken: Set<string>): string {
  const base = title.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 32) || "engine";
  let id = base;
  for (let n = 2; taken.has(id); n++) id = `${base}-${n}`;
  return id;
}

/** Models typed one per line or comma-separated; empty means Engine default. */
export function parseModels(text: string): string[] {
  const seen = new Set<string>();
  return text
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter((s) => s && !seen.has(s) && (seen.add(s), true));
}
