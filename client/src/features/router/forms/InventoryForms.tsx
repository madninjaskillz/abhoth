// The router inventory (D152): servers -> cards -> which engines may run each model type, the
// engine catalog with capabilities (built in, probed, overridden), the models with the engines
// that can run them (computed, with reasons) and Hugging Face search and downloads. Every change
// goes through the same edit -> review -> audited save as the other forms.
import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle, ArrowDown, ArrowUp, Check, Cpu, Download, ExternalLink, HardDrive, Loader2, Lock, Pause, Play, Plus, RefreshCw, Search, Server, Trash2, X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { routerApi } from "../routerApi";
import { obj, type FormEdit, type Obj } from "./formModel";
import { Check2, Field, FormDialog, Grid, ReviewDialog, Select, Text, errText } from "./formKit";
import { useEditFlow } from "./SetupForms";
import {
  KIND_LABELS, cardEnginesEdit, enginesFor, enginesForKind, fitSummary, fmtGib, modelEngineEdits, modelIdFromRepo, moved, newCardEdits, newModelEdits,
  newServerEdits, nextCardId, nextStep, pct, placementLine, repoDir, toggled, validModelDir, validateServerId,
  type DetectedGpu, type DownloadJob, type HfChoice, type HfDetails, type InvCard, type InvEngine, type InvModel, type InvServer, type Inventory, type ModelKind,
} from "./inventoryModel";

const KEY = ["router-inventory"];
type Submit = (edits: FormEdit[], done?: () => void) => void;

export function InventoryForms() {
  const inv = useQuery({ queryKey: KEY, queryFn: routerApi.inventory, refetchOnWindowFocus: false });
  const doc = useQuery({ queryKey: ["router-config-json"], queryFn: routerApi.configJson, refetchOnWindowFocus: false });
  const flow = useEditFlow();
  const [tab, setTab] = useState("servers");
  if (inv.isLoading || doc.isLoading) return <Loader2 className="m-4 size-4 animate-spin text-muted-foreground" />;
  if (inv.isError) return <div className="p-4 text-xs text-destructive" data-testid="inventory-error">{errText(inv.error)}</div>;
  if (doc.isError || !doc.data?.data) return <div className="p-4 text-xs text-destructive">router.toml does not load: fix it in Advanced.</div>;
  const i = inv.data!;
  const raw = doc.data.data as Obj;
  const submit: Submit = (edits, done) => flow.prepare.mutate({ edits, rev: doc.data!.rev, done });
  const step = nextStep(i);
  return (
    <div className="min-h-0 flex-1 overflow-y-auto" data-testid="router-inventory">
      {!i.migrated && (
        <Banner tone="warn">This router.toml predates the inventory: it is read the old way (language models on any engine with a port). The router converts it at its next start, with a backup.</Banner>
      )}
      {i.restartNeeded && (
        <Banner tone="warn" testid="inventory-restart">New models or routes were saved: restart the router to switch to them on demand (the model list and placement shown here are already current).</Banner>
      )}
      {step && <GuideBar step={step} go={setTab} />}
      <Tabs value={tab} onValueChange={setTab} className="flex flex-col">
        <div className="overflow-x-auto border-b px-4 py-2">
          <TabsList>
            <TabsTrigger value="servers" data-testid="inv-servers"><Server className="mr-1 size-3.5" />Servers & cards</TabsTrigger>
            <TabsTrigger value="engines" data-testid="inv-engines"><Cpu className="mr-1 size-3.5" />Engines</TabsTrigger>
            <TabsTrigger value="models" data-testid="inv-models"><HardDrive className="mr-1 size-3.5" />Models</TabsTrigger>
            <TabsTrigger value="downloads" data-testid="inv-downloads"><Download className="mr-1 size-3.5" />Downloads</TabsTrigger>
          </TabsList>
        </div>
        <div className="p-3 sm:p-4">
          {flow.error && !flow.pending && <div className="mb-2 text-xs text-destructive" data-testid="inventory-flow-error">{flow.error}</div>}
          <TabsContent value="servers"><ServersPane inv={i} submit={submit} busy={flow.prepare.isPending} /></TabsContent>
          <TabsContent value="engines"><EnginesPane inv={i} raw={raw} submit={submit} busy={flow.prepare.isPending} /></TabsContent>
          <TabsContent value="models"><ModelsPane inv={i} submit={submit} busy={flow.prepare.isPending} onDownloads={() => setTab("downloads")} /></TabsContent>
          <TabsContent value="downloads"><DownloadsPane /></TabsContent>
        </div>
      </Tabs>
      {flow.pending && (
        <ReviewDialog file="router.toml" review={flow.pending.review} busy={flow.save.isPending} error={flow.error} onClose={() => flow.setPending(null)} onSave={(reason) => flow.save.mutate({ reason })} />
      )}
    </div>
  );
}

function Banner({ tone, children, testid }: { tone: "warn" | "info"; children: React.ReactNode; testid?: string }) {
  return (
    <div className={cn("flex items-start gap-2 border-b px-4 py-2 text-xs", tone === "warn" ? "bg-warning/10" : "bg-muted/40")} data-testid={testid}>
      <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-warning" />
      <span>{children}</span>
    </div>
  );
}

const STEPS = [
  ["server", "Add a server", "servers"],
  ["cards", "Add its GPUs", "servers"],
  ["engines", "Add an engine", "engines"],
  ["models", "Add a model", "models"],
] as const;

function GuideBar({ step, go }: { step: string; go: (t: string) => void }) {
  const at = STEPS.findIndex((s) => s[0] === step);
  return (
    <div className="flex flex-wrap items-center gap-2 border-b bg-muted/30 px-4 py-2 text-xs" data-testid="inventory-guide">
      <span className="font-medium">Set up:</span>
      {STEPS.map(([id, label, tab], n) => (
        <button key={id} onClick={() => go(tab)} className={cn("flex items-center gap-1 rounded-full border px-2 py-0.5", n < at && "border-success/40 text-success", n === at && "border-primary bg-primary/10 text-primary", n > at && "text-muted-foreground")}>
          {n < at ? <Check className="size-3" /> : <span className="font-mono">{n + 1}</span>} {label}
        </button>
      ))}
    </div>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="rounded-md border border-dashed p-4 text-center text-xs text-muted-foreground">{children}</div>;
}

// ── servers & cards ──────────────────────────────────────────────────────────────────────────

function ServersPane({ inv, submit, busy }: { inv: Inventory; submit: Submit; busy: boolean }) {
  const [adding, setAdding] = useState(false);
  const [detect, setDetect] = useState<InvServer | null>(null);
  const [engines, setEngines] = useState<InvCard | null>(null);
  const [folder, setFolder] = useState<InvServer | null>(null);
  return (
    <div className="space-y-3" data-testid="inv-servers-pane">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h3 className="text-sm font-semibold">Servers and their GPUs</h3>
          <p className="text-xs text-muted-foreground">Each server holds model files in its model folder; each card says which engines may run each kind of model on it.</p>
        </div>
        <Button size="sm" variant="outline" onClick={() => setAdding(true)} data-testid="inv-server-add"><Plus /> Add server</Button>
      </div>
      {inv.servers.length === 0 && <Empty>No servers yet. Add the machine with your GPUs: the router reaches it over SSH (or runs commands locally).</Empty>}
      {inv.servers.map((s) => (
        <div key={s.id} className="rounded-md border" data-testid="inv-server" data-server={s.id}>
          <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
            <Server className="size-4 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium">{s.title || s.id} <span className="font-mono text-xs text-muted-foreground">{s.id}</span></div>
              <div className="truncate text-xs text-muted-foreground">
                {s.exec === "local" ? "commands run locally" : `over SSH to ${s.host}`} · model folder{" "}
                {s.modelDir ? <span className="font-mono">{s.modelDir}</span> : <span className="text-warning">not set (downloads need one)</span>}
              </div>
            </div>
            <div className="flex gap-1">
              <Button size="sm" variant="ghost" onClick={() => setFolder(s)} data-testid="inv-folder">Model folder</Button>
              <Button size="sm" variant="outline" onClick={() => setDetect(s)} data-testid="inv-detect"><RefreshCw /> Find GPUs</Button>
            </div>
          </div>
          <div className="divide-y">
            {s.cards.length === 0 && <div className="p-3 text-xs text-muted-foreground">No cards yet: <b>Find GPUs</b> asks the server (nvidia-smi) and adds them in one go.</div>}
            {s.cards.map((c) => <CardLine key={c.id} c={c} inv={inv} onEngines={() => setEngines(c)} />)}
          </div>
        </div>
      ))}
      {adding && <AddServerDialog inv={inv} busy={busy} onClose={() => setAdding(false)} submit={submit} />}
      {detect && <DetectDialog server={detect} inv={inv} busy={busy} onClose={() => setDetect(null)} submit={submit} />}
      {engines && <CardEnginesDialog card={engines} inv={inv} busy={busy} onClose={() => setEngines(null)} submit={submit} />}
      {folder && <FolderDialog server={folder} busy={busy} onClose={() => setFolder(null)} submit={submit} />}
    </div>
  );
}

function CardLine({ c, inv, onEngines }: { c: InvCard; inv: Inventory; onEngines: () => void }) {
  const kinds = Object.entries(c.engines).filter(([, v]) => v.length > 0);
  return (
    <div className="flex flex-wrap items-center gap-2 px-3 py-2" data-testid="inv-card" data-card={c.id}>
      <span className="flex size-7 items-center justify-center rounded bg-muted font-mono text-xs font-semibold">{c.id}</span>
      <div className="min-w-0 flex-1">
        <div className="text-xs font-medium">{c.label || c.gpuUuid} <span className="text-muted-foreground">· {fmtGib(c.vramGib)}</span>{!c.managed && <Badge variant="outline" className="ml-1">not managed</Badge>}{c.onDemand && <Badge variant="secondary" className="ml-1">on demand</Badge>}</div>
        <div className="mt-0.5 flex flex-wrap gap-1">
          {kinds.length === 0 && <span className="text-[11px] text-muted-foreground">runs nothing yet</span>}
          {kinds.map(([k, v]) => (
            <span key={k} className="text-[11px]"><span className="text-muted-foreground">{KIND_LABELS[k as ModelKind] ?? k}:</span> {v.map((e) => inv.engines.find((x) => x.id === e)?.title || e).join(", ")}</span>
          ))}
          {!c.explicit && <Badge variant="outline">default rule</Badge>}
        </div>
      </div>
      <Button size="sm" variant="ghost" onClick={onEngines} data-testid="inv-card-engines">Engines…</Button>
    </div>
  );
}

function AddServerDialog({ inv, busy, onClose, submit }: { inv: Inventory; busy: boolean; onClose: () => void; submit: Submit }) {
  const [f, setF] = useState({ id: "", title: "", host: "", exec: "ssh" as "ssh" | "local", user: "abhoth-exec", key: "", hostKey: "", modelDir: "/srv/models" });
  const [err, setErr] = useState<string | null>(null);
  const set = (k: keyof typeof f) => (v: string) => setF({ ...f, [k]: v });
  const go = () => {
    const e = validateServerId(f.id, inv.servers.map((s) => s.id)) ?? validModelDir(f.modelDir) ?? (f.exec === "ssh" && !f.host ? "the host to reach it at" : null);
    if (e) return setErr(e);
    submit(newServerEdits(f.id, f), onClose);
  };
  return (
    <FormDialog title="Add a server" description="A machine with GPUs. Next, Find GPUs adds its cards." onClose={onClose} onSubmit={go} busy={busy} error={err} testid="inv-server-dialog">
      <Grid>
        <Field label="Name" hint="short, lower case: used in the config"><Text value={f.id} onChange={set("id")} placeholder="gpu1" mono testid="inv-server-id" /></Field>
        <Field label="Title"><Text value={f.title} onChange={set("title")} placeholder="GPU box" /></Field>
        <Field label="How the router reaches it"><Select value={f.exec} onChange={(v) => setF({ ...f, exec: v as "ssh" | "local" })} options={[["ssh", "SSH (a forced-command account)"], ["local", "Locally (same machine)"]]} /></Field>
        <Field label="Model folder" hint="where model files live and downloads go"><Text value={f.modelDir} onChange={set("modelDir")} mono testid="inv-server-dir" /></Field>
        {f.exec === "ssh" && (
          <>
            <Field label="Host"><Text value={f.host} onChange={set("host")} placeholder="gpu-host.lan" mono testid="inv-server-host" /></Field>
            <Field label="SSH user"><Text value={f.user} onChange={set("user")} mono /></Field>
            <Field label="SSH key" hint="a secret reference, e.g. secret:ssh_gpu"><Text value={f.key} onChange={set("key")} placeholder="secret:ssh_gpu" mono /></Field>
            <Field label="Host key" hint="SHA256:… from ssh-keyscan"><Text value={f.hostKey} onChange={set("hostKey")} placeholder="SHA256:…" mono /></Field>
          </>
        )}
      </Grid>
    </FormDialog>
  );
}

function FolderDialog({ server, busy, onClose, submit }: { server: InvServer; busy: boolean; onClose: () => void; submit: Submit }) {
  const [dir, setDir] = useState(server.modelDir ?? "");
  const err = validModelDir(dir);
  return (
    <FormDialog title={`Model folder on ${server.title || server.id}`} description="Downloads go to <folder>/<owner>/<name>/ on this server." onClose={onClose} busy={busy} error={err}
      onSubmit={() => !err && submit([{ path: ["servers", server.id, "model_dir"], value: dir || null }], onClose)}>
      <Field label="Folder"><Text value={dir} onChange={setDir} placeholder="/srv/models" mono testid="inv-folder-input" /></Field>
    </FormDialog>
  );
}

function DetectDialog({ server, inv, busy, onClose, submit }: { server: InvServer; inv: Inventory; busy: boolean; onClose: () => void; submit: Submit }) {
  const q = useQuery({ queryKey: ["router-detect", server.id], queryFn: () => routerApi.detect(server.id), retry: false });
  const taken = inv.servers.flatMap((s) => s.cards.map((c) => c.id));
  const fresh = (q.data?.gpus ?? []).filter((g) => !g.card);
  const [pick, setPick] = useState<Record<string, boolean>>({});
  const chosen: DetectedGpu[] = fresh.filter((g) => pick[g.uuid] ?? true);
  const llm = enginesForKind(inv, "llm").map((e) => e.id);
  const add = () => {
    const ids = [...taken];
    const edits = chosen.flatMap((g) => {
      const id = nextCardId(ids);
      ids.push(id);
      return newCardEdits(id, server.id, g, { llm });
    });
    submit(edits, onClose);
  };
  return (
    <FormDialog title={`GPUs on ${server.title || server.id}`} description="What nvidia-smi reports on the server. New ones become cards (by UUID, so a reboot's reordering is harmless)." onClose={onClose}
      onSubmit={add} busy={busy || q.isLoading} error={q.isError ? errText(q.error) : chosen.length === 0 && q.data ? "every GPU here is already a card" : null} submitLabel={`Add ${chosen.length} card${chosen.length === 1 ? "" : "s"}`} testid="inv-detect-dialog">
      {q.isLoading && <div className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Asking the server…</div>}
      {(q.data?.gpus ?? []).map((g) => (
        <div key={g.uuid} className="flex items-center gap-2 rounded border px-2 py-1.5 text-xs" data-testid="inv-gpu">
          {g.card ? <Badge variant="success">card {g.card}</Badge> : <input type="checkbox" checked={pick[g.uuid] ?? true} onChange={(e) => setPick({ ...pick, [g.uuid]: e.target.checked })} />}
          <span className="font-medium">{g.name}</span>
          <span className="text-muted-foreground">{fmtGib(g.vramGib)}</span>
          <span className="ml-auto hidden truncate font-mono text-[10px] text-muted-foreground sm:inline">{g.uuid}</span>
        </div>
      ))}
      {chosen.length > 0 && <p className="text-[11px] text-muted-foreground">New cards may run language models on {llm.length ? llm.join(", ") : "no engine yet"}; change it per card under Engines….</p>}
    </FormDialog>
  );
}

function CardEnginesDialog({ card, inv, busy, onClose, submit }: { card: InvCard; inv: Inventory; busy: boolean; onClose: () => void; submit: Submit }) {
  const [map, setMap] = useState<Record<string, string[]>>(() => ({ ...card.engines }));
  const kinds = inv.kinds.map((k) => k.id).filter((k) => enginesForKind(inv, k).length > 0 || (map[k] ?? []).length > 0);
  return (
    <FormDialog title={`What card ${card.id} may run`} description="Tick the engines allowed on this card for each kind of model. The router places a model only on a card that allows an engine able to run it." onClose={onClose} busy={busy}
      onSubmit={() => submit([cardEnginesEdit(card.id, map)], onClose)} testid="inv-card-engines-dialog">
      {kinds.length === 0 && <Empty>No engines yet: add one under Engines.</Empty>}
      {kinds.map((k) => (
        <div key={k} className="space-y-1 rounded-md border p-2" data-testid="inv-kind" data-kind={k}>
          <div className="text-xs font-medium">{KIND_LABELS[k]}</div>
          <div className="flex flex-wrap gap-x-4 gap-y-1">
            {enginesForKind(inv, k).map((e) => (
              <Check2 key={e.id} label={e.title || e.id} checked={(map[k] ?? []).includes(e.id) || (map[k] ?? []).includes("*")} onChange={(on) => setMap({ ...map, [k]: toggled(map[k] ?? [], e.id, on) })} testid={`inv-tick-${k}-${e.id}`} />
            ))}
          </div>
          {!inv.engines.some((e) => e.cards.includes(card.id)) && <div className="text-[11px] text-warning">No engine has a port for this card yet.</div>}
        </div>
      ))}
    </FormDialog>
  );
}

// ── engines ──────────────────────────────────────────────────────────────────────────────────

function EnginesPane({ inv, raw, submit, busy }: { inv: Inventory; raw: Obj; submit: Submit; busy: boolean }) {
  const [show, setShow] = useState<InvEngine | null>(null);
  const [fork, setFork] = useState<InvEngine | null>(null);
  return (
    <div className="space-y-3" data-testid="inv-engines-pane">
      <div>
        <h3 className="text-sm font-semibold">Engines</h3>
        <p className="text-xs text-muted-foreground">What each engine can run comes from the built-in catalog, what the running engine reports about itself, and your overrides. Models never list engines by hand: a new engine runs every model it matches.</p>
      </div>
      {inv.engines.length === 0 && <Empty>No engines yet. Add one in Servers & layouts → Engines (image, ports, profiles), then tune what it runs here.</Empty>}
      <div className="grid gap-2 md:grid-cols-2">
        {inv.engines.map((e) => (
          <button key={e.id} onClick={() => setShow(e)} className="rounded-md border p-3 text-left hover:bg-accent/40" data-testid="inv-engine" data-engine={e.id}>
            <div className="flex items-center gap-2">
              <Cpu className="size-4 text-muted-foreground" />
              <span className="text-sm font-medium">{e.title || e.id}</span>
              <span className="font-mono text-[11px] text-muted-foreground">{e.id}</span>
              {e.caps.base && e.caps.base !== e.id && <Badge variant="outline">like {e.caps.base}</Badge>}
            </div>
            <div className="mt-1 flex flex-wrap gap-1">
              {e.caps.kinds.map((k) => <Badge key={k} variant="secondary">{KIND_LABELS[k]}</Badge>)}
              {e.caps.formats.map((f) => <Badge key={f} variant="outline">{f}</Badge>)}
            </div>
            <div className="mt-1 text-[11px] text-muted-foreground">
              {e.usedBy.length ? `runs ${e.usedBy.length} model${e.usedBy.length === 1 ? "" : "s"}` : "runs no model yet"} · cards {e.cards.join(", ") || "none"} · {e.probe ? `probed${e.probe.version ? ` (${e.probe.version})` : ""}` : e.probeHow === "none" ? "not probed" : "not probed yet"}
            </div>
          </button>
        ))}
      </div>
      {show && <EngineCapsDialog e={show} inv={inv} busy={busy} onClose={() => setShow(null)} submit={submit} onFork={() => { setFork(show); setShow(null); }} />}
      {fork && <ForkDialog src={fork} raw={raw} inv={inv} busy={busy} onClose={() => setFork(null)} submit={submit} />}
    </div>
  );
}

const lines = (s: string) => s.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean);

function EngineCapsDialog({ e, inv, busy, onClose, submit, onFork }: { e: InvEngine; inv: Inventory; busy: boolean; onClose: () => void; submit: Submit; onFork: () => void }) {
  const o = obj(e.overrides as Obj | undefined);
  const add = obj(o.add as Obj | undefined);
  const rem = obj(o.remove as Obj | undefined);
  const arr = (v: unknown) => (Array.isArray(v) ? (v as string[]).join(", ") : "");
  const [f, setF] = useState({
    inherits: typeof o.inherits === "string" ? o.inherits : "",
    addArchs: arr(add.archs), addFormats: arr(add.formats), addKinds: arr(add.kinds), addQuants: arr(add.quants),
    remArchs: arr(rem.archs), remFormats: arr(rem.formats), remQuants: arr(rem.quants),
    probe: o.probe !== false,
  });
  const set = (k: keyof typeof f) => (v: string) => setF({ ...f, [k]: v });
  const save = () => {
    const delta = (x: Record<string, string>) => {
      const out: Record<string, string[]> = {};
      for (const [k, v] of Object.entries(x)) if (lines(v).length) out[k] = lines(v);
      return Object.keys(out).length ? out : undefined;
    };
    const caps: Record<string, unknown> = { ...o };
    caps.inherits = f.inherits || undefined;
    caps.add = delta({ archs: f.addArchs, formats: f.addFormats, kinds: f.addKinds, quants: f.addQuants });
    caps.remove = delta({ archs: f.remArchs, formats: f.remFormats, quants: f.remQuants });
    caps.probe = f.probe ? undefined : false;
    for (const k of Object.keys(caps)) if (caps[k] === undefined) delete caps[k];
    submit([{ path: ["engines", e.id, "caps"], value: Object.keys(caps).length ? (caps as FormEdit["value"]) : null }], onClose);
  };
  const kinds = inv.catalog.map((k) => k.id);
  return (
    <FormDialog title={`${e.title || e.id}: what it runs`} description="The effective capabilities and where each comes from. Overrides add or remove on top of the catalog and the probe." onClose={onClose} onSubmit={save} busy={busy} submitLabel="Review overrides" testid="inv-engine-dialog">
      <div className="grid gap-2 rounded-md border bg-muted/30 p-2 text-xs sm:grid-cols-2" data-testid="inv-engine-caps">
        <CapRow label="Kinds of model" v={e.caps.kinds.map((k) => KIND_LABELS[k])} />
        <CapRow label="File formats" v={e.caps.formats} none="none of its own (it brings its model)" />
        <CapRow label="Architectures" v={e.caps.archs} none="any" />
        <CapRow label="Quantisations" v={e.caps.quants} none="any" />
        {e.caps.maxContext && <CapRow label="Longest context" v={[String(e.caps.maxContext)]} />}
        <CapRow label="From" v={e.caps.sources} />
      </div>
      <div className="text-xs">
        <span className="font-medium">Live probe: </span>
        {e.probe ? (
          <span data-testid="inv-engine-probe">version {e.probe.version ?? "?"}{e.probe.archsSeen?.length ? `, serving ${e.probe.archsSeen.join(", ")}` : ""}{e.probe.ctx ? `, context ${e.probe.ctx}` : ""} · {new Date(e.probe.at * 1000).toLocaleString()}</span>
        ) : (
          <span className="text-muted-foreground">{e.probeHow === "none" ? "this engine has nothing to ask" : "not yet: the router asks an idle, running engine (never loads or switches anything)"}</span>
        )}
      </div>
      <div className="text-xs"><span className="font-medium">Used by: </span>{e.usedBy.length ? e.usedBy.join(", ") : <span className="text-muted-foreground">no model</span>}</div>
      <details className="rounded-md border p-2" open={Object.keys(o).length > 0}>
        <summary className="cursor-pointer text-xs font-medium">Overrides</summary>
        <div className="space-y-3 pt-2">
          <Field label="Works like (inherits)" hint="a catalog kind or another engine: start from its capabilities">
            <Select value={f.inherits} onChange={set("inherits")} options={[["", "(its own catalog entry)"], ...kinds.map((k) => [k, k] as [string, string]), ...inv.engines.filter((x) => x.id !== e.id).map((x) => [x.id, `engine ${x.id}`] as [string, string])]} testid="inv-caps-inherits" />
          </Field>
          <Grid>
            <Field label="Also runs architectures"><Text value={f.addArchs} onChange={set("addArchs")} placeholder="qwen3next, gemma4" mono testid="inv-caps-add-archs" /></Field>
            <Field label="Never architectures"><Text value={f.remArchs} onChange={set("remArchs")} mono /></Field>
            <Field label="Also formats"><Text value={f.addFormats} onChange={set("addFormats")} placeholder="gguf" mono /></Field>
            <Field label="Never formats"><Text value={f.remFormats} onChange={set("remFormats")} mono /></Field>
            <Field label="Also kinds of model"><Text value={f.addKinds} onChange={set("addKinds")} placeholder="embedding" mono /></Field>
            <Field label="Never quantisations"><Text value={f.remQuants} onChange={set("remQuants")} placeholder="IQ1_S" mono /></Field>
          </Grid>
          <Check2 label="Ask the running engine what it is (probe)" checked={f.probe} onChange={(v) => setF({ ...f, probe: v })} />
        </div>
      </details>
      <Button size="sm" variant="outline" onClick={onFork} data-testid="inv-engine-fork"><Plus /> Add a fork of this engine</Button>
    </FormDialog>
  );
}

function CapRow({ label, v, none }: { label: string; v: string[]; none?: string }) {
  return (
    <div>
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div className="flex flex-wrap gap-1">{v.length ? v.map((x) => <Badge key={x} variant="outline">{x}</Badge>) : <span className="text-[11px]">{none ?? "none"}</span>}</div>
    </div>
  );
}

/** A copy of an engine with its own image, inheriting what the original runs. */
function ForkDialog({ src, raw, inv, busy, onClose, submit }: { src: InvEngine; raw: Obj; inv: Inventory; busy: boolean; onClose: () => void; submit: Submit }) {
  const [id, setId] = useState(`${src.id}-fork`);
  const [title, setTitle] = useState(`${src.title || src.id} (fork)`);
  const [image, setImage] = useState(src.image ?? "");
  const taken = inv.engines.map((e) => e.id);
  const err = !/^[a-z0-9][a-z0-9_-]{0,31}$/.test(id) ? "lower-case letters, digits, - and _" : taken.includes(id) ? "an engine has that name" : null;
  const go = () => {
    if (err) return;
    const copy = JSON.parse(JSON.stringify(obj(obj(raw.engines)[src.id]))) as Obj;
    copy.title = title;
    if (image) copy.image = image;
    copy.caps = { inherits: src.id };
    if (typeof copy.container === "string") copy.container = (copy.container as string).replace(src.id, id);
    submit([{ path: ["engines", id], value: copy as FormEdit["value"] }], onClose);
  };
  return (
    <FormDialog title={`Fork ${src.title || src.id}`} description={`A new engine with the same launch settings and its own image. It inherits what ${src.id} runs, so every model ${src.id} can run, it can too, with no model edits. Tick it on the cards that may run it.`} onClose={onClose} onSubmit={go} busy={busy} error={err} testid="inv-fork-dialog">
      <Grid>
        <Field label="Name"><Text value={id} onChange={setId} mono testid="inv-fork-id" /></Field>
        <Field label="Title"><Text value={title} onChange={setTitle} /></Field>
        <Field label="Image" wide><Text value={image} onChange={setImage} mono testid="inv-fork-image" /></Field>
      </Grid>
    </FormDialog>
  );
}

// ── models ───────────────────────────────────────────────────────────────────────────────────

function ModelsPane({ inv, submit, busy, onDownloads }: { inv: Inventory; submit: Submit; busy: boolean; onDownloads: () => void }) {
  const [edit, setEdit] = useState<InvModel | null>(null);
  const [hf, setHf] = useState(false);
  const groups = useMemo(() => {
    const g: Partial<Record<ModelKind, InvModel[]>> = {};
    for (const m of inv.models) (g[m.kind] ??= []).push(m);
    return g;
  }, [inv.models]);
  return (
    <div className="space-y-3" data-testid="inv-models-pane">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h3 className="text-sm font-semibold">Models</h3>
          <p className="text-xs text-muted-foreground">Each model runs on the engines able to run it (worked out from its files), primary first. The router picks a card that has the files, allows the engine and has the room.</p>
        </div>
        <Button size="sm" onClick={() => setHf(true)} data-testid="inv-hf-open"><Search /> Add from Hugging Face</Button>
      </div>
      {inv.models.length === 0 && <Empty>No models yet. Search Hugging Face: the router fills in the type, size and context, and downloads to your server.</Empty>}
      {(Object.keys(groups) as ModelKind[]).map((k) => (
        <section key={k} className="space-y-1">
          <div className="text-xs font-medium text-muted-foreground">{KIND_LABELS[k]}</div>
          {groups[k]!.map((m) => (
            <button key={m.id} onClick={() => setEdit(m)} className="flex w-full flex-wrap items-center gap-2 rounded-md border px-3 py-2 text-left hover:bg-accent/40" data-testid="inv-model" data-model={m.id}>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium">{m.title || m.id} <span className="font-mono text-[11px] text-muted-foreground">{m.id}</span></div>
                <div className="flex flex-wrap gap-1 pt-0.5">
                  {[m.format ?? m.files[0]?.format, m.arch ?? m.files[0]?.arch, m.quant ?? m.files[0]?.quant].filter(Boolean).map((x) => <Badge key={x!} variant="outline">{x}</Badge>)}
                  {m.vision && <Badge variant="secondary">vision</Badge>}
                  {m.source?.gated && <Badge variant="warning"><Lock className="mr-0.5 size-3" />gated</Badge>}
                </div>
              </div>
              <div className="text-right text-[11px]">
                <div><span className="text-muted-foreground">primary</span> <b>{m.primary ?? "none"}</b>{m.order.length > 1 && <span className="text-muted-foreground"> +{m.order.length - 1}</span>}</div>
                <div className={cn(m.placement.some((p) => p.ok) ? "text-muted-foreground" : "text-warning")}>{placementLine(m)}</div>
              </div>
            </button>
          ))}
        </section>
      ))}
      {edit && <ModelEnginesDialog m={edit} inv={inv} busy={busy} onClose={() => setEdit(null)} submit={submit} />}
      {hf && <HfDialog inv={inv} busy={busy} onClose={() => setHf(false)} submit={submit} onStarted={() => { setHf(false); onDownloads(); }} />}
    </div>
  );
}

function ModelEnginesDialog({ m, inv, busy, onClose, submit }: { m: InvModel; inv: Inventory; busy: boolean; onClose: () => void; submit: Submit }) {
  const [primary, setPrimary] = useState(m.primaryChoice ?? m.primary ?? "");
  const [prefer, setPrefer] = useState<string[]>(() => m.order.filter((e) => e !== (m.primaryChoice ?? m.primary)));
  const [allow, setAllow] = useState<string[]>(m.allow);
  const [deny, setDeny] = useState<string[]>(m.deny);
  const ok = new Set(m.supported.filter((s) => s.ok).map((s) => s.engine));
  const rows = m.supported;
  const flip = (list: string[], set: (v: string[]) => void, e: string) => set(list.includes(e) ? list.filter((x) => x !== e) : [...list, e]);
  return (
    <FormDialog title={m.title || m.id} description="The engines that can run this model, and why. Pick the primary; the rest are tried in order when the primary cannot be placed." onClose={onClose} busy={busy}
      onSubmit={() => submit(modelEngineEdits(m.id, { primary, prefer: prefer.filter((e) => ok.has(e) || allow.includes(e)), allow, deny }), onClose)} testid="inv-model-dialog">
      <div className="flex flex-wrap gap-1 text-[11px]">
        <Badge variant="secondary">{KIND_LABELS[m.kind]}</Badge>
        {m.files.map((f) => <Badge key={f.key} variant="outline">{f.key}: {[f.format, f.arch, f.quant].filter(Boolean).join(" · ") || "file"}</Badge>)}
        {m.context && <Badge variant="outline">context {m.context}</Badge>}
        {m.licence && <Badge variant="outline">{m.licence}</Badge>}
      </div>
      <div className="divide-y rounded-md border" data-testid="inv-model-engines">
        {rows.map((s) => {
          const rank = s.engine === primary ? 0 : prefer.indexOf(s.engine) + 1;
          return (
            <div key={s.engine} className="flex flex-wrap items-center gap-2 px-2 py-1.5 text-xs" data-testid="inv-model-engine" data-engine={s.engine} data-ok={s.ok}>
              <input type="radio" name="primary" checked={primary === s.engine} disabled={!s.ok} onChange={() => { setPrimary(s.engine); setPrefer(prefer.filter((x) => x !== s.engine)); }} aria-label={`primary ${s.engine}`} />
              <span className="w-28 shrink-0 font-medium">{inv.engines.find((e) => e.id === s.engine)?.title || s.engine}</span>
              <span className={cn("min-w-0 flex-1", s.ok ? "text-success" : "text-muted-foreground")}>
                {!s.ok && <X className="mr-1 inline size-3" />}{s.reason}
                {s.via !== "capability" && s.via !== "none" && <span className="ml-1 text-muted-foreground">({s.via === "settings" ? "has its own settings" : s.via})</span>}
              </span>
              {s.ok && primary === s.engine && <Badge>primary</Badge>}
              {s.ok && primary !== s.engine && rank > 0 && (
                <span className="flex items-center gap-0.5">
                  <span className="text-muted-foreground">#{rank + 1}</span>
                  <Button size="sm" variant="ghost" className="h-6 px-1" onClick={() => setPrefer(moved(prefer, s.engine, -1))} aria-label="earlier"><ArrowUp /></Button>
                  <Button size="sm" variant="ghost" className="h-6 px-1" onClick={() => setPrefer(moved(prefer, s.engine, 1))} aria-label="later"><ArrowDown /></Button>
                </span>
              )}
              <span className="flex gap-1">
                {!s.ok && !deny.includes(s.engine) && <Button size="sm" variant="ghost" className="h-6 text-[11px]" onClick={() => flip(allow, setAllow, s.engine)}>{allow.includes(s.engine) ? "undo allow" : "allow anyway"}</Button>}
                {(s.ok || deny.includes(s.engine)) && <Button size="sm" variant="ghost" className="h-6 text-[11px]" onClick={() => flip(deny, setDeny, s.engine)}>{deny.includes(s.engine) ? "undo never" : "never"}</Button>}
              </span>
            </div>
          );
        })}
      </div>
      <details className="rounded-md border p-2">
        <summary className="cursor-pointer text-xs font-medium">Where it runs now</summary>
        <div className="space-y-1 pt-2 text-[11px]" data-testid="inv-model-placement">
          {m.placement.map((p) => (
            <div key={`${p.card}-${p.engine}`} className={cn(!p.ok && "text-muted-foreground")}>
              {p.ok ? <Check className="mr-1 inline size-3 text-success" /> : <X className="mr-1 inline size-3" />}
              card {p.card} · {p.engine || "no engine"}{p.profile ? ` (${p.profile})` : ""}{p.needGib != null ? ` · needs ${fmtGib(p.needGib)} of ${fmtGib(p.roomGib)}` : ""}{p.why?.length ? ` · ${p.why.join("; ")}` : ""}
            </div>
          ))}
        </div>
      </details>
    </FormDialog>
  );
}

// ── Hugging Face ─────────────────────────────────────────────────────────────────────────────

function HfDialog({ inv, busy, onClose, submit, onStarted }: { inv: Inventory; busy: boolean; onClose: () => void; submit: Submit; onStarted: () => void }) {
  const qc = useQueryClient();
  const [q, setQ] = useState("");
  const [kind, setKind] = useState("");
  const [term, setTerm] = useState<{ q: string; kind: string } | null>(null);
  const [repo, setRepo] = useState<string | null>(null);
  const search = useQuery({ queryKey: ["hf-search", term], queryFn: () => routerApi.hfSearch(term!.q, term!.kind || undefined), enabled: !!term, retry: false });
  const details = useQuery({ queryKey: ["hf-model", repo], queryFn: () => routerApi.hfModel(repo!), enabled: !!repo, retry: false });
  const [token, setToken] = useState("");
  const setTok = useMutation({ mutationFn: () => routerApi.hfSetToken(token), onSuccess: () => { setToken(""); void qc.invalidateQueries({ queryKey: KEY }); } });
  return (
    <FormDialog title="Add a model from Hugging Face" description="The router searches and downloads; files go to the chosen server's model folder." onClose={onClose} onSubmit={onClose} submitLabel="Close" testid="inv-hf-dialog">
      <form className="flex flex-wrap gap-2" onSubmit={(e) => { e.preventDefault(); setRepo(null); setTerm({ q, kind }); }}>
        <input className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 text-sm outline-none focus:border-primary" value={q} onChange={(e) => setQ(e.target.value)} placeholder="e.g. qwen3 gguf" data-testid="inv-hf-q" />
        <div className="w-36"><Select value={kind} onChange={setKind} options={[["", "any type"], ...inv.kinds.map((k) => [k.id, k.label] as [string, string])]} /></div>
        <Button size="sm" type="submit" data-testid="inv-hf-search"><Search /> Search</Button>
      </form>
      <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
        <Lock className="size-3" /> Token: {inv.hf?.set ? <span className="text-success">set</span> : "none (needed for gated models)"}
        {!inv.hf?.set && (
          <>
            <input type="password" className="h-6 w-40 rounded border bg-background px-1 font-mono" value={token} onChange={(e) => setToken(e.target.value)} placeholder="hf_…" autoComplete="off" data-testid="inv-hf-token" />
            <Button size="sm" variant="ghost" className="h-6" disabled={!token} onClick={() => setTok.mutate()}>Save token</Button>
          </>
        )}
        {setTok.isError && <span className="text-destructive">{errText(setTok.error)}</span>}
      </div>
      {search.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {search.isError && <div className="text-xs text-destructive">{errText(search.error)}</div>}
      {!repo && search.data && (
        <div className="max-h-72 divide-y overflow-y-auto rounded-md border" data-testid="inv-hf-results">
          {search.data.length === 0 && <div className="p-3 text-xs text-muted-foreground">Nothing found.</div>}
          {search.data.map((r) => (
            <button key={r.repo} onClick={() => setRepo(r.repo)} className="flex w-full flex-wrap items-center gap-2 px-2 py-1.5 text-left text-xs hover:bg-accent/40" data-testid="inv-hf-row">
              <span className="min-w-0 flex-1 truncate font-medium">{r.repo}</span>
              {r.kind && <Badge variant="secondary">{KIND_LABELS[r.kind]}</Badge>}
              {r.gguf && <Badge variant="outline">gguf</Badge>}
              {r.gated && <Badge variant="warning"><Lock className="mr-0.5 size-3" />gated</Badge>}
              {r.licence && <span className="text-muted-foreground">{r.licence}</span>}
              <span className="text-muted-foreground">{r.downloads.toLocaleString()} dl</span>
            </button>
          ))}
        </div>
      )}
      {repo && details.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {repo && details.isError && <div className="text-xs text-destructive">{errText(details.error)}</div>}
      {repo && details.data && <HfModel d={details.data} inv={inv} busy={busy} submit={submit} onBack={() => setRepo(null)} onStarted={onStarted} />}
    </FormDialog>
  );
}

function HfModel({ d, inv, busy, submit, onBack, onStarted }: { d: HfDetails; inv: Inventory; busy: boolean; submit: Submit; onBack: () => void; onStarted: () => void }) {
  const qc = useQueryClient();
  const [choice, setChoice] = useState<HfChoice | null>(d.choices.find((c) => fitSummary(d.fit?.[c.id]).ok) ?? d.choices[0] ?? null);
  const servers = inv.servers.filter((s) => s.modelDir);
  const [server, setServer] = useState(servers[0]?.id ?? "");
  const kind = (d.kind ?? "llm") as ModelKind;
  const engines = choice ? enginesFor(inv, kind, choice.format, d.arch) : [];
  const [engine, setEngine] = useState("");
  const eng = engine || engines[0]?.id || "";
  const [id, setId] = useState(() => modelIdFromRepo(d.repo, inv.models.map((m) => m.id)));
  const [err, setErr] = useState<string | null>(null);
  const start = useMutation({
    mutationFn: () => routerApi.startDownload({ server, repo: d.repo, revision: d.revision, files: choice!.files, model: id, gated: d.gated }),
    onSuccess: () => { void qc.invalidateQueries({ queryKey: ["router-downloads"] }); onStarted(); },
    onError: (e) => setErr(errText(e)),
  });
  const go = () => {
    setErr(null);
    if (!choice) return setErr("pick a file");
    if (!server) return setErr("set a model folder on a server first (Servers & cards → Model folder)");
    if (!eng) return setErr(`no engine here runs ${choice.format} ${KIND_LABELS[kind].toLowerCase()}${d.arch ? ` (${d.arch})` : ""}: add or fork one under Engines`);
    if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(id) || inv.models.some((m) => m.id === id)) return setErr("pick another model name");
    const dir = repoDir(servers.find((s) => s.id === server)!.modelDir!, d.repo);
    submit(newModelEdits(id, d, choice, eng, [], dir), () => start.mutate());
  };
  return (
    <div className="space-y-3 rounded-md border p-3" data-testid="inv-hf-model">
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="ghost" className="h-6 px-1" onClick={onBack}>← results</Button>
        <a className="flex items-center gap-1 text-sm font-medium hover:underline" href={`https://huggingface.co/${d.repo}`} target="_blank" rel="noreferrer">{d.repo}<ExternalLink className="size-3" /></a>
      </div>
      <div className="flex flex-wrap gap-1 text-[11px]">
        <Badge variant="secondary">{KIND_LABELS[kind]}</Badge>
        {d.arch && <Badge variant="outline">{d.arch}</Badge>}
        {d.context && <Badge variant="outline">context {d.context.toLocaleString()}</Badge>}
        {d.vision && <Badge variant="outline">vision</Badge>}
        {d.licence && <Badge variant="outline">licence {d.licence}</Badge>}
        {d.gated && <Badge variant="warning"><Lock className="mr-0.5 size-3" />gated{d.gatedHow ? ` (${d.gatedHow})` : ""}: accept on huggingface.co, set a token</Badge>}
      </div>
      <div className="space-y-1" data-testid="inv-hf-choices">
        <div className="text-xs font-medium">File</div>
        {d.choices.length === 0 && <div className="text-xs text-muted-foreground">No downloadable files.</div>}
        {d.choices.map((c) => {
          const fit = fitSummary(d.fit?.[c.id]);
          return (
            <label key={c.id} className={cn("flex cursor-pointer flex-wrap items-center gap-2 rounded border px-2 py-1 text-xs", choice?.id === c.id && "border-primary bg-primary/5")} data-testid="inv-hf-choice">
              <input type="radio" checked={choice?.id === c.id} onChange={() => { setChoice(c); setEngine(""); }} />
              <span className="font-medium">{c.label}</span>
              <span className="text-muted-foreground">{fmtGib(c.sizeGib)} · needs ~{fmtGib(c.needGib)}</span>
              <span className={cn("ml-auto", fit.ok ? "text-success" : "text-warning")}>{fit.text}</span>
            </label>
          );
        })}
      </div>
      <Grid>
        <Field label="Download to" hint={servers.length ? undefined : "no server has a model folder yet"}>
          <Select value={server} onChange={setServer} options={servers.length ? servers.map((s) => [s.id, `${s.title || s.id} (${s.modelDir})`] as [string, string]) : [["", "none"]]} testid="inv-hf-server" />
        </Field>
        <Field label="Primary engine" hint={engines.length ? `${engines.length} engine${engines.length === 1 ? "" : "s"} can run it` : "none can run this file"}>
          <Select value={eng} onChange={setEngine} options={engines.length ? engines.map((e) => [e.id, e.title || e.id] as [string, string]) : [["", "none"]]} testid="inv-hf-engine" />
        </Field>
        <Field label="Model name" hint="the id in the catalog"><Text value={id} onChange={setId} mono testid="inv-hf-id" /></Field>
      </Grid>
      {err && <div className="text-xs text-destructive" data-testid="inv-hf-error">{err}</div>}
      <Button size="sm" onClick={go} disabled={busy || start.isPending || !choice} data-testid="inv-hf-add">
        {(busy || start.isPending) ? <Loader2 className="animate-spin" /> : <Download />} Add and download
      </Button>
    </div>
  );
}

// ── downloads ────────────────────────────────────────────────────────────────────────────────

function DownloadsPane() {
  const qc = useQueryClient();
  const list = useQuery({
    queryKey: ["router-downloads"],
    queryFn: routerApi.downloads,
    refetchInterval: (q) => ((q.state.data ?? []).some((d) => d.state === "running" || d.state === "queued") ? 2000 : false),
  });
  const act = useMutation({
    mutationFn: ({ id, what }: { id: string; what: "cancel" | "resume" | "remove" }): Promise<unknown> =>
      what === "cancel" ? routerApi.cancelDownload(id) : what === "resume" ? routerApi.resumeDownload(id) : routerApi.removeDownload(id),
    onSettled: () => { void qc.invalidateQueries({ queryKey: ["router-downloads"] }); void qc.invalidateQueries({ queryKey: KEY }); },
  });
  if (list.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (list.isError) return <div className="text-xs text-destructive">{errText(list.error)}</div>;
  const rows = list.data ?? [];
  return (
    <div className="space-y-2" data-testid="inv-downloads-pane">
      <div>
        <h3 className="text-sm font-semibold">Downloads</h3>
        <p className="text-xs text-muted-foreground">Run on the server itself. A stopped download keeps its part file: Resume carries on where it stopped.</p>
      </div>
      {act.isError && <div className="text-xs text-destructive">{errText(act.error)}</div>}
      {rows.length === 0 && <Empty>No downloads yet. Add a model from Hugging Face under Models.</Empty>}
      {rows.map((d) => <DownloadRow key={d.id} d={d} act={(what) => act.mutate({ id: d.id, what })} />)}
    </div>
  );
}

function DownloadRow({ d, act }: { d: DownloadJob; act: (what: "cancel" | "resume" | "remove") => void }) {
  const p = pct(d.bytes, d.total);
  const live = d.state === "running" || d.state === "queued";
  return (
    <div className="space-y-1 rounded-md border px-3 py-2 text-xs" data-testid="inv-download" data-state={d.state}>
      <div className="flex flex-wrap items-center gap-2">
        <span className="min-w-0 flex-1 truncate font-medium">{d.repo}</span>
        <Badge variant={d.state === "done" ? "success" : d.state === "failed" ? "destructive" : live ? "default" : "outline"}>{d.state}</Badge>
        <span className="text-muted-foreground">→ {d.server}</span>
        {live && <Button size="sm" variant="ghost" className="h-6" onClick={() => act("cancel")} data-testid="inv-dl-cancel"><Pause /> Stop</Button>}
        {(d.state === "failed" || d.state === "cancelled") && <Button size="sm" variant="ghost" className="h-6" onClick={() => act("resume")} data-testid="inv-dl-resume"><Play /> Resume</Button>}
        {!live && <Button size="sm" variant="ghost" className="h-6" onClick={() => act("remove")} aria-label="Remove from the list"><Trash2 /></Button>}
      </div>
      <div className="h-1.5 overflow-hidden rounded bg-muted"><div className={cn("h-full", d.state === "failed" ? "bg-destructive" : "bg-primary")} style={{ width: `${p}%` }} /></div>
      <div className="flex flex-wrap gap-x-3 text-[11px] text-muted-foreground">
        <span>{p}% · {fmtGib(d.bytes / 2 ** 30)} of {fmtGib(d.total / 2 ** 30)}</span>
        <span>{d.files.filter((f) => f.done).length}/{d.files.length} files</span>
        {d.model && <span>for {d.model}</span>}
        <span className="truncate font-mono">{d.dir}</span>
      </div>
      {d.error && <div className="text-[11px] text-destructive">{d.error}</div>}
    </div>
  );
}
