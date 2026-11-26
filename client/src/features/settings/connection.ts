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

export interface AuthLike {
  required: boolean;
  keys: boolean;
  keyPrefix?: string | null;
  keyUrl?: string | null;
  keyRequests?: boolean;
}

/**
 * What the panel says about the key for a router, and what it gives: `tone` "ok" (nothing to do),
 * "info" (a hint) or "warn" (the router needs one). Only Abhoth keys are sent to a router: LM
 * Studio's placeholder and another server's key are kept for server mode and read as "no key",
 * never as an error. `claudeForKey`: the router's models list says this caller may use Claude.
 */
export function keyHint(
  kind: KeyKind | undefined,
  auth: AuthLike,
  hasClaude: boolean,
  claudeForKey?: boolean,
): { tone: "ok" | "info" | "warn"; text: string } {
  const prefix = auth.keyPrefix ?? "abk_";
  if (kind === "abhoth") {
    const claude = !hasClaude ? "" : claudeForKey === undefined ? "" : claudeForKey ? " It can use Claude." : " It can't use Claude (an admin can allow it).";
    return { tone: "ok", text: `Using your Abhoth key: the models this key may use, from anywhere the router is reachable.${claude}` };
  }
  const before =
    kind === "placeholder"
      ? "LM Studio's placeholder key isn't sent to the router. "
      : kind === "other"
        ? `The API key from server mode isn't an Abhoth key (${prefix}…), so it isn't sent to the router. `
        : "";
  if (auth.required) return { tone: "warn", text: `${before}This router needs an Abhoth key (${prefix}…): request one or paste one below.` };
  const more = !auth.keys ? "" : hasClaude ? " Add a key for Claude and for access from outside your LAN." : " Add a key for access from outside your LAN.";
  return { tone: more || before ? "info" : "ok", text: `${before}No key: LAN access to the local models.${more}` };
}

/** One line on how to get a key from this router. */
export function howToGetKey(auth: AuthLike): string {
  if (!auth.keys) return "This router doesn't use keys.";
  if (auth.keyRequests) return "Get a key: click Request a key, then an admin approves it in the router's web UI (Settings > Service keys).";
  return "Get a key from the router's admin (its web UI: Settings > Account > API keys, or Service keys), then paste it here.";
}

/** "14 min" / "40 s": how long a key request still waits. */
export function waitLeft(seconds: number | undefined): string {
  const s = Math.max(0, Math.round(seconds ?? 0));
  return s >= 90 ? `${Math.round(s / 60)} min` : `${s} s`;
}

/** The connection endpoints of a scope: a project's running session, or (null) the server's config.json. */
export function connectionPath(pid: string | null): string {
  return pid ? `/api/projects/${encodeURIComponent(pid)}/settings/connection` : "/api/settings/connection";
}

/** Where the panel's changes go, as it tells the user. */
export function savedNote(project: boolean): string {
  return project
    ? "Saved to this project's session settings and its config.json, as /config in its terminal would: the session connects this way when it restarts."
    : "Saved to the server's config.json: new sessions use it (restart the app to update the profile picker).";
}

/** What GET /api/settings/connection/router-admin answers (D151). */
export interface RouterAdminDoc {
  available: boolean;
  /** The router's own admin page ("Open in browser"). */
  url?: string;
  /** The page through this server (the key stays on the server). */
  embedded?: string;
  principal?: string;
  reason?: string;
}

/**
 * Whether the panel shows "Router admin" (D151): router mode, and the router says the key the
 * server keeps for it (config.json's, whichever scope the page shows) is an admin's.
 */
export function routerAdminEntry(mode: ConnectionMode, doc: RouterAdminDoc | null | undefined): { show: boolean; url?: string; embedded?: string } {
  if (mode !== "router" || !doc?.available || !doc.embedded) return { show: false };
  return { show: true, url: doc.url, embedded: doc.embedded };
}

export type RouterPick = { kind: "adopt"; url: string } | { kind: "find" } | { kind: "none" };

/**
 * What clicking the "Abhoth router" card does: nothing when already in router mode; router mode on
 * the Base URL when that is a router, or on the one router found on the network; otherwise ask for
 * one (the router search and URL box, focused).
 */
export function routerPick(
  mode: ConnectionMode,
  status: { baseUrl: string; router: unknown | null } | null | undefined,
  found: { url: string; verified: boolean }[] | undefined,
): RouterPick {
  if (mode === "router") return { kind: "none" };
  if (status?.router) return { kind: "adopt", url: status.baseUrl };
  const live = (found ?? []).filter((r) => r.verified);
  if (live.length === 1) return { kind: "adopt", url: live[0].url };
  return { kind: "find" };
}
