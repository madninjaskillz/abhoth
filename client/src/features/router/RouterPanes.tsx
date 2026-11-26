// The router control plane's admin pages: settings tabs for admins (D143 registry), talking to
// the router through the web server's signed proxy. Live state comes from one SSE stream.
import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, KeyRound, Loader2, MoreHorizontal, Plus, Radio, RefreshCw, ServerCog, Trash2, Unlock } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { RouterApiError, routerApi, serviceKeyApi, useLiveState, type ServiceKey } from "./routerApi";
import { accessLabel, ago, coldLabel, type Job, type LayoutView, type Plan, type RouteEdit, type RouteView } from "./routerModel";
import { CardTile, GuardChips, JobStateBadge, JobSteps, LayoutList, PlanView, SwitchProgress } from "./widgets";

const ConfigEditor = lazy(() => import("./ConfigEditor"));

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";
const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

function PaneHead({ icon, title, children }: { icon: React.ReactNode; title: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1 border-b px-4 py-3">
      <div className="flex items-center gap-2 text-sm font-semibold">{icon} {title}</div>
      <p className="text-sm text-muted-foreground">{children}</p>
    </div>
  );
}

function StreamDot({ status }: { status: string }) {
  return (
    <span className="flex items-center gap-1 text-[11px] text-muted-foreground" data-testid="stream-status">
      <Radio className={cn("size-3", status === "live" ? "text-success" : "text-warning")} />
      {status === "live" ? "live" : status === "connecting" ? "connecting…" : "reconnecting…"}
    </span>
  );
}

// ── Overview: servers, cards, guards, layouts, plan and switch ────────────────────────────────

export function RouterOverviewPane() {
  const { state, status, lastJob } = useLiveState();
  const [planFor, setPlanFor] = useState<LayoutView | null>(null);
  const [cardAct, setCardAct] = useState<{ card: string; action: "offline" | "online" } | null>(null);
  const [menu, setMenu] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [, tick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, []);
  const cardMut = useMutation({
    mutationFn: (a: { card: string; action: "offline" | "online" }) => routerApi.card(a.card, a.action, "from the admin page"),
    onError: (e) => setError(errText(e)),
  });

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="router-overview">
      <PaneHead icon={<ServerCog className="size-4 text-muted-foreground" />} title="Inference">
        The GPU servers the router manages: what runs on each card, its health, and the layouts you can switch to. A switch is planned first and refused while a guard or a lease blocks it.
      </PaneHead>
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4">
        {!state && <div className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Connecting to the router…</div>}
        {state?.configError && <div className="rounded-md border border-destructive/50 p-2 text-xs text-destructive" role="alert">The config on disk does not load; the router keeps the last good one. {state.configError}</div>}
        {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
        {state && (
          <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            <span>config <span className="font-mono text-foreground">{state.rev}</span></span>
            <Badge variant={state.mode === "act" ? "default" : "secondary"}>{state.mode}</Badge>
            <span className="ml-auto"><StreamDot status={status} /></span>
          </div>
        )}
        {state && <GuardChips guards={state.guards} />}
        {state?.job && <SwitchProgress job={state.job} />}
        {!state?.job && lastJob && lastJob.state !== "running" && (
          <div className="flex items-center gap-2 text-xs text-muted-foreground" data-testid="last-job">
            Last job <span className="font-mono">{lastJob.id}</span> <JobStateBadge state={lastJob.state} /> {lastJob.target}
          </div>
        )}
        {state?.servers.map((srv) => (
          <section key={srv.id} className="space-y-3" data-testid="server" data-server={srv.id}>
            <div className="flex flex-wrap items-center gap-2">
              <h3 className="text-sm font-semibold">{srv.title}</h3>
              <Badge variant={srv.reachability === "ok" ? "success" : srv.reachability === "degraded" ? "destructive" : "secondary"}>{srv.reachability === "ok" ? `${srv.exec} ok` : srv.reachability}</Badge>
              {srv.drift && <Badge variant="warning">active layout changed: apply</Badge>}
              {!srv.hostctlOk && <Badge variant="warning">hostctl allowlist out of date</Badge>}
              <span className="ml-auto text-xs text-muted-foreground">layout <span className="font-mono text-foreground">{srv.activeLayout ?? "custom"}</span></span>
            </div>
            <div className="grid gap-3 lg:grid-cols-2">
              {srv.cards.map((c) => (
                <CardTile
                  key={c.id}
                  card={c}
                  actions={c.managed ? (
                    <div className="relative">
                      <Button size="icon-sm" variant="ghost" aria-label={`Card ${c.id} actions`} onClick={() => setMenu(menu === c.id ? null : c.id)}><MoreHorizontal /></Button>
                      {menu === c.id && (
                        <div className="absolute right-0 top-full z-20 mt-1 w-40 rounded-md border bg-panel p-1 text-xs shadow-lg" role="menu">
                          <button role="menuitem" className="w-full rounded px-2 py-1.5 text-left hover:bg-accent" onClick={() => { setMenu(null); setCardAct({ card: c.id, action: c.adminState === "offline" ? "online" : "offline" }); }}>
                            {c.adminState === "offline" ? "Bring online" : "Take offline"}
                          </button>
                        </div>
                      )}
                    </div>
                  ) : undefined}
                />
              ))}
            </div>
            <h4 className="pt-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Layouts</h4>
            <LayoutList layouts={state.layouts.filter((l) => l.server === srv.id)} onPick={setPlanFor} disabled={!!state.job} />
          </section>
        ))}
      </div>
      {planFor && <PlanDialog layout={planFor} onClose={() => setPlanFor(null)} />}
      <ConfirmDialog
        open={!!cardAct}
        title={cardAct?.action === "offline" ? `Take card ${cardAct?.card} offline?` : `Bring card ${cardAct?.card} online?`}
        body={cardAct?.action === "offline" ? "Its engine drains and stops; routes and slots skip it until it is back online." : "Its engine for the active layout starts again."}
        confirm={cardAct?.action === "offline" ? "Take offline" : "Bring online"}
        destructive={cardAct?.action === "offline"}
        onCancel={() => setCardAct(null)}
        onConfirm={() => { if (cardAct) cardMut.mutate(cardAct); setCardAct(null); }}
      />
    </div>
  );
}

function PlanDialog({ layout, onClose }: { layout: LayoutView; onClose: () => void }) {
  const plan = useQuery({ queryKey: ["router-plan", layout.server, layout.id], queryFn: () => routerApi.plan(layout.server, layout.id), retry: false });
  const [job, setJob] = useState<Job | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = useMutation({
    mutationFn: () => routerApi.switchTo(layout.server, layout.id),
    onSuccess: (j) => setJob(j),
    onError: (e) => {
      setError(errText(e));
      const p = (e as RouterApiError).data?.plan as Plan | undefined;
      if (p) void plan.refetch();
    },
  });
  // Follow the job until it ends.
  const live = useQuery({
    queryKey: ["router-job", job?.id],
    queryFn: () => routerApi.job(job!.id),
    enabled: !!job,
    refetchInterval: (q) => (q.state.data && q.state.data.state !== "running" && q.state.data.state !== "queued" ? false : 1000),
  });
  const shown = live.data ?? job;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[88vh] max-w-3xl overflow-y-auto" data-testid="plan-dialog">
        <DialogTitle>Switch {layout.server} to {layout.title || layout.id}</DialogTitle>
        <DialogDescription>The plan is a dry run: nothing changes until you run it. Running re-plans first and refuses if anything that matters changed.</DialogDescription>
        {plan.isLoading && <div className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Planning…</div>}
        {plan.isError && <div className="text-xs text-destructive" role="alert">{errText(plan.error)}</div>}
        {shown ? <JobSteps job={shown} /> : plan.data && <PlanView plan={plan.data} />}
        {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
        <div className="flex justify-end gap-2 pt-2">
          <Button size="sm" variant="ghost" onClick={onClose}>{shown ? "Close" : "Cancel"}</Button>
          {!shown && (
            <>
              <Button size="sm" variant="outline" onClick={() => void plan.refetch()} disabled={plan.isFetching}><RefreshCw /> Re-plan</Button>
              <Button size="sm" onClick={() => { setError(null); run.mutate(); }} disabled={!plan.data?.runnable || run.isPending} data-testid="plan-run">
                {run.isPending && <Loader2 className="animate-spin" />} Run switch
              </Button>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Routes: cold policy, fallbacks, access ───────────────────────────────────────────────────

export function RouterRoutesPane() {
  const qc = useQueryClient();
  const routes = useQuery({ queryKey: ["router-routes"], queryFn: routerApi.routes });
  const [editing, setEditing] = useState<RouteView | null>(null);
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="router-routes">
      <PaneHead icon={<ServerCog className="size-4 text-muted-foreground" />} title="Routes">
        The model names clients use. What happens when a name's model is not loaded (cold): refuse, use a fallback, or switch a card on demand (only for catalog models, only for callers allowed to). Changes apply to new requests at once and are audited.
      </PaneHead>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {routes.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
        {routes.isError && <div className="text-xs text-destructive">{errText(routes.error)}</div>}
        {routes.data && (
          <table className="w-full text-xs" data-testid="routes-table">
            <thead className="text-left text-muted-foreground">
              <tr className="border-b">
                <th className="py-1.5 pr-2 font-medium">Name</th>
                <th className="pr-2 font-medium">Targets</th>
                <th className="pr-2 font-medium">When cold</th>
                <th className="pr-2 font-medium">Who</th>
                <th className="pr-2 font-medium">Now</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {routes.data.map((r) => (
                <tr key={r.name} className={cn("border-b last:border-0 align-top", r.hidden && "text-muted-foreground")} data-testid="route-row" data-route={r.name}>
                  <td className="whitespace-nowrap py-1.5 pr-2 font-mono">{r.name}{r.hidden && <Badge variant="outline" className="ml-1">hidden</Badge>}</td>
                  <td className="pr-2 text-muted-foreground">{r.sameAs ? `same as ${r.sameAs}` : r.targets.join(", ")}</td>
                  <td className="pr-2">{coldLabel(r.cold, r.fallback)}</td>
                  <td className="pr-2">{accessLabel(r.access)}{r.allowUserSwitch && <div className="text-[11px] text-warning">users may switch</div>}</td>
                  <td className="pr-2">{r.servingNow.length ? <span className="text-success">{r.servingNow.join(", ")}</span> : <span className="text-muted-foreground">cold</span>}</td>
                  <td className="text-right"><Button size="sm" variant="ghost" onClick={() => setEditing(r)} data-testid="route-edit">Edit</Button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {editing && routes.data && (
        <RouteDialog route={editing} names={routes.data.map((r) => r.name)} onClose={() => setEditing(null)} onSaved={() => { setEditing(null); void qc.invalidateQueries({ queryKey: ["router-routes"] }); }} />
      )}
    </div>
  );
}

function RouteDialog({ route, names, onClose, onSaved }: { route: RouteView; names: string[]; onClose: () => void; onSaved: () => void }) {
  const [cold, setCold] = useState(route.cold);
  const [fallback, setFallback] = useState<string[]>(route.fallback);
  const [accessKind, setAccessKind] = useState(route.access.startsWith("keys:") ? "keys" : route.access || "users");
  const [keys, setKeys] = useState(route.access.startsWith("keys:") ? route.access.slice(5) : "");
  const [hidden, setHidden] = useState(route.hidden);
  const [userSwitch, setUserSwitch] = useState(route.allowUserSwitch);
  const [error, setError] = useState<string | null>(null);
  const catalog = route.targets.length > 0 && route.targets.every((t) => t.startsWith("model:"));
  const save = useMutation({
    mutationFn: () => {
      const edit: RouteEdit = {
        cold,
        fallback,
        access: accessKind === "keys" ? `keys:${keys.split(",").map((k) => k.trim()).filter(Boolean).join(",")}` : accessKind,
        hidden,
        allowUserSwitch: userSwitch,
      };
      return routerApi.editRoute(route.name, edit);
    },
    onSuccess: onSaved,
    onError: (e) => setError(errText(e)),
  });
  const others = names.filter((n) => n !== route.name);
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-lg" data-testid="route-dialog">
        <DialogTitle>Route <span className="font-mono">{route.name}</span></DialogTitle>
        <DialogDescription>{route.targets.join(", ")}</DialogDescription>
        <div className="space-y-3 text-sm">
          <fieldset className="space-y-1">
            <legend className="text-xs font-medium">When its model is not loaded</legend>
            {[
              ["reject", "Refuse (503 model_not_live)"],
              ["fallback", "Use a fallback route"],
              ["on_demand", "Switch a card on demand"],
            ].map(([v, label]) => (
              <label key={v} className={cn("flex items-center gap-2 text-xs", v === "on_demand" && !catalog && "opacity-50")}>
                <input type="radio" name="cold" value={v} checked={cold === v} onChange={() => setCold(v)} disabled={v === "on_demand" && !catalog} />
                {label}
                {v === "on_demand" && !catalog && <span className="text-muted-foreground">(only for catalog models; generic and pinned names never switch)</span>}
              </label>
            ))}
          </fieldset>
          {cold === "fallback" && (
            <div className="space-y-1">
              <div className="text-xs font-medium">Fallbacks, in order</div>
              <div className="flex flex-wrap gap-1">
                {others.map((n) => (
                  <button
                    key={n}
                    className={cn("rounded-full border px-2 py-0.5 text-[11px] font-mono", fallback.includes(n) ? "border-primary bg-primary/10 text-primary" : "text-muted-foreground hover:bg-accent")}
                    onClick={() => setFallback(fallback.includes(n) ? fallback.filter((x) => x !== n) : [...fallback, n])}
                    data-testid="fallback-pick"
                  >
                    {fallback.includes(n) ? `${fallback.indexOf(n) + 1}. ` : ""}{n}
                  </button>
                ))}
              </div>
            </div>
          )}
          <div className="grid grid-cols-[auto,1fr] items-center gap-2">
            <label className="text-xs font-medium" htmlFor="route-access">Who may use it</label>
            <select id="route-access" className={field} value={accessKind} onChange={(e) => setAccessKind(e.target.value)}>
              <option value="users">Every user</option>
              <option value="admins">Admins only</option>
              <option value="keys">Named service keys</option>
            </select>
            {accessKind === "keys" && (
              <>
                <span className="text-xs text-muted-foreground">Keys</span>
                <input className={field} value={keys} onChange={(e) => setKeys(e.target.value)} placeholder="jarvis, switchers" aria-label="Key names or groups" />
              </>
            )}
          </div>
          <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={hidden} onChange={(e) => setHidden(e.target.checked)} /> Hidden from the model list</label>
          {cold === "on_demand" && (
            <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={userSwitch} onChange={(e) => setUserSwitch(e.target.checked)} /> Users (not only admins and switch keys) may cause a switch</label>
          )}
          {error && <div className="text-xs text-destructive" role="alert" data-testid="route-error">{error}</div>}
        </div>
        <div className="flex justify-end gap-2 pt-2">
          <Button size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
          <Button size="sm" onClick={() => { setError(null); save.mutate(); }} disabled={save.isPending} data-testid="route-save">
            {save.isPending && <Loader2 className="animate-spin" />} Save
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Config: the TOML with inline validation ──────────────────────────────────────────────────

export function RouterConfigPane() {
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="router-config">
      <PaneHead icon={<ServerCog className="size-4 text-muted-foreground" />} title="Router config">
        router.toml as text, checked as you type. Saving backs up the file, writes it atomically, records who and why, and reloads it. Nothing restarts: a changed active layout shows as drift with a plan.
      </PaneHead>
      <Suspense fallback={<div className="p-4"><Loader2 className="size-4 animate-spin text-muted-foreground" /></div>}>
        <ConfigEditor />
      </Suspense>
    </div>
  );
}

// ── Activity: jobs, audit, leases ────────────────────────────────────────────────────────────

export function RouterActivityPane() {
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="router-activity">
      <PaneHead icon={<ServerCog className="size-4 text-muted-foreground" />} title="Jobs, audit & leases">
        Switches and card changes with their steps, every change to the router's config and state, and the slot leases held now.
      </PaneHead>
      <Tabs defaultValue="jobs" className="flex min-h-0 flex-1 flex-col">
        <div className="border-b px-4 py-2">
          <TabsList>
            <TabsTrigger value="jobs" data-testid="activity-jobs">Jobs</TabsTrigger>
            <TabsTrigger value="audit" data-testid="activity-audit">Audit</TabsTrigger>
            <TabsTrigger value="leases" data-testid="activity-leases">Leases</TabsTrigger>
          </TabsList>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto p-4">
          <TabsContent value="jobs"><JobsList /></TabsContent>
          <TabsContent value="audit"><AuditList /></TabsContent>
          <TabsContent value="leases"><LeasesList /></TabsContent>
        </div>
      </Tabs>
    </div>
  );
}

function JobsList() {
  const jobs = useQuery({ queryKey: ["router-jobs"], queryFn: () => routerApi.jobs(50), refetchInterval: 5000 });
  const [open, setOpen] = useState<string | null>(null);
  if (jobs.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (jobs.isError) return <div className="text-xs text-destructive">{errText(jobs.error)}</div>;
  if (!jobs.data?.length) return <div className="text-xs text-muted-foreground">No jobs yet.</div>;
  return (
    <div className="divide-y rounded-md border" data-testid="jobs-list">
      {jobs.data.map((j) => (
        <div key={j.id}>
          <button className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs hover:bg-accent/50" onClick={() => setOpen(open === j.id ? null : j.id)} aria-expanded={open === j.id}>
            <span className="font-mono">{j.id}</span>
            <JobStateBadge state={j.state} />
            <span>{j.kind} {j.from ? `${j.from} → ` : ""}{j.target}</span>
            <span className="ml-auto text-muted-foreground">{j.requestedBy} · {ago(j.started)}</span>
          </button>
          {open === j.id && <div className="px-3 pb-3"><JobSteps job={j} /></div>}
        </div>
      ))}
    </div>
  );
}

function AuditList() {
  const audit = useQuery({ queryKey: ["router-audit"], queryFn: () => routerApi.audit(200) });
  const [open, setOpen] = useState<number | null>(null);
  if (audit.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (audit.isError) return <div className="text-xs text-destructive">{errText(audit.error)}</div>;
  return (
    <div className="divide-y rounded-md border" data-testid="audit-list">
      {audit.data?.map((a) => (
        <div key={a.id} className="px-3 py-1.5 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-muted-foreground" title={new Date(a.time).toLocaleString()}>{ago(a.time)}</span>
            <span className="font-medium">{a.action}</span>
            <span>{a.target}</span>
            {a.revTo && <span className="font-mono text-muted-foreground">{a.revFrom} → {a.revTo}</span>}
            <span className="ml-auto text-muted-foreground">{a.who}</span>
            {a.diff && <Button size="sm" variant="ghost" onClick={() => setOpen(open === a.id ? null : a.id)}>{open === a.id ? "Hide" : "Diff"}</Button>}
          </div>
          {a.reason && <div className="text-muted-foreground">“{a.reason}”</div>}
          {open === a.id && a.diff && <DiffBlock diff={a.diff} />}
        </div>
      ))}
    </div>
  );
}

export function DiffBlock({ diff }: { diff: string }) {
  return (
    <pre className="mt-1 max-h-64 overflow-auto rounded bg-muted p-2 font-mono text-[11px]" data-testid="diff">
      {diff.split("\n").map((l, i) => (
        <div key={i} className={cn(l.startsWith("+") && "text-success", l.startsWith("-") && "text-destructive")}>{l || " "}</div>
      ))}
    </pre>
  );
}

function LeasesList() {
  const qc = useQueryClient();
  const leases = useQuery({ queryKey: ["router-leases"], queryFn: routerApi.leases, refetchInterval: 5000 });
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const release = useMutation({
    mutationFn: (k: string) => routerApi.release(k),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["router-leases"] }),
    onError: (e) => setError(errText(e)),
  });
  if (leases.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (leases.isError) return <div className="text-xs text-destructive">{errText(leases.error)}</div>;
  const v = leases.data!;
  return (
    <div className="space-y-2">
      {v.frozen && <Badge variant="warning">slots frozen (a switch is running)</Badge>}
      {error && <div className="text-xs text-destructive">{error}</div>}
      {!v.leases.length ? <div className="text-xs text-muted-foreground">No leases held.</div> : (
        <table className="w-full text-xs" data-testid="leases-table">
          <thead className="text-left text-muted-foreground"><tr className="border-b"><th className="py-1.5 pr-2 font-medium">Slot</th><th className="pr-2 font-medium">Kind</th><th className="pr-2 font-medium">Holder</th><th className="pr-2 font-medium">Who</th><th className="pr-2 font-medium">Expires</th><th /></tr></thead>
          <tbody>
            {v.leases.map((l) => (
              <tr key={l.key} className="border-b last:border-0">
                <td className="py-1.5 pr-2 font-mono">{l.slot} <span className="text-muted-foreground">({l.card})</span></td>
                <td className="pr-2"><Badge variant={l.kind === "guard" ? "warning" : l.kind === "main" ? "default" : "secondary"}>{l.kind}</Badge>{l.reason && <span className="ml-1 text-muted-foreground">{l.reason}</span>}</td>
                <td className="pr-2">{l.holder} <span className="font-mono text-muted-foreground">{l.key}</span></td>
                <td className="pr-2 text-muted-foreground">{l.principal}</td>
                <td className="pr-2 tabular-nums">{l.expiresInSeconds} s</td>
                <td className="text-right"><Button size="sm" variant="ghost" onClick={() => setPending(l.key)} data-testid="lease-release"><Unlock /> Release</Button></td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <ConfirmDialog
        open={!!pending}
        title="Release this lease?"
        body="Its holder loses the slot at once; a running agent or bench may then queue behind other work."
        confirm="Release"
        destructive
        onCancel={() => setPending(null)}
        onConfirm={() => { if (pending) release.mutate(pending); setPending(null); }}
      />
    </div>
  );
}

// ── Service keys ─────────────────────────────────────────────────────────────────────────────

const PRESETS: { name: string; label: string; scopes: string[]; note: string }[] = [
  { name: "jarvis", label: "Jarvis", scopes: ["chat", "switch"], note: "Jarvis: chat and layout switches" },
  { name: "video-pipelines", label: "Video pipelines", scopes: ["chat"], note: "AI News and Video Game News: chat only" },
  { name: "home-assistant", label: "Home Assistant", scopes: ["switch", "read"], note: "HA buttons: switch layouts, read state" },
  { name: "dash", label: "Dash buttons", scopes: ["read"], note: "The dashboard: read state (its switch buttons open the admin page)" },
];

const SCOPE_HELP: Record<string, string> = {
  chat: "send chats to the routes it may use",
  models: "list the models (without chat)",
  switch: "plan and switch layouts; start on-demand switches",
  leases: "hold slot leases",
  read: "read state, cards and jobs (no config text, no audit)",
};

export function ServiceKeysPane() {
  const qc = useQueryClient();
  const list = useQuery({ queryKey: ["service-keys"], queryFn: serviceKeyApi.list });
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<string[]>([]);
  const [group, setGroup] = useState("");
  const [note, setNote] = useState("");
  const [made, setMade] = useState<ServiceKey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revoke, setRevoke] = useState<ServiceKey | null>(null);
  const refresh = () => void qc.invalidateQueries({ queryKey: ["service-keys"] });
  const create = useMutation({
    mutationFn: () => serviceKeyApi.create(name, [...scopes, ...group.split(",").map((g) => g.trim()).filter(Boolean).map((g) => `group:${g}`)], note),
    onSuccess: (k) => { setMade(k); setName(""); setScopes([]); setGroup(""); setNote(""); refresh(); },
    onError: (e) => setError(errText(e)),
  });
  const del = useMutation({ mutationFn: (id: number) => serviceKeyApi.revoke(id), onSuccess: refresh, onError: (e) => setError(errText(e)) });
  const all = useMemo(() => list.data?.scopes ?? Object.keys(SCOPE_HELP), [list.data]);
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="service-keys">
      <PaneHead icon={<KeyRound className="size-4 text-muted-foreground" />} title="Service keys">
        Named keys for the clients that talk to the router without a person: Jarvis, the video pipelines, Home Assistant, the dash. Each has only the scopes you give it, works only at the router (never in this web UI), and stops working within a minute of being revoked.
      </PaneHead>
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4">
        {list.data && !list.data.router && <div className="rounded-md border border-warning/50 p-2 text-xs text-warning">No router is linked to this server yet (ABHOTH_ROUTER_URL, ABHOTH_ROUTER_TOKEN_FILE). Keys made now work once it is.</div>}
        {made?.key && (
          <div className="space-y-1 rounded-md border border-success/50 bg-success/5 p-3 text-xs" data-testid="service-key-made" role="status">
            <div className="font-medium">Key for {made.name}: copy it now, it is not shown again.</div>
            <div className="flex items-center gap-2">
              <code className="flex-1 break-all rounded bg-muted px-2 py-1 font-mono">{made.key}</code>
              <Button size="icon-sm" variant="ghost" aria-label="Copy the key" onClick={() => void navigator.clipboard?.writeText(made.key!)}><Copy /></Button>
            </div>
          </div>
        )}
        <form className="space-y-2 rounded-md border p-3" onSubmit={(e) => { e.preventDefault(); setError(null); create.mutate(); }} data-testid="service-key-form">
          <div className="flex flex-wrap gap-1.5">
            <span className="text-xs text-muted-foreground">Start from:</span>
            {PRESETS.map((p) => (
              <button type="button" key={p.name} className="rounded-full border px-2 py-0.5 text-[11px] hover:bg-accent" onClick={() => { setName(p.name); setScopes(p.scopes); setNote(p.note); }} data-testid="service-key-preset">
                {p.label}
              </button>
            ))}
          </div>
          <div className="grid gap-2 sm:grid-cols-2">
            <input className={field} placeholder="name (jarvis)" value={name} onChange={(e) => setName(e.target.value)} aria-label="Key name" data-testid="service-key-name" />
            <input className={field} placeholder="note (optional)" value={note} onChange={(e) => setNote(e.target.value)} aria-label="Note" />
          </div>
          <div className="grid gap-1 sm:grid-cols-2">
            {all.map((s) => (
              <label key={s} className="flex items-start gap-2 text-xs">
                <input type="checkbox" className="mt-0.5" checked={scopes.includes(s)} onChange={(e) => setScopes(e.target.checked ? [...scopes, s] : scopes.filter((x) => x !== s))} />
                <span><span className="font-medium">{s}</span> <span className="text-muted-foreground">{SCOPE_HELP[s]}</span></span>
              </label>
            ))}
          </div>
          <input className={field} placeholder="groups for route access, e.g. switchers (optional)" value={group} onChange={(e) => setGroup(e.target.value)} aria-label="Groups" />
          {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
          <div className="flex justify-end"><Button size="sm" type="submit" disabled={!name.trim() || create.isPending} data-testid="service-key-create">{create.isPending ? <Loader2 className="animate-spin" /> : <Plus />} Issue key</Button></div>
        </form>
        <div className="rounded-md border">
          {list.isLoading && <div className="p-3"><Loader2 className="size-4 animate-spin text-muted-foreground" /></div>}
          {list.data && !list.data.keys.length && <div className="p-3 text-xs text-muted-foreground">No service keys yet.</div>}
          {!!list.data?.keys.length && (
            <table className="w-full text-xs" data-testid="service-keys-table">
              <thead className="text-left text-muted-foreground"><tr className="border-b"><th className="px-3 py-1.5 font-medium">Name</th><th className="px-2 font-medium">Scopes</th><th className="px-2 font-medium">Issued by</th><th className="px-2 font-medium">Last used</th><th /></tr></thead>
              <tbody>
                {list.data.keys.map((k) => (
                  <tr key={k.id} className="border-b last:border-0" data-testid="service-key-row">
                    <td className="px-3 py-1.5"><div className="font-medium">{k.name}</div><div className="text-[11px] text-muted-foreground"><span className="font-mono">{k.prefix}…</span>{k.note ? ` · ${k.note}` : ""}</div></td>
                    <td className="px-2"><div className="flex flex-wrap gap-1">{k.scopes.map((s) => <Badge key={s} variant={s === "switch" ? "warning" : "secondary"}>{s}</Badge>)}</div></td>
                    <td className="px-2 text-muted-foreground">{k.owner}</td>
                    <td className="px-2 text-muted-foreground">{k.lastUsed ? ago(k.lastUsed * 1000) : "never"}</td>
                    <td className="px-2 text-right"><Button size="sm" variant="ghost" onClick={() => setRevoke(k)} aria-label={`Revoke ${k.name}`}><Trash2 /></Button></td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
      <ConfirmDialog
        open={!!revoke}
        title={`Revoke ${revoke?.name}?`}
        body="The router stops accepting it within a minute. Whatever uses it needs a new key."
        confirm="Revoke"
        destructive
        onCancel={() => setRevoke(null)}
        onConfirm={() => { if (revoke) del.mutate(revoke.id); setRevoke(null); }}
      />
    </div>
  );
}


