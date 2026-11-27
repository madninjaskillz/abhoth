// The connection (D149): which settings rows router mode shows, and how router models read (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CONNECTION_KEY, capabilityBadges, connectionPath, formatTokens, hiddenInMode, modeOf, normalizeUrl, pickerOrder, routerPick, savedNote, statusLine, visibleInMode, keyHint, howToGetKey, waitLeft,
} from "../src/features/settings/connection.ts";

const s = (key: string, routerMode?: "shown" | "hidden" | "router") => ({ key, routerMode });
// The LLM Server and Context sections as the server describes them (orca-core config::schema).
const sections = [
  {
    name: "LLM Server",
    settings: [
      s(CONNECTION_KEY, "shown"), s("lmStudio.baseUrl", "shown"), s("lmStudio.apiKey", "shown"), s("lmStudio.models", "router"),
      s("lmStudio.backupMinResponseLength", "hidden"), s("lmStudio.backupOnRefusal", "hidden"), s("lmStudio.pairModelProgramming", "hidden"),
      s("lmStudio.temperature", "shown"), s("lmStudio.maxTokens", "shown"), s("lmStudio.timeoutSeconds", "shown"), s("lmStudio.nativeToolCalling", "router"),
    ],
  },
  { name: "Context", settings: [s("context.contextWindowSize", "router"), s("context.autoCompactEnabled", "shown")] },
  { name: "Session", settings: [s("session.autoSave"), s("profiles.active", "hidden")] },
];
const shown = (mode: "server" | "router") => sections.flatMap((x) => x.settings.filter((y) => visibleInMode(y, mode)).map((y) => y.key));

test("server mode shows every row but the connection itself (the panel draws it)", () => {
  const keys = shown("server");
  assert.ok(!keys.includes(CONNECTION_KEY));
  assert.equal(keys.length, sections.flatMap((x) => x.settings).length - 1);
});

test("router mode keeps the URL, key and the client's own choices, and hides what the router owns", () => {
  assert.deepEqual(shown("router"), [
    "lmStudio.baseUrl", "lmStudio.apiKey", "lmStudio.temperature", "lmStudio.maxTokens", "lmStudio.timeoutSeconds",
    "context.autoCompactEnabled", "session.autoSave",
  ]);
  assert.deepEqual(hiddenInMode(sections, "router").map((x) => x.key), [
    "lmStudio.models", "lmStudio.backupMinResponseLength", "lmStudio.backupOnRefusal", "lmStudio.pairModelProgramming",
    "lmStudio.nativeToolCalling", "context.contextWindowSize", "profiles.active",
  ]);
  assert.deepEqual(hiddenInMode(sections, "server"), []);
});

test("a setting from an older server (no routerMode) stays visible in router mode", () => {
  assert.ok(visibleInMode({ key: "x.y" }, "router"));
});

test("the document's mode, older servers being server mode", () => {
  assert.equal(modeOf({ connection: "router" }), "router");
  assert.equal(modeOf({ connection: "server" }), "server");
  assert.equal(modeOf({}), "server");
  assert.equal(modeOf(undefined), "server");
});

test("token counts read short", () => {
  assert.equal(formatTokens(65536), "64K");
  assert.equal(formatTokens(131072), "128K");
  assert.equal(formatTokens(1048576), "1M");
  assert.equal(formatTokens(200000), "195K");
  assert.equal(formatTokens(512), "512");
  assert.equal(formatTokens(undefined), "?");
});

test("capabilities and the picker's order", () => {
  assert.deepEqual(capabilityBadges({ id: "c", tools: true, vision: true, reasoning: true, kind: "cloud" }), ["tools", "vision", "reasoning", "cloud"]);
  assert.deepEqual(capabilityBadges({ id: "m" }), []);
  const order = pickerOrder([
    { id: "z-cold", status: "cold", kind: "local" },
    { id: "claude-opus", kind: "cloud" },
    { id: "b-live", status: "live", kind: "local" },
    { id: "a-live", status: "live", kind: "local" },
  ]).map((m) => m.id);
  assert.deepEqual(order, ["a-live", "b-live", "claude-opus", "z-cold"]);
});

test("typed URLs get a scheme", () => {
  assert.equal(normalizeUrl(" 10.1.2.3:8010 "), "http://10.1.2.3:8010");
  assert.equal(normalizeUrl("https://r.example/v1"), "https://r.example/v1");
  assert.equal(normalizeUrl(""), "");
});

test("router mode is offered when server mode points at a router, never forced", () => {
  assert.deepEqual(statusLine({ connection: "server", suggestRouter: true, router: { name: "DT" } }), { text: "The Base URL is DT, a Sothoth Router.", offer: true });
  assert.equal(statusLine({ connection: "server", suggestRouter: false, router: null }).offer, false);
  assert.deepEqual(statusLine({ connection: "router", suggestRouter: false, router: { name: "DT" } }), { text: "Connected to DT.", offer: false });
  assert.match(statusLine({ connection: "router", suggestRouter: false, router: null }).text, /does not answer/);
});

const lan = { required: false, keys: true, keyPrefix: "abk_" };

test("keyHint: LM Studio's placeholder reads as no key, not a problem", () => {
  const h = keyHint("placeholder", lan, false);
  assert.equal(h.tone, "info");
  assert.match(h.text, /isn't sent to the router/);
  assert.match(h.text, /No key: LAN access to the local models/);
});

test("keyHint: says what a key adds (Claude, outside the LAN)", () => {
  assert.match(keyHint("none", lan, true).text, /Add a key for Claude and for access from outside your LAN/);
  assert.match(keyHint("none", lan, false).text, /Add a key for access from outside your LAN/);
  assert.equal(keyHint("none", { ...lan, keys: false }, false).tone, "ok");
});

test("keyHint: another server's key is a gentle note, never an error", () => {
  const h = keyHint("other", lan, true);
  assert.equal(h.tone, "info");
  assert.match(h.text, /isn't an Abhoth key \(abk_…\), so it isn't sent/);
  assert.doesNotMatch(h.text, /refuse/);
});

test("keyHint: warns only when the router needs a key", () => {
  assert.equal(keyHint("placeholder", { ...lan, required: true }, false).tone, "warn");
  assert.match(keyHint("none", { ...lan, required: true }, false).text, /request one or paste one/);
  assert.equal(keyHint("abhoth", { ...lan, required: true }, true).tone, "ok");
  assert.match(keyHint("abhoth", lan, true, true).text, /can use Claude/);
  assert.match(keyHint("abhoth", lan, true, false).text, /can't use Claude/);
});

test("how to get a key, and how long a request waits", () => {
  assert.match(howToGetKey({ ...lan, keyRequests: true }), /click Request a key, then an admin approves it in the router's web UI \(Settings > Service keys\)/);
  assert.match(howToGetKey(lan), /paste it here/);
  assert.equal(waitLeft(840), "14 min");
  assert.equal(waitLeft(40), "40 s");
  assert.equal(waitLeft(undefined), "0 s");
});

test("the panel talks to the scope it is in: a project's session, or the server's config.json", () => {
  assert.equal(connectionPath(null), "/api/settings/connection");
  assert.equal(connectionPath("abc"), "/api/projects/abc/settings/connection");
  assert.equal(connectionPath("a b"), "/api/projects/a%20b/settings/connection");
  assert.match(savedNote(true), /this project's session/);
  assert.match(savedNote(false), /server's config\.json/);
});

test("clicking the router card switches when a router is known, and asks for one otherwise", () => {
  const router = { name: "R" };
  // Already router mode: nothing.
  assert.deepEqual(routerPick("router", { baseUrl: "http://r:8010/v1", router }, []), { kind: "none" });
  // The Base URL is a router: use it.
  assert.deepEqual(routerPick("server", { baseUrl: "http://r:8010/v1", router }, []), { kind: "adopt", url: "http://r:8010/v1" });
  // LM Studio's URL, one router found on the network: that one.
  const lm = { baseUrl: "http://localhost:1234/v1", router: null };
  assert.deepEqual(routerPick("server", lm, [{ url: "http://gpu:8010", verified: true }, { url: "http://old:8010", verified: false }]), {
    kind: "adopt",
    url: "http://gpu:8010",
  });
  // None, several, or no search yet: ask (the URL box).
  assert.deepEqual(routerPick("server", lm, undefined), { kind: "find" });
  assert.deepEqual(routerPick("server", lm, [{ url: "http://a:8010", verified: true }, { url: "http://b:8010", verified: true }]), { kind: "find" });
  assert.deepEqual(routerPick("server", undefined, []), { kind: "find" });
});

test("Router admin shows only in router mode, for an admin key (D151)", async () => {
  const { routerAdminEntry } = await import("../src/features/settings/connection.ts");
  const doc = { available: true, url: "http://router:8010/admin/", embedded: "/api/settings/connection/router-admin/ui/", principal: "router-key:ops#1" };
  assert.deepEqual(routerAdminEntry("router", doc), { show: true, url: doc.url, embedded: doc.embedded });
  assert.equal(routerAdminEntry("server", doc).show, false, "server mode");
  assert.equal(routerAdminEntry("router", { available: false, reason: "not an admin key" }).show, false, "not an admin");
  assert.equal(routerAdminEntry("router", { available: true }).show, false, "nowhere to open it");
  assert.equal(routerAdminEntry("router", undefined).show, false, "not known yet");
});
