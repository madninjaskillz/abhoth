import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, CheckCircle2, Loader2, Server } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { api, type CardPick, type LayoutDefinition, type ProfileInfo, type ProfileList, type SwitchJob, type SwitchPlan } from "@/lib/api";
import { menuRow } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { isAdmin } from "@/lib/auth";
import { engineProfileOptions, hiddenModels, modelOptions, pickProblem } from "@/features/bench/queueLogic";

/**
 * Inference profiles: a profile names a server layout (what runs on each GPU of the inference box)
 * plus the client settings that go with it. Each project picks its profile (its session restarts
 * on the same conversation); the server layout is shared, so switching it is confirmed first.
 */
export function useProfiles(enabled = true) {
  return useQuery({ queryKey: ["profiles"], queryFn: api.profiles, refetchInterval: 15000, enabled });
}

/** "A strata×3, B strata×2". */
export function layoutSummary(l?: LayoutDefinition | null) {
  if (!l) return "";
  return Object.entries(l.cards ?? {})
    .map(([card, c]) => `${card} ${c.engine === "off" ? "off" : `${c.engine}×${c.slots}`}`)
    .join(", ");
}

type Flow =
  | { step: "idle" }
  | { step: "planning"; profile: string }
  | { step: "confirm"; profile: string; plan: SwitchPlan }
  | { step: "running"; profile: string; plan: SwitchPlan; job: SwitchJob }
  | { step: "done"; profile: string; job: SwitchJob }
  | { step: "failed"; profile: string; message: string; job?: SwitchJob };

/** Plan → confirm → switch → follow, for a profile whose layout isn't live. */
export function useServerSwitch(onDone?: (profile: string) => void) {
  const qc = useQueryClient();
  const [flow, setFlow] = useState<Flow>({ step: "idle" });

  const start = async (profile: string) => {
    setFlow({ step: "planning", profile });
    try {
      const plan = await api.profilePlan(profile);
      if (plan.fits) {
        setFlow({ step: "idle" });
        onDone?.(profile);
        return;
      }
      setFlow({ step: "confirm", profile, plan });
    } catch (e) {
      setFlow({ step: "failed", profile, message: e instanceof Error ? e.message : String(e) });
    }
  };

  const confirm = async () => {
    if (flow.step !== "confirm") return;
    const { profile, plan } = flow;
    try {
      const job = await api.profileSwitch(profile);
      if (job.already) {
        setFlow({ step: "done", profile, job });
        onDone?.(profile);
        return;
      }
      setFlow({ step: "running", profile, plan, job });
    } catch (e) {
      const data = (e as { data?: SwitchJob }).data;
      setFlow({ step: "failed", profile, message: data?.error ?? (e instanceof Error ? e.message : String(e)) });
    }
  };

  // Follow the job every 2 s until it finishes.
  const jobId = flow.step === "running" ? flow.job.id : null;
  useEffect(() => {
    if (!jobId) return;
    let stop = false;
    const tick = async () => {
      if (stop) return;
      try {
        const job = await api.profileJob(jobId);
        if (stop) return;
        if (job.state === "done") {
          setFlow({ step: "done", profile: flowProfile(), job });
          void qc.invalidateQueries({ queryKey: ["profiles"] });
          onDone?.(flowProfile());
          return;
        }
        if (job.state && ["failed", "rolled-back", "refused"].includes(job.state)) {
          setFlow({ step: "failed", profile: flowProfile(), message: job.error ?? job.state, job });
          void qc.invalidateQueries({ queryKey: ["profiles"] });
          return;
        }
        setFlow((f) => (f.step === "running" ? { ...f, job } : f));
      } catch {
        // the API restarts or is busy: keep following (the switch runs on the server regardless)
      }
      setTimeout(tick, 2000);
    };
    const flowProfile = () => (flow.step === "running" ? flow.profile : "");
    const t = setTimeout(tick, 1500);
    return () => {
      stop = true;
      clearTimeout(t);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jobId]);

  return { flow, start, confirm, close: () => setFlow({ step: "idle" }) };
}

/** The confirm dialog and the live progress of a server layout switch. */
export function ServerSwitchDialog({ sw }: { sw: ReturnType<typeof useServerSwitch> }) {
  const f = sw.flow;
  const open = f.step !== "idle" && f.step !== "planning";
  const minutes = (s: number) => Math.max(1, Math.ceil(s / 60));
  return (
    <Dialog open={open} onOpenChange={(o) => !o && f.step !== "running" && sw.close()}>
      <DialogContent className="max-w-lg" data-testid="server-switch-dialog">
        {f.step === "confirm" && (
          <>
            <DialogTitle>Switch the server to {f.plan.layout}?</DialogTitle>
            <DialogDescription>
              Profile <span className="font-mono">{f.profile}</span> needs layout <span className="font-mono">{f.plan.layout}</span>; live now:{" "}
              <span className="font-mono">{f.plan.live ?? "unknown"}</span>.
            </DialogDescription>
            {f.plan.refusals.length > 0 ? (
              <div className="rounded-md border border-warning/40 bg-warning/10 p-2 text-sm text-warning" data-testid="switch-refusals">
                <p className="mb-1 font-medium">The server can't switch now:</p>
                <ul className="list-disc pl-5">
                  {f.plan.refusals.map((r) => (
                    <li key={r}>{r}</li>
                  ))}
                </ul>
              </div>
            ) : (
              <div className="space-y-1.5 text-sm">
                <p className="flex items-start gap-2 text-warning">
                  <AlertTriangle className="mt-0.5 size-4 shrink-0" />
                  <span>
                    This {f.plan.effect ?? "restarts model servers, affects everything using the models"}: about {minutes(f.plan.estimateSeconds)}{" "}
                    minutes.
                  </span>
                </p>
                {f.plan.restore?.text && <p className="text-muted-foreground">Its model files are offline: {f.plan.restore.text}, copied back first.</p>}
                <p className="text-muted-foreground">On a failure the server goes back to the layout it had.</p>
              </div>
            )}
            <div className="flex justify-end gap-2 pt-1">
              <Button size="sm" variant="ghost" onClick={sw.close}>
                Cancel
              </Button>
              <Button size="sm" disabled={f.plan.refusals.length > 0} onClick={() => void sw.confirm()} data-testid="switch-confirm">
                <Server /> Switch the server
              </Button>
            </div>
          </>
        )}
        {f.step === "running" && (
          <>
            <DialogTitle className="flex items-center gap-2">
              <Loader2 className="size-4 animate-spin" /> Switching to {f.plan.layout}
            </DialogTitle>
            <DialogDescription>About {minutes(f.plan.estimateSeconds)} minutes; this window can be closed once it's done.</DialogDescription>
            <Steps job={f.job} />
          </>
        )}
        {f.step === "done" && (
          <>
            <DialogTitle className="flex items-center gap-2">
              <CheckCircle2 className="size-4 text-success" /> Server on {f.job.layout ?? f.job.live}
            </DialogTitle>
            <DialogDescription>
              {f.job.already ? "It was already running." : `Switched in ${Math.round(f.job.seconds ?? 0)} s.`}
            </DialogDescription>
            <Steps job={f.job} />
            <div className="flex justify-end">
              <Button size="sm" onClick={sw.close}>
                Close
              </Button>
            </div>
          </>
        )}
        {f.step === "failed" && (
          <>
            <DialogTitle className="text-destructive">Switch not done</DialogTitle>
            <p className="text-sm text-destructive">{f.message}</p>
            {f.job?.live && <p className="text-xs text-muted-foreground">Live layout: {f.job.live}</p>}
            {f.job && <Steps job={f.job} />}
            <div className="flex justify-end">
              <Button size="sm" onClick={sw.close}>
                Close
              </Button>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function Steps({ job }: { job: SwitchJob }) {
  if (!job.steps?.length) return null;
  return (
    <ol className="max-h-60 space-y-0.5 overflow-auto rounded-md border bg-background p-2 font-mono text-[11px]" data-testid="switch-steps">
      {job.steps.map((s, i) => (
        <li key={i} className="flex gap-2">
          <span className="w-12 shrink-0 text-right tabular-nums text-muted-foreground">{s.t.toFixed(0)}s</span>
          <span className="min-w-0 break-words">{s.msg}</span>
        </li>
      ))}
    </ol>
  );
}

/**
 * App bar: the live server layout; opens the layout list with a switch per profile.
 * `menu`: a labelled row of the header's menu.
 */
export function ServerLayoutControl({ narrow, menu }: { narrow?: boolean; menu?: boolean }) {
  const profiles = useProfiles();
  const [open, setOpen] = useState(false);
  const sw = useServerSwitch();
  const d = profiles.data;
  if (!d) return null;
  const live = d.live;
  const busy = !!live?.job && live.job.state !== "done";
  return (
    <>
      <button
        onClick={() => setOpen(true)}
        role={menu ? "menuitem" : undefined}
        className={menu ? menuRow : "flex shrink-0 items-center gap-1.5 self-center rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"}
        title={d.available ? `Server layout: ${live?.layout ?? "unknown"}${d.offline ? " (offline copy)" : ""}` : d.error ?? "Profiles unavailable"}
        data-testid="server-layout"
      >
        <Server className={cn("size-4 shrink-0", menu && "text-muted-foreground")} />
        {menu && <span>Server layout</span>}
        <span className={cn("size-1.5 shrink-0 rounded-full", menu && "ml-auto", busy ? "animate-pulse bg-warning" : live?.healthy ? "bg-success" : "bg-destructive")} />
        {(menu || !narrow) && <span className={cn("font-mono", menu && "truncate text-xs text-muted-foreground")}>{busy ? `→ ${live?.job?.layout}` : live?.layout ?? "?"}</span>}
      </button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-2xl">
          <DialogTitle>Inference server layout</DialogTitle>
          <DialogDescription>
            What runs on each GPU of the inference box, shared by everyone using it. Live:{" "}
            <span className="font-mono text-foreground">{live?.layout ?? "unknown"}</span>
            {live?.layout && d.layouts?.[live.layout] && <> ({layoutSummary(d.layouts[live.layout])})</>}
            {d.offline && " · offline copy of the list"}
            {!!live?.leases?.length && ` · slots in use: ${live.leases.map((l) => l.slot).join(", ")}`}
          </DialogDescription>
          <LayoutTable data={d} onSwitch={(p) => { setOpen(false); void sw.start(p); }} />
        </DialogContent>
      </Dialog>
      <ServerSwitchDialog sw={sw} />
    </>
  );
}

/** "A: dense-pi-abl on ninfer-gaps-4lane · B: same". */
export function cardsText(p: ProfileInfo | undefined | null): string {
  const c = p?.cards;
  if (!c) return p?.bridge ? "no cards (CLI bridge)" : "";
  const one = (v: CardPick | null | undefined) => (!v || v === "off" ? "off" : v === "same" ? "same as A" : `${v.model} on ${v.engineProfile}`);
  return `A: ${one(c.A)} · B: ${one(c.B)}`;
}

function LayoutTable({ data, onSwitch, onDelete }: { data: ProfileList; onSwitch?: (profile: string) => void; onDelete?: (profile: string) => void }) {
  const entries = Object.entries(data.profiles ?? {}).filter(([, p]) => !p.hidden);
  return (
    <div className="max-h-[50vh] overflow-auto">
      <table className="w-full text-xs" data-testid="profile-table">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="py-1.5 pr-2 font-medium">Profile</th>
            <th className="px-2 font-medium">About</th>
            <th className="px-2" />
          </tr>
        </thead>
        <tbody>
          {entries.map(([name, p]) => (
            <tr key={name} className="border-b last:border-0 align-top">
              <td className="py-1.5 pr-2 font-mono">
                {name}
                {p.source === "user" && <div className="font-sans text-[10px] text-muted-foreground">saved here</div>}
              </td>
              <td className="px-2">
                <div className="font-medium">{p.title}</div>
                {p.description && <div className="text-[11px] text-muted-foreground">{p.description}</div>}
                <div className="font-mono text-[11px]">
                  {cardsText(p)}
                  {p.mode ? ` · mode ${p.mode}` : ""}
                </div>
                {p.restore && <div className="text-[11px] text-warning">{p.restore}</div>}
              </td>
              <td className="space-y-1 px-2 text-right">
                {p.fits ? (
                  <span className="text-[11px] text-success">{p.layout ? "live" : "no server needed"}</span>
                ) : onSwitch && (
                  <Button size="sm" variant="outline" className="h-6" disabled={data.offline} onClick={() => onSwitch(name)} data-testid={`switch-${name}`}>
                    Switch…
                  </Button>
                )}
                {p.source === "user" && onDelete && (
                  <button className="block text-[11px] text-muted-foreground underline" onClick={() => onDelete(name)}>
                    delete
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

const MODE_HELP =
  "auto: the Lead decides when to hand work to helpers. hands-on: the Lead does the work itself. orchestrator: the Lead plans and delegates. peer: parallel pieces go to equal helpers.";

type PickState = { engine: string; engineProfile: string; model: string };
const emptyPick: PickState = { engine: "", engineProfile: "", model: "" };

/** One card: engine → engine profile → model, each narrowed to what fits (a model's allowed engine profiles, VRAM),
 *  whichever of engine profile / model is picked first. */
function CardBuilder({ card, d, pick, onPick, allowOff }: { card: string; d: ProfileList; pick: PickState; onPick: (p: PickState) => void; allowOff: boolean }) {
  const epNames = engineProfileOptions(d, card, pick.engine, pick.model);
  const eps = epNames.map((n) => [n, d.engineProfiles![n]] as const);
  const models = modelOptions(d, card, pick.engine, pick.engineProfile).map((n) => [n, d.models![n]] as const);
  const hidden = hiddenModels(d, card, pick.engine, pick.engineProfile);
  // a choice that rules out the other one clears it, so an invalid combo is never left selected
  const pickEngineProfile = (engineProfile: string) =>
    onPick({ ...pick, engineProfile, model: pick.model && !modelOptions(d, card, pick.engine, engineProfile).includes(pick.model) ? "" : pick.model });
  const pickModel = (model: string) =>
    onPick({ ...pick, model, engineProfile: pick.engineProfile && !engineProfileOptions(d, card, pick.engine, model).includes(pick.engineProfile) ? "" : pick.engineProfile });
  const problem = pick.engine && pick.engine !== "off" && pick.engineProfile && pick.model ? pickProblem(d, card, pick) : null;
  const sel = "rounded border bg-background px-1 py-0.5 text-xs";
  return (
    <div className="space-y-1" data-testid={`builder-card-${card}`}>
      <div className="flex flex-wrap items-center gap-1.5 text-xs">
        <span className="w-14 font-mono">card {card}</span>
        <select className={sel} value={pick.engine} onChange={(e) => onPick({ engine: e.target.value, engineProfile: "", model: "" })} data-testid={`builder-engine-${card}`}>
          <option value="">engine…</option>
          {allowOff && <option value="off">off</option>}
          {Object.entries(d.engines ?? {}).map(([n, e]) => (
            <option key={n} value={n}>
              {e.title ?? n}
            </option>
          ))}
        </select>
        {pick.engine && pick.engine !== "off" && (
          <>
            <select className={sel} value={pick.engineProfile} onChange={(e) => pickEngineProfile(e.target.value)} data-testid={`builder-engine-profile-${card}`}>
              <option value="">engine profile…</option>
              {eps.map(([n, ep]) => (
                <option key={n} value={n}>
                  {ep.title ?? n}
                </option>
              ))}
            </select>
            <select className={sel} value={pick.model} onChange={(e) => pickModel(e.target.value)} data-testid={`builder-model-${card}`}>
              <option value="">model…</option>
              {models.map(([n, m]) => (
                <option key={n} value={n}>
                  {m.title ?? n}
                </option>
              ))}
            </select>
          </>
        )}
      </div>
      {pick.engineProfile && <p className="pl-16 text-[11px] text-muted-foreground">{d.engineProfiles?.[pick.engineProfile]?.description}</p>}
      {pick.model && <p className="pl-16 text-[11px] text-muted-foreground">{d.models?.[pick.model]?.description}</p>}
      {hidden.length > 0 && (
        <p className="pl-16 text-[11px] text-muted-foreground" data-testid={`builder-hidden-${card}`}>
          Not offered here: {hidden.map((h) => `${d.models?.[h.model]?.title ?? h.model} (${h.reason})`).join("; ")}
        </p>
      )}
      {problem && <p className="pl-16 text-[11px] text-destructive">{problem}</p>}
    </div>
  );
}

/** Build a setup per card (+ mode): use it in this project as it is, or save it under a name. */
function ProfileBuilder({ d, open, onClose, onUse }: { d: ProfileList; open: boolean; onClose: () => void; onUse: (profile: string, mode: string | null) => Promise<void> }) {
  const qc = useQueryClient();
  const [a, setA] = useState<PickState>(emptyPick);
  const [b, setB] = useState<PickState>(emptyPick);
  const [same, setSame] = useState(true);
  const [mode, setMode] = useState("auto");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const complete = (p: PickState) => p.engine === "off" || (!!p.engine && !!p.engineProfile && !!p.model);
  const asCard = (p: PickState): CardPick => (p.engine === "off" ? "off" : { engine: p.engine, engineProfile: p.engineProfile, model: p.model });
  const cards: Record<string, CardPick> = { A: asCard(a), B: same ? "same" : asCard(b) };
  const problems = [a.engine && a.engine !== "off" && complete(a) ? pickProblem(d, "A", a) : null, !same && b.engine && b.engine !== "off" && complete(b) ? pickProblem(d, "B", b) : null].filter(Boolean);
  const ready = complete(a) && (same || complete(b)) && !(a.engine === "off" && (same || b.engine === "off")) && problems.length === 0;
  const go = async (save: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const profile = save ? (await api.profileSave({ name, cards, mode })).profile : (await api.profileCompose(cards)).profile;
      await qc.invalidateQueries({ queryKey: ["profiles"] });
      await onUse(profile, save ? null : mode);
      onClose();
    } catch (e) {
      setError((e as { data?: { error?: string } }).data?.error ?? (e instanceof Error ? e.message : String(e)));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-2xl" data-testid="profile-builder">
        <DialogTitle>Build a setup</DialogTitle>
        <DialogDescription>
          Per card: an engine, one of its engine profiles (slots, cache, images) and a model that runs on that engine. Card A is the fast V100-SXM3, B the
          V100-PCIe. Only combinations that can run are offered; the VRAM check is a rough estimate.
        </DialogDescription>
        <CardBuilder card="A" d={d} pick={a} onPick={setA} allowOff={!same} />
        <label className="flex items-center gap-2 text-xs">
          <input type="checkbox" checked={same} onChange={(e) => setSame(e.target.checked)} data-testid="builder-same" /> same on both cards
        </label>
        {!same && <CardBuilder card="B" d={d} pick={b} onPick={setB} allowOff />}
        <label className="flex items-center gap-2 text-xs">
          Mode
          <select className="rounded border bg-background px-1 py-0.5" value={mode} onChange={(e) => setMode(e.target.value)} title={MODE_HELP} data-testid="builder-mode">
            {(d.allModes ?? ["auto", "hands-on", "orchestrator", "peer"]).map((m) => (
              <option key={m}>{m}</option>
            ))}
          </select>
        </label>
        <div className="flex flex-wrap items-center gap-2 border-t pt-2 text-xs">
          <input className="h-7 w-48 rounded border bg-background px-1" placeholder="name to save it as" value={name} onChange={(e) => setName(e.target.value)} data-testid="builder-name" />
          {isAdmin() && (
            <Button size="sm" variant="outline" disabled={!ready || !name.trim() || busy} onClick={() => void go(true)} data-testid="builder-save">
              Save and use
            </Button>
          )}
          <Button size="sm" disabled={!ready || busy} onClick={() => void go(false)} data-testid="builder-use">
            Use without saving
          </Button>
          {busy && <Loader2 className="size-3.5 animate-spin" />}
        </div>
        {error && <p className="text-xs text-destructive">{error}</p>}
      </DialogContent>
    </Dialog>
  );
}

/**
 * Chat footer: this project's profile (a saved setup, or one built here) and its mode, chosen on its own.
 * Changing either restarts the session on the same conversation with the next prompt.
 */
export function ProfilePicker({ pid, busy, onChanged }: { pid: string; busy: boolean; onChanged?: () => void }) {
  const qc = useQueryClient();
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const primary = info.data?.primary === pid;
  const profiles = useProfiles(!!info.data?.profiles);
  const sw = useServerSwitch();
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [about, setAbout] = useState(false);
  const [building, setBuilding] = useState(false);
  const d = profiles.data;
  if (!d?.available || !d.profiles) return null;
  const all = d.profiles;
  const current = d.projects[pid] ?? null;
  const prof = current ? all[current] : null;
  const mode = d.projectModes?.[pid] ?? null;
  const defaultMode = prof?.defaultMode ?? "auto";
  const modes = d.allModes ?? ["auto", "hands-on", "orchestrator", "peer"];
  const disabled = busy || saving || primary;
  const why = primary
    ? "This project's session is the TUI's own: /profile there switches it"
    : busy
      ? "Change this when the current turn has finished"
      : undefined;

  const run = async (f: () => Promise<{ ok: boolean; message?: string | null }>) => {
    setError(null);
    setSaving(true);
    try {
      const r = await f();
      if (!r.ok) setError(r.message ?? "Not changed.");
      await qc.invalidateQueries({ queryKey: ["profiles"] });
      onChanged?.();
    } catch (e) {
      setError((e as { data?: { message?: string } }).data?.message ?? (e instanceof Error ? e.message : String(e)));
    } finally {
      setSaving(false);
    }
  };
  const select = "rounded border bg-background px-1 py-0.5 text-[11px] disabled:opacity-50";

  return (
    <span className="inline-flex flex-wrap items-center gap-1.5" data-testid="profile-picker">
      <label className="inline-flex items-center gap-1">
        <span>Profile</span>
        <select
          className={select}
          value={current ?? ""}
          disabled={disabled}
          title={why ?? (prof ? `${prof.title ?? current}: ${cardsText(prof)}` : "The inference profile this project's session runs with")}
          onChange={(e) => void run(() => api.setProjectProfile(pid, e.target.value || null))}
          data-testid="profile-select"
        >
          <option value="">config's own</option>
          {Object.entries(all)
            .filter(([name, p]) => !p.hidden || name === current)
            .map(([name, p]) => (
              <option key={name} value={name}>
                {p.source === "composed" ? `built: ${p.title ?? name}` : name}
                {p.fits ? "" : " (switch server)"}
              </option>
            ))}
        </select>
      </label>
      <button className="underline decoration-dotted disabled:opacity-50" disabled={disabled} onClick={() => setBuilding(true)} data-testid="profile-build">
        build…
      </button>
      {!prof?.bridge && (
        <label className="inline-flex items-center gap-1">
          <span>Mode</span>
          <select
            className={select}
            value={mode ?? ""}
            disabled={disabled}
            title={why ?? MODE_HELP}
            onChange={(e) => void run(() => api.setProjectMode(pid, e.target.value || null))}
            data-testid="mode-select"
          >
            <option value="">default: {defaultMode}</option>
            {modes.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </label>
      )}
      <button className="underline decoration-dotted" onClick={() => setAbout(true)} title="What the profiles do" data-testid="profile-about">
        about
      </button>
      {saving && <Loader2 className="size-3 animate-spin" />}
      {current && prof && !prof.fits && (
        <span className="text-warning">
          needs layout <span className="font-mono">{prof.layout}</span> (live: {d.live?.layout ?? "?"}){" "}
          {isAdmin() ? (
            <button className="underline" onClick={() => void sw.start(current)} data-testid="profile-switch-server">
              switch server…
            </button>
          ) : (
            "an admin can switch the server"
          )}
        </span>
      )}
      {error && <span className="text-destructive">{error}</span>}
      <ProfileBuilder
        d={d}
        open={building}
        onClose={() => setBuilding(false)}
        onUse={async (profile, m) => {
          await run(() => api.setProjectProfile(pid, profile));
          if (m) await run(() => api.setProjectMode(pid, m));
        }}
      />
      <Dialog open={about} onOpenChange={setAbout}>
        <DialogContent className="max-w-3xl" data-testid="profile-about-dialog">
          <DialogTitle>Inference profiles</DialogTitle>
          <DialogDescription>
            A profile = per card an engine, an engine profile and a model, plus a default mode. This project:{" "}
            <span className="font-mono text-foreground">{current ?? "the config's own settings"}</span>
            {!prof?.bridge && (
              <>
                {" "}
                · mode <span className="font-mono text-foreground">{mode ?? `${defaultMode} (the profile's default)`}</span>
              </>
            )}{" "}
            · live server layout <span className="font-mono text-foreground">{d.live?.layout ?? "unknown"}</span>
          </DialogDescription>
          {prof && (
            <div className="space-y-1 rounded-md border p-2 text-sm" data-testid="profile-active">
              <div className="font-medium">{prof.title ?? current}</div>
              {prof.description && <p className="text-muted-foreground">{prof.description}</p>}
              <p className="font-mono text-xs">{cardsText(prof)}</p>
              {prof.summary && <p className="text-xs text-muted-foreground">{prof.summary}</p>}
            </div>
          )}
          <p className="text-xs text-muted-foreground">Modes work with any layout; a profile's own mode is only its default. {MODE_HELP}</p>
          <LayoutTable
            data={d}
            onSwitch={isAdmin() ? (p) => {
              setAbout(false);
              void sw.start(p);
            } : undefined}
            onDelete={isAdmin() ? (p) => {
              if (confirm(`Delete the saved profile ${p}?`)) void api.profileDelete(p).then(() => qc.invalidateQueries({ queryKey: ["profiles"] }));
            } : undefined}
          />
        </DialogContent>
      </Dialog>
      <ServerSwitchDialog sw={sw} />
    </span>
  );
}
