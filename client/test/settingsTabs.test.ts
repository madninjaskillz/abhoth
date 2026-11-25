// The settings dialog's tabs: who sees which, where each setting goes, search, hash and keys (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  SETTINGS_TABS, nextTabIndex, paneMatches, parseSettingsHash, pickTab, searchTabs, sectionsOfTab, settingsHash, tabById,
  tabForSection, unsavedByTab, visibleTabs,
} from "../src/features/settings/settingsTabs.ts";

const s = (key: string, label = key, description = "") => ({ key, label, description, profile: null as string | null });
// The server's sections today (orca-core config::schema SECTIONS).
const schema = [
  { name: "LLM Server", settings: [s("lmStudio.baseUrl", "Base URL", "The OpenAI-compatible server"), s("lmStudio.temperature", "Temperature")] },
  { name: "Image Generation", settings: [s("imageGen.enabled", "Enabled"), s("imageGen.baseUrl", "Base URL")] },
  { name: "Permissions", settings: [s("permissions.autoApproveAll", "Auto-approve ALL"), s("permissions.disabledTools", "Disabled tools")] },
  { name: "Session", settings: [s("session.autoSave", "Auto-save"), s("profiles.active", "Default profile")] },
  { name: "Context", settings: [s("context.contextWindowSize", "Context window size")] },
  { name: "Desktop", settings: [s("desktop.closeToTray", "Close to tray"), s("desktop.systemTitleBar", "Use system title bar")] },
];
const ids = (t: { id: string }[]) => t.map((x) => x.id);
const admin = { admin: true, desktop: false };
const user = { admin: false, desktop: false };

test("every server section has a tab, and nothing lands in Advanced today", () => {
  for (const sec of schema) assert.notEqual(tabForSection(sec.name), "advanced", sec.name);
  assert.equal(tabForSection("LLM Server"), "models");
  assert.equal(tabForSection("Context"), "models");
  assert.equal(tabForSection("Desktop"), "desktop");
  assert.equal(tabForSection("Something New"), "advanced");
});

test("admins see the settings tabs, Account and Users; Advanced only when a section needs it", () => {
  assert.deepEqual(ids(visibleTabs(admin, schema)), ["general", "models", "agent", "images", "desktop", "account", "users"]);
  const more = [...schema, { name: "Something New", settings: [s("x.y")] }];
  assert.ok(ids(visibleTabs(admin, more)).includes("advanced"));
  // Before the schema loads the tabs are there already (no jump when it arrives).
  assert.deepEqual(ids(visibleTabs(admin)), ["general", "models", "agent", "images", "desktop", "account", "users"]);
});

test("other users see only their Account (server settings and users are admin-only)", () => {
  assert.deepEqual(ids(visibleTabs(user, schema)), ["account"]);
  assert.deepEqual(ids(visibleTabs(user)), ["account"]);
});

test("a config tab with no settings is left out", () => {
  const noImages = schema.filter((x) => x.name !== "Image Generation");
  assert.ok(!ids(visibleTabs(admin, noImages)).includes("images"));
});

test("a tab's sections come in the tab's order", () => {
  assert.deepEqual(sectionsOfTab(tabById("models")!, schema).map((x) => x.name), ["LLM Server", "Context"]);
  assert.deepEqual(sectionsOfTab(tabById("account")!, schema), []);
});

test("search covers all tabs and says which tab each match is on", () => {
  const tabs = visibleTabs(admin, schema);
  const hits = searchTabs(tabs, schema, "base url");
  assert.deepEqual(ids(hits.map((h) => h.tab)), ["models", "images"]);
  assert.deepEqual(hits[0].sections[0].settings.map((x) => x.key), ["lmStudio.baseUrl"]);
  // By key, by description, by section name.
  assert.deepEqual(ids(searchTabs(tabs, schema, "closeToTray").map((h) => h.tab)), ["desktop"]);
  assert.deepEqual(ids(searchTabs(tabs, schema, "openai-compatible").map((h) => h.tab)), ["models"]);
  assert.deepEqual(ids(searchTabs(tabs, schema, "permissions").map((h) => h.tab)), ["agent"]);
  // The pane tabs by their keywords.
  const pw = searchTabs(tabs, schema, "password");
  assert.ok(pw.some((h) => h.tab.id === "account" && h.pane));
  assert.ok(searchTabs(tabs, schema, "audit").some((h) => h.tab.id === "users"));
  assert.deepEqual(searchTabs(tabs, schema, "zzz-nothing"), []);
  // Hidden keys are not found; a non-admin's search finds no server settings.
  assert.deepEqual(ids(searchTabs(tabs, schema, "default profile", (x) => x.key === "profiles.active").map((h) => h.tab)), []);
  assert.deepEqual(ids(searchTabs(visibleTabs(user, schema), schema, "base url").map((h) => h.tab)), []);
});

test("pane matching ignores config tabs and empty queries", () => {
  assert.equal(paneMatches(tabById("account")!, "2FA"), true);
  assert.equal(paneMatches(tabById("account")!, " "), false);
  assert.equal(paneMatches(tabById("models")!, "models"), false);
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
  assert.equal(pickTab("desktop", visibleTabs(admin, schema)), "desktop");
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
  assert.deepEqual(unsavedByTab(["lmStudio.baseUrl", "context.contextWindowSize", "permissions.disabledTools"], schema), { models: 2, agent: 1 });
  assert.deepEqual(unsavedByTab([], schema), {});
});

test("tab ids are unique", () => {
  assert.equal(new Set(SETTINGS_TABS.map((t) => t.id)).size, SETTINGS_TABS.length);
});
