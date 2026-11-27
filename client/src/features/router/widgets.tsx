// Presentational widgets for the inference control plane. They take data, fetch nothing, and
// are shared by the admin pages (settings tabs), the supervisor dashboard and the app's Models
// tab and status areas.
import { AlertTriangle, ArrowRight, Check, CircleDashed, Cpu, Gauge, Loader2, Lock, Minus, Thermometer, X, Zap } from "lucide-react";
import { Badge } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import {
  ago, cardHealth, coldLabel, fmtBytes, fmtGib, fmtSeconds, jobProgress, pct,
  type CardView, type CatalogModel, type GuardView, type Job, type JobSummary, type LayoutView, type Level, type Plan,
} from "./routerModel";

const levelText: Record<Level, string> = { ok: "text-success", warn: "text-warning", bad: "text-destructive", idle: "text-muted-foreground" };
const levelBar: Record<Level, string> = { ok: "bg-success", warn: "bg-warning", bad: "bg-destructive", idle: "bg-muted-foreground/40" };
const levelRing: Record<Level, string> = { ok: "border-border", warn: "border-warning/60", bad: "border-destructive/70", idle: "border-dashed border-border" };

/** A thin labelled bar (VRAM, utilisation, power). */
export function Meter({ label, value, max, text, level = "ok", icon }: { label: string; value: number; max: number; text: string; level?: Level; icon?: React.ReactNode }) {
  const p = pct(value, max);
  return (
    <div className="min-w-0" title={`${label}: ${text}`}>
      <div className="flex items-center justify-between gap-2 text-[11px] text-muted-foreground">
        <span className="flex min-w-0 items-center gap-1 truncate">{icon}{label}</span>
        <span className="whitespace-nowrap tabular-nums text-foreground">{text}</span>
      </div>
      <div className="mt-0.5 h-1.5 overflow-hidden rounded-full bg-muted" role="meter" aria-label={label} aria-valuenow={p} aria-valuemin={0} aria-valuemax={100}>
        <div className={cn("h-full rounded-full transition-[width] duration-500", levelBar[level])} style={{ width: `${p}%` }} />
      </div>
    </div>
  );
}

const engineStates: Record<string, { v: "success" | "warning" | "destructive" | "secondary"; spin?: boolean }> = {
  ready: { v: "success" },
  preparing: { v: "warning", spin: true },
  starting: { v: "warning", spin: true },
  warming: { v: "warning", spin: true },
  draining: { v: "warning" },
  stopping: { v: "warning", spin: true },
  failed: { v: "destructive" },
  unknown: { v: "destructive" },
  off: { v: "secondary" },
};

export function EngineState({ state }: { state: string }) {
  const s = engineStates[state] ?? { v: "secondary" as const };
  return (
    <Badge variant={s.v} data-testid="engine-state">
      {s.spin && <Loader2 className="mr-1 size-3 animate-spin" />}
      {state}
    </Badge>
  );
}

/** Lanes as dots: busy (in flight), leased, free; queued as a number. */
export function Lanes({ lanes, inflight, leases, queued }: { lanes: number; inflight: number; leases: number; queued: number }) {
  if (!lanes) return null;
  return (
    <div className="flex items-center gap-1" title={`${inflight} in flight of ${lanes} lanes, ${leases} leased, ${queued} queued`}>
      {Array.from({ length: lanes }, (_, i) => (
        <span
          key={i}
          className={cn(
            "size-2.5 rounded-sm border",
            i < inflight ? "border-primary bg-primary" : i < inflight + leases ? "border-primary/60 bg-primary/25" : "border-border bg-transparent",
          )}
        />
      ))}
      {queued > 0 && <span className="ml-1 text-[11px] text-warning">+{queued} queued</span>}
    </div>
  );
}

/** One GPU: engine, model and state; VRAM, utilisation, power; temperature and clocks; lanes. */
export function CardTile({ card, actions, compact }: { card: CardView; actions?: React.ReactNode; compact?: boolean }) {
  const h = cardHealth(card);
  const t = card.telemetry;
  const e = card.engine;
  return (
    <div className={cn("flex min-w-0 flex-col gap-2 rounded-lg border bg-background/40 p-3", levelRing[h.level])} data-testid="card-tile" data-card={card.id}>
      <div className="flex items-start gap-2">
        <div className={cn("grid size-8 shrink-0 place-items-center rounded-md bg-muted text-sm font-semibold", levelText[h.level])}>{card.id}</div>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium" title={card.label}>{card.device === "cpu" && <Badge variant="secondary" className="mr-1">CPU</Badge>}{card.label || t?.name || (card.device === "cpu" ? "CPU" : card.gpuUuid)}</div>
          <div className="truncate text-[11px] text-muted-foreground" title={card.device === "cpu" ? undefined : card.gpuUuid}>
            {card.device === "cpu" ? `${card.ramGib != null ? `${card.ramGib} GiB RAM` : "CPU"}${card.cores ? ` · ${card.cores} cores` : ""} · never switched · ` : ""}
            {card.managed ? (card.adminState === "online" ? (card.onDemand ? "managed · on-demand allowed" : "managed") : card.adminState) : "not managed"}
            {card.adminNote ? ` · ${card.adminNote}` : ""}
          </div>
        </div>
        {actions}
      </div>
      {e ? (
        <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-xs">
          <EngineState state={e.state} />
          <span className="truncate font-medium" title={`${e.engine} / ${e.profile} / ${e.model}`}>{e.model}</span>
          <span className="truncate text-muted-foreground">{e.engine} · {e.profile}</span>
          {!compact && e.since > 0 && <span className="text-[11px] text-muted-foreground">· {ago(e.since)}</span>}
        </div>
      ) : (
        <div className="text-xs text-muted-foreground">{card.foreign.length ? `in use by ${card.foreign.join(", ")}` : "no engine"}</div>
      )}
      {t && (
        <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-3">
          <Meter label="VRAM" icon={<Cpu className="size-3" />} value={t.memUsedMib} max={t.memTotalMib} text={`${Math.round(t.memUsedMib / 1024)}/${fmtGib(t.memTotalMib)}`} level={pct(t.memUsedMib, t.memTotalMib) > 97 ? "warn" : "ok"} />
          <Meter label="Load" icon={<Gauge className="size-3" />} value={t.utilPct} max={100} text={`${t.utilPct}%`} />
          <Meter label="Power" icon={<Zap className="size-3" />} value={t.powerW} max={t.powerLimitW ?? 300} text={`${Math.round(t.powerW)} W`} />
        </div>
      )}
      {t && !compact && (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
          <span className={cn("flex items-center gap-1", t.tempC >= 80 && levelText[t.tempC >= 88 ? "bad" : "warn"])}>
            <Thermometer className="size-3" /> {t.tempC} °C{t.memTempC ? ` · mem ${t.memTempC} °C` : ""}
          </span>
          <span className={cn(t.clockCheck !== "ok" && "text-warning")} title={t.clockReasons.join(", ") || "no clock events"}>
            {t.smMhz} / {t.smMaxMhz} MHz{t.appClockMhz ? ` · app ${t.appClockMhz}` : ""}{t.clockReasons.length ? ` · ${t.clockReasons.join(", ")}` : ""}
          </span>
          {t.stale && <span className="text-warning">stale</span>}
        </div>
      )}
      {card.lanes > 0 && <Lanes lanes={card.lanes} inflight={card.inflight} leases={card.leases} queued={card.queued} />}
      {h.reasons.length > 0 && h.level !== "ok" && (
        <div className={cn("flex items-center gap-1 text-[11px]", levelText[h.level])}>
          <AlertTriangle className="size-3" /> {h.reasons.join(" · ")}
        </div>
      )}
    </div>
  );
}

export function GuardChips({ guards }: { guards: GuardView[] }) {
  if (!guards.length) return null;
  return (
    <div className="flex flex-wrap gap-1.5" data-testid="guards">
      {guards.map((g) => (
        <span
          key={g.id}
          className={cn(
            "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px]",
            g.active ? "border-warning/50 bg-warning/10 text-warning" : g.stale ? "border-destructive/50 text-destructive" : "text-muted-foreground",
          )}
          title={`${g.kind}; blocks ${g.blocks.join(", ")}${g.reason ? `; ${g.reason}` : ""}`}
        >
          {g.active ? <Lock className="size-3" /> : <CircleDashed className="size-3" />}
          {g.id}
          {g.active && g.reason ? `: ${g.reason}` : g.stale ? ": stale" : ""}
        </span>
      ))}
    </div>
  );
}

/** The layouts of a server; the active one marked; `onPick` opens its plan. */
export function LayoutList({ layouts, onPick, disabled }: { layouts: LayoutView[]; onPick?: (l: LayoutView) => void; disabled?: boolean }) {
  return (
    <div className="divide-y rounded-md border" data-testid="layout-list">
      {layouts.map((l) => (
        <div key={`${l.server}/${l.id}`} className={cn("flex items-center gap-3 px-3 py-2", l.active && "bg-primary/5")}>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 text-sm">
              <span className="truncate font-medium">{l.title || l.id}</span>
              {l.active && <Badge variant="success">live</Badge>}
              {l.source !== "config" && <Badge variant="outline">{l.source}</Badge>}
            </div>
            <div className="mt-0.5 flex flex-wrap gap-x-3 text-[11px] text-muted-foreground">
              <span className="font-mono">{l.id}</span>
              {Object.entries(l.cards).map(([c, spec]) => (
                <span key={c}><span className="font-semibold text-foreground/80">{c}</span> {spec}</span>
              ))}
            </div>
          </div>
          {onPick && !l.active && (
            <button
              className="shrink-0 rounded-md border px-2 py-1 text-xs hover:bg-accent disabled:opacity-50"
              onClick={() => onPick(l)}
              disabled={disabled}
              data-testid="layout-plan"
            >
              Plan switch
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

/** A running job as one bar: step n of 12, its title, elapsed vs estimate. */
export function SwitchProgress({ job, now = Date.now() }: { job: JobSummary; now?: number }) {
  const p = job.steps ? Math.round(((job.step - 1) / job.steps) * 100) : 0;
  const elapsed = Math.max(0, (now - job.started) / 1000);
  return (
    <div className="rounded-lg border border-primary/40 bg-primary/5 p-3" data-testid="switch-progress" role="status">
      <div className="flex items-center gap-2 text-sm">
        <Loader2 className="size-4 animate-spin text-primary" />
        <span className="font-medium">{job.kind === "switch" ? `Switching to ${job.target}` : `${job.kind} ${job.target}`}</span>
        <span className="ml-auto text-xs tabular-nums text-muted-foreground">
          {fmtSeconds(elapsed)}{job.estimateSeconds ? ` of ~${fmtSeconds(job.estimateSeconds)}` : ""}
        </span>
      </div>
      <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted">
        <div className="h-full rounded-full bg-primary transition-[width] duration-500" style={{ width: `${p}%` }} />
      </div>
      <div className="mt-1 text-[11px] text-muted-foreground">Step {job.step} of {job.steps}: {job.title}</div>
    </div>
  );
}

const stepIcon = (s: string) =>
  s === "ok" ? <Check className="size-3.5 text-success" /> :
  s === "failed" ? <X className="size-3.5 text-destructive" /> :
  s === "running" ? <Loader2 className="size-3.5 animate-spin text-primary" /> :
  s === "skipped" ? <Minus className="size-3.5 text-muted-foreground" /> :
  <CircleDashed className="size-3.5 text-muted-foreground" />;

/** A job's steps with their times and output. */
export function JobSteps({ job }: { job: Job }) {
  const p = jobProgress(job);
  return (
    <div className="space-y-1" data-testid="job-steps">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <JobStateBadge state={job.state} />
        <span className="text-muted-foreground">{job.from ? `${job.from} → ` : ""}{job.target}</span>
        <span className="text-muted-foreground">· by {job.requestedBy}</span>
        <span className="ml-auto tabular-nums text-muted-foreground">{p.done}/{p.total}</span>
      </div>
      {job.error && <div className="text-xs text-destructive">{job.error}</div>}
      <ol className="space-y-0.5">
        {job.steps.map((s) => (
          <li key={s.n} className="text-xs">
            <div className="flex items-center gap-2">
              {stepIcon(s.state)}
              <span className={cn(s.state === "pending" && "text-muted-foreground")}>{s.n}. {s.title}</span>
              {s.seconds > 0 && <span className="ml-auto tabular-nums text-muted-foreground">{s.seconds.toFixed(1)} s</span>}
            </div>
            {s.output && <pre className="ml-6 mt-0.5 max-h-24 overflow-auto rounded bg-muted px-2 py-1 font-mono text-[10px] text-muted-foreground">{s.output}</pre>}
          </li>
        ))}
      </ol>
    </div>
  );
}

export function JobStateBadge({ state }: { state: string }) {
  const v = state === "done" ? "success" : state === "running" || state === "queued" ? "default" : state === "refused" ? "warning" : state === "rolled-back" || state === "failed" || state === "interrupted" ? "destructive" : "secondary";
  return <Badge variant={v}>{state}</Badge>;
}

const actionStyle: Record<string, string> = { keep: "text-muted-foreground", restart: "text-warning", start: "text-success", stop: "text-destructive" };

/** The dry run of a switch (5.3, 11.4): cards before/after, blockers, checks, steps, estimate. */
export function PlanView({ plan }: { plan: Plan }) {
  return (
    <div className="space-y-4 text-sm" data-testid="plan-view">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{plan.from ?? "custom"}</span>
        <ArrowRight className="size-4 text-muted-foreground" />
        <span className="font-medium">{plan.to}</span>
        <span className="ml-auto text-xs text-muted-foreground">on {plan.server}</span>
        {plan.note ? <Badge variant="secondary">{plan.note}</Badge> : plan.runnable ? <Badge variant="success">ready to run · ~{fmtSeconds(plan.estimateSeconds)}</Badge> : <Badge variant="warning">blocked</Badge>}
      </div>
      <table className="w-full text-xs" data-testid="plan-cards">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b"><th className="py-1 pr-2 font-medium">Card</th><th className="pr-2 font-medium">Now</th><th className="pr-2 font-medium">After</th><th className="font-medium">Action</th></tr>
        </thead>
        <tbody>
          {plan.cards.map((c) => (
            <tr key={c.card} className="border-b last:border-0 align-top">
              <td className="py-1.5 pr-2 font-semibold">{c.card}</td>
              <td className="pr-2 text-muted-foreground">{c.before}</td>
              <td className="pr-2">{c.after}</td>
              <td><span className={cn("font-medium", actionStyle[c.action])}>{c.action}</span><div className="text-[11px] text-muted-foreground">{c.reason}</div></td>
            </tr>
          ))}
        </tbody>
      </table>
      {plan.blockers.length > 0 && (
        <Section title="Blocked by">
          {plan.blockers.map((b) => (
            <div key={`${b.kind}/${b.id}`} className="flex items-start gap-2 text-xs text-warning" data-testid="plan-blocker">
              <Lock className="mt-0.5 size-3.5 shrink-0" />
              <span><span className="font-medium">{b.kind} {b.id}</span>: {b.reason}{b.holder ? ` (held by ${b.holder})` : ""}{b.retrySeconds ? ` · clears in ~${fmtSeconds(b.retrySeconds)}` : ""}</span>
            </div>
          ))}
        </Section>
      )}
      <Section title="Preconditions">
        <div className="grid gap-x-4 gap-y-0.5 sm:grid-cols-2">
          {plan.preconditions.map((c) => (
            <div key={c.name} className="flex items-center gap-1.5 text-xs">
              {c.status === "pass" ? <Check className="size-3.5 text-success" /> : c.status === "fail" ? <X className="size-3.5 text-destructive" /> : <CircleDashed className="size-3.5 text-muted-foreground" />}
              <span>{c.name}</span>
              {c.reason && <span className="text-muted-foreground">· {c.reason}</span>}
            </div>
          ))}
        </div>
      </Section>
      {(plan.restores.length > 0 || plan.drains.length > 0) && (
        <Section title="Before it starts">
          {plan.restores.map((r) => <div key={r.file} className="text-xs">Restore <span className="font-mono">{r.file}</span> ({fmtBytes(r.bytes)}, ~{fmtSeconds(r.seconds)})</div>)}
          {plan.drains.map((d) => <div key={d.card} className="text-xs">Card {d.card}: {d.inflight} request{d.inflight === 1 ? "" : "s"} in flight finish first</div>)}
        </Section>
      )}
      {plan.steps.length > 0 && (
        <Section title="Steps">
          <ol className="grid gap-x-4 gap-y-0.5 sm:grid-cols-2">
            {plan.steps.map((s) => (
              <li key={s.n} className="text-xs" title={s.detail}>
                <span className="text-muted-foreground">{s.n}.</span> {s.title}
                {s.detail && <span className="text-muted-foreground"> · {s.detail}</span>}
              </li>
            ))}
          </ol>
        </Section>
      )}
      {plan.routes.length > 0 && (
        <Section title="Routes that change">
          {plan.routes.map((r) => <div key={r.route} className="text-xs"><span className="font-mono">{r.route}</span>: {r.before.join(", ") || "cold"} → {r.after.join(", ") || "cold"}</div>)}
        </Section>
      )}
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <h4 className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h4>
      <div className="space-y-1">{children}</div>
    </div>
  );
}

/** The models a principal may use, with their state (the app's Models tab, supervisor mode). */
export function CatalogList({ models }: { models: CatalogModel[] }) {
  return (
    <div className="divide-y rounded-md border" data-testid="catalog-list">
      {models.map((m) => (
        <div key={m.name} className="flex items-center gap-2 px-3 py-1.5 text-xs">
          <span className={cn("size-2 rounded-full", m.status === "live" ? "bg-success" : m.status === "switching" ? "bg-warning" : "bg-muted-foreground/40")} />
          <span className="font-mono">{m.name}</span>
          <span className="ml-auto text-muted-foreground">{m.status === "live" ? (m.serving?.join(", ") || "live") : coldLabel(m.cold, m.fallback)}</span>
        </div>
      ))}
    </div>
  );
}
