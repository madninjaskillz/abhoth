// The settings dialog's tabs: who sees which, where each setting goes, search, hash and keys (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  SETTINGS_TABS, TAB_ALIASES, TAB_GROUPS, groupsOfTab, nextTabIndex, paneMatches, parseSettingsHash, pickTab, placeSetting, searchTabs, settingsHash, tabById,
  tabForSection, unsavedByTab, visibleTabs,
} from "../src/features/settings/settingsTabs.ts";

const s = (key: string, label = key, description = "") => ({ key, label, description, profile: null as string | null });

// The server's real schema (orca-core config/schema.rs): every section and key, read from the source,
// so a setting added there without a place here fails this test instead of vanishing.
const rust = readFileSync(new URL("../../crates/orca-core/src/config/schema.rs", import.meta.url), "utf8");
const consts: Record<string, string> = {};
for (const m of rust.matchAll(/pub const (\w+): &str = "([^"]*)";/g)) consts[m[1]] = m[2];
const realSchema: { name: string; settings: ReturnType<typeof s>[] }[] = [];
for (const m of rust.matchAll(/SettingDef::new\(\s*(\w+),\s*("([^"]+)"|(\w+)),/g)) {
  const section = consts[m[1]];
  const key = m[3] ?? consts[m[4]];
  assert.ok(section && key, `parsed ${m[0]}`);
  let sec = realSchema.find((x) => x.name === section);
  if (!sec) realSchema.push((sec = { name: section, settings: [] }));
  sec.settings.push(s(key));
}

// A small schema for the visibility and search tests.
const schema = [
  { name: "LLM Server", settings: [s("lmStudio.baseUrl", "Base URL", "The OpenAI-compatible server"), s("lmStudio.temperature", "Temperature"), s("lmStudio.backupOnRefusal", "Backup On Refusal")] },
  { name: "Image Generation", settings: [s("imageGen.enabled", "Enabled"), s("imageGen.baseUrl", "Base URL")] },
  { name: "Permissions", settings: [s("permissions.autoApproveAll", "Auto-approve ALL"), s("permissions.disabledTools", "Disabled tools")] },
  { name: "Session", settings: [s("session.autoSave", "Auto-save"), s("profiles.active", "Default profile")] },
  { name: "Context", settings: [s("context.contextWindowSize", "Context window size"), s("context.charsPerToken", "Chars per token")] },
  { name: "Desktop", settings: [s("desktop.closeToTray", "Close to tray"), s("desktop.systemTitleBar", "Use system title bar"), s("desktop.bridgeMode", "Migo Bridge"), s("desktop.bridgeConfig", "Migo Bridge engines file")] },
];
const ids = (t: { id: string }[]) => t.map((x) => x.id);
const admin = { admin: true, desktop: false };
const desk = { admin: true, desktop: true };
const user = { admin: false, desktop: false };

test("the real schema was read", () => {
  assert.ok(realSchema.length >= 6);
  assert.ok(realSchema.flatMap((x) => x.settings).length >= 40);
});

test("every real setting has a named place (a group names its key), and nothing lands in Advanced", () => {
  const named = new Set(SETTINGS_TABS.flatMap((t) => (t.groups ?? []).flatMap((g) => g.keys ?? [])));
  for (const sec of realSchema)
    for (const x of sec.settings) {
      assert.ok(named.has(x.key), `${x.key} is not in any group`);
      assert.notEqual(placeSetting(sec.name, x.key).tab, "advanced", x.key);
    }
});

test("every real setting is reachable: its tab is visible to a desktop admin and shows it", () => {
  const tabs = visibleTabs(desk, realSchema);
  for (const sec of realSchema)
    for (const x of sec.settings) {
      const p = placeSetting(sec.name, x.key);
      const tab = tabs.find((t) => t.id === p.tab);
      assert.ok(tab, `${x.key}: tab ${p.tab} not visible`);
      assert.ok(groupsOfTab(tab, realSchema).some((g) => g.name === p.group && g.settings.some((y) => y.key === x.key)), `${x.key} not shown on ${p.tab}`);
      // ...and the search finds it by its key.
      assert.ok(searchTabs(tabs, realSchema, x.key).some((h) => h.sections.some((g) => g.settings.some((y) => y.key === x.key))), `${x.key} not found by search`);
    }
});

test("on the web (not the desktop app), only the desktop app's own settings are out of reach", () => {
  const tabs = visibleTabs(admin, realSchema);
  for (const sec of realSchema)
    for (const x of sec.settings) {
      const shown = tabs.some((t) => t.id === placeSetting(sec.name, x.key).tab);
      assert.equal(shown, !x.key.startsWith("desktop."), x.key);
    }
});

test("each key is named by at most one group, and every group has a line under it", () => {
  const seen = new Map<string, string>();
  for (const t of SETTINGS_TABS)
    for (const g of t.groups ?? []) {
      assert.ok(g.blurb, `${t.id}/${g.title} has no blurb`);
      for (const k of g.keys ?? []) {
        assert.ok(!seen.has(k), `${k} in ${seen.get(k)} and ${t.id}`);
        seen.set(k, t.id);
      }
    }
  for (const t of SETTINGS_TABS) assert.ok(t.blurb.length > 10, t.id);
});

test("settings go where their group says; unknown keys follow their section; unknown sections go to Advanced", () => {
  assert.deepEqual(placeSetting("LLM Server", "lmStudio.baseUrl"), { tab: "connection", group: "Address and key", advanced: false });
  assert.deepEqual(placeSetting("LLM Server", "lmStudio.backupOnRefusal"), { tab: "models", group: "Failover between models", advanced: true });
  assert.equal(placeSetting("Context", "context.charsPerToken").advanced, true);
  assert.equal(placeSetting("Desktop", "desktop.bridgeMode").tab, "migo");
  assert.equal(placeSetting("Desktop", "desktop.closeToTray").tab, "desktop");
  // a key added to a known section later
  assert.deepEqual(placeSetting("LLM Server", "lmStudio.somethingNew"), { tab: "models", group: "Responses", advanced: false });
  assert.equal(placeSetting("Desktop", "desktop.somethingNew").tab, "desktop");
  assert.equal(tabForSection("Context"), "models");
  assert.equal(tabForSection("Something New"), "advanced");
});

test("admins see the app's tabs; desktop-only tabs only in the desktop app; Advanced only when a section needs it", () => {
  assert.deepEqual(ids(visibleTabs(admin, schema)), ["general", "connection", "models", "agent", "images", "account", "users", "about"]);
  assert.deepEqual(ids(visibleTabs(desk, schema)), ["general", "connection", "models", "agent", "images", "desktop", "updates", "migo", "local-engines", "machine", "account", "users", "about"]);
  const more = [...schema, { name: "Something New", settings: [s("x.y")] }];
  assert.ok(ids(visibleTabs(admin, more)).includes("advanced"));
  // Before the schema loads the tabs are there already (no jump when it arrives).
  assert.deepEqual(ids(visibleTabs(admin)), ["general", "connection", "models", "agent", "images", "account", "users", "about"]);
});

test("tabs come in their groups' order, so the list's headings never repeat", () => {
  const order = TAB_GROUPS.map((g) => g.id as string);
  const all = visibleTabs({ ...desk, router: true }, [...schema, { name: "Something New", settings: [s("x.y")] }]);
  const seq = all.map((t) => order.indexOf(t.group));
  assert.deepEqual(seq, [...seq].sort((a, b) => a - b));
});

test("other users see only their Account and About (server settings and users are admin-only)", () => {
  assert.deepEqual(ids(visibleTabs(user, schema)), ["account", "about"]);
  assert.deepEqual(ids(visibleTabs({ ...user, desktop: true })), ["account", "about"]);
  const routerTabs = ["inference", "routes", "router-config", "router-activity", "calendar", "service-keys"];
  assert.deepEqual(ids(visibleTabs({ ...admin, router: true })).filter((t) => routerTabs.includes(t)), routerTabs);
  assert.deepEqual(ids(visibleTabs({ ...user, router: true })), ["account", "about"]);
});

test("a config tab with no settings is left out, but one with a panel of its own stays", () => {
  const noImages = schema.filter((x) => x.name !== "Image Generation");
  assert.ok(!ids(visibleTabs(admin, noImages)).includes("images"));
  const bare = schema.filter((x) => x.name !== "LLM Server" && x.name !== "Desktop");
  assert.ok(ids(visibleTabs(desk, bare)).includes("connection"));
  assert.ok(ids(visibleTabs(desk, bare)).includes("migo"));
});

test("a tab's groups come in the tab's order, advanced ones flagged", () => {
  const g = groupsOfTab(tabById("models")!, schema);
  assert.deepEqual(g.map((x) => [x.name, x.advanced]), [["Responses", false], ["Context window", false], ["Failover between models", true], ["Compaction tuning", true]]);
  assert.deepEqual(groupsOfTab(tabById("account")!, schema), []);
  // keys in the group's order, not the schema's
  const real = groupsOfTab(tabById("migo")!, realSchema);
  assert.deepEqual(real[0].settings.map((x) => x.key), ["desktop.bridgeMode", "desktop.bridgeRouter", "desktop.bridgeId", "desktop.bridgeToken"]);
});

test("search covers all tabs and says which tab each match is on", () => {
  const tabs = visibleTabs(admin, schema);
  const hits = searchTabs(tabs, schema, "base url");
  assert.deepEqual(ids(hits.map((h) => h.tab)), ["connection", "images"]);
  assert.deepEqual(hits[0].sections[0].settings.map((x) => x.key), ["lmStudio.baseUrl"]);
  // By key, by description, by the server's section name, by the group's heading.
  assert.deepEqual(ids(searchTabs(visibleTabs(desk, schema), schema, "closeToTray").map((h) => h.tab)), ["desktop"]);
  assert.deepEqual(ids(searchTabs(tabs, schema, "openai-compatible").map((h) => h.tab)), ["connection"]);
  assert.deepEqual(ids(searchTabs(tabs, schema, "permissions").map((h) => h.tab)), ["agent"]);
  assert.ok(searchTabs(tabs, schema, "failover").some((h) => h.tab.id === "models" && h.sections[0].settings[0].key === "lmStudio.backupOnRefusal"));
  // Settings in collapsed Advanced groups are found too.
  assert.ok(searchTabs(tabs, schema, "chars per token").some((h) => h.tab.id === "models"));
  // The pane tabs, and the config tabs' own panels, by their keywords.
  const pw = searchTabs(tabs, schema, "password");
  assert.ok(pw.some((h) => h.tab.id === "account" && h.pane));
  assert.ok(searchTabs(tabs, schema, "audit").some((h) => h.tab.id === "users"));
  assert.ok(searchTabs(tabs, schema, "sign in with abhoth").some((h) => h.tab.id === "connection" && h.pane));
  assert.ok(searchTabs(visibleTabs(desk, schema), schema, "approvals").some((h) => h.tab.id === "migo" && h.pane));
  assert.deepEqual(searchTabs(tabs, schema, "zzz-nothing"), []);
  // Hidden keys are not found; a non-admin's search finds no server settings.
  assert.deepEqual(ids(searchTabs(tabs, schema, "default profile", (x) => x.key === "profiles.active").map((h) => h.tab)), []);
  assert.deepEqual(ids(searchTabs(visibleTabs(user, schema), schema, "base url").map((h) => h.tab)), []);
});

test("pane matching ignores plain config tabs and empty queries", () => {
  assert.equal(paneMatches(tabById("account")!, "2FA"), true);
  assert.equal(paneMatches(tabById("account")!, " "), false);
  assert.equal(paneMatches(tabById("models")!, "models"), false);
  assert.equal(paneMatches(tabById("migo")!, "migo"), true);
});

test("the hash names the tab", () => {
  assert.equal(parseSettingsHash("#settings"), "");
  assert.equal(parseSettingsHash("#settings/models"), "models");
  assert.equal(parseSettingsHash(settingsHash("agent")), "agent");
  assert.equal(parseSettingsHash("#bench"), null);
  assert.equal(parseSettingsHash(""), null);
  assert.equal(parseSettingsHash("#settingsx"), null);
});

test("a tab the user can't see opens the first one they can", () => {
  const tabs = visibleTabs(user, schema);
  assert.equal(pickTab("users", tabs), "account");
  assert.equal(pickTab("", visibleTabs(admin, schema)), "general");
  assert.equal(pickTab("desktop", visibleTabs(desk, schema)), "desktop");
  assert.equal(pickTab("desktop", visibleTabs(admin, schema)), "general");
});

test("old links keep working: every earlier tab id still opens, and old names map to the new tabs", () => {
  const before = ["general", "models", "agent", "images", "desktop", "advanced", "account", "users", "machine", "inference", "routes", "router-config", "router-activity", "calendar", "service-keys"];
  for (const id of before) assert.ok(tabById(id), id);
  const tabs = visibleTabs({ ...desk, router: true }, schema);
  assert.equal(pickTab("bridge", tabs), "migo");
  assert.equal(pickTab("router", tabs), "connection");
  assert.equal(pickTab("this-machine", tabs), "machine");
  assert.equal(pickTab("engines", tabs), "local-engines");
  assert.equal(pickTab("cloud", tabs), "local-engines");
  assert.equal(tabById("sothoth")?.id, "connection");
  for (const to of Object.values(TAB_ALIASES)) assert.ok(tabById(to), to);
});

test("arrow keys, Home and End move through the tabs and wrap", () => {
  assert.equal(nextTabIndex(0, 5, "ArrowDown"), 1);
  assert.equal(nextTabIndex(4, 5, "ArrowDown"), 0);
  assert.equal(nextTabIndex(0, 5, "ArrowUp"), 4);
  assert.equal(nextTabIndex(2, 5, "ArrowRight"), 3);
  assert.equal(nextTabIndex(2, 5, "ArrowLeft"), 1);
  assert.equal(nextTabIndex(3, 5, "Home"), 0);
  assert.equal(nextTabIndex(1, 5, "End"), 4);
  assert.equal(nextTabIndex(1, 5, "a"), 1);
  assert.equal(nextTabIndex(0, 0, "ArrowDown"), 0);
});

test("unsaved edits are counted per tab", () => {
  assert.deepEqual(unsavedByTab(["lmStudio.baseUrl", "context.contextWindowSize", "permissions.disabledTools", "lmStudio.backupOnRefusal"], schema), { connection: 1, models: 2, agent: 1 });
  assert.deepEqual(unsavedByTab(["desktop.bridgeMode"], schema), { migo: 1 });
  assert.deepEqual(unsavedByTab([], schema), {});
});

test("tab ids are unique, and the labels name the parts in full", () => {
  assert.equal(new Set(SETTINGS_TABS.map((t) => t.id)).size, SETTINGS_TABS.length);
  assert.equal(tabById("connection")!.label, "Sothoth Router");
  assert.equal(tabById("migo")!.label, "Migo Bridge");
  assert.match(tabById("migo")!.blurb, /^Migo Bridge: share this PC's GPUs and engines with your Sothoth Router/);
  // No bare "Migo"/"Sothoth"/"Nodens" in tab labels, group headings or blurbs.
  const texts = SETTINGS_TABS.flatMap((t) => [t.label, t.blurb, ...(t.groups ?? []).flatMap((g) => [g.title, g.blurb ?? ""])]);
  for (const x of texts) assert.doesNotMatch(x, /\b(Migo(?! Bridge)|Sothoth(?! Router)|Nodens(?! Supervisor))\b/, x);
});

test("the desktop owner sees This Machine and Migo Bridge in router mode too (a Sothoth Router linked or connected)", () => {
  // Router mode is a connection setting: it never changes who sees the PC's own tabs.
  const routerMode = { admin: true, desktop: true, router: true };
  const tabs = ids(visibleTabs(routerMode, schema));
  for (const id of ["desktop", "migo", "local-engines", "machine"]) assert.ok(tabs.includes(id), id);
  assert.equal(pickTab("machine", visibleTabs(routerMode, schema)), "machine");
});

test("Local Engines and This Machine are on every host that runs them, for admins only; Migo Bridge and the window stay desktop-only", () => {
  const web = { admin: true, desktop: false, machine: true };
  const tabs = ids(visibleTabs(web, schema));
  for (const id of ["local-engines", "machine"]) assert.ok(tabs.includes(id), id);
  for (const id of ["desktop", "migo"]) assert.ok(!tabs.includes(id), id);
  assert.ok(!ids(visibleTabs({ admin: false, desktop: false, machine: true }, schema)).includes("local-engines"));
  assert.ok(!ids(visibleTabs({ admin: true, desktop: false, machine: false }, schema)).includes("local-engines"));
  // found by search for what it holds
  const hits = searchTabs(visibleTabs(web, schema), schema, "ollama").map((h) => h.tab.id);
  assert.ok(hits.includes("local-engines"), hits.join(","));
});

test("Updates is a desktop-app tab with its own panel and the automatic-check setting", () => {
  assert.ok(ids(visibleTabs(desk, schema)).includes("updates"));
  assert.ok(!ids(visibleTabs({ admin: true, desktop: false, machine: true }, schema)).includes("updates"));
  assert.ok(!ids(visibleTabs({ admin: false, desktop: true }, schema)).includes("updates"));
  assert.equal(tabById("updates")!.panel, "updates");
  assert.deepEqual(placeSetting("Desktop", "desktop.autoUpdate"), { tab: "updates", group: "Automatic checks", advanced: false });
  const tabs = visibleTabs(desk, schema);
  assert.equal(pickTab("check-for-updates", tabs), "updates");
  assert.ok(searchTabs(tabs, schema, "check for updates").some((h) => h.tab.id === "updates"));
});
