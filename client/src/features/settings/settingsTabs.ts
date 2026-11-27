// The settings dialog's tabs (D143): which tabs there are, who sees them, where each server
// setting goes (tab, group, Advanced or not), the search across all tabs, the URL hash and the
// ARIA tab keys. Pure (no imports, no DOM), so plain node can test it (test/settingsTabs.test.ts).
//
// Tabs come from one list with visibility rules, in groups shown as headings in the tab list.
// Two kinds: "config" tabs show the server's settings, "pane" tabs show their own page (Account,
// Users, This Machine...). A config tab's settings come in groups, each a heading with one line
// under it; rarely used groups are "advanced" (collapsed until opened). A setting goes where a
// group names its key; a key no group names goes to the group that claims its schema section,
// and a section no group claims lands in Advanced. Setting keys never change here, only where
// they are shown, so stored configs keep loading.

export type TabKind = "config" | "pane";

export interface TabContext {
  /** The signed-in user is an admin (server settings and users are admin-only; the server enforces it). */
  admin: boolean;
  /** The desktop app: its window bridge is there, or the server says it is the desktop app's. */
  desktop: boolean;
  /** A Sothoth Router control plane is linked to this server (its admin tabs). */
  router?: boolean;
}

/** The headings of the tab list, in order. */
export const TAB_GROUPS = [
  { id: "app", label: "This app" },
  { id: "pc", label: "This PC" },
  { id: "you", label: "People" },
  { id: "router", label: "Sothoth Router admin" },
  { id: "more", label: "More" },
] as const;
export type TabGroup = (typeof TAB_GROUPS)[number]["id"];

export interface GroupDef {
  title: string;
  /** One line under the heading. */
  blurb?: string;
  /** The keys shown, in order. */
  keys?: string[];
  /** Also every other key of this schema section that no group names (keys added later land here). */
  section?: string;
  /** Rarely used: collapsed under the tab's other groups until opened. */
  advanced?: boolean;
}

export interface TabDef {
  id: string;
  label: string;
  kind: TabKind;
  group: TabGroup;
  /** One line under the tab's title. */
  blurb: string;
  /** Config tabs: the groups of settings shown, in order. */
  groups?: GroupDef[];
  /** Config tabs with a panel of their own besides the settings (always shown when the tab is visible). */
  panel?: "connection" | "migo";
  /** Words the search finds the tab (and its panel) by. */
  keywords?: string[];
  visible: (c: TabContext) => boolean;
}

const admins = (c: TabContext) => c.admin;
const desktopAdmins = (c: TabContext) => c.admin && c.desktop;
const routerAdmins = (c: TabContext) => c.admin && !!c.router;

export const SETTINGS_TABS: TabDef[] = [
  {
    id: "general",
    label: "General",
    kind: "config",
    group: "app",
    blurb: "How chat sessions are kept, and the profile new ones start with.",
    groups: [{ title: "Sessions", blurb: "Saved chat sessions.", keys: ["session.autoSave", "session.maxSessions", "profiles.active"], section: "Session" }],
    visible: admins,
  },
  {
    id: "connection",
    label: "Sothoth Router",
    kind: "config",
    group: "app",
    panel: "connection",
    blurb: "Sothoth Router: where this app's models come from. Connect to your Sothoth Router, or set up an OpenAI-compatible server instead.",
    keywords: ["sothoth", "router", "connection", "connect", "key", "abhoth key", "sign in with abhoth", "key request", "openai", "lm studio", "default model", "find router"],
    groups: [
      {
        title: "Address and key",
        blurb: "The server the models run on and the key sent to it.",
        keys: ["lmStudio.connection", "lmStudio.baseUrl", "lmStudio.apiKey", "lmStudio.models", "lmStudio.timeoutSeconds"],
      },
    ],
    visible: admins,
  },
  {
    id: "models",
    label: "Models & Providers",
    kind: "config",
    group: "app",
    blurb: "How the model answers, how much it remembers, and what happens when a model fails.",
    groups: [
      {
        title: "Responses",
        blurb: "Sampling and the size of each answer.",
        keys: ["lmStudio.temperature", "lmStudio.maxTokens", "lmStudio.nativeToolCalling"],
        section: "LLM Server",
      },
      {
        title: "Context window",
        blurb: "How much of the conversation the model sees, and when it is compacted.",
        keys: ["context.contextWindowSize", "context.autoCompactEnabled", "context.autoCompactThreshold"],
        section: "Context",
      },
      {
        title: "Failover between models",
        blurb: "Moving on to the next model in the list, or running them side by side.",
        keys: ["lmStudio.backupMinResponseLength", "lmStudio.backupOnRefusal", "lmStudio.pairModelProgramming"],
        advanced: true,
      },
      {
        title: "Compaction tuning",
        blurb: "Token estimates and how much compaction keeps.",
        keys: ["context.charsPerToken", "context.reserveTokens", "context.keepRecentTokens"],
        advanced: true,
      },
    ],
    visible: admins,
  },
  {
    id: "agent",
    label: "Agent & tools",
    kind: "config",
    group: "app",
    blurb: "What the agent may do without asking, and which tools it has.",
    groups: [
      {
        title: "Approvals",
        blurb: "Which tools run without asking first.",
        keys: ["permissions.autoApproveReadOnly", "permissions.autoApproveModerate", "permissions.autoApproveAll"],
        section: "Permissions",
      },
      { title: "Tool lists", blurb: "Tools that never ask, and tools that are turned off.", keys: ["permissions.alwaysApprove", "permissions.disabledTools"] },
    ],
    visible: admins,
  },
  {
    id: "images",
    label: "Images",
    kind: "config",
    group: "app",
    blurb: "Image generation: the server the agent's image tool uses.",
    groups: [
      { title: "Image server", blurb: "Turn the image tool on and point it at a server.", keys: ["imageGen.enabled", "imageGen.baseUrl", "imageGen.apiKey", "imageGen.model"], section: "Image Generation" },
      { title: "More image settings", blurb: "Timeouts, where images are saved, and the /uncensored model.", keys: ["imageGen.timeoutSeconds", "imageGen.outputDirectory", "agent.uncensored.imageModel"], advanced: true },
    ],
    visible: admins,
  },
  {
    id: "desktop",
    label: "Desktop app",
    kind: "config",
    group: "app",
    blurb: "The desktop app's window: its title bar, and closing to the tray.",
    groups: [{ title: "Window", blurb: "Applies to this desktop app's window.", keys: ["desktop.closeToTray", "desktop.systemTitleBar"], section: "Desktop" }],
    visible: desktopAdmins,
  },
  {
    id: "migo",
    label: "Migo Bridge",
    kind: "config",
    group: "pc",
    panel: "migo",
    blurb: "Migo Bridge: share this PC's GPUs and engines with your Sothoth Router.",
    keywords: ["migo", "bridge", "share", "sharing", "approve", "approvals", "pending", "remote config", "undo", "audit", "changes by a router admin"],
    groups: [
      {
        title: "Migo Bridge",
        blurb: "While the app runs, this PC dials your Sothoth Router and offers it the engines you share (no port opens here).",
        keys: ["desktop.bridgeMode", "desktop.bridgeRouter", "desktop.bridgeId", "desktop.bridgeToken"],
      },
      {
        title: "Remote config",
        blurb: "Whether a Sothoth Router admin may change this machine's setup. Changes are checked, backed up and can be undone below.",
        keys: ["desktop.bridgeRemoteConfig"],
      },
      { title: "Engines file", blurb: "Run Migo Bridge from a migo.toml instead of This Machine (headless-style setups).", keys: ["desktop.bridgeConfig"], advanced: true },
    ],
    visible: desktopAdmins,
  },
  {
    id: "machine",
    label: "This Machine",
    kind: "pane",
    group: "pc",
    blurb: "This PC's cards, engines and models: run them here, and pick what Migo Bridge shares with your Sothoth Router.",
    keywords: ["this machine", "local", "engines", "cards", "gpu", "kokoro", "comfyui", "engine store", "manual engines", "inventory", "running", "huggingface", "models"],
    visible: desktopAdmins,
  },
  {
    id: "account",
    label: "Account",
    kind: "pane",
    group: "you",
    blurb: "Your password, two-factor sign-in, API keys and signed-in sessions.",
    keywords: ["account", "password", "two-factor", "2fa", "totp", "authenticator", "recovery codes", "api keys", "tokens", "sessions", "sign out", "devices"],
    visible: () => true,
  },
  {
    id: "users",
    label: "Users",
    kind: "pane",
    group: "you",
    blurb: "The users of this server and the audit log.",
    keywords: ["users", "people", "admin", "roles", "add user", "disable", "reset password", "audit log", "sign-ins"],
    visible: admins,
  },
  {
    id: "inference",
    label: "Inference",
    kind: "pane",
    group: "router",
    blurb: "Your Sothoth Router's GPU servers: cards, engines, temperatures, layouts and switches.",
    keywords: ["sothoth", "router", "gpu", "cards", "engines", "layouts", "switch", "vram", "temperature", "clocks", "guards", "nodens", "supervisor"],
    visible: routerAdmins,
  },
  {
    id: "routes",
    label: "Routes",
    kind: "pane",
    group: "router",
    blurb: "Sothoth Router's model names, what happens when their model is cold, and who may use them.",
    keywords: ["routes", "model names", "cold", "fallback", "on demand", "access"],
    visible: routerAdmins,
  },
  {
    id: "router-config",
    label: "Router config",
    kind: "pane",
    group: "router",
    blurb: "Sothoth Router's sothoth.toml, checked as you type; saves are backed up and audited.",
    keywords: ["sothoth.toml", "router.toml", "config", "toml", "validation"],
    visible: routerAdmins,
  },
  {
    id: "router-activity",
    label: "Jobs & audit",
    kind: "pane",
    group: "router",
    blurb: "Sothoth Router's switch jobs and their steps, its audit log, and slot leases.",
    keywords: ["jobs", "history", "audit", "leases", "slots"],
    visible: routerAdmins,
  },
  {
    id: "calendar",
    label: "Calendar",
    kind: "pane",
    group: "router",
    blurb: "Events that reserve or block cards (pipelines, benches, maintenance), with live detection and event keys.",
    keywords: ["calendar", "schedule", "events", "pipelines", "bench", "maintenance", "reserve", "event keys", "cron", "ical"],
    visible: routerAdmins,
  },
  {
    id: "service-keys",
    label: "Service keys",
    kind: "pane",
    group: "router",
    blurb: "Key requests to approve, and named keys for Jarvis, pipelines, Home Assistant and the dash.",
    keywords: ["service keys", "jarvis", "home assistant", "pipelines", "dash", "scopes", "revoke", "key requests", "approve", "request a key"],
    visible: routerAdmins,
  },
  {
    id: "advanced",
    label: "Advanced",
    kind: "config",
    group: "more",
    blurb: "Settings without a tab of their own yet.",
    groups: [],
    visible: admins,
  },
  {
    id: "about",
    label: "About",
    kind: "pane",
    group: "more",
    blurb: "This app's version, and what Sothoth Router, Migo Bridge and Nodens Supervisor are.",
    keywords: ["about", "version", "names", "sothoth", "migo", "nodens", "supervisor", "router", "bridge"],
    visible: () => true,
  },
];

/** Old and plain names for tabs, so earlier links (#settings/<id>) keep opening the right one. */
export const TAB_ALIASES: Record<string, string> = {
  sothoth: "connection",
  router: "connection",
  "router-mode": "connection",
  bridge: "migo",
  "migo-bridge": "migo",
  sharing: "migo",
  approvals: "migo",
  "remote-config": "migo",
  "this-machine": "machine",
  window: "desktop",
  context: "models",
  permissions: "agent",
  "sothoth-config": "router-config",
};

export function tabById(id: string | null | undefined): TabDef | undefined {
  const real = id && (TAB_ALIASES[id] ?? id);
  return SETTINGS_TABS.find((t) => t.id === real);
}

export interface Placement {
  tab: string;
  /** The group's title (Advanced: the schema section's name). */
  group: string;
  advanced: boolean;
}

/** Where a setting is shown: by its key, else by its schema section, else in Advanced. */
export function placeSetting(section: string, key: string): Placement {
  for (const t of SETTINGS_TABS)
    for (const g of t.groups ?? []) if (g.keys?.includes(key)) return { tab: t.id, group: g.title, advanced: !!g.advanced };
  for (const t of SETTINGS_TABS)
    for (const g of t.groups ?? []) if (g.section === section) return { tab: t.id, group: g.title, advanced: !!g.advanced };
  return { tab: "advanced", group: section, advanced: false };
}

/** The tab a schema section's settings go to by default (Advanced for a section no group claims). */
export function tabForSection(section: string): string {
  return placeSetting(section, "\u0000").tab;
}

export interface SectionLike<S> {
  name: string;
  settings: S[];
}

/** A group of settings as a tab shows it. `name` is the heading. */
export interface Group<S> extends SectionLike<S> {
  blurb?: string;
  advanced: boolean;
}

/**
 * A config tab's groups from the schema, in the tab's order (Advanced: one per unclaimed schema
 * section, in schema order), each with its settings in the group's key order (then schema order
 * for the keys its section brings). Empty groups are left out.
 */
export function groupsOfTab<S extends { key: string }>(tab: TabDef, all: SectionLike<S>[]): Group<S>[] {
  if (tab.kind !== "config") return [];
  const placed = all.flatMap((sec) => sec.settings.map((s) => ({ s, p: placeSetting(sec.name, s.key), section: sec.name })));
  if (tab.id === "advanced")
    return all
      .map((sec) => ({ name: sec.name, advanced: false, settings: placed.filter((x) => x.p.tab === "advanced" && x.section === sec.name).map((x) => x.s) }))
      .filter((g) => g.settings.length > 0);
  return (tab.groups ?? [])
    .map((g) => {
      const mine = placed.filter((x) => x.p.tab === tab.id && x.p.group === g.title);
      const order = (k: string) => {
        const i = g.keys?.indexOf(k) ?? -1;
        return i < 0 ? Number.MAX_SAFE_INTEGER : i;
      };
      const settings = mine.map((x, i) => ({ x, i })).sort((a, b) => order(a.x.s.key) - order(b.x.s.key) || a.i - b.i).map((y) => y.x.s);
      return { name: g.title, blurb: g.blurb, advanced: !!g.advanced, settings };
    })
    .filter((g) => g.settings.length > 0);
}

/** Kept for callers that think in sections: a tab's groups. */
export const sectionsOfTab = groupsOfTab;

/**
 * The tabs a user sees: visible by its rule, and a config tab only when it has settings (once
 * the schema is loaded; `all` undefined = not loaded yet, show them) or a panel of its own.
 */
export function visibleTabs(c: TabContext, all?: SectionLike<{ key: string }>[]): TabDef[] {
  return SETTINGS_TABS.filter((t) => {
    if (!t.visible(c)) return false;
    if (t.kind === "config" && t.panel) return true;
    if (t.kind === "config" && all) return groupsOfTab(t, all).some((s) => s.settings.length > 0);
    if (t.kind === "config" && !all) return t.id !== "advanced";
    return true;
  });
}

export interface SearchableSetting {
  key: string;
  label: string;
  description: string;
  profile?: string | null;
}

/** Whether a setting matches the search (label, key, description, section, profile). */
export function settingMatches(s: SearchableSetting, section: string, q: string): boolean {
  const t = q.trim().toLowerCase();
  if (!t) return true;
  return [s.label, s.key, s.description, section, s.profile ?? ""].some((x) => x.toLowerCase().includes(t));
}

/** Whether a pane tab, or a config tab's own panel, matches the search (its name, blurb or keywords). */
export function paneMatches(tab: TabDef, q: string): boolean {
  const t = q.trim().toLowerCase();
  if (!t || (tab.kind !== "pane" && !tab.panel)) return false;
  return [tab.label, tab.blurb, ...(tab.keywords ?? [])].some((x) => x.toLowerCase().includes(t));
}

export interface SearchHit<T extends SectionLike<SearchableSetting>> {
  tab: TabDef;
  /** Config tabs: the groups with their matching settings. */
  sections: T[];
  /** Pane tabs (and config tabs' panels): the tab itself matched. */
  pane: boolean;
}

/** The search across every visible tab, grouped by tab in tab order. */
export function searchTabs<S extends SearchableSetting>(
  tabs: TabDef[],
  all: SectionLike<S>[],
  q: string,
  hide: (s: S) => boolean = () => false,
): SearchHit<Group<S>>[] {
  const out: SearchHit<Group<S>>[] = [];
  // A setting is found by its schema section's name too (the server's own grouping).
  const sectionOf = new Map<string, string>();
  for (const sec of all) for (const x of sec.settings) sectionOf.set(x.key, sec.name);
  for (const tab of tabs) {
    const pane = paneMatches(tab, q);
    if (tab.kind === "pane") {
      if (pane) out.push({ tab, sections: [], pane: true });
      continue;
    }
    const sections = groupsOfTab(tab, all)
      .map((g) => ({ ...g, settings: g.settings.filter((x) => !hide(x) && settingMatches(x, `${g.name} ${sectionOf.get(x.key) ?? ""}`, q)) }))
      .filter((g) => g.settings.length > 0);
    if (sections.length > 0 || pane) out.push({ tab, sections, pane });
  }
  return out;
}

const HASH = "#settings";

/** The tab a URL hash opens: `#settings` (the first tab: ""), `#settings/models`; null when the hash isn't the settings'. */
export function parseSettingsHash(hash: string): string | null {
  if (hash === HASH) return "";
  if (!hash.startsWith(HASH + "/")) return null;
  return decodeURIComponent(hash.slice(HASH.length + 1));
}

export function settingsHash(tab: string): string {
  return `${HASH}/${encodeURIComponent(tab)}`;
}

/** The tab to show for a requested id (or an old name for it): it if the user sees it, else the first they see. */
export function pickTab(requested: string | null | undefined, tabs: TabDef[]): string | undefined {
  const id = requested ? (TAB_ALIASES[requested] ?? requested) : requested;
  return tabs.find((t) => t.id === id)?.id ?? tabs[0]?.id;
}

/** ARIA tabs keys (vertical list, also left/right): the index the focus moves to, or `at` for other keys. */
export function nextTabIndex(at: number, count: number, key: string): number {
  if (count === 0) return at;
  switch (key) {
    case "ArrowDown":
    case "ArrowRight":
      return (at + 1) % count;
    case "ArrowUp":
    case "ArrowLeft":
      return (at - 1 + count) % count;
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return at;
  }
}

/** How many unsaved edits each tab has, from the edited setting keys and the schema. */
export function unsavedByTab(dirty: Iterable<string>, all: SectionLike<{ key: string }>[]): Record<string, number> {
  const out: Record<string, number> = {};
  const keys = new Set(dirty);
  for (const s of all)
    for (const x of s.settings)
      if (keys.has(x.key)) {
        const t = placeSetting(s.name, x.key).tab;
        out[t] = (out[t] ?? 0) + 1;
      }
  return out;
}
