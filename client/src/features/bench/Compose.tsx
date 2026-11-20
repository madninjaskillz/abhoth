import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Play, Plus, Save, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, type BenchSuite } from "@/lib/api";
import { cn } from "@/lib/utils";
import { overridesToSets, setsToOverrides } from "./grid";

interface ArmDraft {
  name: string;
  leadMode: string;
  overrides: string;
  /** Kept from a copied suite (the form doesn't edit profiles). */
  profile?: string | null;
}

interface TargetDraft {
  name: string;
  endpoint: string;
  model: string;
  apiKeyEnv: string;
}

interface Draft {
  name: string;
  description: string;
  tasks: string[];
  arms: ArmDraft[];
  reps: number;
  timeoutMinutes: string;
  targets: TargetDraft[];
  targetMode: "pool" | "each";
  /** The copied suite's own profile, kept. */
  profile?: string | null;
}

const blank: Draft = {
  name: "",
  description: "",
  tasks: [],
  arms: [{ name: "base", leadMode: "hands-on", overrides: "{}" }],
  reps: 3,
  timeoutMinutes: "",
  targets: [],
  targetMode: "pool",
};

function draftOf(s: BenchSuite): Draft {
  return {
    name: s.name,
    description: s.description ?? "",
    tasks: [...s.tasks],
    arms: s.arms.map((a) => ({ name: a.name, leadMode: a.leadMode, overrides: setsToOverrides(a.set), profile: a.profile ?? null })),
    profile: s.profile ?? null,
    reps: s.reps,
    timeoutMinutes: s.timeoutSeconds ? String(Math.round(s.timeoutSeconds / 60)) : "",
    targets: (s.targets ?? []).map((t) => ({ name: t.name, endpoint: t.endpoint, model: t.model ?? "", apiKeyEnv: t.apiKeyEnv ?? "" })),
    targetMode: s.targetMode === "each" ? "each" : "pool",
  };
}

/** The suite a draft describes, or what is wrong with it (the server checks again). */
function suiteOf(d: Draft): { suite?: BenchSuite; error?: string } {
  if (!d.name.trim()) return { error: "Give the suite a name." };
  if (!d.tasks.length) return { error: "Pick at least one task." };
  const arms = [];
  for (const a of d.arms) {
    if (!a.name.trim()) return { error: "Give each arm a name." };
    const { sets, error } = overridesToSets(a.overrides);
    if (error) return { error: `Arm ${a.name || "?"}: ${error}` };
    arms.push({ name: a.name.trim(), leadMode: a.leadMode.trim() || "auto", set: sets.length ? sets : null, profile: a.profile ?? null });
  }
  const minutes = d.timeoutMinutes.trim() ? Number(d.timeoutMinutes) : null;
  if (minutes !== null && !(minutes > 0)) return { error: "The time limit is a number of minutes." };
  if (new Set(arms.map((a) => a.name)).size !== arms.length) return { error: "Give each arm a name of its own." };
  const targets = d.targets.map((t) => ({ name: t.name.trim(), endpoint: t.endpoint.trim(), model: t.model.trim() || null, apiKeyEnv: t.apiKeyEnv.trim() || null }));
  for (const t of targets) {
    if (!t.name) return { error: "Give each target a name." };
    if (!/^https?:\/\/\S+$/i.test(t.endpoint)) return { error: `Target ${t.name}: give its endpoint, e.g. http://host:8080/v1` };
  }
  return {
    suite: {
      name: d.name.trim(),
      description: d.description.trim() || null,
      tasks: d.tasks,
      arms,
      reps: d.reps,
      timeoutSeconds: minutes === null ? null : Math.round(minutes * 60),
      profile: d.profile ?? null,
      targets: targets.length ? targets : null,
      targetMode: targets.length ? d.targetMode : null,
    },
  };
}

const input = "min-w-0 rounded-md border bg-background px-2 py-1 text-xs";

/**
 * Compose a suite: tasks × arms (a name, a lead mode and JSON config overrides) × reps, and
 * optionally the targets its runs go to. Saved as suites/&lt;name&gt;.json on the server, like the
 * built-in ones; "Save and queue" also starts it.
 */
export function Compose({ suites, from, onQueued, onSaved }: { suites: BenchSuite[]; from?: BenchSuite; onQueued: (batch: string) => void; onSaved: (name: string) => void }) {
  const qc = useQueryClient();
  const tasks = useQuery({ queryKey: ["bench-tasks"], queryFn: api.benchTasks, staleTime: 60_000 });
  const [d, setD] = useState<Draft>(blank);
  const [extraTask, setExtraTask] = useState("");
  const set = (patch: Partial<Draft>) => {
    setD((x) => ({ ...x, ...patch }));
    save.reset(); // what the server said about the last attempt no longer describes the form
  };
  const exists = suites.some((s) => s.name === d.name.trim());
  const built = suiteOf(d);
  const save = useMutation({
    mutationFn: (start: boolean) => api.benchSaveSuite(built.suite!, { overwrite: exists, start }),
    onSuccess: (r) => {
      void qc.invalidateQueries({ queryKey: ["bench"] });
      onSaved(r.suite.name);
      if (r.batch) onQueued(r.batch);
    },
  });
  const known = [...new Set([...(tasks.data ?? []).map((t) => t.name), ...d.tasks])];
  const runs = d.tasks.length * d.arms.length * Math.max(1, d.reps) * (d.targetMode === "each" && d.targets.length ? d.targets.length : 1);

  return (
    <div className="space-y-3 text-xs" data-testid="bench-compose">
      <div className="flex flex-wrap items-end gap-2">
        <label className="flex min-w-0 flex-1 basis-40 flex-col gap-0.5">
          <span className="text-muted-foreground">Name</span>
          <input className={cn(input, "font-mono")} value={d.name} onChange={(e) => set({ name: e.target.value })} placeholder="cap-confirm" data-testid="compose-name" />
        </label>
        <label className="flex w-16 flex-col gap-0.5">
          <span className="text-muted-foreground">Reps</span>
          <input className={input} type="number" min={1} max={100} value={d.reps} onChange={(e) => set({ reps: Math.max(1, Number(e.target.value) || 1) })} data-testid="compose-reps" />
        </label>
        <label className="flex w-24 flex-col gap-0.5">
          <span className="text-muted-foreground">Limit (min)</span>
          <input className={input} inputMode="numeric" value={d.timeoutMinutes} onChange={(e) => set({ timeoutMinutes: e.target.value })} placeholder="task's own" />
        </label>
        {from && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              setD(draftOf(from));
              save.reset();
            }}
            title="Fill the form from the suite picked above"
          >
            Copy from {from.name}
          </Button>
        )}
      </div>
      <label className="flex flex-col gap-0.5">
        <span className="text-muted-foreground">Description</span>
        <input className={input} value={d.description} onChange={(e) => set({ description: e.target.value })} placeholder="What this suite compares" />
      </label>

      <fieldset>
        <legend className="mb-1 text-muted-foreground">Tasks ({d.tasks.length})</legend>
        <div className="flex max-h-28 flex-wrap gap-1 overflow-auto" data-testid="compose-tasks">
          {known.map((t) => {
            const on = d.tasks.includes(t);
            return (
              <button
                key={t}
                type="button"
                role="checkbox"
                aria-checked={on}
                onClick={() => set({ tasks: on ? d.tasks.filter((x) => x !== t) : [...d.tasks, t] })}
                className={cn("rounded-md border px-1.5 py-0.5 font-mono", on ? "border-primary bg-primary/10 text-foreground" : "text-muted-foreground hover:bg-accent")}
              >
                {t}
              </button>
            );
          })}
          {!known.length && <span className="text-muted-foreground">{tasks.isLoading ? "Loading tasks…" : "The task kit lists no tasks; type a name below."}</span>}
        </div>
        <form
          className="mt-1 flex gap-1"
          onSubmit={(e) => {
            e.preventDefault();
            const t = extraTask.trim();
            if (t && !d.tasks.includes(t)) set({ tasks: [...d.tasks, t] });
            setExtraTask("");
          }}
        >
          <input className={cn(input, "w-44 font-mono")} value={extraTask} onChange={(e) => setExtraTask(e.target.value)} placeholder="another task name" />
          <Button size="sm" variant="outline" type="submit" disabled={!extraTask.trim()}>
            <Plus /> Add
          </Button>
        </form>
      </fieldset>

      <fieldset>
        <legend className="mb-1 text-muted-foreground">Arms: name, lead mode, config overrides (JSON)</legend>
        <div className="space-y-1.5">
          {d.arms.map((a, i) => {
            const bad = overridesToSets(a.overrides).error;
            const edit = (patch: Partial<ArmDraft>) => set({ arms: d.arms.map((x, j) => (j === i ? { ...x, ...patch } : x)) });
            return (
              <div key={i} className="flex flex-wrap items-start gap-1" data-testid="compose-arm">
                <input className={cn(input, "w-28 font-mono")} value={a.name} onChange={(e) => edit({ name: e.target.value })} placeholder="arm name" aria-label="Arm name" />
                {a.profile && (
                  <span className="rounded-md border px-1.5 py-1 font-mono text-muted-foreground" title="This arm runs on an inference profile (kept from the copied suite)">
                    profile {a.profile}
                  </span>
                )}
                <input className={cn(input, "w-24 font-mono")} value={a.leadMode} onChange={(e) => edit({ leadMode: e.target.value })} list="bench-lead-modes" placeholder="lead mode" aria-label="Lead mode" />
                <textarea
                  className={cn(input, "min-h-[1.75rem] flex-1 basis-56 font-mono", bad && "border-destructive")}
                  rows={Math.min(6, Math.max(1, a.overrides.split("\n").length))}
                  value={a.overrides}
                  onChange={(e) => edit({ overrides: e.target.value })}
                  spellCheck={false}
                  placeholder='{"agent.reasoning.maxStepTokens": 4096}'
                  title={bad}
                  aria-label="Config overrides"
                />
                <Button size="icon-sm" variant="ghost" disabled={d.arms.length === 1} onClick={() => set({ arms: d.arms.filter((_, j) => j !== i) })} aria-label="Remove arm">
                  <Trash2 />
                </Button>
              </div>
            );
          })}
          <datalist id="bench-lead-modes">
            <option value="auto" />
            <option value="hands-on" />
          </datalist>
          <Button size="sm" variant="outline" onClick={() => set({ arms: [...d.arms, { name: `arm${d.arms.length + 1}`, leadMode: d.arms[0]?.leadMode ?? "auto", overrides: "{}" }] })} data-testid="compose-add-arm">
            <Plus /> Arm
          </Button>
        </div>
      </fieldset>

      <fieldset>
        <legend className="mb-1 text-muted-foreground">
          Targets (optional): model endpoints the runs go straight to, one run per target at a time
        </legend>
        <div className="space-y-1.5">
          {d.targets.map((t, i) => {
            const edit = (patch: Partial<TargetDraft>) => set({ targets: d.targets.map((x, j) => (j === i ? { ...x, ...patch } : x)) });
            return (
              <div key={i} className="flex flex-wrap items-center gap-1" data-testid="compose-target">
                <input className={cn(input, "w-20 font-mono")} value={t.name} onChange={(e) => edit({ name: e.target.value })} placeholder="name" aria-label="Target name" />
                <input className={cn(input, "flex-1 basis-56 font-mono")} value={t.endpoint} onChange={(e) => edit({ endpoint: e.target.value })} placeholder="http://host:8080/v1" aria-label="Endpoint" />
                <input className={cn(input, "w-36 font-mono")} value={t.model} onChange={(e) => edit({ model: e.target.value })} placeholder="model (optional)" aria-label="Model" />
                <input
                  className={cn(input, "w-40 font-mono")}
                  value={t.apiKeyEnv}
                  onChange={(e) => edit({ apiKeyEnv: e.target.value })}
                  placeholder="key env var NAME"
                  title="The name of an environment variable on the server that holds the API key. Never the key itself."
                  aria-label="API key environment variable name"
                />
                <Button size="icon-sm" variant="ghost" onClick={() => set({ targets: d.targets.filter((_, j) => j !== i) })} aria-label="Remove target">
                  <Trash2 />
                </Button>
              </div>
            );
          })}
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              onClick={() => set({ targets: [...d.targets, { name: String.fromCharCode(97 + d.targets.length), endpoint: "", model: "", apiKeyEnv: "" }] })}
              data-testid="compose-add-target"
            >
              <Plus /> Target
            </Button>
            {d.targets.length > 1 && (
              <select className={input} value={d.targetMode} onChange={(e) => set({ targetMode: e.target.value as "pool" | "each" })} aria-label="How runs use the targets">
                <option value="pool">each run on whichever target is free</option>
                <option value="each">every run on every target (compare targets)</option>
              </select>
            )}
            {!d.targets.length && <span className="text-muted-foreground">None: one run at a time on the base config, when no slots are leased.</span>}
          </div>
        </div>
      </fieldset>

      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" disabled={!built.suite || save.isPending} onClick={() => save.mutate(true)} data-testid="compose-queue">
          <Play /> Save and queue ({runs} run{runs === 1 ? "" : "s"})
        </Button>
        <Button size="sm" variant="outline" disabled={!built.suite || save.isPending} onClick={() => save.mutate(false)} data-testid="compose-save">
          <Save /> {exists ? "Replace suite" : "Save suite"}
        </Button>
        <span className={cn("min-w-0 flex-1 basis-40", (save.error || built.error) && d.name ? "text-destructive" : "text-muted-foreground")} data-testid="compose-message">
          {save.error ? (save.error as Error).message : (built.error ?? (exists ? `Replaces the saved suite ${d.name.trim()}.` : ""))}
        </span>
      </div>
    </div>
  );
}
