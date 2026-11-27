// This machine (MACHINE.md, D155): the desktop app's page for the PC it runs on. The inventory
// and the Engine store are the router admin's own forms over an in-process router
// (/api/admin/machine); the rest is the machine's: its manual engines, what it shares with a
// router when bridged, changes a router admin asks for, and what the router did.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cable, Check, Cpu, History, Loader2, Package, Play, Server, Square, Undo2, Wrench, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { InventoryForms } from "@/features/router/forms/InventoryForms";
import { EngineStore } from "@/features/router/forms/EngineStore";
import { area } from "@/features/router/forms/formKit";
import { ago } from "@/features/router/routerModel";
import { machineApi, type MachineStatus } from "./machineApi";

const STATUS = ["machine-status"];
const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function MachineApp() {
  const st = useQuery({ queryKey: STATUS, queryFn: machineApi.status, refetchInterval: 4000 });
  const [tab, setTab] = useState("inventory");
  const s = st.data;
  const pending = s?.pending.length ?? 0;
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="machine-app">
      <header className="flex flex-wrap items-center gap-2 border-b px-4 py-2 text-xs">
        <Server className="size-4 text-muted-foreground" />
        <span className="text-sm font-semibold">{s?.title || "This machine"}</span>
        {s && <span className="font-mono text-muted-foreground">{s.server}</span>}
        {s && <BridgeBadge s={s} />}
        {s?.error && <span className="text-destructive">{s.error}</span>}
      </header>
      {pending > 0 && (
        <button className="border-b bg-warning/10 px-4 py-2 text-left text-xs" onClick={() => setTab("sharing")} data-testid="machine-pending-banner">
          A Sothoth admin asked for {pending} change{pending > 1 ? "s" : ""} that need{pending > 1 ? "" : "s"} your approval: open <b>Sharing & Migo</b>.
        </button>
      )}
      <Tabs value={tab} onValueChange={setTab} className="flex min-h-0 flex-1 flex-col">
        <div className="overflow-x-auto border-b px-4 py-2">
          <TabsList>
            <TabsTrigger value="inventory" data-testid="machine-tab-inventory"><Cpu className="mr-1 size-3.5" />Inventory</TabsTrigger>
            <TabsTrigger value="running" data-testid="machine-tab-running"><Play className="mr-1 size-3.5" />Running</TabsTrigger>
            <TabsTrigger value="manual" data-testid="machine-tab-manual"><Wrench className="mr-1 size-3.5" />Manual engines</TabsTrigger>
            <TabsTrigger value="store" data-testid="machine-tab-store"><Package className="mr-1 size-3.5" />Engine store</TabsTrigger>
            <TabsTrigger value="sharing" data-testid="machine-tab-sharing"><Cable className="mr-1 size-3.5" />Sharing & Migo{pending > 0 && <Badge variant="warning" className="ml-1">{pending}</Badge>}</TabsTrigger>
          </TabsList>
        </div>
        <TabsContent value="inventory" className="flex min-h-0 flex-1 flex-col data-[state=inactive]:hidden"><InventoryForms single /></TabsContent>
        <TabsContent value="running" className="min-h-0 flex-1 overflow-y-auto p-4 data-[state=inactive]:hidden">{s ? <Running s={s} /> : <Spinner />}</TabsContent>
        <TabsContent value="manual" className="min-h-0 flex-1 overflow-y-auto p-4 data-[state=inactive]:hidden"><ManualEngines /></TabsContent>
        <TabsContent value="store" className="flex min-h-0 flex-1 flex-col data-[state=inactive]:hidden"><EngineStore /></TabsContent>
        <TabsContent value="sharing" className="min-h-0 flex-1 overflow-y-auto p-4 data-[state=inactive]:hidden">{s ? <Sharing s={s} /> : <Spinner />}</TabsContent>
      </Tabs>
    </div>
  );
}

const Spinner = () => <Loader2 className="m-4 size-4 animate-spin text-muted-foreground" />;

function BridgeBadge({ s }: { s: MachineStatus }) {
  if (!s.bridgeMode) return <Badge variant="outline">runs its engines itself</Badge>;
  if (s.bridge?.connected) return <Badge variant="success" data-testid="machine-bridge-online">Migo to {s.bridge.router ?? s.bridgeRouter} · online</Badge>;
  return <Badge variant="warning" title={s.bridge?.lastError ?? ""}>Migo · offline{s.bridge?.lastError ? `: ${s.bridge.lastError}` : ""}</Badge>;
}

function Running({ s }: { s: MachineStatus }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const act = useMutation({
    mutationFn: (v: { op: "start" | "stop"; engine: string; card: string }) => (v.op === "start" ? machineApi.start(v.engine, v.card) : machineApi.stop(v.engine, v.card)),
    onSuccess: () => { setError(null); void qc.invalidateQueries({ queryKey: STATUS }); },
    onError: (e) => setError(errText(e)),
  });
  const list = s.engines.engines ?? [];
  return (
    <div className="space-y-3 text-xs" data-testid="machine-running">
      <div>
        <h3 className="text-sm font-semibold">Engines on this machine</h3>
        <p className="text-muted-foreground">On-demand engines start at the first request (from this app, or from the router when shared) and stop when idle. Their images and packages are fetched ahead of time.</p>
      </div>
      {s.problems.map((p) => <div key={p} className="text-warning">{p}</div>)}
      {error && <div className="text-destructive">{error}</div>}
      {list.length === 0 && <div className="rounded-md border border-dashed p-4 text-center text-muted-foreground">No engines yet: add one from the Engine store, or a manual one.</div>}
      <div className="divide-y rounded-md border">
        {list.map((e) => (
          <div key={`${e.engine}@${e.card}`} className="flex flex-wrap items-center gap-2 px-3 py-2" data-testid="machine-engine" data-engine={e.engine}>
            <span className="font-medium">{e.engine}</span>
            <span className="font-mono text-muted-foreground">{e.card}{e.port ? `:${e.port}` : ""}</span>
            <span className="text-muted-foreground">{e.method} · {e.lifecycle?.replace("_", " ")}</span>
            {s.bridgeMode && (s.share.engines[e.engine] ? <Badge variant="secondary">shared</Badge> : <Badge variant="outline">not shared</Badge>)}
            <span className={cn("ml-auto", e.state === "running" ? "text-success" : e.state === "failed" ? "text-destructive" : "text-muted-foreground")} title={e.error ?? ""}>
              {e.state}{e.state === "running" && e.up_seconds != null ? ` ${Math.round(e.up_seconds / 60)} min` : ""}{e.active ? ` · ${e.active} in use` : ""}
            </span>
            {e.method !== "external" && (e.state === "running"
              ? <Button size="sm" variant="ghost" onClick={() => act.mutate({ op: "stop", engine: e.engine, card: e.card })} aria-label="Stop"><Square /></Button>
              : <Button size="sm" variant="ghost" disabled={act.isPending} onClick={() => act.mutate({ op: "start", engine: e.engine, card: e.card })} aria-label="Start"><Play /></Button>)}
            {e.prepare?.state === "failed" && <div className="w-full text-destructive">Cannot start: {e.prepare.error}</div>}
            {e.prepare?.state === "fetching" && <div className="w-full text-muted-foreground">Fetching its image…</div>}
            {e.state === "failed" && e.error && e.prepare?.state !== "failed" && <div className="w-full text-destructive">{e.error}</div>}
          </div>
        ))}
      </div>
    </div>
  );
}

/** Engines this app runs as written (a command line or a container), kept in engines.toml and
 * mirrored into the inventory as external engines. */
function ManualEngines() {
  const qc = useQueryClient();
  const doc = useQuery({ queryKey: ["machine-engines"], queryFn: machineApi.engines, refetchOnWindowFocus: false });
  const [text, setText] = useState<string | null>(null);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const save = useMutation({
    mutationFn: () => machineApi.saveEngines(JSON.parse(text ?? "{}"), doc.data!.rev),
    onSuccess: () => { setNote({ ok: true, text: "Saved: the inventory lists them as engines of this machine." }); setText(null); void doc.refetch(); void qc.invalidateQueries(); },
    onError: (e) => setNote({ ok: false, text: (e as { status?: number }).status === 412 ? "They changed since you loaded them (a router admin?): reload." : errText(e) }),
  });
  if (doc.isLoading) return <Spinner />;
  if (doc.isError) return <div className="text-xs text-destructive">{errText(doc.error)}</div>;
  const shown = text ?? JSON.stringify(doc.data?.engines ?? {}, null, 2);
  let bad: string | null = null;
  try { JSON.parse(shown); } catch (e) { bad = errText(e); }
  return (
    <div className="space-y-2 text-xs" data-testid="machine-manual">
      <div>
        <h3 className="text-sm font-semibold">Manual engines</h3>
        <p className="text-muted-foreground">
          Engines you run as written: <code>process.command</code> (a program and its arguments, no shell), <code>docker</code> (an image and its flags), or <code>lifecycle: "external"</code> for something already running on a port. Each has <code>kinds</code>, <code>cards</code>, a <code>port</code> and a <code>health</code> path. Packages from the Engine store don't belong here.
        </p>
      </div>
      <textarea className={cn(area, "min-h-[320px]")} value={shown} onChange={(e) => setText(e.target.value)} spellCheck={false} data-testid="machine-manual-text" />
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" disabled={text == null || !!bad || save.isPending} onClick={() => save.mutate()} data-testid="machine-manual-save">{save.isPending && <Loader2 className="animate-spin" />} Check and save</Button>
        <Button size="sm" variant="ghost" onClick={() => { setText(null); setNote(null); void doc.refetch(); }}>Reload</Button>
        {bad && <span className="text-destructive">{bad}</span>}
        <span className="font-mono text-muted-foreground">rev {doc.data?.rev}</span>
      </div>
      {note && <div className={note.ok ? "text-success" : "text-destructive"}>{note.text}</div>}
    </div>
  );
}

function Sharing({ s }: { s: MachineStatus }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const done = { onSuccess: () => { setError(null); void qc.invalidateQueries({ queryKey: STATUS }); }, onError: (e: unknown) => setError(errText(e)) };
  const share = useMutation({ mutationFn: machineApi.share, ...done });
  const decide = useMutation({ mutationFn: (v: { id: string; approve: boolean }) => machineApi.decide(v.id, v.approve), ...done });
  const undo = useMutation({ mutationFn: machineApi.undo, ...done });
  const engines = Object.entries(s.share.engines);
  const models = Object.entries(s.share.models);
  return (
    <div className="space-y-4 text-xs" data-testid="machine-sharing">
      <section className="space-y-1">
        <h3 className="text-sm font-semibold">Migo</h3>
        {!s.bridgeMode && <p className="text-muted-foreground">Migo mode (formerly Bridge mode) is off: this app runs these engines for itself only. Turn it on in Settings → Connection to offer them to Sothoth, the router (your requests still go through it then).</p>}
        {s.bridgeMode && (
          <div className="space-y-0.5" data-testid="machine-bridge-status">
            <div>Sothoth: <span className="font-mono">{s.bridge?.router ?? s.bridgeRouter ?? "not set"}</span> · {s.bridge?.connected ? <span className="text-success">connected as {s.bridge.server}</span> : <span className="text-warning">not connected</span>}{s.bridge?.enabled === false && " · the Sothoth admin has turned it off"}</div>
            {s.bridge?.lastError && <div className="text-destructive">{s.bridge.lastError}</div>}
            <div className="text-muted-foreground">Remote config: {s.remoteConfig ? "a Sothoth admin may change this inventory (each change is logged below and can be undone)" : "off: Sothoth sees this machine read-only"}.</div>
            {s.advancedFile && <div className="text-warning">Advanced: Migo runs from {s.advancedFile}, not this inventory.</div>}
          </div>
        )}
      </section>
      {error && <div className="text-destructive">{error}</div>}
      <section className="space-y-1">
        <h3 className="text-sm font-semibold">Share with Sothoth</h3>
        <p className="text-muted-foreground">What Sothoth (the router) may use through Migo. New engines and models are shared by default while Migo mode is on.</p>
        {engines.length + models.length === 0 && <div className="text-muted-foreground">Nothing to share yet.</div>}
        <div className="grid gap-1 sm:grid-cols-2">
          {engines.map(([id, on]) => (
            <label key={`e-${id}`} className="flex items-center gap-2 rounded border px-2 py-1" data-testid="machine-share-engine">
              <input type="checkbox" checked={on} onChange={(e) => share.mutate({ engines: { [id]: e.target.checked } })} /> engine <span className="font-mono">{id}</span>
            </label>
          ))}
          {models.map(([id, on]) => (
            <label key={`m-${id}`} className="flex items-center gap-2 rounded border px-2 py-1">
              <input type="checkbox" checked={on} onChange={(e) => share.mutate({ models: { [id]: e.target.checked } })} /> model <span className="font-mono">{id}</span>
            </label>
          ))}
        </div>
      </section>
      <section className="space-y-1">
        <h3 className="text-sm font-semibold">Waiting for your approval</h3>
        {s.pending.length === 0 && <div className="text-muted-foreground">Nothing waiting.</div>}
        {s.pending.map((p) => (
          <div key={p.id} className="space-y-1 rounded-md border border-warning/50 p-2" data-testid="machine-pending">
            <div><b>{p.who}</b> asked {ago(p.at * 1000)} ({p.op === "config.apply" ? "an inventory change" : "an engine install"}). It needs your approval because:</div>
            <ul className="list-disc pl-5">{p.reasons.map((r) => <li key={r}>{r}</li>)}</ul>
            <div className="flex gap-2">
              <Button size="sm" disabled={decide.isPending} onClick={() => decide.mutate({ id: p.id, approve: true })} data-testid="machine-approve"><Check /> Approve and apply</Button>
              <Button size="sm" variant="outline" disabled={decide.isPending} onClick={() => decide.mutate({ id: p.id, approve: false })}><X /> Refuse</Button>
            </div>
          </div>
        ))}
      </section>
      <section className="space-y-1">
        <h3 className="flex items-center gap-1 text-sm font-semibold"><History className="size-4" /> Changes by a router admin</h3>
        {s.remoteChanges.length === 0 && <div className="text-muted-foreground">None yet.</div>}
        <div className="divide-y rounded-md border">
          {s.remoteChanges.map((c) => (
            <div key={c.id} className="flex flex-wrap items-center gap-2 px-2 py-1.5" data-testid="machine-change">
              <span>Changed by router admin <b>{c.who.replace(/^admin:/, "")}</b> {ago(c.at * 1000)}{c.what ? `: ${c.what}` : ""}</span>
              {c.undone ? <Badge variant="outline" className="ml-auto">undone</Badge> : (
                <Button size="sm" variant="ghost" className="ml-auto" disabled={undo.isPending} onClick={() => undo.mutate(c.id)} data-testid="machine-undo"><Undo2 /> Undo</Button>
              )}
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
