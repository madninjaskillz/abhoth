import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, KeyRound, Layers, RotateCcw, Search, Settings } from "lucide-react";
import { api, settingsApi, type Project, type Setting, type SettingsDoc } from "@/lib/api";
import { ProfilePicker, useProfiles } from "@/features/profiles/Profiles";
import { AccountPane } from "@/features/auth/AccountDialog";
import { UsersPane } from "@/features/auth/UsersDialog";
import { CalendarPane } from "@/features/calendar/CalendarPane";
import { RouterActivityPane, RouterConfigPane, RouterOverviewPane, RouterRoutesPane, ServiceKeysPane } from "@/features/router/RouterPanes";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Tip, menuRow } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { isAdmin } from "@/lib/auth";
import { inDesktop, windowCommands } from "@/lib/desktop";
import { openSettings, useSettingsNav } from "./settingsNav";
import { ConnectionPanel } from "./ConnectionPanel";
import { modeOf, visibleInMode } from "./connection";
import { nextTabIndex, pickTab, searchTabs, sectionsOfTab, unsavedByTab, visibleTabs, type SearchHit, type TabDef } from "./settingsTabs";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/** The gear in the header: opens the settings (the web's /config). `menu`: a labelled row of the header's menu. */
export function SettingsButton({ narrow, menu }: { narrow: boolean; menu?: boolean }) {
  return menu ? (
    <button role="menuitem" className={menuRow} onClick={() => openSettings()} data-testid="settings-open">
      <Settings className="size-4 shrink-0 text-muted-foreground" />
      <span>Settings</span>
    </button>
  ) : (
    <Tip label="Settings (/config)">
      <button
        className="flex shrink-0 items-center gap-1 self-center rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:bg-accent hover:text-foreground"
        onClick={() => openSettings()}
        aria-label="Settings"
        data-testid="settings-open"
      >
        <Settings className="size-4" />
        {!narrow && <span className="sr-only">Settings</span>}
      </button>
    </Tip>
  );
}

/** The settings dialog while it is open (the gear, the account menu or a #settings/<tab> link opens it). Rendered once. */
export function SettingsHost({ project }: { project: Project | undefined }) {
  const tab = useSettingsNav((s) => s.tab);
  const close = useSettingsNav((s) => s.close);
  if (tab === null) return null;
  return <SettingsDialog project={project} requested={tab} onClose={close} />;
}

type Scope = "project" | "server";

/** Setting keys with an edit typed but not saved yet (a text box, a list), per dialog. */
const DirtyContext = createContext<(key: string, dirty: boolean) => void>(() => {});

/**
 * The settings, in tabs (D143): the server's settings /config edits (admins: the server refuses
 * the rest), grouped by topic from the schema's sections, then the user's Account and the admins'
 * Users. One search covers every tab and says which tab each match is on; the URL hash names the
 * tab. Two scopes for the server settings: the current project's running session (as /config in
 * its terminal: live where it can be, and saved), or the server's defaults (its config.json;
 * running projects keep theirs until they restart).
 */
export function SettingsDialog({ project, requested, onClose }: { project: Project | undefined; requested: string; onClose: () => void }) {
  const admin = isAdmin();
  const show = useSettingsNav((s) => s.show);
  const canProject = !!project && !project.readOnly && project.exists;
  const [scope, setScope] = useState<Scope>(canProject ? "project" : "server");
  const pid = scope === "project" && project ? project.id : null;
  const [query, setQuery] = useState("");
  const doc = useQuery({ queryKey: ["settings", pid ?? "server"], queryFn: () => settingsApi.get(pid), enabled: admin });
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000, enabled: admin });
  const profiles = useProfiles(admin && !!info.data?.profiles);
  const haveProfiles = !!profiles.data?.available && !!profiles.data.profiles;

  const all = useMemo(() => doc.data?.sections ?? [], [doc.data]);
  const router = !!info.data?.router;
  const tabs = useMemo(() => visibleTabs({ admin, desktop: inDesktop(), router }, doc.data ? all : undefined), [admin, all, doc.data, router]);
  const active = pickTab(requested, tabs) ?? "account";
  const activeTab = tabs.find((t) => t.id === active) ?? tabs[0];
  // The URL says which tab is open (and the first one when it asked for none or one this user can't see).
  // Not before the server's info says whether a router is linked: a link to a router tab
  // (#settings/service-keys, where key requests are approved) must not fall back to the first tab.
  const settled = !admin || info.isSuccess || info.isError;
  useEffect(() => {
    if (settled && requested !== active) show(active);
  }, [requested, active, show, settled]);

  // The default profile is picked in the profile slot (server scope); a project picks its own there.
  // Router mode (D149): the rows the router owns are left out (the connection panel shows what it says).
  const mode = modeOf(doc.data);
  const hide = useCallback(
    (x: { key: string; routerMode?: "shown" | "hidden" | "router" }) =>
      !visibleInMode(x, mode) || (x.key === PROFILES_ACTIVE && (scope === "project" || haveProfiles)),
    [scope, haveProfiles, mode],
  );
  const q = query.trim();
  const hits = useMemo(() => (q ? searchTabs(tabs, all, q, hide) : []), [tabs, all, q, hide]);
  const modified = all.reduce((n, s) => n + s.settings.filter((x) => x.modified).length, 0);

  // Unsaved edits: a dot on their tab, a count in the header, and a question before closing.
  const [dirty, setDirty] = useState<Set<string>>(() => new Set());
  const markDirty = useCallback((key: string, d: boolean) => {
    setDirty((prev) => {
      if (prev.has(key) === d) return prev;
      const next = new Set(prev);
      if (d) next.add(key);
      else next.delete(key);
      return next;
    });
  }, []);
  const unsaved = useMemo(() => unsavedByTab(dirty, all), [dirty, all]);
  const [askClose, setAskClose] = useState(false);
  const tryClose = () => (dirty.size > 0 ? setAskClose(true) : onClose());

  const onTab = (id: string) => {
    setQuery("");
    show(id);
  };

  return (
    <Dialog open onOpenChange={(o) => !o && tryClose()}>
      <DialogContent className="flex h-[min(88vh,860px)] max-w-4xl flex-col gap-0 p-0" data-testid="settings-dialog">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b p-4 pb-3 pr-12">
          <DialogTitle>Settings</DialogTitle>
          <DialogDescription className="sr-only">The server's settings, your account and the users, in tabs.</DialogDescription>
          <div className="relative min-w-[12rem] flex-1">
            <Search className="absolute left-2 top-2 size-4 text-muted-foreground" />
            <input
              className={cn(field, "pl-8")}
              placeholder="Search all settings…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              aria-label="Search all settings"
              data-testid="settings-search"
            />
          </div>
          {dirty.size > 0 && (
            <span className="shrink-0 text-xs text-warning" data-testid="settings-unsaved">
              {dirty.size} unsaved
            </span>
          )}
          {admin && doc.isSuccess && <span className="shrink-0 text-xs text-muted-foreground">{modified} changed from default</span>}
        </div>
        <DirtyContext.Provider value={markDirty}>
          <div className="flex min-h-0 flex-1 flex-col sm:flex-row">
            <TabList tabs={tabs} active={active} unsaved={unsaved} searching={!!q} onSelect={onTab} admin={admin} />
            <div className="min-h-0 flex-1 overflow-y-auto" id="settings-panel" role="tabpanel" aria-labelledby={q ? undefined : `settings-tab-${active}`}>
              {q ? (
                <SearchResults hits={hits} query={query} onOpen={onTab} render={(s) => <SettingRow key={s.key} setting={s} pid={pid} scope={scope} />} loading={admin && doc.isLoading} />
              ) : activeTab?.kind === "pane" ? (
                <Pane id={activeTab.id} />
              ) : activeTab ? (
                <div className="px-4 pb-4">
                  <ScopeBar
                    scope={scope}
                    setScope={setScope}
                    canProject={canProject}
                    project={project}
                    doc={doc.data}
                    haveProfiles={haveProfiles}
                    profileChosen={pid ? (profiles.data?.projects[pid] ?? null) : null}
                    primary={!!pid && info.data?.primary === pid}
                  />
                  <p className="pt-2 text-xs text-muted-foreground">{activeTab.blurb}</p>
                  {doc.isLoading && <div className="py-6 text-sm text-muted-foreground">Loading…</div>}
                  {doc.isError && <div className="py-6 text-sm text-destructive">{(doc.error as Error).message}</div>}
                  {activeTab.id === "models" && doc.isSuccess && <ConnectionPanel key={pid ?? "server"} pid={pid} mode={mode} sections={all} />}
                  {sectionsOfTab(activeTab, all).map((s) => (
                    <section key={s.name} className="py-2" data-testid="settings-section">
                      <h3 className="sticky top-0 z-10 bg-panel py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{s.name}</h3>
                      <div className="divide-y">
                        {s.settings.filter((x) => !hide(x)).map((x) => (
                          <SettingRow key={x.key} setting={x} pid={pid} scope={scope} />
                        ))}
                      </div>
                    </section>
                  ))}
                </div>
              ) : null}
            </div>
          </div>
        </DirtyContext.Provider>
        <ConfirmDialog
          open={askClose}
          title="Discard unsaved changes?"
          body={`${dirty.size} setting${dirty.size === 1 ? " has" : "s have"} an edit that isn't saved yet (press Enter in a box, or Save under a list, to save it).`}
          confirm="Discard"
          destructive
          onConfirm={() => {
            setAskClose(false);
            onClose();
          }}
          onCancel={() => setAskClose(false)}
        />
      </DialogContent>
    </Dialog>
  );
}

/**
 * The tabs: a vertical list (ARIA tabs: arrows, Home/End, the focused tab opens) beside the
 * page, or a dropdown at phone width.
 */
function TabList({
  tabs,
  active,
  unsaved,
  searching,
  onSelect,
  admin,
}: {
  tabs: TabDef[];
  active: string;
  unsaved: Record<string, number>;
  searching: boolean;
  onSelect: (id: string) => void;
  admin: boolean;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  return (
    <>
      <div className="border-b px-4 py-2 sm:hidden">
        <label className="flex items-center gap-2 text-xs text-muted-foreground">
          Section
          <select
            className="h-8 flex-1 rounded-md border bg-background px-2 text-sm text-foreground"
            value={active}
            onChange={(e) => onSelect(e.target.value)}
            data-testid="settings-tab-select"
          >
            {tabs.map((t) => (
              <option key={t.id} value={t.id}>
                {t.label}
                {unsaved[t.id] ? " •" : ""}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="hidden w-44 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-2 sm:flex">
        <div role="tablist" aria-orientation="vertical" aria-label="Settings" className="flex flex-col gap-0.5" data-testid="settings-tabs">
          {tabs.map((t, i) => {
            const selected = !searching && t.id === active;
            return (
              <button
                key={t.id}
                ref={(el) => {
                  refs.current[i] = el;
                }}
                id={`settings-tab-${t.id}`}
                role="tab"
                aria-selected={selected}
                aria-controls="settings-panel"
                tabIndex={t.id === active ? 0 : -1}
                onClick={() => onSelect(t.id)}
                onKeyDown={(e) => {
                  const to = nextTabIndex(i, tabs.length, e.key);
                  if (to === i) return;
                  e.preventDefault();
                  refs.current[to]?.focus();
                  onSelect(tabs[to].id);
                }}
                className={cn(
                  "flex items-center justify-between rounded-md px-2.5 py-1.5 text-left text-sm outline-none focus-visible:ring-1 focus-visible:ring-ring",
                  selected ? "bg-accent font-medium text-foreground" : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
                )}
                data-testid={`settings-tab-${t.id}`}
              >
                <span>{t.label}</span>
                {!!unsaved[t.id] && <span className="size-1.5 rounded-full bg-warning" title="Unsaved edits" aria-label="unsaved edits" />}
              </button>
            );
          })}
        </div>
        {!admin && (
          <p className="mt-3 px-2.5 text-[11px] leading-snug text-muted-foreground" data-testid="settings-admin-only">
            The server's settings are shared, so only an admin can change them. A project's profile and mode are picked in the chat footer.
          </p>
        )}
      </div>
    </>
  );
}

function Pane({ id }: { id: string }) {
  if (id === "account") return <div className="flex h-full flex-col"><AccountPane /></div>;
  if (id === "users") return <div className="flex h-full flex-col"><UsersPane /></div>;
  if (id === "inference") return <div className="flex h-full flex-col"><RouterOverviewPane /></div>;
  if (id === "routes") return <div className="flex h-full flex-col"><RouterRoutesPane /></div>;
  if (id === "router-config") return <div className="flex h-full flex-col"><RouterConfigPane /></div>;
  if (id === "router-activity") return <div className="flex h-full flex-col"><RouterActivityPane /></div>;
  if (id === "calendar") return <div className="flex h-full flex-col"><CalendarPane /></div>;
  if (id === "service-keys") return <div className="flex h-full flex-col"><ServiceKeysPane /></div>;
  return null;
}

/** The search across all tabs: matches grouped under their tab, each with a way to open it. */
function SearchResults({
  hits,
  query,
  onOpen,
  render,
  loading,
}: {
  hits: SearchHit<{ name: string; settings: Setting[] }>[];
  query: string;
  onOpen: (tab: string) => void;
  render: (s: Setting) => React.ReactNode;
  loading: boolean;
}) {
  if (loading) return <div className="p-4 text-sm text-muted-foreground">Loading…</div>;
  if (hits.length === 0) return <div className="p-4 text-sm text-muted-foreground">No setting matches "{query}".</div>;
  return (
    <div className="px-4 pb-4" data-testid="settings-results">
      {hits.map((h) => (
        <section key={h.tab.id} className="py-2" data-testid="settings-result" data-tab={h.tab.id}>
          <div className="sticky top-0 z-10 flex items-center gap-2 bg-panel py-1.5">
            <span className="rounded bg-accent px-1.5 py-0.5 text-[11px] font-medium text-foreground">{h.tab.label}</span>
            <button className="text-xs text-primary hover:underline" onClick={() => onOpen(h.tab.id)} data-testid="settings-result-open">
              Open tab
            </button>
          </div>
          {h.pane ? (
            <p className="py-1 text-xs text-muted-foreground">{h.tab.blurb}</p>
          ) : (
            h.sections.map((s) => (
              <div key={s.name}>
                <h3 className="pt-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{s.name}</h3>
                <div className="divide-y">{s.settings.map(render)}</div>
              </div>
            ))
          )}
        </section>
      ))}
    </div>
  );
}

/** Which settings are edited (the project's running session or the server's defaults), and the profile. */
function ScopeBar({
  scope,
  setScope,
  canProject,
  project,
  doc,
  haveProfiles,
  profileChosen,
  primary,
}: {
  scope: Scope;
  setScope: (s: Scope) => void;
  canProject: boolean;
  project: Project | undefined;
  doc: SettingsDoc | undefined;
  haveProfiles: boolean;
  profileChosen: string | null;
  primary: boolean;
}) {
  const pending = scope === "project" && haveProfiles && !primary && !!doc && (doc.profile ?? null) !== profileChosen;
  return (
    <div className="space-y-2 border-b pb-3 pt-3">
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex rounded-md border p-0.5 text-xs" role="radiogroup" aria-label="Which settings">
          <button
            role="radio"
            aria-checked={scope === "project"}
            disabled={!canProject}
            onClick={() => setScope("project")}
            className={cn("rounded px-2.5 py-1", scope === "project" ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground", !canProject && "opacity-50")}
            data-testid="settings-scope-project"
          >
            Project{project ? `: ${project.name}` : ""}
          </button>
          <button
            role="radio"
            aria-checked={scope === "server"}
            onClick={() => setScope("server")}
            className={cn("rounded px-2.5 py-1", scope === "server" ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground")}
            data-testid="settings-scope-server"
          >
            Server defaults
          </button>
        </div>
        <ProfileSlot
          scope={scope}
          project={project}
          primary={primary}
          running={doc?.profile ?? null}
          defaultSetting={findSetting(doc?.sections, PROFILES_ACTIVE)}
          enabled={haveProfiles}
        />
      </div>
      <p className="text-xs text-muted-foreground" data-testid="settings-scope-note">
        {scope === "project" ? (
          <>
            Editing <b className="text-foreground">{project?.name}</b>'s running session, as <code>/config</code> in its terminal would: most changes apply at once,
            the rest when a session starts. Saved to <code>{doc?.configFile ?? "config.json"}</code> too.
            {doc?.profile && (
              <>
                {" "}It runs with profile <b className="text-foreground">{doc.profile}</b>: the settings that profile sets are shown as its own, and change
                with the profile.
              </>
            )}
            {pending && (
              <span className="text-warning" data-testid="settings-profile-pending">
                {" "}Profile change pending: the session restarts with {profileChosen ?? "the config's own settings"} on your next prompt, and these settings follow.
              </span>
            )}
          </>
        ) : (
          <>
            Editing the server's <code>{doc?.configFile ?? "config.json"}</code>: new sessions use it. Open projects keep their current settings until they restart.
            {doc?.profile && (
              <>
                {" "}Sessions on the default profile <b className="text-foreground">{doc.profile}</b> take its values for the settings marked with it;
                the values here are for sessions without it.
              </>
            )}
          </>
        )}
      </p>
    </div>
  );
}

const PROFILES_ACTIVE = "profiles.active";
const SYSTEM_TITLE_BAR = "desktop.systemTitleBar";

function findSetting(sections: { settings: Setting[] }[] | undefined, key: string) {
  for (const s of sections ?? []) for (const x of s.settings) if (x.key === key) return x;
  return undefined;
}

/**
 * The inference profile: in project scope, the project's own (its session restarts with it, as
 * from the chat footer); in server scope, the default profile new sessions start with.
 */
function ProfileSlot({
  scope,
  project,
  primary,
  running,
  defaultSetting,
  enabled,
}: {
  scope: Scope;
  project: Project | undefined;
  primary: boolean;
  running: string | null;
  defaultSetting: Setting | undefined;
  enabled: boolean;
}) {
  const qc = useQueryClient();
  const profiles = useProfiles(enabled);
  const [error, setError] = useState<string | null>(null);
  const setDefault = useMutation({
    mutationFn: (name: string) => settingsApi.set(null, PROFILES_ACTIVE, name),
    onSuccess: (r) => {
      setError(r.ok ? null : (r.error ?? "Refused."));
      void qc.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  if (!enabled || !profiles.data?.profiles)
    return (
      <span className="ml-auto text-xs text-muted-foreground" data-testid="settings-profile-slot" title="No profiles server is configured (profiles.serverUrl or slots.registryUrl).">
        No inference profiles
      </span>
    );
  if (scope === "project" && primary)
    return (
      <span className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground" data-testid="settings-profile-slot" title="This project's session is the terminal's own: /profile there switches it">
        <Layers className="size-3.5" />
        Profile <b className="text-foreground">{running ?? "config's own"}</b> (set in the terminal)
      </span>
    );
  if (scope === "project")
    return (
      <span className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground" data-testid="settings-profile-slot">
        <Layers className="size-3.5" />
        {project && <ProfilePicker pid={project.id} busy={false} onChanged={() => void qc.invalidateQueries({ queryKey: ["settings"] })} />}
      </span>
    );
  const current = (defaultSetting?.value as string | null) ?? "";
  return (
    <span className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground" data-testid="settings-profile-slot">
      <Layers className="size-3.5" />
      <label className="inline-flex items-center gap-1" title="The profile new sessions start with when none is chosen (profiles.active)">
        Default profile
        <select
          className="h-7 rounded-md border bg-background px-1.5 text-xs disabled:opacity-50"
          value={current}
          disabled={setDefault.isPending}
          onChange={(e) => setDefault.mutate(e.target.value)}
          data-testid="settings-default-profile"
        >
          <option value="">none (config as it is)</option>
          {current && !profiles.data.profiles[current] && <option value={current}>{current} (unknown)</option>}
          {Object.keys(profiles.data.profiles).map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
      </label>
      {error && <span className="text-destructive">{error}</span>}
    </span>
  );
}

function SettingRow({ setting: s, pid, scope }: { setting: Setting; pid: string | null; scope: Scope }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [confirm, setConfirm] = useState<{ value: unknown } | null>(null);
  const save = useMutation({
    mutationFn: ({ value, reset }: { value: unknown; reset?: boolean }) => settingsApi.set(pid, s.key, value, reset),
    onSuccess: (r, v) => {
      if (!r.ok) return setError(r.error ?? "Refused.");
      setError(null);
      // The desktop window switches its title bar at once.
      if (s.key === SYSTEM_TITLE_BAR && inDesktop()) windowCommands.systemTitleBar(v.reset ? false : v.value === true);
      setSaved(true);
      setTimeout(() => setSaved(false), 1600);
      void qc.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  // In a project, a key its profile sets can't be changed here: the profile would put its value back.
  const locked = scope === "project" && !!s.profile;
  const commit = (value: unknown) => {
    if (s.dangerous && value === true) setConfirm({ value });
    else save.mutate({ value });
  };

  return (
    <div className="grid grid-cols-1 gap-x-4 gap-y-1.5 py-2.5 sm:grid-cols-[minmax(0,1fr)_minmax(0,18rem)]" data-testid="setting" data-key={s.key}>
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-1.5 text-sm">
          <span className="font-medium">{s.label}</span>
          {s.modified && (
            <span className="rounded bg-primary/15 px-1 text-[10px] font-medium text-primary" title={`Default: ${s.defaultDisplay}`} data-testid="setting-modified">
              modified
            </span>
          )}
          {s.profile && (
            <span
              className="inline-flex items-center gap-0.5 rounded bg-warning/15 px-1 text-[10px] font-medium text-warning"
              title={
                scope === "project"
                  ? `Profile ${s.profile} sets this: choose another profile (or none) to change it.`
                  : `Sessions on the default profile ${s.profile} use its value; this one is for sessions without it.`
              }
              data-testid="setting-profile"
            >
              <Layers className="size-2.5" />
              {scope === "project" ? `set by profile ${s.profile}` : `profile ${s.profile} overrides`}
            </span>
          )}
          {!s.live && scope === "project" && !s.profile && (
            <span className="rounded bg-muted px-1 text-[10px] text-muted-foreground" title="The running session keeps its current value; new sessions use the new one.">
              applies to new sessions
            </span>
          )}
          {saved && <Check className="size-3.5 text-success" />}
        </div>
        <div className="text-xs text-muted-foreground">{s.description}</div>
        <div className="font-mono text-[10px] text-muted-foreground/70">{s.key}</div>
      </div>
      <div className="flex min-w-0 items-start gap-1">
        <div className="min-w-0 flex-1">
          {locked ? (
            <div className="flex h-8 items-center truncate rounded-md border border-dashed px-2 font-mono text-xs text-muted-foreground" data-testid="setting-locked">
              {s.display}
            </div>
          ) : (
            <Editor setting={s} busy={save.isPending} onCommit={commit} onRemove={() => save.mutate({ value: null })} />
          )}
          {error && <div className="pt-1 text-xs text-destructive" data-testid="setting-error">{error}</div>}
        </div>
        {s.modified && s.type !== "secret" && !locked && (
          <Tip label={`Reset to default (${s.defaultDisplay})`}>
            <Button size="icon-sm" variant="ghost" disabled={save.isPending} onClick={() => save.mutate({ value: null, reset: true })} data-testid="setting-reset">
              <RotateCcw />
            </Button>
          </Tip>
        )}
      </div>
      <ConfirmDialog
        open={!!confirm}
        title={`Turn on ${s.label}?`}
        body={s.description}
        confirm="Turn on"
        destructive
        onConfirm={() => {
          const v = confirm!.value;
          setConfirm(null);
          save.mutate({ value: v });
        }}
        onCancel={() => setConfirm(null)}
      />
    </div>
  );
}

function Editor({ setting: s, busy, onCommit, onRemove }: { setting: Setting; busy: boolean; onCommit: (v: unknown) => void; onRemove: () => void }) {
  const initial = s.type === "list" ? ((s.value as string[] | null) ?? []).join("\n") : s.value === null || s.value === undefined ? "" : String(s.value);
  const [text, setText] = useState(initial);
  const [replacing, setReplacing] = useState(false);
  useEffect(() => setText(initial), [initial]);
  // Typed but not saved yet: the dialog marks the tab and asks before closing.
  const markDirty = useContext(DirtyContext);
  const listChanged =
    s.type === "list" &&
    JSON.stringify(text.split(/[\n,]/).map((t) => t.trim()).filter(Boolean)) !== JSON.stringify(s.value ?? []);
  const isDirty = s.type === "bool" ? false : s.type === "secret" ? replacing && !!text.trim() : s.type === "list" ? listChanged : text !== initial;
  useEffect(() => {
    markDirty(s.key, isDirty);
  }, [markDirty, s.key, isDirty]);
  useEffect(() => () => markDirty(s.key, false), [markDirty, s.key]);

  if (s.type === "bool") {
    const on = s.value === true;
    return (
      <button
        role="switch"
        aria-checked={on}
        disabled={busy}
        onClick={() => onCommit(!on)}
        className={cn("relative h-5 w-9 rounded-full border transition-colors", on ? "border-primary bg-primary" : "bg-muted")}
        data-testid="setting-switch"
      >
        <span className={cn("absolute top-0.5 size-3.5 rounded-full bg-background shadow transition-all", on ? "left-[18px]" : "left-0.5")} />
      </button>
    );
  }

  if (s.type === "secret") {
    return replacing ? (
      <form
        className="flex gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim()) {
            onCommit(text.trim());
            setReplacing(false);
            setText("");
          }
        }}
      >
        <input type="password" autoComplete="off" className={field} autoFocus value={text} onChange={(e) => setText(e.target.value)} placeholder="New value" data-testid="secret-input" />
        <Button type="submit" size="sm" disabled={!text.trim() || busy}>Save</Button>
        <Button type="button" size="sm" variant="ghost" onClick={() => { setReplacing(false); setText(""); }}>Cancel</Button>
      </form>
    ) : (
      <div className="flex items-center gap-1">
        <span className="flex min-w-0 flex-1 items-center gap-1.5 truncate font-mono text-xs text-muted-foreground" data-testid="secret-mask">
          <KeyRound className="size-3.5 shrink-0" /> {s.display}
        </span>
        <Button size="sm" variant="ghost" onClick={() => { setText(""); setReplacing(true); }} data-testid="secret-replace">Replace</Button>
        {s.set && <Button size="sm" variant="ghost" disabled={busy} onClick={onRemove} data-testid="secret-remove">Remove</Button>}
      </div>
    );
  }

  if (s.type === "list") {
    const parse = () => text.split(/[\n,]/).map((t) => t.trim()).filter(Boolean);
    const changed = JSON.stringify(parse()) !== JSON.stringify(s.value ?? []);
    return (
      <div className="space-y-1">
        <textarea
          className={cn(field, "h-16 resize-y py-1 font-mono text-xs")}
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={s.blankMeans ? `(${s.blankMeans})` : "one per line"}
          data-testid="setting-list"
        />
        {changed && <Button size="sm" className="h-7" disabled={busy} onClick={() => onCommit(parse())}>Save</Button>}
      </div>
    );
  }

  const numeric = s.type === "int" || s.type === "float";
  const changed = text !== initial;
  return (
    <form
      className="flex gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        if (changed) onCommit(text);
      }}
    >
      <input
        className={cn(field, numeric && "font-mono")}
        inputMode={numeric ? "decimal" : undefined}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => changed && onCommit(text)}
        placeholder={s.blankMeans ? `(${s.blankMeans})` : undefined}
        data-testid="setting-input"
      />
    </form>
  );
}
