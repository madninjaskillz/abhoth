// The connection (D149): which settings rows router mode shows, and how router models read (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CONNECTION_KEY, capabilityBadges, formatTokens, hiddenInMode, modeOf, normalizeUrl, pickerOrder, statusLine, visibleInMode,
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
  assert.deepEqual(statusLine({ connection: "server", suggestRouter: true, router: { name: "DT" } }), { text: "The Base URL is DT, an Abhoth router.", offer: true });
  assert.equal(statusLine({ connection: "server", suggestRouter: false, router: null }).offer, false);
  assert.deepEqual(statusLine({ connection: "router", suggestRouter: false, router: { name: "DT" } }), { text: "Connected to DT.", offer: false });
  assert.match(statusLine({ connection: "router", suggestRouter: false, router: null }).text, /does not answer/);
});
