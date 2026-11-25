// The settings dialog's tabs (D143): which tabs there are, who sees them, which tab each server
// setting belongs to, the search across all tabs, the URL hash and the ARIA tab keys.
// Pure (no imports, no DOM), so plain node can test it (test/settingsTabs.test.ts).
//
// Tabs come from one list with visibility rules. Two kinds: "config" tabs show the server's
// settings (sections of the /config schema), "pane" tabs show their own page (Account, Users).
// A new section on the server lands in Advanced until it is given a tab here.

export type TabKind = "config" | "pane";

export interface TabContext {
  /** The signed-in user is an admin (server settings and users are admin-only; the server enforces it). */
  admin: boolean;
  /** Running inside the desktop app (its window bridge is there). */
  desktop: boolean;
}

export interface TabDef {
  id: string;
  label: string;
  kind: TabKind;
  /** One line under the tab's title. */
  blurb: string;
  /** Config tabs: the schema sections shown, in order. */
  sections?: string[];
  /** Pane tabs: words the search finds them by. */
  keywords?: string[];
  visible: (c: TabContext) => boolean;
}

const admins = (c: TabContext) => c.admin;

export const SETTINGS_TABS: TabDef[] = [
  {
    id: "general",
    label: "General",
    kind: "config",
    blurb: "Sessions and the default profile.",
    sections: ["Session"],
    visible: admins,
  },
  {
    id: "models",
    label: "Models",
    kind: "config",
    blurb: "The model server, sampling, failover and the context window.",
    sections: ["LLM Server", "Context"],
    visible: admins,
  },
  {
    id: "agent",
    label: "Agent & tools",
    kind: "config",
    blurb: "What the agent may do without asking, and which tools it has.",
    sections: ["Permissions"],
    visible: admins,
  },
  {
    id: "images",
    label: "Images",
    kind: "config",
    blurb: "Image generation.",
    sections: ["Image Generation"],
    visible: admins,
  },
  {
    id: "desktop",
    label: "Desktop",
    kind: "config",
    blurb: "The desktop app's window: title bar and closing to the tray.",
    sections: ["Desktop"],
    visible: admins,
  },
  {
    id: "advanced",
    label: "Advanced",
    kind: "config",
    blurb: "Settings without a tab of their own.",
    sections: [],
    visible: admins,
  },
  {
    id: "account",
    label: "Account",
    kind: "pane",
    blurb: "Your password, two-factor sign-in, API keys and signed-in sessions.",
    keywords: ["account", "password", "two-factor", "2fa", "totp", "authenticator", "recovery codes", "api keys", "tokens", "sessions", "sign out", "devices"],
    visible: () => true,
  },
  {
    id: "users",
    label: "Users",
    kind: "pane",
    blurb: "The users of this server and the audit log.",
    keywords: ["users", "people", "admin", "roles", "add user", "disable", "reset password", "audit log", "sign-ins"],
    visible: admins,
  },
];

export function tabById(id: string | null | undefined): TabDef | undefined {
  return SETTINGS_TABS.find((t) => t.id === id);
}

/** The tab a schema section belongs to (Advanced for a section no tab names). */
export function tabForSection(section: string): string {
  for (const t of SETTINGS_TABS) if (t.kind === "config" && t.sections?.includes(section)) return t.id;
  return "advanced";
}

export interface SectionLike<S> {
  name: string;
  settings: S[];
}

/** A config tab's sections from the schema, in the tab's order (Advanced: the unclaimed ones, in schema order). */
export function sectionsOfTab<T extends SectionLike<unknown>>(tab: TabDef, all: T[]): T[] {
  if (tab.kind !== "config") return [];
  if (tab.id === "advanced") return all.filter((s) => tabForSection(s.name) === "advanced");
  return (tab.sections ?? []).map((n) => all.find((s) => s.name === n)).filter((s): s is T => !!s);
}

/**
 * The tabs a user sees: visible by its rule, and a config tab only when it has settings (once
 * the schema is loaded; `all` undefined = not loaded yet, show them).
 */
export function visibleTabs(c: TabContext, all?: SectionLike<unknown>[]): TabDef[] {
  return SETTINGS_TABS.filter((t) => {
    if (!t.visible(c)) return false;
    if (t.kind === "config" && all) return sectionsOfTab(t, all).some((s) => s.settings.length > 0);
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

/** Whether a pane tab matches the search (its name, blurb or keywords). */
export function paneMatches(tab: TabDef, q: string): boolean {
  const t = q.trim().toLowerCase();
  if (!t || tab.kind !== "pane") return false;
  return [tab.label, tab.blurb, ...(tab.keywords ?? [])].some((x) => x.toLowerCase().includes(t));
}

export interface SearchHit<T extends SectionLike<SearchableSetting>> {
  tab: TabDef;
  /** Config tabs: the sections with their matching settings. */
  sections: T[];
  /** Pane tabs: the tab itself matched. */
  pane: boolean;
}

/** The search across every visible tab, grouped by tab in tab order. */
export function searchTabs<T extends SectionLike<SearchableSetting>>(
  tabs: TabDef[],
  all: T[],
  q: string,
  hide: (s: SearchableSetting) => boolean = () => false,
): SearchHit<T>[] {
  const out: SearchHit<T>[] = [];
  for (const tab of tabs) {
    if (tab.kind === "pane") {
      if (paneMatches(tab, q)) out.push({ tab, sections: [], pane: true });
      continue;
    }
    const sections = sectionsOfTab(tab, all)
      .map((s) => ({ ...s, settings: s.settings.filter((x) => !hide(x) && settingMatches(x, s.name, q)) }))
      .filter((s) => s.settings.length > 0);
    if (sections.length > 0) out.push({ tab, sections, pane: false });
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

/** The tab to show for a requested id: it if the user sees it, else the first they see. */
export function pickTab(requested: string | null | undefined, tabs: TabDef[]): string | undefined {
  return tabs.find((t) => t.id === requested)?.id ?? tabs[0]?.id;
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
        const t = tabForSection(s.name);
        out[t] = (out[t] ?? 0) + 1;
      }
  return out;
}
