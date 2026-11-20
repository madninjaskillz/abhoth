import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, CheckCircle2, Loader2, Server } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { api, type LayoutDefinition, type ProfileList, type SwitchJob, type SwitchPlan } from "@/lib/api";
import { menuRow } from "@/components/ui/misc";
import { cn } from "@/lib/utils";

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

function LayoutTable({ data, onSwitch }: { data: ProfileList; onSwitch: (profile: string) => void }) {
  const entries = Object.entries(data.profiles ?? {});
  return (
    <div className="max-h-[60vh] overflow-auto">
      <table className="w-full text-xs" data-testid="profile-table">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="py-1.5 pr-2 font-medium">Profile</th>
            <th className="px-2 font-medium">Layout</th>
            <th className="px-2 font-medium">About</th>
            <th className="px-2" />
          </tr>
        </thead>
        <tbody>
          {entries.map(([name, p]) => (
            <tr key={name} className="border-b last:border-0 align-top">
              <td className="py-1.5 pr-2 font-mono">{name}</td>
              <td className="px-2">
                <span className="font-mono">{p.layout ?? "none"}</span>
                {p.layout && <div className="text-[11px] text-muted-foreground">{layoutSummary(data.layouts?.[p.layout])}</div>}
              </td>
              <td className="px-2">
                {p.title}
                {p.restore && <div className="text-[11px] text-warning">{p.restore}</div>}
              </td>
              <td className="px-2 text-right">
                {p.fits ? (
                  <span className="text-[11px] text-success">{p.layout ? "live" : "no server needed"}</span>
                ) : (
                  <Button size="sm" variant="outline" className="h-6" disabled={data.offline} onClick={() => onSwitch(name)} data-testid={`switch-${name}`}>
                    Switch…
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Chat footer: this project's profile. Its session restarts on the same conversation with the next prompt. */
export function ProfilePicker({ pid, busy, onChanged }: { pid: string; busy: boolean; onChanged?: () => void }) {
  const qc = useQueryClient();
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const primary = info.data?.primary === pid;
  const profiles = useProfiles(!!info.data?.profiles);
  const sw = useServerSwitch();
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const d = profiles.data;
  if (!d?.available || !d.profiles) return null;
  const current = d.projects[pid] ?? null;
  const cur = current ? d.profiles[current] : null;

  const choose = async (name: string | null) => {
    setError(null);
    setSaving(true);
    try {
      const r = await api.setProjectProfile(pid, name);
      if (!r.ok) setError(r.message ?? "Not switched.");
      await qc.invalidateQueries({ queryKey: ["profiles"] });
      onChanged?.();
    } catch (e) {
      setError((e as { data?: { message?: string } }).data?.message ?? (e instanceof Error ? e.message : String(e)));
    } finally {
      setSaving(false);
    }
  };

  return (
    <span className="inline-flex items-center gap-1.5" data-testid="profile-picker">
      <label className="inline-flex items-center gap-1">
        <span>Profile</span>
        <select
          className="rounded border bg-background px-1 py-0.5 text-[11px] disabled:opacity-50"
          value={current ?? ""}
          disabled={busy || saving || primary}
          title={
            primary
              ? "This project's session is the TUI's own: /profile there switches it"
              : busy
                ? "Switch profiles when the current turn has finished"
                : "The inference profile this project's session runs with"
          }
          onChange={(e) => void choose(e.target.value || null)}
          data-testid="profile-select"
        >
          <option value="">config's own</option>
          {Object.entries(d.profiles).map(([name, p]) => (
            <option key={name} value={name}>
              {name}
              {p.fits ? "" : " (switch server)"}
            </option>
          ))}
        </select>
      </label>
      {saving && <Loader2 className="size-3 animate-spin" />}
      {cur && !cur.fits && (
        <span className="text-warning">
          needs layout <span className="font-mono">{cur.layout}</span> (live: {d.live?.layout ?? "?"}){" "}
          <button className="underline" onClick={() => void sw.start(current!)} data-testid="profile-switch-server">
            switch server…
          </button>
        </span>
      )}
      {error && <span className="text-destructive">{error}</span>}
      <ServerSwitchDialog sw={sw} />
    </span>
  );
}
