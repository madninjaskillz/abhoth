import { useMemo, useState } from "react";
import { brandName } from "@/lib/brand";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowDown, ArrowUp, Eye, ListPlus, Loader2, Pause, Play, RotateCcw, Square, Trash2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { api, type BenchQueueItem, type BenchQueuePlan, type BenchSuite, type ProfileList } from "@/lib/api";
import { useProfiles } from "@/features/profiles/Profiles";
import { ALL, MAX_ADD, MODES, OFF, engineProfileOptions, expand, picksFor, type CardFilter, type Selection } from "./queueLogic";
export { ALL, expand, matches, type Selection } from "./queueLogic";
import { cn } from "@/lib/utils";

const minutes = (s: number) => (s >= 3600 ? `${(s / 3600).toFixed(1)} h` : `${Math.max(1, Math.round(s / 60))} min`);
const select = "h-7 rounded border bg-background px-1 text-xs";

/** Engine → engine profile → model for one card, each with "All"; options narrow to compatible ones. */
function CardFilterPickers({ card, f, onChange, d, allowOff }: { card: string; f: CardFilter; onChange: (f: CardFilter) => void; d: ProfileList | undefined; allowOff: boolean }) {
  // only engine profiles / models with at least one valid combo (allowed engine profiles, compat, VRAM) given the other picks
  const eps = engineProfileOptions(d, card, f.engine, f.model).map((name) => [name] as const);
  const modelsFor = (engineProfile: string) => [...new Set(picksFor(d, card, { ...f, engineProfile, model: ALL }).map((p) => p.model))];
  const models = modelsFor(f.engineProfile).map((name) => [name] as const);
  // a pick that rules out the other one resets it to All
  const pickEngineProfile = (engineProfile: string) => onChange({ ...f, engineProfile, model: f.model !== ALL && !modelsFor(engineProfile).includes(f.model) ? ALL : f.model });
  const pickModel = (model: string) =>
    onChange({ ...f, model, engineProfile: f.engineProfile !== ALL && !engineProfileOptions(d, card, f.engine, model).includes(f.engineProfile) ? ALL : f.engineProfile });
  return (
    <span className="inline-flex flex-wrap items-end gap-1" data-testid={`queue-card-${card}`}>
      <span className="self-center font-mono text-muted-foreground">{card}</span>
      <select className={select} value={f.engine} onChange={(e) => onChange({ engine: e.target.value, engineProfile: ALL, model: ALL })} data-testid={`queue-engine-${card}`}>
        <option value={ALL}>All engines</option>
        {allowOff && <option value={OFF}>off</option>}
        {Object.entries(d?.engines ?? {}).map(([name, e]) => (
          <option key={name} value={name}>
            {e.title ?? name}
          </option>
        ))}
      </select>
      {f.engine !== OFF && (
        <>
          <select className={select} value={f.engineProfile} onChange={(e) => pickEngineProfile(e.target.value)} data-testid={`queue-engine-profile-${card}`}>
            <option value={ALL}>All engine profiles</option>
            {eps.map(([name]) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <select className={select} value={f.model} onChange={(e) => pickModel(e.target.value)} data-testid={`queue-model-${card}`}>
            <option value={ALL}>All models</option>
            {models.map(([name]) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </>
      )}
    </span>
  );
}

/** Suite + (saved profile | engine / engine profile / model per card) + mode, each with "All" → "Add N runs to queue". */
export function QueuePicker({ suites, sel, onSel, matching }: { suites: BenchSuite[]; sel: Selection; onSel: (s: Selection) => void; matching: number }) {
  const qc = useQueryClient();
  const profiles = useProfiles();
  const d = profiles.data;
  const [reps, setReps] = useState(1);
  const setups = useMemo(() => expand(sel, suites, d), [sel, suites, d]);
  const tooMany = setups.length > MAX_ADD;
  const add = useMutation({
    mutationFn: () => api.benchQueueAdd(setups.map((s) => ({ ...s, reps }))),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["bench-queue"] }),
  });
  const prof = sel.source === "saved" && sel.profile && sel.profile !== ALL ? d?.profiles?.[sel.profile] : undefined;
  return (
    <div className="space-y-2" data-testid="queue-picker">
      <div className="flex flex-wrap items-end gap-2 text-xs">
        <label className="flex flex-col gap-0.5">
          <span className="text-muted-foreground">Suite</span>
          <select className={select} value={sel.suite} onChange={(e) => onSel({ ...sel, suite: e.target.value })} data-testid="queue-suite">
            <option value={ALL}>All suites</option>
            {suites
              .filter((s) => !s.targets?.length)
              .map((s) => (
                <option key={s.name} value={s.name}>
                  {s.name}
                </option>
              ))}
          </select>
        </label>
        <label className="flex flex-col gap-0.5">
          <span className="text-muted-foreground">Setup</span>
          <select className={select} value={sel.source} onChange={(e) => onSel({ ...sel, source: e.target.value as Selection["source"] })} data-testid="queue-source">
            <option value="saved">saved profile</option>
            <option value="build">build per card</option>
          </select>
        </label>
        {sel.source === "saved" ? (
          <label className="flex flex-col gap-0.5">
            <span className="text-muted-foreground">Profile</span>
            <select className={select} value={sel.profile} onChange={(e) => onSel({ ...sel, profile: e.target.value })} data-testid="queue-profile">
              <option value={ALL}>All profiles</option>
              <option value="">config's own</option>
              {Object.entries(d?.profiles ?? {})
                .filter(([, p]) => !p.hidden)
                .map(([name, p]) => (
                  <option key={name} value={name}>
                    {name}
                    {p.fits ? " (live)" : ""}
                  </option>
                ))}
            </select>
          </label>
        ) : (
          <span className="flex flex-col gap-1">
            <CardFilterPickers card="A" f={sel.A} d={d} allowOff={sel.perCard} onChange={(A) => onSel({ ...sel, A })} />
            {sel.perCard && <CardFilterPickers card="B" f={sel.B} d={d} allowOff onChange={(B) => onSel({ ...sel, B })} />}
            <label className="inline-flex items-center gap-1 text-muted-foreground">
              <input type="checkbox" checked={!sel.perCard} onChange={(e) => onSel({ ...sel, perCard: !e.target.checked })} data-testid="queue-same" />
              same on both cards
            </label>
          </span>
        )}
        <label className="flex flex-col gap-0.5">
          <span className="text-muted-foreground">Mode</span>
          <select className={select} value={sel.mode} disabled={!!prof?.bridge} onChange={(e) => onSel({ ...sel, mode: e.target.value })} data-testid="queue-mode">
            <option value={ALL}>All modes</option>
            <option value="">each profile's default{prof?.mode ? ` (${prof.mode})` : ""}</option>
            {MODES.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-0.5">
          <span className="text-muted-foreground">Reps</span>
          <input
            type="number"
            min={1}
            max={100}
            className={cn(select, "w-14")}
            value={reps}
            onChange={(e) => setReps(Math.max(1, Math.min(100, Number(e.target.value) || 1)))}
            data-testid="queue-reps"
          />
        </label>
        <Button size="sm" disabled={!setups.length || tooMany || add.isPending} onClick={() => add.mutate()} data-testid="queue-add">
          <ListPlus /> Add {setups.length} run{setups.length === 1 ? "" : "s"} to queue
        </Button>
      </div>
      <p className="text-[11px] text-muted-foreground">
        {matching} existing result{matching === 1 ? "" : "s"} match this selection (shown below).
        {prof?.bridge && " Bridge profiles have no cards or mode."}
        {sel.source === "build" && " Only compatible engine / model pairs that fit the card (rough VRAM estimate) are counted."}
        {setups.length > 1 && ` Each setup runs every task of its suite ${reps}×.`}
      </p>
      {tooMany && <p className="text-[11px] text-warning">That is {setups.length} setups: at most {MAX_ADD} per add. Narrow a picker.</p>}
      {add.data?.skipped?.length ? <p className="text-[11px] text-warning">Skipped: {add.data.skipped.join("; ")}</p> : null}
      {add.error && <p className="text-[11px] text-destructive">{(add.error as Error).message}</p>}
    </div>
  );
}

const tone: Record<string, string> = {
  queued: "text-muted-foreground",
  started: "text-primary",
  done: "text-success",
  failed: "text-destructive",
  cancelled: "text-muted-foreground",
};

/** The queue: items with status, reorder / remove / clear, one confirmation to start it all. */
export function QueuePanel({ onWatch, onShowBatch, running }: { onWatch: (pid: string) => void; onShowBatch: (batch: string) => void; running: { id: string; projectId?: string | null; batch?: string | null }[] }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["bench-queue"], queryFn: api.benchQueue, refetchInterval: 3000 });
  const [plan, setPlan] = useState<BenchQueuePlan | null>(null);
  const [planning, setPlanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const refresh = () => qc.invalidateQueries({ queryKey: ["bench-queue"] });
  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
    } catch (e) {
      setError((e as { data?: { error?: string } }).data?.error ?? (e instanceof Error ? e.message : String(e)));
    }
    await refresh();
  };
  const openPlan = async () => {
    setPlanning(true);
    setError(null);
    try {
      setPlan(await api.benchQueuePlan());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPlanning(false);
    }
  };
  const s = q.data;
  const items = s?.items ?? [];
  const queued = items.filter((i) => i.status === "queued").length;
  const finished = items.filter((i) => ["done", "failed", "cancelled"].includes(i.status)).length;
  const failed = items.filter((i) => ["failed", "cancelled"].includes(i.status)).length;
  return (
    <div className="space-y-2" data-testid="queue-panel">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <span className={cn("rounded-full px-2 py-0.5 font-medium", s?.running ? "bg-primary/15 text-primary" : "bg-muted text-muted-foreground")} data-testid="queue-state">
          {s?.running ? "queue running" : "queue paused"}
        </span>
        <span className="text-muted-foreground">
          {queued} queued · {finished} finished
        </span>
        <span className="ml-auto" />
        {s?.running ? (
          <>
            <Button size="sm" variant="outline" onClick={() => void act(() => api.benchQueuePause(false))} data-testid="queue-pause">
              <Pause /> Pause after this item
            </Button>
            <Button size="sm" variant="outline" onClick={() => void act(() => api.benchQueuePause(true))} data-testid="queue-stop">
              <Square /> Stop now
            </Button>
          </>
        ) : (
          <Button size="sm" disabled={!queued || planning} onClick={() => void openPlan()} data-testid="queue-start">
            {planning ? <Loader2 className="animate-spin" /> : <Play />} Start queue…
          </Button>
        )}
        {failed > 0 && (
          <Button size="sm" variant="ghost" onClick={() => void act(() => api.benchQueueRequeue())} data-testid="queue-requeue">
            <RotateCcw /> Re-queue failed
          </Button>
        )}
        <Button size="sm" variant="ghost" disabled={!finished} onClick={() => void act(() => api.benchQueueClear(true))} data-testid="queue-clear-finished">
          Clear finished
        </Button>
        <Button size="sm" variant="ghost" disabled={!items.length} onClick={() => confirm("Remove every item that is not in progress?") && void act(() => api.benchQueueClear(false))} data-testid="queue-clear">
          <Trash2 /> Clear
        </Button>
      </div>
      {s?.restoreScheduled && s.restoreLayout && <p className="text-xs text-warning">Switching the server back to {s.restoreLayout}{s.gate ? `: ${s.gate}` : ""}</p>}
      {error && <p className="text-xs text-destructive">{error}</p>}
      <ol className="max-h-80 space-y-0.5 overflow-auto" data-testid="queue-items">
        {items.map((i, n) => (
          <QueueLine
            key={i.id}
            item={i}
            first={n === 0}
            last={n === items.length - 1}
            onMove={(d) => void act(() => api.benchQueueMove(i.id, n + d))}
            onRemove={() => void act(() => api.benchQueueRemove(i.id))}
            onRequeue={() => void act(() => api.benchQueueRequeue(i.id))}
            watch={running.find((r) => r.batch && r.batch === i.batch && r.projectId)?.projectId ?? null}
            onWatch={onWatch}
            onShowBatch={onShowBatch}
          />
        ))}
        {!items.length && <li className="text-xs text-muted-foreground">Empty: pick a setup above and add it.</li>}
      </ol>
      <StartDialog
        plan={plan}
        items={items}
        onClose={() => setPlan(null)}
        onStart={async () => {
          setPlan(null);
          await act(() => api.benchQueueStart());
        }}
      />
    </div>
  );
}

function QueueLine({
  item,
  first,
  last,
  onMove,
  onRemove,
  onRequeue,
  watch,
  onWatch,
  onShowBatch,
}: {
  item: BenchQueueItem;
  first: boolean;
  last: boolean;
  onMove: (delta: number) => void;
  onRemove: () => void;
  onRequeue: () => void;
  watch: string | null;
  onWatch: (pid: string) => void;
  onShowBatch: (batch: string) => void;
}) {
  const status = item.status === "started" ? (item.detail ?? "starting") : item.status;
  return (
    <li className="flex min-w-0 items-center gap-2 text-xs" data-testid="queue-item">
      <span className={cn("w-40 shrink-0 truncate", tone[item.status], item.status === "started" && status.startsWith("waiting") && "text-warning")} title={status}>
        {item.status === "started" && <Loader2 className="mr-1 inline size-3 animate-spin" />}
        {status}
      </span>
      <span className="min-w-0 flex-1 truncate font-mono" title={item.note ?? undefined}>
        {item.suite} · {item.profile ?? "config's own"}
        {item.mode ? ` · ${item.mode}` : ""}
        {item.reps > 1 ? ` · ×${item.reps}` : ""}
        {item.note && item.status !== "queued" ? <span className="text-muted-foreground"> · {item.note}</span> : null}
      </span>
      {watch && (
        <Button size="sm" variant="outline" className="h-6" onClick={() => onWatch(watch)}>
          <Eye /> Watch
        </Button>
      )}
      {item.batch && item.status !== "queued" && (
        <button className="shrink-0 underline" onClick={() => onShowBatch(item.batch!)} data-testid="queue-results">
          results
        </button>
      )}
      {["failed", "cancelled"].includes(item.status) && (
        <button className="shrink-0 text-muted-foreground hover:text-foreground" onClick={onRequeue} title="Queue it again">
          <RotateCcw className="size-3.5" />
        </button>
      )}
      <button className="shrink-0 text-muted-foreground hover:text-foreground disabled:opacity-30" disabled={first} onClick={() => onMove(-1)} title="Move up">
        <ArrowUp className="size-3.5" />
      </button>
      <button className="shrink-0 text-muted-foreground hover:text-foreground disabled:opacity-30" disabled={last} onClick={() => onMove(1)} title="Move down">
        <ArrowDown className="size-3.5" />
      </button>
      <button className="shrink-0 text-muted-foreground hover:text-destructive" onClick={onRemove} title={item.status === "started" ? "Cancel this item" : "Remove"} data-testid="queue-remove">
        <X className="size-3.5" />
      </button>
    </li>
  );
}

/** The one confirmation: run order grouped by layout, switches, time, who is on the server now. */
function StartDialog({ plan, items, onClose, onStart }: { plan: BenchQueuePlan | null; items: BenchQueueItem[]; onClose: () => void; onStart: () => Promise<void> }) {
  const byId = new Map(items.map((i) => [i.id, i]));
  return (
    <Dialog open={!!plan} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-2xl" data-testid="queue-start-dialog">
        {plan && (
          <>
            <DialogTitle>Start the queue: {plan.items.length} item{plan.items.length === 1 ? "" : "s"}, about {minutes(plan.totalSeconds)}</DialogTitle>
            <DialogDescription>
              Live layout now: <span className="font-mono text-foreground">{plan.live ?? "unknown"}</span>. Items run grouped by server layout to switch as rarely as
              possible; when the queue finishes, is paused or stopped, the server switches back to{" "}
              <span className="font-mono text-foreground">{plan.backTo ?? "the layout live now"}</span>.
            </DialogDescription>
            <div className="space-y-1.5 text-sm">
              {plan.switches > 0 && (
                <p className="flex items-start gap-2 text-warning">
                  <AlertTriangle className="mt-0.5 size-4 shrink-0" />
                  <span>
                    {plan.switches} server switch{plan.switches === 1 ? "" : "es"} (≈{minutes(plan.switchSeconds)} in all, including the switch back): each restarts the model servers, and
                    everything using the models (Jarvis, PiG, other {brandName()} sessions) waits while it happens.
                  </span>
                </p>
              )}
              <p className="text-muted-foreground">
                Runs ≈{minutes(plan.runSeconds)}
                {plan.unknownTasks > 0 && ` (${plan.unknownTasks} task run${plan.unknownTasks === 1 ? " has" : "s have"} no earlier timing: counted at half their timeout)`}.
              </p>
              {plan.leases?.length ? (
                <p className="text-warning" data-testid="queue-start-leases">
                  In use now: {plan.leases.map((l) => `${l.slot}${l.who ? ` (${l.who})` : ""}`).join(", ")}. The queue waits for them, never interrupts them.
                </p>
              ) : (
                <p className="text-muted-foreground">Nobody holds a model slot right now.</p>
              )}
              <p className="text-muted-foreground">No switches or runs 06:45–07:30 UK: the queue waits there and carries on after.</p>
              {plan.error && <p className="text-destructive">{plan.error}</p>}
            </div>
            <ol className="max-h-56 space-y-1 overflow-auto rounded-md border p-2 text-xs" data-testid="queue-start-groups">
              {plan.groups.map((g, n) => (
                <li key={n}>
                  <div className="font-medium">
                    {g.switchFrom ? `switch ${g.switchFrom} → ${g.layout} (≈${minutes(g.switchSeconds ?? 180)}), then ` : `on ${g.layout ?? "the live layout"}: `}
                    {g.items.length} item{g.items.length === 1 ? "" : "s"}, ≈{minutes(g.runSeconds)}
                  </div>
                  <ul className="pl-3 font-mono text-muted-foreground">
                    {g.items.map((id) => {
                      const i = byId.get(id);
                      return i ? (
                        <li key={id}>
                          {i.suite} · {i.profile ?? "config's own"}
                          {i.mode ? ` · ${i.mode}` : ""}
                          {i.reps > 1 ? ` ×${i.reps}` : ""}
                        </li>
                      ) : null;
                    })}
                  </ul>
                </li>
              ))}
            </ol>
            <div className="flex justify-end gap-2 pt-1">
              <Button size="sm" variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button size="sm" onClick={() => void onStart()} data-testid="queue-start-confirm">
                <Play /> Start {plan.items.length} item{plan.items.length === 1 ? "" : "s"}
              </Button>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
