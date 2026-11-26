// The Supervisor's calendar (SUPERVISOR.md 8.3, 9): an admin settings tab (D143 registry). A week
// grid and a list of entries, live state ("now"), create/edit with a preview of the next 10
// occurrences and the overlaps a save would make, per-occurrence skip/end, and each event's key
// panel (issue shown once, rotate, revoke, uses).
import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, CalendarClock, ChevronLeft, ChevronRight, Copy, KeyRound, Loader2, Pencil, Plus, RotateCcw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { calendarApi, type IssuedKey } from "./calendarApi";
import {
  EFFECT_KINDS, KEY_SCOPES, colourOf, dayLabel, effectRow, effectValue, hhmm, keyState, phaseOf, placeWeek, tidy, weekDays, whenKind,
  type CalendarDoc, type EffectRow, type EntryConfig, type EntryView, type KeyScope, type Occurrence, type Preview, type Priority, type WhenKind,
} from "./calendarModel";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";
const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const nowS = () => Math.floor(Date.now() / 1000);

// Eight entry colours that read in light and dark themes.
const COLOURS = [
  "bg-sky-500/25 border-sky-500/70",
  "bg-violet-500/25 border-violet-500/70",
  "bg-emerald-500/25 border-emerald-500/70",
  "bg-amber-500/25 border-amber-500/70",
  "bg-rose-500/25 border-rose-500/70",
  "bg-teal-500/25 border-teal-500/70",
  "bg-fuchsia-500/25 border-fuchsia-500/70",
  "bg-lime-500/25 border-lime-500/70",
];

const PRIORITY_BADGE: Record<Priority, "outline" | "secondary" | "warning" | "destructive"> = {
  low: "outline", normal: "secondary", high: "warning", critical: "destructive",
};

function PaneHead({ children }: { children: React.ReactNode }) {
  return (
    <div className="space-y-1 border-b px-4 py-3">
      <div className="flex items-center gap-2 text-sm font-semibold"><CalendarClock className="size-4 text-muted-foreground" /> Calendar</div>
      <p className="text-sm text-muted-foreground">{children}</p>
    </div>
  );
}

export function CalendarPane() {
  const qc = useQueryClient();
  const doc = useQuery({ queryKey: ["calendar"], queryFn: calendarApi.doc, refetchInterval: 30_000 });
  const [view, setView] = useState<"week" | "list">("week");
  const [anchor, setAnchor] = useState(nowS());
  const [edit, setEdit] = useState<{ id: string; entry: EntryConfig; isNew: boolean } | null>(null);
  const [picked, setPicked] = useState<Occurrence | null>(null);
  const tz = doc.data?.tz ?? "Europe/London";
  const days = useMemo(() => weekDays(anchor, tz), [anchor, tz]);
  const occ = useQuery({
    queryKey: ["calendar-occ", days[0], days[7]],
    queryFn: () => calendarApi.occurrences(days[0], days[7]),
    enabled: !!doc.data,
    refetchInterval: 30_000,
  });
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["calendar"] });
    void qc.invalidateQueries({ queryKey: ["calendar-occ"] });
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="calendar-pane">
      <PaneHead>
        Events that reserve or block cards while they run: the video pipelines, benches, maintenance. Cron still starts the jobs; the calendar describes and protects them, and live detection extends a run that goes long.
      </PaneHead>
      <div className="min-h-0 flex-1 space-y-3 overflow-y-auto p-4">
        {doc.isLoading && <div className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Loading the calendar…</div>}
        {doc.error && <div className="rounded-md border border-destructive/50 p-2 text-xs text-destructive" role="alert">{errText(doc.error)}</div>}
        {doc.data && (
          <>
            <NowStrip doc={doc.data} />
            {doc.data.overlaps.length > 0 && (
              <div className="space-y-1 rounded-md border border-warning/50 p-2 text-xs" data-testid="overlaps">
                {doc.data.overlaps.slice(0, 4).map((o) => (
                  <div key={`${o.a}-${o.b}-${o.card}`} className="flex items-start gap-1.5 text-warning"><AlertTriangle className="mt-0.5 size-3.5 shrink-0" /> {o.message}</div>
                ))}
              </div>
            )}
            {doc.data.issues.filter((i) => i.severity === "error").map((i) => (
              <div key={i.path + i.message} className="text-xs text-destructive" role="alert">{i.path}: {i.message}</div>
            ))}
            <div className="flex flex-wrap items-center gap-2">
              <div className="inline-flex rounded-md border p-0.5" role="tablist" aria-label="Calendar view">
                {(["week", "list"] as const).map((v) => (
                  <button key={v} role="tab" aria-selected={view === v} className={cn("rounded px-2.5 py-1 text-xs", view === v ? "bg-primary/15 text-primary" : "text-muted-foreground hover:text-foreground")} onClick={() => setView(v)}>
                    {v === "week" ? "Week" : "List"}
                  </button>
                ))}
              </div>
              {view === "week" && (
                <div className="flex items-center gap-1">
                  <Button size="icon-sm" variant="ghost" aria-label="Previous week" onClick={() => setAnchor(days[0] - 3 * 86400)}><ChevronLeft /></Button>
                  <Button size="sm" variant="outline" onClick={() => setAnchor(nowS())}>This week</Button>
                  <Button size="icon-sm" variant="ghost" aria-label="Next week" onClick={() => setAnchor(days[7] + 3 * 86400)}><ChevronRight /></Button>
                  <span className="ml-1 text-xs text-muted-foreground">{dayLabel(days[0], tz)} – {dayLabel(days[6], tz)} · {tz}</span>
                </div>
              )}
              <Button size="sm" className="ml-auto" onClick={() => setEdit({ id: "", isNew: true, entry: { title: "", at: "", duration: "1h", effects: [{ block_switch: "all" }], priority: "normal" } })}>
                <Plus className="size-3.5" /> New entry
              </Button>
            </div>
            {view === "week" ? (
              <WeekGrid days={days} tz={tz} occ={occ.data?.occurrences ?? []} now={occ.data?.at ?? doc.data.at} onPick={setPicked} />
            ) : (
              <EntryList doc={doc.data} onEdit={(e) => setEdit({ id: e.id, isNew: false, entry: stripView(e) })} />
            )}
            {doc.data.entries.some((e) => e.frequent) && view === "week" && (
              <p className="text-xs text-muted-foreground">
                Not drawn (they fire many times a day): {doc.data.entries.filter((e) => e.frequent).map((e) => `${e.title} (${e.schedule})`).join(" · ")}
              </p>
            )}
          </>
        )}
      </div>
      {edit && doc.data && <EntryEditor doc={doc.data} init={edit} onClose={() => setEdit(null)} onSaved={refresh} />}
      {picked && doc.data && (
        <OccurrenceDialog occ={picked} doc={doc.data} onClose={() => setPicked(null)} onChanged={refresh}
          onEdit={() => {
            const e = doc.data!.entries.find((x) => x.id === picked.entry);
            setPicked(null);
            if (e) setEdit({ id: e.id, isNew: false, entry: stripView(e) });
          }} />
      )}
    </div>
  );
}

function stripView(e: EntryView): EntryConfig {
  const { id: _id, schedule: _s, effects_text: _t, origin: _o, frequent: _f, broken: _b, keys: _k, ...rest } = e;
  return rest;
}

function NowStrip({ doc }: { doc: CalendarDoc }) {
  const active = doc.now.active;
  const holders = Object.entries(doc.now.holders);
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-md border px-3 py-2 text-xs" data-testid="now-strip">
      <span className="font-medium">Now</span>
      {active.length === 0 && <span className="text-muted-foreground">nothing reserved or blocked</span>}
      {active.map((o) => (
        <span key={o.id} className="inline-flex items-center gap-1.5">
          <Badge variant={o.extended ? "warning" : "default"}>{o.title}</Badge>
          <span className="text-muted-foreground">
            {phaseOf(o, doc.at)}{o.unplanned ? " (detected)" : ""} until ~{hhmm(o.block_end, doc.tz)}
            {o.run?.ongoing ? ` · running (${o.run.detail ?? "seen"})` : ""}
          </span>
        </span>
      ))}
      {holders.map(([card, h]) => (
        <span key={card} className="text-muted-foreground">card <span className="font-mono text-foreground">{card}</span> reserved by {h.entry}{h.contenders.length ? ` (also wanted by ${h.contenders.join(", ")})` : ""}</span>
      ))}
    </div>
  );
}

function WeekGrid({ days, tz, occ, now, onPick }: { days: number[]; tz: string; occ: Occurrence[]; now: number; onPick: (o: Occurrence) => void }) {
  const placed = useMemo(() => placeWeek(occ, days), [occ, days]);
  const hours = [0, 3, 6, 9, 12, 15, 18, 21];
  const nowDay = days.findIndex((d, i) => i < 7 && now >= d && now < days[i + 1]);
  return (
    <div className="overflow-x-auto rounded-md border" data-testid="week-grid">
      <div className="grid min-w-[600px] grid-cols-[2.75rem_repeat(7,minmax(0,1fr))]">
        <div className="border-b" />
        {days.slice(0, 7).map((d, i) => (
          <div key={d} className={cn("border-b border-l px-1.5 py-1 text-xs", i === nowDay && "font-semibold text-primary")}>{dayLabel(d, tz)}</div>
        ))}
        <div className="relative h-[480px]">
          {hours.map((h) => (
            <div key={h} className="absolute right-1 text-[10px] text-muted-foreground" style={{ top: `${(h / 24) * 100}%` }}>{String(h).padStart(2, "0")}:00</div>
          ))}
        </div>
        {days.slice(0, 7).map((d, i) => (
          <div key={d} className="relative h-[480px] border-l">
            {hours.map((h) => <div key={h} className="absolute inset-x-0 border-t border-dashed border-border/50" style={{ top: `${(h / 24) * 100}%` }} />)}
            {i === nowDay && <div className="absolute inset-x-0 z-20 border-t-2 border-destructive" style={{ top: `${((now - d) / (days[i + 1] - d)) * 100}%` }} />}
            {placed.filter((p) => p.day === i).map((p) => {
              const c = COLOURS[colourOf(p.occ.entry)];
              const w = 100 / p.lanes;
              const coreT = (p.coreTop - p.top) / Math.max(p.bottom - p.top, 1e-6);
              const coreB = (p.coreBottom - p.top) / Math.max(p.bottom - p.top, 1e-6);
              return (
                <button
                  key={p.occ.id + p.day}
                  data-testid="occ-block"
                  data-entry={p.occ.entry}
                  title={`${p.occ.title}: ${hhmm(p.occ.block_start, tz)}–${hhmm(p.occ.block_end, tz)}`}
                  onClick={() => onPick(p.occ)}
                  className={cn("absolute z-10 overflow-hidden rounded-sm border text-left text-[10px] leading-tight", c, p.occ.skipped && "opacity-40 line-through", p.occ.extended && "ring-1 ring-warning")}
                  style={{ top: `${p.top * 100}%`, height: `max(${(p.bottom - p.top) * 100}%, 14px)`, left: `calc(${p.lane * w}% + 1px)`, width: `calc(${w}% - 2px)` }}
                >
                  {/* lead and tail lighter than the planned core */}
                  <span className="pointer-events-none absolute inset-x-0 bg-background/40" style={{ top: 0, height: `${Math.max(0, coreT) * 100}%` }} />
                  <span className="pointer-events-none absolute inset-x-0 bottom-0 bg-background/40" style={{ top: `${Math.min(1, coreB) * 100}%` }} />
                  <span className="relative block truncate px-1 pt-0.5 font-medium">{p.occ.title}</span>
                  <span className="relative block truncate px-1 text-muted-foreground">{hhmm(p.occ.start, tz)}{p.occ.unplanned ? " · detected" : ""}{p.occ.extended ? " · extended" : ""}</span>
                </button>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

function EntryList({ doc, onEdit }: { doc: CalendarDoc; onEdit: (e: EntryView) => void }) {
  return (
    <div className="overflow-x-auto rounded-md border" data-testid="entry-list">
      <table className="w-full text-xs">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="px-2 py-1.5 font-medium">Entry</th>
            <th className="px-2 py-1.5 font-medium">When</th>
            <th className="px-2 py-1.5 font-medium">Effects</th>
            <th className="px-2 py-1.5 font-medium">Priority</th>
            <th className="px-2 py-1.5 font-medium">Key</th>
            <th className="px-2 py-1.5" />
          </tr>
        </thead>
        <tbody>
          {doc.entries.map((e) => {
            const live = e.keys.filter((k) => keyState(k, doc.at) === "live" || keyState(k, doc.at) === "retiring");
            return (
              <tr key={e.id} className={cn("border-b last:border-0", !e.enabled && "opacity-50")} data-testid="entry-row" data-entry={e.id}>
                <td className="px-2 py-1.5">
                  <div className="font-medium">{e.title}</div>
                  <div className="font-mono text-[11px] text-muted-foreground">{e.id}{e.origin !== "file" ? ` · ${e.origin}` : ""}{e.source ? ` · ${e.source}` : ""}{!e.enabled ? " · disabled" : ""}</div>
                </td>
                <td className="px-2 py-1.5">{e.schedule}{e.duration ? `, ${e.duration}` : ""}{e.lead ? ` (lead ${e.lead})` : ""}{e.detect ? " · detect" : ""}</td>
                <td className="px-2 py-1.5 text-muted-foreground">{e.effects_text.join("; ")}</td>
                <td className="px-2 py-1.5"><Badge variant={PRIORITY_BADGE[e.priority ?? "normal"]}>{e.priority ?? "normal"}</Badge></td>
                <td className="px-2 py-1.5">{e.key ? (live.length ? <Badge variant="success">{live.length} live</Badge> : <Badge variant="outline" className="whitespace-nowrap">no key yet</Badge>) : <span className="text-muted-foreground">—</span>}</td>
                <td className="px-2 py-1.5 text-right">
                  {e.origin === "file" && <Button size="icon-sm" variant="ghost" aria-label={`Edit ${e.id}`} onClick={() => onEdit(e)}><Pencil /></Button>}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function OccurrenceDialog({ occ, doc, onClose, onChanged, onEdit }: { occ: Occurrence; doc: CalendarDoc; onClose: () => void; onChanged: () => void; onEdit: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const act = useMutation({
    mutationFn: (a: "skip" | "cancel" | "restore") => calendarApi.occurrence(occ.id, a, "from the calendar page"),
    onSuccess: () => { onChanged(); onClose(); },
    onError: (e) => setError(errText(e)),
  });
  const phase = phaseOf(occ, doc.at);
  const running = phase !== "scheduled" && phase !== "done" && phase !== "skipped";
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent data-testid="occ-dialog">
        <DialogTitle>{occ.title}</DialogTitle>
        <DialogDescription>
          {dayLabel(occ.start, doc.tz)} · planned {hhmm(occ.start, doc.tz)}–{hhmm(occ.end, doc.tz)}, blocks {hhmm(occ.block_start, doc.tz)}–{hhmm(occ.block_end, doc.tz)} · {phase}
        </DialogDescription>
        {occ.run && (
          <p className="text-xs text-muted-foreground">
            Detected run from {hhmm(occ.run.start, doc.tz)}{occ.run.ongoing ? " and still running" : ` to ${hhmm(occ.run.last_seen, doc.tz)}`}{occ.extended ? ` · extended until ${hhmm(occ.block_end, doc.tz)}` : ""}
          </p>
        )}
        {error && <p className="text-xs text-destructive" role="alert">{error}</p>}
        <div className="flex flex-wrap justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={onEdit}>Edit the entry</Button>
          {occ.skipped || occ.ended_at ? (
            <Button size="sm" variant="outline" onClick={() => act.mutate("restore")}><RotateCcw className="size-3.5" /> Restore</Button>
          ) : running ? (
            <Button size="sm" variant="destructive" onClick={() => act.mutate("cancel")}>End it now</Button>
          ) : phase === "scheduled" ? (
            <Button size="sm" variant="outline" onClick={() => act.mutate("skip")}>Skip this one</Button>
          ) : null}
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── the editor ────────────────────────────────────────────────────────────────────────────────

function EntryEditor({ doc, init, onClose, onSaved }: { doc: CalendarDoc; init: { id: string; entry: EntryConfig; isNew: boolean }; onClose: () => void; onSaved: () => void }) {
  const [id, setId] = useState(init.id);
  const [e, setE] = useState<EntryConfig>(init.entry);
  const [when, setWhen] = useState<WhenKind>(whenKind(init.entry));
  const [rows, setRows] = useState<EffectRow[]>((init.entry.effects ?? ["info"]).map(effectRow));
  const [error, setError] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const set = (p: Partial<EntryConfig>) => setE((x) => ({ ...x, ...p }));
  const entry = useMemo<EntryConfig>(() => {
    const out: EntryConfig = { ...e, effects: rows.map(effectValue) };
    if (when !== "at") delete out.at;
    if (when !== "rrule") { delete out.rrule; delete out.start; }
    if (when !== "cron") delete out.cron;
    return tidy(out);
  }, [e, rows, when]);
  const [preview, setPreview] = useState<Preview | null>(null);
  useEffect(() => {
    if (!id) return;
    const t = setTimeout(() => {
      calendarApi.preview(id, entry).then(setPreview, (x) => setError(errText(x)));
    }, 350);
    return () => clearTimeout(t);
  }, [id, entry]);
  const save = useMutation({
    mutationFn: () => calendarApi.save(id, entry, doc.rev, init.isNew ? "created in the UI" : "edited in the UI"),
    onSuccess: (r) => {
      onSaved();
      const w = [...r.warnings.map((i) => i.message), ...r.overlaps.map((o) => o.message)];
      if (w.length) setWarnings(w);
      else onClose();
    },
    onError: (x) => setError(errText(x)),
  });
  const del = useMutation({
    mutationFn: () => calendarApi.remove(id, doc.rev),
    onSuccess: () => { onSaved(); onClose(); },
    onError: (x) => setError(errText(x)),
  });
  const errors = preview?.issues.filter((i) => i.severity === "error") ?? [];
  const policy = e.key;

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-4xl" data-testid="entry-editor">
        <DialogTitle>{init.isNew ? "New calendar entry" : `Edit ${init.id}`}</DialogTitle>
        <DialogDescription>Saved to calendar.toml with a backup and an audit row.</DialogDescription>
        <div className="grid min-h-0 gap-4 overflow-y-auto md:grid-cols-[minmax(0,1fr)_18rem]">
          <div className="space-y-3">
            <div className="grid grid-cols-2 gap-2">
              <label className="space-y-1 text-xs">Id
                <input className={cn(field, "font-mono")} value={id} disabled={!init.isNew} onChange={(x) => setId(x.target.value.toLowerCase())} placeholder="maintenance" />
              </label>
              <label className="space-y-1 text-xs">Title
                <input className={field} value={e.title} onChange={(x) => set({ title: x.target.value })} />
              </label>
            </div>
            <div className="grid grid-cols-[8rem_minmax(0,1fr)] gap-2">
              <label className="space-y-1 text-xs">When
                <select className={field} value={when} onChange={(x) => setWhen(x.target.value as WhenKind)}>
                  <option value="at">once</option>
                  <option value="rrule">repeats (RRULE)</option>
                  <option value="cron">cron</option>
                </select>
              </label>
              {when === "at" && <label className="space-y-1 text-xs">Local time<input className={field} value={e.at ?? ""} placeholder="2026-12-05T20:00" onChange={(x) => set({ at: x.target.value })} /></label>}
              {when === "rrule" && <label className="space-y-1 text-xs">RRULE<input className={cn(field, "font-mono")} value={e.rrule ?? ""} placeholder="FREQ=DAILY;BYHOUR=7;BYMINUTE=0" onChange={(x) => set({ rrule: x.target.value })} /></label>}
              {when === "cron" && <label className="space-y-1 text-xs">Cron<input className={cn(field, "font-mono")} value={e.cron ?? ""} placeholder="0 7 * * *" onChange={(x) => set({ cron: x.target.value })} /></label>}
            </div>
            <div className="grid grid-cols-5 gap-2">
              <label className="space-y-1 text-xs">Duration<input className={field} value={e.duration ?? ""} onChange={(x) => set({ duration: x.target.value })} /></label>
              <label className="space-y-1 text-xs">Lead<input className={field} value={e.lead ?? ""} placeholder="15m" onChange={(x) => set({ lead: x.target.value })} /></label>
              <label className="space-y-1 text-xs">Tail<input className={field} value={e.tail ?? ""} onChange={(x) => set({ tail: x.target.value })} /></label>
              <label className="space-y-1 text-xs">After
                <select className={field} value={e.after ?? ""} onChange={(x) => set({ after: x.target.value || undefined })}>
                  <option value="">—</option>
                  {doc.entries.filter((x) => x.id !== id).map((x) => <option key={x.id} value={x.id}>{x.id}</option>)}
                </select>
              </label>
              <label className="space-y-1 text-xs">Priority
                <select className={field} value={e.priority ?? "normal"} onChange={(x) => set({ priority: x.target.value as Priority })}>
                  {(["low", "normal", "high", "critical"] as const).map((p) => <option key={p}>{p}</option>)}
                </select>
              </label>
            </div>
            <div className="grid grid-cols-[minmax(0,1fr)_7rem_7rem] gap-2">
              <label className="space-y-1 text-xs">Detect a process (argv word · label)
                <div className="flex gap-1">
                  <input className={cn(field, "font-mono")} value={e.detect?.process?.argv_any ?? ""} placeholder="run_daily.sh" onChange={(x) => set({ detect: x.target.value ? { ...e.detect, process: { ...e.detect?.process, argv_any: x.target.value, label_arg: e.detect?.process?.label_arg ?? "--channel" } } : undefined })} />
                  <input className={cn(field, "w-28 font-mono")} value={e.detect?.process?.label ?? ""} placeholder="ai-news" disabled={!e.detect?.process} onChange={(x) => set({ detect: { ...e.detect, process: { ...e.detect!.process!, label: x.target.value } } })} />
                </div>
              </label>
              <label className="space-y-1 text-xs">Mode
                <select className={field} value={e.detect?.mode ?? "both"} disabled={!e.detect} onChange={(x) => set({ detect: { ...e.detect, mode: x.target.value as "both" } })}>
                  <option>both</option><option>extend</option><option>start</option>
                </select>
              </label>
              <label className="space-y-1 text-xs">Grace<input className={field} value={e.detect?.grace ?? ""} placeholder="10m" disabled={!e.detect} onChange={(x) => set({ detect: { ...e.detect, grace: x.target.value } })} /></label>
            </div>
            <fieldset className="space-y-1.5">
              <legend className="text-xs font-medium">Effects</legend>
              {rows.map((r, i) => (
                <div key={i} className="flex gap-1.5" data-testid="effect-row">
                  <select className={cn(field, "w-44")} value={r.kind} onChange={(x) => setRows(rows.map((y, j) => (j === i ? { ...y, kind: x.target.value as EffectRow["kind"] } : y)))}>
                    {EFFECT_KINDS.map((k) => <option key={k.id} value={k.id}>{k.label}</option>)}
                  </select>
                  {r.kind !== "info" && <input className={cn(field, "font-mono")} value={r.value} placeholder={EFFECT_KINDS.find((k) => k.id === r.kind)?.placeholder} onChange={(x) => setRows(rows.map((y, j) => (j === i ? { ...y, value: x.target.value } : y)))} />}
                  {r.kind === "reserve_card" && (
                    <select className={cn(field, "w-28")} value={r.onReserve ?? "wait"} onChange={(x) => setRows(rows.map((y, j) => (j === i ? { ...y, onReserve: x.target.value as "wait" } : y)))}>
                      <option value="wait">wait</option><option value="drain">drain</option><option value="preempt">preempt</option>
                    </select>
                  )}
                  {r.kind === "reserve_slots" && <input type="number" min={1} className={cn(field, "w-16")} value={r.n ?? 1} onChange={(x) => setRows(rows.map((y, j) => (j === i ? { ...y, n: +x.target.value } : y)))} />}
                  <Button size="icon" variant="ghost" aria-label="Remove the effect" onClick={() => setRows(rows.filter((_, j) => j !== i))}><Trash2 className="size-3.5" /></Button>
                </div>
              ))}
              <Button size="sm" variant="outline" onClick={() => setRows([...rows, { kind: "block_switch", value: "all" }])}><Plus className="size-3.5" /> Effect</Button>
            </fieldset>
            <fieldset className="space-y-1.5 rounded-md border p-2" data-testid="key-policy">
              <legend className="px-1 text-xs font-medium">Event key policy</legend>
              <label className="flex items-center gap-2 text-xs">
                <input type="checkbox" checked={!!policy} onChange={(x) => set({ key: x.target.checked ? { scopes: ["chat", "use_reserved"], grace_before: "5m", grace_after: "15m" } : undefined })} />
                This event can have its own key (least privilege: chat and reserved cards only)
              </label>
              {policy && (
                <>
                  <div className="grid grid-cols-2 gap-x-3 gap-y-1">
                    {KEY_SCOPES.map((s) => (
                      <label key={s.id} className="flex items-start gap-1.5 text-xs" title={s.hint}>
                        <input type="checkbox" className="mt-0.5" checked={policy.scopes.includes(s.id)}
                          onChange={(x) => set({ key: { ...policy, scopes: x.target.checked ? [...policy.scopes, s.id] : policy.scopes.filter((y: KeyScope) => y !== s.id) } })} />
                        <span>{s.label} <span className="text-muted-foreground">— {s.hint}</span></span>
                      </label>
                    ))}
                  </div>
                  <div className="grid grid-cols-4 gap-2">
                    <label className="col-span-2 space-y-1 text-xs">Routes<input className={cn(field, "font-mono")} value={(policy.routes ?? []).join(", ")} placeholder="default: routes on the reserved cards" onChange={(x) => set({ key: { ...policy, routes: x.target.value.split(",").map((s) => s.trim()).filter(Boolean) } })} /></label>
                    <label className="space-y-1 text-xs">Grace before<input className={field} value={policy.grace_before} onChange={(x) => set({ key: { ...policy, grace_before: x.target.value } })} /></label>
                    <label className="space-y-1 text-xs">Grace after<input className={field} value={policy.grace_after} onChange={(x) => set({ key: { ...policy, grace_after: x.target.value } })} /></label>
                  </div>
                  {policy.scopes.includes("switch") && (
                    <label className="space-y-1 text-xs">Layouts the key may switch to (empty: none)
                      <input className={cn(field, "font-mono")} value={(policy.layouts ?? []).join(", ")} onChange={(x) => set({ key: { ...policy, layouts: x.target.value.split(",").map((s) => s.trim()).filter(Boolean) } })} />
                    </label>
                  )}
                  <label className="flex items-center gap-2 text-xs text-warning">
                    <input type="checkbox" checked={!!policy.overrides_lower} onChange={(x) => set({ key: { ...policy, overrides_lower: x.target.checked || undefined } })} />
                    Also lift blocks and reservations of lower-priority events
                  </label>
                </>
              )}
            </fieldset>
          </div>
          <aside className="space-y-3 text-xs" data-testid="preview">
            <div className="font-medium">Next occurrences {preview?.schedule ? <span className="font-normal text-muted-foreground">({preview.schedule})</span> : null}</div>
            {!id && <p className="text-muted-foreground">Give the entry an id to see its occurrences.</p>}
            {errors.map((i) => <p key={i.path + i.message} className="text-destructive" role="alert">{i.message}</p>)}
            {preview?.issues.filter((i) => i.severity === "warning").map((i) => <p key={i.path + i.message} className="text-warning">{i.message}</p>)}
            <ol className="space-y-0.5">
              {preview?.next.map((o) => (
                <li key={o.id} className="flex justify-between gap-2"><span>{dayLabel(o.start, doc.tz)}</span><span className="font-mono">{hhmm(o.block_start, doc.tz)}–{hhmm(o.block_end, doc.tz)}</span></li>
              ))}
            </ol>
            {preview && preview.effects.length > 0 && <div className="text-muted-foreground">{preview.effects.join("; ")}</div>}
            {preview?.overlaps.map((o) => <p key={o.message} className="flex gap-1 text-warning"><AlertTriangle className="mt-0.5 size-3 shrink-0" />{o.message}</p>)}
            {!init.isNew && policy && <KeyPanel event={init.id} doc={doc} onChanged={onSaved} />}
          </aside>
        </div>
        {error && <p className="text-xs text-destructive" role="alert">{error}</p>}
        {warnings.length > 0 && (
          <div className="space-y-1 rounded-md border border-warning/50 p-2 text-xs text-warning" role="status">
            Saved, with warnings: {warnings.map((w) => <div key={w}>{w}</div>)}
          </div>
        )}
        <div className="flex justify-end gap-2">
          {!init.isNew && <Button variant="ghost" size="sm" className="mr-auto text-destructive" onClick={() => setConfirmDelete(true)}>Delete</Button>}
          <Button variant="ghost" size="sm" onClick={onClose}>{warnings.length ? "Close" : "Cancel"}</Button>
          <Button size="sm" disabled={!id || !e.title || errors.length > 0 || save.isPending} onClick={() => { setError(null); save.mutate(); }}>
            {save.isPending && <Loader2 className="size-3.5 animate-spin" />} Save
          </Button>
        </div>
        <ConfirmDialog open={confirmDelete} title={`Delete ${init.id}?`} body="The entry leaves calendar.toml (a backup is kept). Its keys stop working." confirm="Delete" destructive
          onConfirm={() => { setConfirmDelete(false); del.mutate(); }} onCancel={() => setConfirmDelete(false)} />
      </DialogContent>
    </Dialog>
  );
}

function KeyPanel({ event, doc, onChanged }: { event: string; doc: CalendarDoc; onChanged: () => void }) {
  const entry = doc.entries.find((e) => e.id === event);
  const [made, setMade] = useState<IssuedKey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revoke, setRevoke] = useState<number | null>(null);
  const uses = useQuery({ queryKey: ["calendar-uses", event], queryFn: () => calendarApi.uses(event) });
  const issue = useMutation({ mutationFn: () => calendarApi.issueKey(event), onSuccess: (k) => { setMade(k); onChanged(); }, onError: (e) => setError(errText(e)) });
  const rotate = useMutation({ mutationFn: () => calendarApi.rotateKey(event), onSuccess: (k) => { setMade(k); onChanged(); }, onError: (e) => setError(errText(e)) });
  const rev = useMutation({ mutationFn: (id: number) => calendarApi.revokeKey(id), onSuccess: () => onChanged(), onError: (e) => setError(errText(e)) });
  const keys = entry?.keys ?? [];
  const anyLive = keys.some((k) => keyState(k, doc.at) === "live");
  return (
    <div className="space-y-2 rounded-md border p-2" data-testid="key-panel">
      <div className="flex items-center gap-1.5 font-medium"><KeyRound className="size-3.5" /> Event key</div>
      {made && (
        <div className="space-y-1 rounded-md border border-primary/50 p-2" data-testid="new-key">
          <div className="text-[11px] text-muted-foreground">Shown once. Write it to a key file (ABHOTH_API_KEY_FILE), never into argv or crontab.</div>
          <div className="flex items-center gap-1">
            <code className="min-w-0 flex-1 truncate rounded bg-muted px-1 py-0.5 font-mono text-[11px]">{made.key}</code>
            <Button size="icon-sm" variant="ghost" aria-label="Copy the key" onClick={() => void navigator.clipboard?.writeText(made.key)}><Copy /></Button>
          </div>
          {made.old_retire_at && <div className="text-[11px] text-muted-foreground">The old key works until {dayLabel(made.old_retire_at, doc.tz)} {hhmm(made.old_retire_at, doc.tz)}.</div>}
        </div>
      )}
      <ul className="space-y-1">
        {keys.map((k) => {
          const s = keyState(k, doc.at);
          return (
            <li key={k.id} className="flex items-center gap-1.5">
              <span className="font-mono">{k.prefix}…</span>
              <Badge variant={s === "live" ? "success" : s === "retiring" ? "warning" : "outline"}>{s}</Badge>
              <span className="truncate text-muted-foreground">{k.last_used ? `used ${hhmm(k.last_used, doc.tz)}` : "never used"}</span>
              {(s === "live" || s === "retiring") && <Button size="icon-sm" variant="ghost" className="ml-auto" aria-label={`Revoke ${k.prefix}`} onClick={() => setRevoke(k.id)}><Trash2 /></Button>}
            </li>
          );
        })}
        {keys.length === 0 && <li className="text-muted-foreground">No key issued.</li>}
      </ul>
      <div className="flex gap-1.5">
        <Button size="sm" variant="outline" onClick={() => issue.mutate()} disabled={anyLive}>Issue</Button>
        <Button size="sm" variant="outline" onClick={() => rotate.mutate()} disabled={!anyLive}>Rotate</Button>
      </div>
      {error && <p className="text-destructive" role="alert">{error}</p>}
      {uses.data && uses.data.length > 0 && (
        <div className="space-y-0.5">
          <div className="text-muted-foreground">Recent uses</div>
          {uses.data.slice(0, 6).map((u, i) => (
            <div key={i} className="flex justify-between gap-2"><span className="truncate">{u.action}</span><span className={u.decision === "allow" ? "text-success" : "text-destructive"}>{u.decision}</span></div>
          ))}
        </div>
      )}
      <ConfirmDialog open={revoke !== null} title="Revoke this key?" body="Requests with it are refused at once." confirm="Revoke" destructive
        onConfirm={() => { if (revoke !== null) rev.mutate(revoke); setRevoke(null); }} onCancel={() => setRevoke(null)} />
    </div>
  );
}
