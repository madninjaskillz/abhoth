// The Create package wizard (section 10 of the router media plan): source -> probe -> form ->
// review -> save. The router drafts a manifest from an image, a compose file, a pip spec, a
// GitHub release or an existing manual engine (registry metadata only: nothing runs); an optional
// test run starts a throwaway container only after asking. The review step checks the manifest
// live with the router's validator; then save to My packages, install now, or export.
import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowLeft, ArrowRight, Check, Copy, Download, FlaskConical, Loader2, Package, Plus, Trash2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { RouterApiError, engineStoreApi, routerApi } from "../routerApi";
import { Check2, Field, Grid, Select, Text, area, errText } from "./formKit";
import { serverLabel, type Inventory } from "./inventoryModel";
import {
  APIS, GPU_KINDS, KINDS, LAYOUTS, METHOD_TYPES, PARAM_TYPES, PROBES, SOURCES, UNPACK, applyGuesses, emptyAsset, emptyForm, emptyMethod, firstImage, formProblems,
  formToManifest, isHostRun, manifestJson, manifestToForm, parseManifest, sortIssues,
  type AssetRow, type DraftSource, type ImageProbe, type Manifest, type MethodRow, type MethodType, type ParamRow, type Report, type TestRun, type WizardForm,
} from "./engineStoreModel";

const STEPS = ["Source", "Probe", "Form", "Review", "Save"] as const;
type StepN = 0 | 1 | 2 | 3 | 4;

export interface PackageWizardProps {
  /** "Package this engine": start from this inventory engine. */
  engine?: string;
  /** Edit a package (My packages): start at the form. */
  initial?: { manifest: Manifest; files: Record<string, string> };
  onClose: () => void;
  /** After "Install now": open the install flow for this package in My packages. */
  onInstall: (id: string) => void;
}

export function PackageWizard({ engine, initial, onClose, onInstall }: PackageWizardProps) {
  const inv = useQuery({ queryKey: ["router-inventory"], queryFn: routerApi.inventory, refetchOnWindowFocus: false });
  const [step, setStep] = useState<StepN>(initial ? 2 : 0);
  const [source, setSource] = useState<DraftSource>(engine ? "engine" : "docker");
  const [value, setValue] = useState(engine ?? "");
  const [notes, setNotes] = useState<string[]>([]);
  const [probe, setProbe] = useState<ImageProbe | null>(null);
  const [form, setForm] = useState<WizardForm>(() => (initial ? manifestToForm(initial.manifest) : emptyForm()));
  const [files, setFiles] = useState<Record<string, string>>(initial?.files ?? {});
  const [json, setJson] = useState("");
  const [jsonErr, setJsonErr] = useState<string | null>(null);
  const [mode, setMode] = useState<"private" | "store">("private");

  const draft = useMutation({
    mutationFn: () => engineStoreApi.draft(source, source === "scratch" ? "" : value.trim()),
    onSuccess: (d) => {
      setForm(manifestToForm(d.manifest));
      setFiles(d.files ?? {});
      setNotes(d.notes ?? []);
      setProbe(d.probe ?? null);
      setStep(d.probe || firstImage(d.manifest) ? 1 : 2);
    },
  });

  const toReview = () => {
    setJson(manifestJson(formToManifest(form)));
    setJsonErr(null);
    setStep(3);
  };
  const backToForm = () => {
    const p = parseManifest(json);
    if (!p.manifest) return setJsonErr(`The JSON does not parse: ${p.error}`);
    setForm(manifestToForm(p.manifest));
    setStep(2);
  };
  const go = (n: StepN) => {
    if (n === step) return;
    if (step === 3 && n < 3) return backToForm();
    if (n === 3) return toReview();
    if (n === 4 && step !== 3) toReview();
    setStep(n);
  };

  return (
    <div className="space-y-3" data-testid="pkg-wizard">
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="ghost" onClick={onClose} data-testid="pkg-wizard-close"><ArrowLeft /> Back to the store</Button>
        <h3 className="flex items-center gap-1.5 text-sm font-semibold"><Package className="size-4" /> {initial ? `Edit ${initial.manifest.id}` : "Create a package"}</h3>
      </div>
      <div className="flex flex-wrap gap-1.5" data-testid="pkg-wizard-steps">
        {STEPS.map((s, n) => (
          <button
            key={s}
            onClick={() => go(n as StepN)}
            disabled={n > 0 && step === 0 && !initial}
            className={cn("flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs disabled:opacity-50", n < step && "border-success/40 text-success", n === step && "border-primary bg-primary/10 text-primary", n > step && "text-muted-foreground")}
          >
            {n < step ? <Check className="size-3" /> : <span className="font-mono">{n + 1}</span>} {s}
          </button>
        ))}
      </div>

      {step === 0 && (
        <SourceStep inv={inv.data} source={source} setSource={setSource} value={value} setValue={setValue} busy={draft.isPending} error={draft.error ? errText(draft.error) : null} onNext={() => draft.mutate()} />
      )}
      {step === 1 && (
        <ProbeStep inv={inv.data} probe={probe} notes={notes} form={form} setForm={setForm} onNext={() => setStep(2)} />
      )}
      {step === 2 && (
        <div className="space-y-3">
          {notes.length > 0 && <Notes notes={notes} />}
          <PackageForm form={form} setForm={setForm} />
          <div className="flex justify-end gap-2">
            {formProblems(form).length > 0 && <span className="self-center text-xs text-muted-foreground">{formProblems(form)[0]}</span>}
            <Button size="sm" onClick={toReview} data-testid="pkg-to-review">Review <ArrowRight /></Button>
          </div>
        </div>
      )}
      {step === 3 && (
        <ReviewStep json={json} setJson={(t) => { setJson(t); setJsonErr(null); }} jsonErr={jsonErr} files={files} setFiles={setFiles} mode={mode} setMode={setMode} onBack={backToForm} onNext={() => setStep(4)} />
      )}
      {step === 4 && <SaveStep json={json} files={files} onInstall={onInstall} onEdit={() => setStep(3)} />}
    </div>
  );
}

function Notes({ notes }: { notes: string[] }) {
  return (
    <ul className="list-disc space-y-0.5 rounded-md border bg-muted/30 py-2 pl-7 pr-3 text-xs text-muted-foreground" data-testid="pkg-draft-notes">
      {notes.map((n, i) => <li key={i}>{n}</li>)}
    </ul>
  );
}

// ── 1. source ────────────────────────────────────────────────────────────────────────────────

function SourceStep({ inv, source, setSource, value, setValue, busy, error, onNext }: {
  inv?: Inventory; source: DraftSource; setSource: (s: DraftSource) => void; value: string; setValue: (v: string) => void; busy: boolean; error: string | null; onNext: () => void;
}) {
  const s = SOURCES.find((x) => x.id === source)!;
  const engines = inv?.engines ?? [];
  const ready = source === "scratch" || value.trim() !== "";
  return (
    <div className="space-y-3 rounded-md border p-3" data-testid="pkg-source">
      <p className="text-xs text-muted-foreground">Start from what you have. The router drafts a manifest from it; you check and finish it in the next steps.</p>
      <div className="grid gap-2 sm:grid-cols-3">
        {SOURCES.map((x) => (
          <button
            key={x.id}
            onClick={() => { setSource(x.id); if (x.id !== source) setValue(""); }}
            className={cn("rounded-md border p-2 text-left text-xs", x.id === source ? "border-primary bg-primary/5" : "hover:bg-accent/50")}
            data-testid={`pkg-source-${x.id}`}
          >
            <div className="font-medium">{x.label}</div>
            <div className="text-muted-foreground">{x.hint}</div>
          </button>
        ))}
      </div>
      {source === "engine" ? (
        <Field label="Engine" hint={engines.length ? undefined : "No engines in the inventory yet."}>
          <Select value={value} onChange={setValue} options={[["", "Pick an engine…"], ...engines.map((e) => [e.id, `${e.title || e.id} (${e.driver})`] as [string, string])]} testid="pkg-source-engine" />
        </Field>
      ) : source !== "scratch" ? (
        <Field label={s.label}>
          {s.multiline
            ? <textarea className={cn(area, "min-h-[160px]")} value={value} onChange={(e) => setValue(e.target.value)} placeholder={s.placeholder} spellCheck={false} data-testid="pkg-source-value" />
            : <Text value={value} onChange={setValue} placeholder={s.placeholder} mono testid="pkg-source-value" />}
        </Field>
      ) : null}
      {error && <div className="flex items-start gap-1.5 text-xs text-destructive" data-testid="pkg-source-error"><AlertTriangle className="mt-0.5 size-3.5 shrink-0" />{error}</div>}
      <div className="flex justify-end">
        <Button size="sm" onClick={onNext} disabled={!ready || busy} data-testid="pkg-source-next">{busy ? <Loader2 className="animate-spin" /> : <ArrowRight />} Draft the package</Button>
      </div>
    </div>
  );
}

// ── 2. probe ─────────────────────────────────────────────────────────────────────────────────

function show(v: unknown): string {
  if (v === undefined || v === null || v === "") return "";
  if (Array.isArray(v)) return v.map(show).join(", ");
  if (typeof v === "object") return Object.entries(v as Record<string, unknown>).map(([k, x]) => `${k}=${show(x)}`).join("\n");
  return String(v);
}

function ProbeStep({ inv, probe, notes, form, setForm, onNext }: {
  inv?: Inventory; probe: ImageProbe | null; notes: string[]; form: WizardForm; setForm: (f: WizardForm) => void; onNext: () => void;
}) {
  const [image, setImage] = useState(() => firstImage(formToManifest(form)) || String(probe?.image ?? ""));
  const [pick, setPick] = useState<{ server: string; card: string }>({ server: "", card: "" });
  const [confirm, setConfirm] = useState(false);
  const [result, setResult] = useState<TestRun | null>(null);
  const run = useMutation({
    mutationFn: () => engineStoreApi.testRun(image.trim(), pick.server, pick.card),
    onSuccess: (r) => setResult(r),
  });
  const known = ["image", "digest", "ports", "env", "labels", "licence"];
  const rows: [string, string][] = probe
    ? [...known.filter((k) => probe[k] !== undefined && probe[k] !== null).map((k) => [k, show(probe[k])] as [string, string]),
       ...Object.entries(probe).filter(([k, v]) => !known.includes(k) && v !== null && v !== undefined).map(([k, v]) => [k, show(v)] as [string, string])]
    : [];
  return (
    <div className="space-y-3" data-testid="pkg-probe">
      {notes.length > 0 && <Notes notes={notes} />}
      <div className="rounded-md border">
        <div className="border-b px-3 py-2 text-xs font-medium">Image metadata (from the registry: nothing ran)</div>
        {rows.length === 0 ? <div className="p-3 text-xs text-muted-foreground">No image metadata for this source.</div> : (
          <table className="w-full text-xs" data-testid="pkg-probe-meta">
            <tbody className="divide-y">
              {rows.map(([k, v]) => (
                <tr key={k}><td className="w-28 px-3 py-1.5 align-top text-muted-foreground">{k}</td><td className="whitespace-pre-wrap break-all px-3 py-1.5 font-mono">{v || "—"}</td></tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <div className="space-y-2 rounded-md border p-3">
        <div className="text-xs font-medium">Test run (optional)</div>
        <p className="text-xs text-muted-foreground">Starts the image once on a card, with no mounts, asks the usual paths (/v1/models, /object_info, /health, /system_stats) and removes it. It can guess the API and the health check.</p>
        <Grid>
          <Field label="Image"><Text value={image} onChange={setImage} mono placeholder="ghcr.io/owner/image@sha256:…" testid="pkg-test-image" /></Field>
          <ServerCardPicker inv={inv} value={pick} onChange={setPick} />
        </Grid>
        <div className="flex justify-end">
          <Button size="sm" variant="outline" disabled={!image.trim() || !pick.server || !pick.card || run.isPending} onClick={() => setConfirm(true)} data-testid="pkg-test-run">
            {run.isPending ? <Loader2 className="animate-spin" /> : <FlaskConical />} Test run on a card…
          </Button>
        </div>
        {run.error && <div className="text-xs text-destructive" data-testid="pkg-test-error">{errText(run.error)}</div>}
        {result && (
          <div className="space-y-2" data-testid="pkg-test-result">
            <div className="text-xs">{result.ok ? <Badge variant="success">answered</Badge> : <Badge variant="destructive">no answer</Badge>}</div>
            <table className="w-full text-xs">
              <tbody className="divide-y">
                {result.probes.map((p) => (
                  <tr key={p.path}><td className="px-2 py-1 font-mono">{p.path}</td><td className="px-2 py-1">{String(p.status)}</td><td className="truncate px-2 py-1 font-mono text-muted-foreground">{p.snippet}</td></tr>
                ))}
              </tbody>
            </table>
            {(result.api_guess || result.health_guess) && (
              <div className="flex flex-wrap items-center gap-2 text-xs">
                <span>Guess: {result.api_guess && <>API <span className="font-mono">{result.api_guess}</span></>} {result.health_guess && <>· health <span className="font-mono">{result.health_guess}</span></>}</span>
                <Button size="sm" variant="outline" onClick={() => setForm(applyGuesses(form, result.api_guess, result.health_guess))} data-testid="pkg-test-apply"><Check /> Use these</Button>
              </div>
            )}
            {result.logs && <details><summary className="cursor-pointer text-xs text-muted-foreground">Container log</summary><pre className="max-h-48 overflow-auto rounded bg-muted p-2 font-mono text-[11px]">{result.logs}</pre></details>}
          </div>
        )}
      </div>
      <div className="flex justify-end"><Button size="sm" onClick={onNext} data-testid="pkg-probe-next">Next <ArrowRight /></Button></div>
      <ConfirmDialog
        open={confirm}
        title="Run a test container?"
        body={<>This runs <span className="font-mono break-all">{image}</span> on server <b>{pick.server}</b>, card <b>{pick.card}</b>. It runs with no mounts and is removed afterwards.</>}
        confirm="Run it"
        onCancel={() => setConfirm(false)}
        onConfirm={() => { setConfirm(false); setResult(null); run.mutate(); }}
      />
    </div>
  );
}

/** Server then card, from the inventory. */
export function ServerCardPicker({ inv, value, onChange }: { inv?: Inventory; value: { server: string; card: string }; onChange: (v: { server: string; card: string }) => void }) {
  const servers = inv?.servers ?? [];
  const cards = servers.find((s) => s.id === value.server)?.cards ?? [];
  return (
    <div className="grid grid-cols-2 gap-2">
      <Field label="Server">
        <Select value={value.server} onChange={(s) => onChange({ server: s, card: "" })} options={[["", "Pick…"], ...servers.map((s) => [s.id, serverLabel(s)] as [string, string])]} testid="pkg-pick-server" />
      </Field>
      <Field label="Card">
        <Select
          value={value.card}
          onChange={(c) => onChange({ ...value, card: c })}
          options={[["", "Pick…"], ...cards.map((c) => [c.id, `${c.id} · ${c.device === "cpu" ? "CPU" : `${c.label || "GPU"}${c.vramGib ? ` ${c.vramGib} GiB` : ""}`}`] as [string, string])]}
          testid="pkg-pick-card"
        />
      </Field>
    </div>
  );
}

// ── 3. form ──────────────────────────────────────────────────────────────────────────────────

function Ticks({ all, value, onChange, testid }: { all: readonly string[]; value: string[]; onChange: (v: string[]) => void; testid?: string }) {
  return (
    <div className="flex flex-wrap gap-x-3 gap-y-1" data-testid={testid}>
      {all.map((k) => (
        <Check2 key={k} label={k} checked={value.includes(k)} onChange={(on) => onChange(on ? [...value, k] : value.filter((x) => x !== k))} />
      ))}
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2 rounded-md border p-3">
      <h4 className="text-xs font-semibold">{title}</h4>
      {children}
    </section>
  );
}

function ListText({ value, onChange, placeholder, rows = 3, testid }: { value: string; onChange: (v: string) => void; placeholder?: string; rows?: number; testid?: string }) {
  return <textarea className={area} rows={rows} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} spellCheck={false} data-testid={testid} />;
}

export function PackageForm({ form, setForm }: { form: WizardForm; setForm: (f: WizardForm) => void }) {
  const set = <K extends keyof WizardForm>(k: K) => (v: WizardForm[K]) => setForm({ ...form, [k]: v });
  const setParam = (i: number, p: Partial<ParamRow>) => set("params")(form.params.map((x, n) => (n === i ? { ...x, ...p } : x)));
  const setMethod = (i: number, m: MethodRow) => set("methods")(form.methods.map((x, n) => (n === i ? m : x)));
  const move = (i: number, d: -1 | 1) => {
    const j = i + d;
    if (j < 0 || j >= form.methods.length) return;
    const ms = [...form.methods];
    [ms[i], ms[j]] = [ms[j], ms[i]];
    set("methods")(ms);
  };
  return (
    <div className="space-y-3" data-testid="pkg-form">
      <Section title="The package">
        <Grid>
          <Field label="Id" hint="lower case: also the engine's id by default"><Text value={form.id} onChange={set("id")} mono placeholder="my-engine" testid="pkg-f-id" /></Field>
          <Field label="Name"><Text value={form.name} onChange={set("name")} placeholder="My engine" testid="pkg-f-name" /></Field>
          <Field label="Version" hint="the engine's own version"><Text value={form.version} onChange={set("version")} mono placeholder="1.0.0" testid="pkg-f-version" /></Field>
          <Field label="Licence"><Text value={form.licence} onChange={set("licence")} placeholder="Apache-2.0" /></Field>
          <Field label="Source" hint="where the code lives"><Text value={form.source} onChange={set("source")} mono placeholder="https://github.com/…" /></Field>
          <Field label="Homepage"><Text value={form.homepage} onChange={set("homepage")} mono /></Field>
          <Field label="Description" wide><ListText value={form.description} onChange={set("description")} /></Field>
          <Field label="Notes" hint="one per line" wide><ListText value={form.notes} onChange={set("notes")} rows={2} /></Field>
        </Grid>
        <Check2 label="Private (My packages only: may use local paths; cannot be published to a store)" checked={form.private} onChange={set("private")} />
      </Section>

      <Section title="What it serves">
        <Field label="Kinds"><Ticks all={KINDS} value={form.kinds} onChange={set("kinds")} testid="pkg-f-kinds" /></Field>
        <Grid>
          <Field label="API" hint="what clients speak to it"><Select value={form.api} onChange={set("api")} options={[["", "Pick…"], ...APIS, ...(form.api && !(APIS as readonly string[]).includes(form.api) ? [form.api] : [])]} testid="pkg-f-api" /></Field>
          <Field label="Also speaks" hint="other APIs, comma separated"><Text value={form.alsoSpeaks} onChange={set("alsoSpeaks")} mono placeholder="openai-embeddings" /></Field>
          <Field label="Probe" hint="how the router asks it what it is"><Select value={form.probe} onChange={set("probe")} options={[["", "(none set)"], ...PROBES]} testid="pkg-f-probe" /></Field>
          <Field label="Capabilities from" hint="caps.inherits: a built-in engine's capabilities"><Text value={form.inherits} onChange={set("inherits")} mono placeholder="llamacpp" /></Field>
        </Grid>
      </Section>

      <Section title="Hardware">
        <Field label="GPU stacks"><Ticks all={GPU_KINDS} value={form.gpu} onChange={set("gpu")} /></Field>
        <Grid>
          <Field label="Minimum VRAM (GiB)"><Text value={form.minVram} onChange={set("minVram")} mono /></Field>
          <Field label="Minimum RAM (GiB)"><Text value={form.minRam} onChange={set("minRam")} mono /></Field>
          <Field label="SIMD" hint="e.g. avx2, comma separated"><Text value={form.simd} onChange={set("simd")} mono /></Field>
        </Grid>
        <Check2 label="Runs on a CPU card too" checked={form.cpuOk} onChange={set("cpuOk")} />
      </Section>

      <Section title="Model folder">
        <Grid>
          <Field label="Layout"><Select value={form.modelsLayout} onChange={set("modelsLayout")} options={[["", "(no model folder)"], ...LAYOUTS]} /></Field>
          {form.modelsLayout && (
            <>
              <Field label="Mount" hint="where the engine sees it, e.g. /models"><Text value={form.modelsMount} onChange={set("modelsMount")} mono /></Field>
              <Field label="Folders" hint="sub-folders it uses, comma separated"><Text value={form.modelsFolders} onChange={set("modelsFolders")} mono /></Field>
              <Field label="Env" hint="an env var pointing at it, e.g. HF_HOME"><Text value={form.modelsEnv} onChange={set("modelsEnv")} mono /></Field>
            </>
          )}
        </Grid>
        {form.modelsLayout && <Check2 label="Read only" checked={form.modelsReadOnly} onChange={set("modelsReadOnly")} />}
      </Section>

      <Section title="Params">
        <p className="text-xs text-muted-foreground">Settings an admin can change per install (and per model when ticked). Use them in run commands as {"{{param.<id>}}"}.</p>
        {form.params.map((p, i) => (
          <div key={i} className="grid items-end gap-2 sm:grid-cols-[1fr_7rem_1fr_2fr_auto_auto]" data-testid="pkg-f-param">
            <Field label="Id"><Text value={p.id} onChange={(v) => setParam(i, { id: v })} mono /></Field>
            <Field label="Type"><Select value={p.type} onChange={(v) => setParam(i, { type: v as ParamRow["type"] })} options={[...PARAM_TYPES]} /></Field>
            <Field label="Default"><Text value={p.def} onChange={(v) => setParam(i, { def: v })} mono /></Field>
            <Field label="Description"><Text value={p.description} onChange={(v) => setParam(i, { description: v })} /></Field>
            <div className="pb-1.5"><Check2 label="per model" checked={p.per_model} onChange={(v) => setParam(i, { per_model: v })} /></div>
            <Button size="sm" variant="ghost" aria-label="Remove param" onClick={() => set("params")(form.params.filter((_, n) => n !== i))}><Trash2 /></Button>
          </div>
        ))}
        <Button size="sm" variant="outline" onClick={() => set("params")([...form.params, { id: "", type: "string", def: "", description: "", per_model: false, raw: {} }])} data-testid="pkg-f-param-add"><Plus /> Add param</Button>
      </Section>

      <Section title="Methods (most preferred first)">
        <p className="text-xs text-muted-foreground">The router uses the first method that fits the host and card. Templates: {"{{port}}, {{host}}, {{model_dir}}, {{model_file}}, {{install}}, {{python}}, {{param.<id>}}"}…</p>
        {form.methods.map((m, i) => (
          <MethodEditor key={i} n={i} m={m} onChange={(x) => setMethod(i, x)} onRemove={() => set("methods")(form.methods.filter((_, k) => k !== i))} onUp={() => move(i, -1)} onDown={() => move(i, 1)} />
        ))}
        <div className="flex flex-wrap gap-1">
          {METHOD_TYPES.map((t) => (
            <Button key={t} size="sm" variant="outline" onClick={() => set("methods")([...form.methods, emptyMethod(t)])} data-testid={`pkg-f-method-add-${t}`}><Plus /> {t}</Button>
          ))}
        </div>
      </Section>
    </div>
  );
}

function MethodEditor({ n, m, onChange, onRemove, onUp, onDown }: { n: number; m: MethodRow; onChange: (m: MethodRow) => void; onRemove: () => void; onUp: () => void; onDown: () => void }) {
  const set = <K extends keyof MethodRow>(k: K) => (v: MethodRow[K]) => onChange({ ...m, [k]: v });
  const setAsset = (i: number, a: Partial<AssetRow>) => set("assets")(m.assets.map((x, k) => (k === i ? { ...x, ...a } : x)));
  return (
    <div className="space-y-2 rounded-md border bg-muted/20 p-2" data-testid="pkg-f-method">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-xs text-muted-foreground">{n + 1}.</span>
        <div className="w-32"><Select value={m.type} onChange={(v) => set("type")(v as MethodType)} options={[...METHOD_TYPES]} /></div>
        <div className="min-w-[10rem] flex-1"><Text value={m.label} onChange={set("label")} placeholder="Label, e.g. GPU (docker)" /></div>
        {isHostRun(m.type) && <Badge variant="warning">runs on the host, not sandboxed</Badge>}
        <Button size="sm" variant="ghost" onClick={onUp} aria-label="Move up">↑</Button>
        <Button size="sm" variant="ghost" onClick={onDown} aria-label="Move down">↓</Button>
        <Button size="sm" variant="ghost" onClick={onRemove} aria-label="Remove method"><X /></Button>
      </div>
      <Grid>
        <Field label="Platforms" hint="os/arch, comma separated: linux/amd64, windows/amd64, macos/arm64, *"><Text value={m.platforms} onChange={set("platforms")} mono /></Field>
        <Field label="GPU stacks"><Ticks all={GPU_KINDS} value={m.gpu} onChange={set("gpu")} /></Field>
        {m.type === "docker" && <Field label="Image" hint="pinned: name:tag@sha256:… (store packages need the digest)" wide><Text value={m.image} onChange={set("image")} mono testid="pkg-f-image" /></Field>}
        {m.type === "compose" && <Field label="Compose file" hint="templated: {{port}}, {{model_dir}}, {{gpu_uuid}}…" wide><ListText value={m.compose} onChange={set("compose")} rows={8} /></Field>}
        {(m.type === "pip" || m.type === "uv" || m.type === "conda") && (
          <>
            <Field label="Python"><Text value={m.python} onChange={set("python")} mono placeholder="3.12" /></Field>
            <Field label="Package" hint="a pinned spec, name==1.2.3"><Text value={m.pkg} onChange={set("pkg")} mono /></Field>
            <Field label="Requirements" hint="one per line; store packages pin with == (and --hash)" wide><ListText value={m.requirements} onChange={set("requirements")} /></Field>
          </>
        )}
        {m.type === "git" && (
          <>
            <Field label="Repo"><Text value={m.repo} onChange={set("repo")} mono placeholder="https://github.com/…" /></Field>
            <Field label="Commit" hint="the full 40-character hash"><Text value={m.commit} onChange={set("commit")} mono /></Field>
          </>
        )}
        {m.type === "existing" && (
          <>
            <Field label="Command" hint="found on PATH"><Text value={m.detectCommand} onChange={set("detectCommand")} mono placeholder="llama-server" /></Field>
            <Field label="Or at these paths" hint="one per line"><ListText value={m.detectPaths} onChange={set("detectPaths")} rows={2} /></Field>
            <Field label="Version args"><Text value={m.detectVersionArgs} onChange={set("detectVersionArgs")} mono /></Field>
            <Field label="Version regex"><Text value={m.detectVersionRegex} onChange={set("detectVersionRegex")} mono placeholder="version: (\d+)" /></Field>
            <Field label="Or a running URL" hint="nothing to start: the engine is already up"><Text value={m.detectUrl} onChange={set("detectUrl")} mono /></Field>
          </>
        )}
        <Field label="Run" hint="the command, one argument per line" wide><ListText value={m.run} onChange={set("run")} rows={4} placeholder={"{{install}}/server\n--port\n{{port}}"} /></Field>
        <Field label="Env" hint="KEY=value, one per line"><ListText value={m.env} onChange={set("env")} rows={2} /></Field>
        <Field label="Mounts" hint="source:target[:ro], one per line"><ListText value={m.mounts} onChange={set("mounts")} rows={2} /></Field>
        <Field label="Ports" hint="id[:container port], e.g. http:8188"><Text value={m.ports} onChange={set("ports")} mono /></Field>
        <div className="grid grid-cols-2 gap-2">
          <Field label="Health path"><Text value={m.healthHttp} onChange={set("healthHttp")} mono placeholder="/health" /></Field>
          <Field label="Start time (s)"><Text value={m.healthStart} onChange={set("healthStart")} mono /></Field>
        </div>
      </Grid>
      {m.type === "binary" && (
        <div className="space-y-2">
          <div className="text-xs font-medium">Assets (one download per platform, each with its sha256)</div>
          {m.assets.map((a, i) => (
            <div key={i} className="grid items-end gap-2 sm:grid-cols-[8rem_2fr_1fr_6rem_auto]">
              <Field label="Platform"><Text value={a.platform} onChange={(v) => setAsset(i, { platform: v })} mono /></Field>
              <Field label="URL"><Text value={a.url} onChange={(v) => setAsset(i, { url: v })} mono /></Field>
              <Field label="sha256"><Text value={a.sha256} onChange={(v) => setAsset(i, { sha256: v })} mono /></Field>
              <Field label="Unpack"><Select value={a.unpack} onChange={(v) => setAsset(i, { unpack: v })} options={[["", "(by name)"], ...UNPACK]} /></Field>
              <Button size="sm" variant="ghost" aria-label="Remove asset" onClick={() => set("assets")(m.assets.filter((_, k) => k !== i))}><Trash2 /></Button>
            </div>
          ))}
          <Button size="sm" variant="outline" onClick={() => set("assets")([...m.assets, emptyAsset()])}><Plus /> Add asset</Button>
        </div>
      )}
    </div>
  );
}

// ── 4. review ────────────────────────────────────────────────────────────────────────────────

/** The validator's report: errors and warnings by path. */
export function ReportView({ report, busy }: { report?: Report | null; busy?: boolean }) {
  if (!report) return busy ? <div className="flex items-center gap-1.5 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Checking…</div> : null;
  const issues = sortIssues(report.issues);
  return (
    <div className="space-y-1.5 text-xs" data-testid="pkg-report">
      <div className="flex flex-wrap items-center gap-2">
        {report.ok ? <Badge variant="success">valid</Badge> : <Badge variant="destructive">not valid</Badge>}
        <span className="text-muted-foreground">checked as {report.mode === "store" ? "a store package" : "a private package"} · {report.files.length} file{report.files.length === 1 ? "" : "s"}, {fmtBytes(report.total_bytes)}</span>
        {busy && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
      </div>
      {issues.length === 0 ? <div className="text-muted-foreground">No problems.</div> : (
        <ul className="space-y-0.5">
          {issues.map((i, n) => (
            <li key={n} className={cn("flex gap-1.5", i.level === "error" ? "text-destructive" : "text-warning")} data-testid={`pkg-issue-${i.level}`}>
              <AlertTriangle className="mt-0.5 size-3 shrink-0" />
              <span><span className="font-mono">{i.at || "manifest"}</span>: {i.message}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export const fmtBytes = (n: number) => (n < 1024 ? `${n} B` : n < 1024 * 1024 ? `${(n / 1024).toFixed(1)} KiB` : `${(n / 1024 / 1024).toFixed(1)} MiB`);

function useDebounced<T>(v: T, ms: number): T {
  const [d, setD] = useState(v);
  useEffect(() => {
    const t = setTimeout(() => setD(v), ms);
    return () => clearTimeout(t);
  }, [v, ms]);
  return d;
}

function ReviewStep({ json, setJson, jsonErr, files, setFiles, mode, setMode, onBack, onNext }: {
  json: string; setJson: (t: string) => void; jsonErr: string | null; files: Record<string, string>; setFiles: (f: Record<string, string>) => void;
  mode: "private" | "store"; setMode: (m: "private" | "store") => void; onBack: () => void; onNext: () => void;
}) {
  const key = useDebounced(useMemo(() => ({ json, files, mode }), [json, files, mode]), 600);
  const check = useQuery({
    queryKey: ["engine-store-validate", key],
    queryFn: () => engineStoreApi.validate(key.json, key.files, key.mode),
    refetchOnWindowFocus: false,
    retry: false,
    placeholderData: (prev) => prev,
  });
  const [newFile, setNewFile] = useState("");
  const parsed = parseManifest(json);
  return (
    <div className="space-y-3" data-testid="pkg-review">
      <div className="flex flex-wrap items-center gap-3 text-xs">
        <span className="font-medium">Check as</span>
        <label className="flex items-center gap-1"><input type="radio" checked={mode === "private"} onChange={() => setMode("private")} /> private (My packages)</label>
        <label className="flex items-center gap-1"><input type="radio" checked={mode === "store"} onChange={() => setMode("store")} data-testid="pkg-mode-store" /> store (what publishing needs: every artifact pinned, metadata only)</label>
      </div>
      <div className="grid gap-3 lg:grid-cols-2">
        <Field label="abhoth-engine.json" hint="edit freely: Back to the form reads it again">
          <textarea className={cn(area, "min-h-[480px]")} value={json} onChange={(e) => setJson(e.target.value)} spellCheck={false} data-testid="pkg-review-json" />
        </Field>
        <div className="space-y-2">
          <div className="text-xs font-medium">Templates and files</div>
          {Object.keys(files).length === 0 && <div className="text-xs text-muted-foreground">None. A method that lists files (a compose file, a small server script) needs them here.</div>}
          {Object.entries(files).map(([path, text]) => (
            <div key={path} className="space-y-1">
              <div className="flex items-center justify-between text-xs"><span className="font-mono">{path}</span>
                <Button size="sm" variant="ghost" aria-label={`Remove ${path}`} onClick={() => { const f = { ...files }; delete f[path]; setFiles(f); }}><Trash2 /></Button>
              </div>
              <textarea className={cn(area, "min-h-[160px]")} value={text} onChange={(e) => setFiles({ ...files, [path]: e.target.value })} spellCheck={false} />
            </div>
          ))}
          <div className="flex gap-2">
            <Text value={newFile} onChange={setNewFile} mono placeholder="server.py" />
            <Button size="sm" variant="outline" disabled={!newFile.trim() || newFile.trim() in files} onClick={() => { setFiles({ ...files, [newFile.trim()]: "" }); setNewFile(""); }}><Plus /> Add file</Button>
          </div>
        </div>
      </div>
      <div className="rounded-md border p-3">
        {parsed.error ? <div className="text-xs text-destructive">The JSON does not parse: {parsed.error}</div>
          : check.error ? <div className="text-xs text-destructive">{errText(check.error)}</div>
          : <ReportView report={check.data} busy={check.isFetching} />}
      </div>
      {jsonErr && <div className="text-xs text-destructive">{jsonErr}</div>}
      <div className="flex justify-between">
        <Button size="sm" variant="ghost" onClick={onBack}><ArrowLeft /> Back to the form</Button>
        <Button size="sm" onClick={onNext} disabled={!!parsed.error} data-testid="pkg-review-next">Next <ArrowRight /></Button>
      </div>
    </div>
  );
}

// ── 5. save ──────────────────────────────────────────────────────────────────────────────────

function SaveStep({ json, files, onInstall, onEdit }: { json: string; files: Record<string, string>; onInstall: (id: string) => void; onEdit: () => void }) {
  const qc = useQueryClient();
  const id = parseManifest(json).manifest?.id ?? "";
  const [saved, setSaved] = useState<string | null>(null);
  const [report, setReport] = useState<Report | null>(null);
  const [copied, setCopied] = useState(false);
  const save = useMutation({
    mutationFn: async (then: "save" | "install") => {
      setReport(null);
      try {
        const r = await engineStoreApi.saveMy(id, json, files);
        return { then, report: r.report };
      } catch (e) {
        if (e instanceof RouterApiError && e.data?.report) setReport(e.data.report as Report);
        throw e;
      }
    },
    onSuccess: ({ then, report }) => {
      setSaved(id);
      setReport(report);
      void qc.invalidateQueries({ queryKey: ["engine-store-packages"] });
      void qc.invalidateQueries({ queryKey: ["engine-store-my"] });
      if (then === "install") onInstall(id);
    },
  });
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(json);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* no clipboard (an insecure origin): the JSON is on the Review step */
    }
  };
  return (
    <div className="space-y-3 rounded-md border p-3" data-testid="pkg-save">
      <p className="text-xs text-muted-foreground">
        My packages live on the router (<span className="font-mono">packages/{id || "<id>"}</span> in its state folder). Saving checks the package again; installing then works like any store package.
      </p>
      {!id && <div className="text-xs text-destructive">The manifest has no id: go back to Review.</div>}
      <div className="flex flex-wrap gap-2">
        <Button size="sm" onClick={() => save.mutate("save")} disabled={!id || save.isPending} data-testid="pkg-save-my">{save.isPending ? <Loader2 className="animate-spin" /> : <Check />} Save to My packages</Button>
        <Button size="sm" variant="outline" onClick={() => save.mutate("install")} disabled={!id || save.isPending} data-testid="pkg-save-install"><Package /> Install now</Button>
        <Button size="sm" variant="outline" asChild>
          <a href={saved === id ? engineStoreApi.exportUrl(id) : undefined} download={`${id}.zip`} aria-disabled={saved !== id} className={cn(saved !== id && "pointer-events-none opacity-50")} data-testid="pkg-export"><Download /> Export (zip)</a>
        </Button>
        <Button size="sm" variant="ghost" onClick={() => void copy()} data-testid="pkg-copy-json"><Copy /> {copied ? "Copied" : "Copy JSON"}</Button>
      </div>
      {saved !== id && <p className="text-[11px] text-muted-foreground">Export downloads the saved package: save it first.</p>}
      {saved === id && !save.isPending && <div className="text-xs text-success" data-testid="pkg-saved">Saved to My packages.</div>}
      {save.error && <div className="text-xs text-destructive" data-testid="pkg-save-error">Not saved: {errText(save.error)}</div>}
      {report && <ReportView report={report} />}
      <Button size="sm" variant="ghost" onClick={onEdit}><ArrowLeft /> Back to Review</Button>
    </div>
  );
}
