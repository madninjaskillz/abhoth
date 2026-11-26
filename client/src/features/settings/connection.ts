// The connection (D149, docs/design/ROUTER-MODE.md): an Abhoth router, or an OpenAI-compatible
// server set up here. Which settings rows each mode shows, and how the router's models read.
// Pure (no imports, no DOM), so plain node can test it (test/connection.test.ts).

export type ConnectionMode = "server" | "router";

/** The connection setting itself: the panel draws it, never a row. */
export const CONNECTION_KEY = "lmStudio.connection";

export interface ModeAware {
  key: string;
  /** From the server's schema: "shown" (default), "hidden" or "router" (from the router's model metadata). */
  routerMode?: "shown" | "hidden" | "router";
}

/** Whether a settings row is shown in `mode`. */
export function visibleInMode(s: ModeAware, mode: ConnectionMode): boolean {
  if (s.key === CONNECTION_KEY) return false;
  if (mode === "server") return true;
  return (s.routerMode ?? "shown") === "shown";
}

/** The rows `mode` hides, by key (to say how many the router takes care of). */
export function hiddenInMode<S extends ModeAware>(all: { settings: S[] }[], mode: ConnectionMode): S[] {
  return all.flatMap((sec) => sec.settings.filter((s) => s.key !== CONNECTION_KEY && !visibleInMode(s, mode)));
}

/** The mode a settings document is in (older servers: server). */
export function modeOf(doc: { connection?: string } | undefined): ConnectionMode {
  return doc?.connection === "router" ? "router" : "server";
}

/** 131072 -> "128K", 200000 -> "195K", 1048576 -> "1M"; undefined -> "?". */
export function formatTokens(n: number | null | undefined): string {
  if (!n || n <= 0) return "?";
  if (n >= 1048576 && n % 1048576 === 0) return `${n / 1048576}M`;
  if (n >= 1024) return `${Math.round(n / 1024)}K`;
  return String(n);
}

export interface ModelLike {
  id: string;
  vision?: boolean;
  tools?: boolean;
  reasoning?: boolean;
  kind?: string;
  status?: string;
  engine?: string;
}

/** Short capability labels for a router model, in a fixed order. */
export function capabilityBadges(m: ModelLike): string[] {
  const out: string[] = [];
  if (m.tools) out.push("tools");
  if (m.vision) out.push("vision");
  if (m.reasoning) out.push("reasoning");
  if (m.kind === "cloud") out.push("cloud");
  return out;
}

/** The models for the default-model picker: live ones first, then the rest, each group by name. */
export function pickerOrder<M extends ModelLike>(models: M[]): M[] {
  const rank = (m: M) => (m.status === "live" ? 0 : m.kind === "cloud" ? 1 : m.status === "switching" ? 2 : 3);
  return [...models].sort((a, b) => rank(a) - rank(b) || a.id.localeCompare(b.id));
}

/** A typed URL as the server expects it: trimmed, http:// added when there's no scheme. */
export function normalizeUrl(input: string): string {
  const t = input.trim();
  if (!t) return "";
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(t) ? t : `http://${t}`;
}

export interface StatusLike {
  connection: ConnectionMode;
  suggestRouter: boolean;
  router: { name: string } | null;
}

/** The line the panel shows about the configured URL, and whether to offer router mode. */
export function statusLine(s: StatusLike | undefined): { text: string; offer: boolean } {
  if (!s) return { text: "", offer: false };
  if (s.connection === "router")
    return s.router ? { text: `Connected to ${s.router.name}.`, offer: false } : { text: "The Base URL does not answer as an Abhoth router.", offer: false };
  if (s.suggestRouter && s.router) return { text: `The Base URL is ${s.router.name}, an Abhoth router.`, offer: true };
  return { text: "", offer: false };
}

/** The configured key's kind, as the web server reports it (never the key). */
export type KeyKind = "none" | "placeholder" | "abhoth" | "other";

/**
 * What the panel says about the API key for a router: `tone` "ok" (nothing to do), "info" (a hint)
 * or "warn" (the router will refuse it / needs one). LM Studio's placeholder (`lm-studio`) is never
 * sent to a router, so it reads as "no key", not as a problem.
 */
export function keyHint(
  kind: KeyKind | undefined,
  auth: { required: boolean; keys: boolean; keyPrefix?: string | null },
  hasClaude: boolean,
): { tone: "ok" | "info" | "warn"; text: string } {
  const prefix = auth.keyPrefix ?? "abk_";
  const placeholder = kind === "placeholder" ? "LM Studio's placeholder key (lm-studio) isn't sent to the router. " : "";
  if (kind === "abhoth") return { tone: "ok", text: `Using your Abhoth key (${prefix}…).` };
  if (kind === "other" && auth.keys) {
    return { tone: "warn", text: `The API Key isn't an Abhoth key (${prefix}…): the router will refuse it. Clear it, or paste an Abhoth key.` };
  }
  if (auth.required) return { tone: "warn", text: `${placeholder}This router needs an Abhoth key (${prefix}…) in API Key below.` };
  const claude = hasClaude && auth.keys ? ` Add an Abhoth key (${prefix}…) to use Claude.` : "";
  return { tone: claude || placeholder ? "info" : "ok", text: `${placeholder}No key needed on your LAN.${claude}` };
}
