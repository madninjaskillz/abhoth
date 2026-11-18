import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, KeyRound, Layers, RotateCcw, Search, Settings } from "lucide-react";
import { api, settingsApi, type Project, type Setting } from "@/lib/api";
import { ProfilePicker, useProfiles } from "@/features/profiles/Profiles";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Tip } from "@/components/ui/misc";
import { cn } from "@/lib/utils";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/** The gear in the header: opens the settings (the web's /config). */
export function SettingsButton({ project, narrow }: { project: Project | undefined; narrow: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Tip label="Settings (/config)">
        <button
          className="flex shrink-0 items-center gap-1 self-center rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:bg-accent hover:text-foreground"
          onClick={() => setOpen(true)}
          aria-label="Settings"
          data-testid="settings-open"
        >
          <Settings className="size-4" />
          {!narrow && <span className="sr-only">Settings</span>}
        </button>
      </Tip>
      {open && <SettingsDialog project={project} onClose={() => setOpen(false)} />}
    </>
  );
}

type Scope = "project" | "server";

/**
 * The settings /config edits, in its sections, from the server's schema. Two scopes: the
 * current project's running session (as /config in its terminal: live where it can be, and
 * saved), or the server's defaults (its config.json; running projects keep theirs until they
 * restart).
 */
export function SettingsDialog({ project, onClose }: { project: Project | undefined; onClose: () => void }) {
  const canProject = !!project && !project.readOnly && project.exists;
  const [scope, setScope] = useState<Scope>(canProject ? "project" : "server");
  const pid = scope === "project" && project ? project.id : null;
  const [query, setQuery] = useState("");
  const doc = useQuery({ queryKey: ["settings", pid ?? "server"], queryFn: () => settingsApi.get(pid) });
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const profiles = useProfiles(!!info.data?.profiles);
  const haveProfiles = !!profiles.data?.available && !!profiles.data.profiles;
  const chosen = pid ? (profiles.data?.projects[pid] ?? null) : null;
  // The TUI's own session (the primary project) takes its profile from --profile or /profile, not the picker.
  const primary = !!pid && info.data?.primary === pid;
  const pending = scope === "project" && haveProfiles && !primary && doc.isSuccess && (doc.data.profile ?? null) !== chosen;

  const q = query.trim().toLowerCase();
  const sections = useMemo(
    () =>
      (doc.data?.sections ?? [])
        .map((s) => ({
          ...s,
          settings: s.settings.filter(
            // The default profile is picked in the slot above (server scope); a project picks its own there.
            (x) =>
              !(x.key === PROFILES_ACTIVE && (scope === "project" || haveProfiles)) &&
              (!q || [x.label, x.key, x.description, s.name, x.profile ?? ""].some((t) => t.toLowerCase().includes(q))),
          ),
        }))
        .filter((s) => s.settings.length > 0),
    [doc.data, q, scope, haveProfiles],
  );
  const modified = (doc.data?.sections ?? []).reduce((n, s) => n + s.settings.filter((x) => x.modified).length, 0);

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="flex h-[min(86vh,820px)] max-w-3xl flex-col gap-0 p-0" data-testid="settings-dialog">
        <div className="space-y-2.5 border-b p-4 pb-3">
          <DialogTitle>Settings</DialogTitle>
          <div className="flex flex-wrap items-center gap-2">
            <div className="flex rounded-md border p-0.5 text-xs" role="tablist" aria-label="Which settings">
              <button
                role="tab"
                aria-selected={scope === "project"}
                disabled={!canProject}
                onClick={() => setScope("project")}
                className={cn("rounded px-2.5 py-1", scope === "project" ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground", !canProject && "opacity-50")}
                data-testid="settings-scope-project"
              >
                Project{project ? `: ${project.name}` : ""}
              </button>
              <button
                role="tab"
                aria-selected={scope === "server"}
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
              running={doc.data?.profile ?? null}
              defaultSetting={findSetting(doc.data?.sections, PROFILES_ACTIVE)}
              enabled={haveProfiles}
            />
          </div>
          <DialogDescription data-testid="settings-scope-note">
            {scope === "project" ? (
              <>
                Editing <b className="text-foreground">{project?.name}</b>'s running session, as <code>/config</code> in its terminal would: most changes apply at once,
                the rest when a session starts. Saved to <code>{doc.data?.configFile ?? "config.json"}</code> too.
                {doc.data?.profile && (
                  <>
                    {" "}It runs with profile <b className="text-foreground">{doc.data.profile}</b>: the settings that profile sets are shown as its own, and change
                    with the profile.
                  </>
                )}
                {pending && (
                  <span className="text-warning" data-testid="settings-profile-pending">
                    {" "}Profile change pending: the session restarts with {chosen ?? "the config's own settings"} on your next prompt, and these settings follow.
                  </span>
                )}
              </>
            ) : (
              <>
                Editing the server's <code>{doc.data?.configFile ?? "config.json"}</code>: new sessions use it. Open projects keep their current settings until they restart.
                {doc.data?.profile && (
                  <>
                    {" "}Sessions on the default profile <b className="text-foreground">{doc.data.profile}</b> take its values for the settings marked with it;
                    the values here are for sessions without it.
                  </>
                )}
              </>
            )}
          </DialogDescription>
          <div className="flex items-center gap-2">
            <div className="relative flex-1">
              <Search className="absolute left-2 top-2 size-4 text-muted-foreground" />
              <input
                className={cn(field, "pl-8")}
                placeholder="Search settings…"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                autoFocus
                data-testid="settings-search"
              />
            </div>
            <span className="shrink-0 text-xs text-muted-foreground">{modified} changed from default</span>
          </div>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-2">
          {doc.isLoading && <div className="py-6 text-sm text-muted-foreground">Loading…</div>}
          {doc.isError && <div className="py-6 text-sm text-destructive">{(doc.error as Error).message}</div>}
          {doc.isSuccess && sections.length === 0 && <div className="py-6 text-sm text-muted-foreground">No setting matches "{query}".</div>}
          {sections.map((s) => (
            <section key={s.name} className="py-2" data-testid="settings-section">
              <h3 className="sticky top-0 z-10 bg-panel py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{s.name}</h3>
              <div className="divide-y">
                {s.settings.map((x) => (
                  <SettingRow key={x.key} setting={x} pid={pid} scope={scope} />
                ))}
              </div>
            </section>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}

const PROFILES_ACTIVE = "profiles.active";

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
    onSuccess: (r) => {
      if (!r.ok) return setError(r.error ?? "Refused.");
      setError(null);
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
