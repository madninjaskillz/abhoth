// The Engine store (section 10 of the router media plan): packages from every store and My
// packages, each with its manifest, templates and validation report; the install flow (server ->
// fit per card -> the manifest read -> every approval ticked -> a job with its log); installed
// packages with update and remove; and the store list. Manual engines stay first-class: they are
// added in Inventory and listed here as they are.
import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowLeft, ArrowRight, Check, ChevronRight, Download, ExternalLink, Loader2, Package, Pencil, Plus, RefreshCw, Search, Store, Trash2, Wrench } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { engineStoreApi, routerApi } from "../routerApi";
import { Check2, Field, Select, Text, errText, input } from "./formKit";
import { KIND_LABELS, serverLabel, type ModelKind } from "./inventoryModel";
import { PackageWizard, ReportView, fmtBytes } from "./PackageWizard";
import {
  KINDS, allTicked, anyRunning, approvalItems, approvalsFrom, changedParams, filterPackages, fitLine, installedByServer, isHostRun, manualEngines, methodTypes,
  paramText, rejectionLines, validStoreUrl,
  type CardFit, type InstalledPkg, type Manifest, type PackageDetail, type PackageSummary, type StoreJob,
} from "./engineStoreModel";

const PKGS = ["engine-store-packages"];
const MY = ["engine-store-my"];
const JOBS = ["engine-store-jobs"];
const INSTALLED = ["engine-store-installed"];
const STORES = ["engine-store-stores"];

type Open = { store: string; id: string; install?: boolean };
type View = { kind: "store" } | { kind: "wizard"; engine?: string; initial?: { manifest: Manifest; files: Record<string, string> } };

export interface EngineStoreProps {
  /** Open the Create package wizard on this inventory engine ("Package this engine"). */
  packageEngine?: string;
}

export function EngineStore({ packageEngine }: EngineStoreProps) {
  const [view, setView] = useState<View>(packageEngine ? { kind: "wizard", engine: packageEngine } : { kind: "store" });
  const [open, setOpen] = useState<Open | null>(null);
  const [tab, setTab] = useState("packages");
  useEffect(() => {
    if (packageEngine) setView({ kind: "wizard", engine: packageEngine });
  }, [packageEngine]);
  return (
    <div className="min-h-0 flex-1 overflow-y-auto" data-testid="engine-store">
      <div className="space-y-3 p-3 sm:p-4">
        {view.kind === "wizard" ? (
          <PackageWizard
            engine={view.engine}
            initial={view.initial}
            onClose={() => setView({ kind: "store" })}
            onInstall={(id) => { setView({ kind: "store" }); setTab("packages"); setOpen({ store: "my", id, install: true }); }}
          />
        ) : (
          <>
            <div className="flex flex-wrap items-start justify-between gap-2">
              <div>
                <h3 className="flex items-center gap-1.5 text-sm font-semibold"><Store className="size-4" /> Engine store</h3>
                <p className="text-xs text-muted-foreground">Engine packages: a manifest says what an engine is and how to install and run it, and the router picks the method that fits each card. Engines can still be added by hand in Inventory: no package needed.</p>
              </div>
              <Button size="sm" variant="outline" onClick={() => setView({ kind: "wizard" })} data-testid="store-create"><Plus /> Create package</Button>
            </div>
            <Tabs value={tab} onValueChange={setTab} className="flex flex-col">
              <div className="overflow-x-auto pb-2">
                <TabsList>
                  <TabsTrigger value="packages" data-testid="store-tab-packages"><Package className="mr-1 size-3.5" />Packages</TabsTrigger>
                  <TabsTrigger value="installed" data-testid="store-tab-installed"><Wrench className="mr-1 size-3.5" />Installed</TabsTrigger>
                  <TabsTrigger value="stores" data-testid="store-tab-stores"><Store className="mr-1 size-3.5" />Stores</TabsTrigger>
                </TabsList>
              </div>
              <TabsContent value="packages"><PackagesPane onOpen={setOpen} /></TabsContent>
              <TabsContent value="installed"><InstalledPane /></TabsContent>
              <TabsContent value="stores"><StoresPane /></TabsContent>
            </Tabs>
          </>
        )}
      </div>
      {open && (
        <PackageDialog
          key={`${open.store}/${open.id}`}
          at={open}
          onClose={() => setOpen(null)}
          onEdit={(initial) => { setOpen(null); setView({ kind: "wizard", initial }); }}
          onJob={() => setTab("installed")}
        />
      )}
    </div>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="rounded-md border border-dashed p-4 text-center text-xs text-muted-foreground">{children}</div>;
}

const kindLabel = (k: string) => KIND_LABELS[k as ModelKind] ?? k;

/** Every package: the stores' and My packages, once each. */
function useAllPackages() {
  const all = useQuery({ queryKey: PKGS, queryFn: engineStoreApi.packages, refetchOnWindowFocus: false });
  const my = useQuery({ queryKey: MY, queryFn: engineStoreApi.my, refetchOnWindowFocus: false, retry: false });
  const packages = useMemo(() => {
    const out = [...(all.data?.packages ?? [])];
    for (const p of my.data?.packages ?? []) if (!out.some((o) => o.store === p.store && o.id === p.id)) out.push(p);
    return out;
  }, [all.data, my.data]);
  return { packages, loading: all.isLoading, error: all.error };
}

// ── packages ─────────────────────────────────────────────────────────────────────────────────

function PackagesPane({ onOpen }: { onOpen: (o: Open) => void }) {
  const { packages, loading, error } = useAllPackages();
  const [kind, setKind] = useState("");
  const [text, setText] = useState("");
  const shown = filterPackages(packages, kind, text);
  if (loading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (error) return <div className="text-xs text-destructive" data-testid="store-error">{errText(error)}</div>;
  return (
    <div className="space-y-3" data-testid="store-packages">
      <div className="flex flex-wrap gap-2">
        <div className="relative min-w-[12rem] flex-1">
          <Search className="absolute left-2 top-2 size-4 text-muted-foreground" />
          <input className={cn(input, "pl-8")} value={text} onChange={(e) => setText(e.target.value)} placeholder="Search packages" data-testid="store-search" />
        </div>
        <div className="w-44"><Select value={kind} onChange={setKind} options={[["", "Every kind"], ...KINDS.map((k) => [k, kindLabel(k)] as [string, string])]} testid="store-kind" /></div>
      </div>
      {packages.length === 0 && <Empty>No packages yet: add a store, or create one.</Empty>}
      {packages.length > 0 && shown.length === 0 && <Empty>Nothing matches.</Empty>}
      <div className="divide-y rounded-md border">
        {shown.map((p) => <PackageRow key={`${p.store}/${p.id}`} p={p} onOpen={() => onOpen({ store: p.store, id: p.id })} />)}
      </div>
    </div>
  );
}

function StoreBadges({ p }: { p: { store: string; trusted: boolean; private?: boolean } }) {
  return (
    <>
      <Badge variant="outline">{p.store === "my" ? "My packages" : p.store}</Badge>
      {!p.trusted && <Badge variant="warning" data-testid="store-untrusted">untrusted</Badge>}
      {p.private && <Badge variant="secondary">private</Badge>}
    </>
  );
}

function PackageRow({ p, onOpen }: { p: PackageSummary; onOpen: () => void }) {
  return (
    <button className="flex w-full items-start gap-3 px-3 py-2 text-left hover:bg-accent/40" onClick={onOpen} data-testid="store-package" data-package={p.id} data-store={p.store}>
      <Package className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
      <div className="min-w-0 flex-1 space-y-1">
        <div className="flex flex-wrap items-center gap-1.5 text-sm">
          <span className="font-medium">{p.name}</span>
          <span className="font-mono text-xs text-muted-foreground">{p.id} {p.version}</span>
          <StoreBadges p={p} />
          {p.update && <Badge variant="default" data-testid="store-update">update: {p.update}</Badge>}
        </div>
        {p.summary && <div className="truncate text-xs text-muted-foreground">{p.summary}</div>}
        <div className="flex flex-wrap gap-1">
          {p.kinds.map((k) => <Badge key={k} variant="secondary">{kindLabel(k)}</Badge>)}
          {methodTypes(p).map((t) => <Badge key={t} variant={isHostRun(t) ? "warning" : "outline"}>{t}</Badge>)}
          {p.installed.map((i) => <Badge key={i.server} variant="success">on {i.server}: {i.version}</Badge>)}
        </div>
      </div>
      <ChevronRight className="mt-1 size-4 shrink-0 text-muted-foreground" />
    </button>
  );
}

// ── one package: details, then install ───────────────────────────────────────────────────────

function PackageDialog({ at, onClose, onEdit, onJob }: { at: Open; onClose: () => void; onEdit: (i: { manifest: Manifest; files: Record<string, string> }) => void; onJob: () => void }) {
  const qc = useQueryClient();
  const detail = useQuery({ queryKey: ["engine-store-package", at.store, at.id], queryFn: () => engineStoreApi.package(at.store, at.id), refetchOnWindowFocus: false });
  const { packages } = useAllPackages();
  const summary = packages.find((p) => p.store === at.store && p.id === at.id);
  const [installing, setInstalling] = useState(!!at.install);
  const [delMy, setDelMy] = useState(false);
  const removeMy = useMutation({
    mutationFn: () => engineStoreApi.removeMy(at.id),
    onSuccess: () => { void qc.invalidateQueries({ queryKey: MY }); void qc.invalidateQueries({ queryKey: PKGS }); onClose(); },
  });
  const d = detail.data;
  const m = d?.package.manifest;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[92vh] max-w-4xl overflow-y-auto" data-testid="store-package-dialog">
        <DialogTitle className="flex flex-wrap items-center gap-1.5">
          {m?.name ?? at.id} <span className="font-mono text-xs font-normal text-muted-foreground">{at.id} {m?.version}</span>
          {d && <StoreBadges p={{ store: d.package.store, trusted: d.package.trusted, private: m?.private }} />}
        </DialogTitle>
        <DialogDescription>{installing ? "Install: pick a server and card, read the manifest, approve what it needs." : "What the package is, how it installs, and what the router's check says."}</DialogDescription>
        {detail.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
        {detail.error && <div className="text-xs text-destructive">{errText(detail.error)}</div>}
        {d && m && !installing && (
          <>
            <PackageDetails d={d} />
            <div className="flex flex-wrap justify-end gap-2 border-t pt-3">
              {at.store === "my" && (
                <>
                  <Button size="sm" variant="ghost" onClick={() => setDelMy(true)} data-testid="store-my-delete"><Trash2 /> Delete</Button>
                  <Button size="sm" variant="ghost" asChild><a href={engineStoreApi.exportUrl(at.id)} download={`${at.id}.zip`} data-testid="store-my-export"><Download /> Export</a></Button>
                  <Button size="sm" variant="outline" onClick={() => onEdit({ manifest: m, files: d.package.files })} data-testid="store-my-edit"><Pencil /> Edit</Button>
                </>
              )}
              <Button size="sm" onClick={() => setInstalling(true)} disabled={!d.report.ok} data-testid="store-install-start"><ArrowRight /> Install…</Button>
            </div>
            {!d.report.ok && <div className="text-right text-xs text-destructive">The router refuses this package until its errors are fixed.</div>}
          </>
        )}
        {d && m && installing && <InstallFlow d={d} summary={summary} onBack={() => setInstalling(false)} onJob={onJob} />}
        <ConfirmDialog open={delMy} title={`Delete ${at.id} from My packages?`} body="Installed copies keep working; this removes the saved package." confirm="Delete" destructive onCancel={() => setDelMy(false)} onConfirm={() => { setDelMy(false); removeMy.mutate(); }} />
      </DialogContent>
    </Dialog>
  );
}

function PackageDetails({ d }: { d: PackageDetail }) {
  const m = d.package.manifest;
  const params = Object.entries(m.params ?? {});
  return (
    <div className="space-y-3 text-xs">
      {m.description && <p className="whitespace-pre-wrap">{m.description}</p>}
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-muted-foreground">
        <span>Kinds: {m.kinds.map(kindLabel).join(", ")}</span>
        <span>API: <span className="font-mono">{m.api}</span>{m.also_speaks?.length ? ` (also ${m.also_speaks.join(", ")})` : ""}</span>
        {m.licence && <span>Licence: {m.licence}</span>}
        {m.source && <a className="inline-flex items-center gap-0.5 text-primary hover:underline" href={m.source} target="_blank" rel="noreferrer noopener">Source <ExternalLink className="size-3" /></a>}
        {m.homepage && <a className="inline-flex items-center gap-0.5 text-primary hover:underline" href={m.homepage} target="_blank" rel="noreferrer noopener">Homepage <ExternalLink className="size-3" /></a>}
        {m.hardware && <span>Hardware: {[m.hardware.gpu?.join("/"), m.hardware.min_vram_gib ? `${m.hardware.min_vram_gib} GiB VRAM` : "", m.hardware.cpu_ok ? "CPU ok" : ""].filter(Boolean).join(" · ") || "any"}</span>}
      </div>
      {m.notes?.length ? <ul className="list-disc space-y-0.5 pl-5 text-muted-foreground">{m.notes.map((n, i) => <li key={i}>{n}</li>)}</ul> : null}
      <div>
        <div className="mb-1 font-medium">Methods, most preferred first</div>
        <table className="w-full" data-testid="store-methods">
          <thead className="text-left text-muted-foreground"><tr><th className="px-2 py-1">#</th><th className="px-2 py-1">Type</th><th className="px-2 py-1">Label</th><th className="px-2 py-1">Platforms</th><th className="px-2 py-1">GPU</th></tr></thead>
          <tbody className="divide-y">
            {m.methods.map((x, i) => (
              <tr key={i}>
                <td className="px-2 py-1 font-mono">{i + 1}</td>
                <td className="px-2 py-1"><Badge variant={isHostRun(x.type) ? "warning" : "outline"}>{x.type}</Badge></td>
                <td className="px-2 py-1">{x.label ?? ""}{isHostRun(x.type) && <span className="ml-1 text-warning">runs on the host, not sandboxed</span>}</td>
                <td className="px-2 py-1 font-mono">{(x.platforms ?? []).join(", ")}</td>
                <td className="px-2 py-1 font-mono">{(x.gpu ?? []).join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {params.length > 0 && (
        <div>
          <div className="mb-1 font-medium">Params</div>
          <table className="w-full">
            <tbody className="divide-y">
              {params.map(([id, p]) => (
                <tr key={id}><td className="px-2 py-1 font-mono">{id}</td><td className="px-2 py-1 text-muted-foreground">{p.type}{p.per_model ? " · per model" : ""}</td><td className="px-2 py-1 font-mono">{paramText(p.default)}</td><td className="px-2 py-1">{p.description}</td></tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="rounded-md border p-2"><ReportView report={d.report} /></div>
      <ManifestFiles d={d} />
    </div>
  );
}

/** The manifest and its templates, read only. */
function ManifestFiles({ d, open = false }: { d: PackageDetail; open?: boolean }) {
  const files: [string, string][] = [["abhoth-engine.json", JSON.stringify(d.package.manifest, null, 2)], ...Object.entries(d.package.files)];
  return (
    <div className="space-y-1.5" data-testid="store-files">
      {files.map(([path, text]) => (
        <details key={path} open={open} className="rounded-md border">
          <summary className="cursor-pointer px-2 py-1 font-mono text-xs">{path} <span className="text-muted-foreground">· {fmtBytes(new TextEncoder().encode(text).length)}</span></summary>
          <pre className="max-h-80 overflow-auto border-t bg-muted p-2 font-mono text-[11px]">{text}</pre>
        </details>
      ))}
    </div>
  );
}

// ── install ──────────────────────────────────────────────────────────────────────────────────

function InstallFlow({ d, summary, onBack, onJob }: { d: PackageDetail; summary?: PackageSummary; onBack: () => void; onJob: () => void }) {
  const qc = useQueryClient();
  const m = d.package.manifest;
  const inv = useQuery({ queryKey: ["router-inventory"], queryFn: routerApi.inventory, refetchOnWindowFocus: false });
  const [server, setServer] = useState("");
  const [card, setCard] = useState("");
  const [read, setRead] = useState(false);
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [engine, setEngine] = useState(m.id);
  const [params, setParams] = useState<Record<string, string>>(() => Object.fromEntries(Object.entries(m.params ?? {}).map(([k, p]) => [k, paramText(p.default)])));
  const [jobId, setJobId] = useState<string | null>(null);
  const fit = useQuery({
    queryKey: ["engine-store-fit", d.package.store, m.id, server],
    queryFn: () => engineStoreApi.fit(d.package.store, m.id, server),
    enabled: !!server,
    refetchOnWindowFocus: false,
    retry: false,
  });
  const chosen: CardFit | undefined = fit.data?.cards.find((c) => c.card === card);
  const items = useMemo(
    () => (chosen ? approvalItems({ fit: chosen.fit, needsApproval: summary?.needsApproval ?? [], trusted: d.package.trusted, methodType: chosen.fit.method_type }) : []),
    [chosen, summary, d.package.trusted],
  );
  useEffect(() => setTicked(new Set()), [server, card]);
  const install = useMutation({
    mutationFn: () => {
      const p = changedParams(m.params, params);
      return engineStoreApi.install({
        store: d.package.store, id: m.id, server, card, approvals: approvalsFrom(items, ticked),
        ...(engine.trim() && engine.trim() !== m.id ? { engine: engine.trim() } : {}),
        ...(Object.keys(p).length ? { params: p } : {}),
      });
    },
    onSuccess: (r) => {
      setJobId(r.job.id);
      qc.setQueryData<{ jobs: StoreJob[] }>(JOBS, (old) => ({ jobs: [r.job, ...(old?.jobs ?? []).filter((j) => j.id !== r.job.id)] }));
    },
  });
  if (jobId) return <JobWatch id={jobId} onDone={onJob} />;
  const ready = !!chosen?.fit.ok && read && allTicked(items, ticked) && /^[a-z0-9][a-z0-9._-]{0,63}$/.test(engine.trim());
  const servers = inv.data?.servers ?? [];
  return (
    <div className="space-y-3 text-xs" data-testid="store-install">
      <Step n={1} title="Where">
        <div className="w-60">
          <Field label="Server">
            <Select value={server} onChange={(s) => { setServer(s); setCard(""); }} options={[["", "Pick…"], ...servers.map((s) => [s.id, serverLabel(s)] as [string, string])]} testid="store-install-server" />
          </Field>
        </div>
        {fit.isFetching && <div className="flex items-center gap-1.5 text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Checking each card…</div>}
        {fit.error && <div className="text-destructive">{errText(fit.error)}</div>}
        {fit.data && (
          <div className="divide-y rounded-md border" data-testid="store-fit">
            {fit.data.cards.length === 0 && <div className="p-2 text-muted-foreground">This server has no cards.</div>}
            {fit.data.cards.map((c) => (
              <label key={c.card} className={cn("flex items-start gap-2 px-2 py-1.5", c.fit.ok ? "cursor-pointer hover:bg-accent/40" : "opacity-80")} data-testid="store-fit-card" data-card={c.card}>
                <input type="radio" name="card" className="mt-0.5" checked={card === c.card} disabled={!c.fit.ok} onChange={() => setCard(c.card)} />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="flex size-5 items-center justify-center rounded bg-muted font-mono text-[11px] font-semibold">{c.card}</span>
                    <span>{c.device === "cpu" ? "CPU" : c.label}{c.vramGib ? ` · ${c.vramGib} GiB` : ""}</span>
                    {c.fit.ok ? <Badge variant="success">fits</Badge> : <Badge variant="destructive">no fit</Badge>}
                    <span className={c.fit.ok ? "" : "text-muted-foreground"}>{fitLine(c)}</span>
                  </div>
                  {c.fit.warnings.map((w, i) => <div key={i} className="text-warning">{w}</div>)}
                  {c.fit.rejections.length > 0 && (
                    <details className="text-muted-foreground">
                      <summary className="cursor-pointer">Why the other methods were refused ({c.fit.rejections.length})</summary>
                      <ul className="list-disc pl-5">{rejectionLines(c.fit).map((l, i) => <li key={i}>{l}</li>)}</ul>
                    </details>
                  )}
                </div>
              </label>
            ))}
          </div>
        )}
      </Step>

      {chosen?.fit.ok && (
        <Step n={2} title="Read the manifest">
          <p className="text-muted-foreground">This is what runs. Method {(chosen.fit.method ?? 0) + 1} ({chosen.fit.method_label ?? chosen.fit.method_type}) is the one this card gets.</p>
          <ManifestFiles d={d} open />
          <Check2 label="I have read the manifest and its templates" checked={read} onChange={setRead} testid="store-install-read" />
        </Step>
      )}

      {chosen?.fit.ok && read && (
        <Step n={3} title="Approve and install">
          {items.length === 0 ? <p className="text-muted-foreground">Nothing to approve: a trusted package in a container with no extra access.</p> : (
            <div className="space-y-1.5" data-testid="store-approvals">
              <p className="text-muted-foreground">Tick each one: the router refuses anything not approved.</p>
              {items.map((i) => (
                <label key={i.key} className={cn("flex items-start gap-2 rounded-md border px-2 py-1.5", i.warning && "border-warning/50 bg-warning/5")} data-testid="store-approval" data-approval={i.key}>
                  <input type="checkbox" className="mt-0.5" checked={ticked.has(i.key)} onChange={(e) => { const t = new Set(ticked); if (e.target.checked) t.add(i.key); else t.delete(i.key); setTicked(t); }} />
                  <span>
                    <span className="font-medium">{i.label}</span>
                    {i.warning && <span className="flex items-start gap-1 text-warning"><AlertTriangle className="mt-0.5 size-3 shrink-0" />{i.warning}</span>}
                  </span>
                </label>
              ))}
            </div>
          )}
          <div className="grid gap-2 sm:grid-cols-2">
            <Field label="Engine id" hint="the [engines.<id>] entry the install adds"><Text value={engine} onChange={setEngine} mono testid="store-install-engine" /></Field>
          </div>
          {Object.keys(m.params ?? {}).length > 0 && (
            <div className="grid gap-2 sm:grid-cols-2">
              {Object.entries(m.params ?? {}).map(([id, p]) => (
                <Field key={id} label={id} hint={p.description}>
                  {p.type === "bool" ? <Select value={params[id] ?? ""} onChange={(v) => setParams({ ...params, [id]: v })} options={[["", "(default)"], "true", "false"]} />
                    : p.type === "enum" && p.choices?.length ? <Select value={params[id] ?? ""} onChange={(v) => setParams({ ...params, [id]: v })} options={[["", "(default)"], ...p.choices]} />
                    : <Text value={params[id] ?? ""} onChange={(v) => setParams({ ...params, [id]: v })} mono />}
                </Field>
              ))}
            </div>
          )}
        </Step>
      )}
      {install.error && <div className="text-destructive" data-testid="store-install-error">{errText(install.error)}</div>}
      <div className="flex justify-between border-t pt-3">
        <Button size="sm" variant="ghost" onClick={onBack}><ArrowLeft /> Details</Button>
        <Button size="sm" onClick={() => install.mutate()} disabled={!ready || install.isPending} data-testid="store-install-go">
          {install.isPending ? <Loader2 className="animate-spin" /> : <Check />} Install on {server || "…"}{card ? ` / ${card}` : ""}
        </Button>
      </div>
    </div>
  );
}

function Step({ n, title, children }: { n: number; title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <h4 className="flex items-center gap-1.5 text-xs font-semibold"><span className="flex size-5 items-center justify-center rounded-full bg-primary/15 font-mono text-[11px] text-primary">{n}</span>{title}</h4>
      <div className="space-y-2 pl-6">{children}</div>
    </section>
  );
}

/** The jobs list, polled while any job runs. */
function useJobs() {
  return useQuery({
    queryKey: JOBS,
    queryFn: engineStoreApi.jobs,
    refetchOnWindowFocus: false,
    refetchInterval: (q) => (anyRunning(q.state.data?.jobs) ? 1500 : false),
  });
}

function JobWatch({ id, onDone }: { id: string; onDone: () => void }) {
  const qc = useQueryClient();
  const jobs = useJobs();
  const job = jobs.data?.jobs.find((j) => j.id === id);
  const state = job?.state;
  useEffect(() => {
    if (state && state !== "running") {
      void qc.invalidateQueries({ queryKey: INSTALLED });
      void qc.invalidateQueries({ queryKey: PKGS });
      void qc.invalidateQueries({ queryKey: ["router-inventory"] });
    }
  }, [state, qc]);
  return (
    <div className="space-y-2" data-testid="store-job-watch">
      {job ? <JobView job={job} open /> : <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {state && state !== "running" && <div className="flex justify-end"><Button size="sm" variant="outline" onClick={onDone}>See installed</Button></div>}
    </div>
  );
}

function JobView({ job, open }: { job: StoreJob; open?: boolean }) {
  return (
    <details open={open} className="rounded-md border text-xs" data-testid="store-job" data-state={job.state}>
      <summary className="flex cursor-pointer flex-wrap items-center gap-1.5 px-2 py-1.5">
        {job.state === "running" ? <Badge variant="default"><Loader2 className="mr-1 size-3 animate-spin" />running</Badge> : job.state === "done" ? <Badge variant="success">done</Badge> : <Badge variant="destructive">failed</Badge>}
        <span className="font-medium">{job.op}</span> <span className="font-mono">{job.package}</span>
        <span className="text-muted-foreground">→ {job.engine} on {job.server}{job.card ? ` / ${job.card}` : ""} · {new Date(job.started * 1000).toLocaleString()}</span>
      </summary>
      {job.error && <div className="border-t px-2 py-1 text-destructive">{job.error}</div>}
      <pre className="max-h-72 overflow-auto border-t bg-muted p-2 font-mono text-[11px]" data-testid="store-job-log">{job.log.length ? job.log.join("\n") : "(no output yet)"}</pre>
    </details>
  );
}

// ── installed ────────────────────────────────────────────────────────────────────────────────

function InstalledPane() {
  const qc = useQueryClient();
  const installed = useQuery({ queryKey: INSTALLED, queryFn: engineStoreApi.installed, refetchOnWindowFocus: false });
  const inv = useQuery({ queryKey: ["router-inventory"], queryFn: routerApi.inventory, refetchOnWindowFocus: false });
  const { packages } = useAllPackages();
  const jobs = useJobs();
  const [removing, setRemoving] = useState<string | null>(null);
  const [uninstall, setUninstall] = useState(false);
  const started = (r: { job: StoreJob }) => {
    qc.setQueryData<{ jobs: StoreJob[] }>(JOBS, (old) => ({ jobs: [r.job, ...(old?.jobs ?? []).filter((j) => j.id !== r.job.id)] }));
  };
  const update = useMutation({ mutationFn: (engine: string) => engineStoreApi.update(engine), onSuccess: started });
  const remove = useMutation({ mutationFn: (a: { engine: string; uninstall: boolean }) => engineStoreApi.remove(a.engine, a.uninstall), onSuccess: started });
  const running = anyRunning(jobs.data?.jobs);
  useEffect(() => {
    if (!running) void qc.invalidateQueries({ queryKey: INSTALLED });
  }, [running, qc]);
  if (installed.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (installed.error) return <div className="text-xs text-destructive">{errText(installed.error)}</div>;
  const rows = installed.data?.installed ?? [];
  const manual = manualEngines(inv.data?.engines ?? [], rows);
  const updateFor = (r: InstalledPkg) => packages.find((p) => p.store === r.store && p.id === r.id)?.update ?? null;
  const err = update.error ?? remove.error;
  return (
    <div className="space-y-3" data-testid="store-installed">
      {err && <div className="text-xs text-destructive">{errText(err)}</div>}
      {rows.length === 0 && <Empty>No packages installed yet.</Empty>}
      {installedByServer(rows).map(([server, list]) => (
        <div key={server} className="rounded-md border" data-testid="store-installed-server" data-server={server}>
          <div className="border-b px-3 py-2 text-sm font-medium">{server}</div>
          <div className="divide-y">
            {list.map((r) => {
              const up = updateFor(r);
              return (
                <div key={`${r.id}/${r.version}`} className="space-y-1 px-3 py-2 text-xs" data-testid="store-installed-row" data-package={r.id}>
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="font-medium">{r.id}</span>
                    <span className="font-mono text-muted-foreground">{r.version}</span>
                    <Badge variant={isHostRun(r.method_type) ? "warning" : "outline"}>method {r.method + 1}: {r.method_type}</Badge>
                    <Badge variant="outline">{r.store === "my" ? "My packages" : r.store}</Badge>
                    {up && <Badge variant="default">update: {up}</Badge>}
                    <span className="text-muted-foreground">installed {new Date(r.at * 1000).toLocaleString()}</span>
                  </div>
                  {r.pins.length > 0 && (
                    <details>
                      <summary className="cursor-pointer text-muted-foreground">Pins ({r.pins.length})</summary>
                      <ul className="mt-1 space-y-0.5 pl-4 font-mono text-[11px]">{r.pins.map((p) => <li key={p} className="break-all">{p}</li>)}</ul>
                    </details>
                  )}
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="text-muted-foreground">Engines:</span>
                    {r.engines.length === 0 && <span className="text-muted-foreground">none use it</span>}
                    {r.engines.map((e) => (
                      <span key={e} className="flex items-center gap-1 rounded-md border px-1.5 py-0.5">
                        <span className="font-mono">{e}</span>
                        {up && <Button size="sm" variant="ghost" disabled={running} onClick={() => update.mutate(e)} data-testid="store-update-engine"><RefreshCw /> Update</Button>}
                        <Button size="sm" variant="ghost" disabled={running} onClick={() => { setUninstall(false); setRemoving(e); }} data-testid="store-remove-engine"><Trash2 /> Remove</Button>
                      </span>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      ))}
      {manual.length > 0 && (
        <div className="rounded-md border" data-testid="store-manual">
          <div className="border-b px-3 py-2">
            <div className="text-sm font-medium">Manual engines</div>
            <div className="text-xs text-muted-foreground">Added by hand in Inventory, with no package: they work exactly the same. Edit them there.</div>
          </div>
          <div className="divide-y">
            {manual.map((e) => (
              <div key={e.id} className="flex flex-wrap items-center gap-1.5 px-3 py-1.5 text-xs" data-testid="store-manual-engine">
                <span className="font-mono">{e.id}</span>
                {e.title && e.title !== e.id && <span>{e.title}</span>}
                <Badge variant="outline">{e.driver}</Badge>
                {e.kinds.map((k) => <Badge key={k} variant="secondary">{kindLabel(k)}</Badge>)}
                {e.cards.length > 0 && <span className="text-muted-foreground">cards {e.cards.join(", ")}</span>}
              </div>
            ))}
          </div>
        </div>
      )}
      {(jobs.data?.jobs.length ?? 0) > 0 && (
        <div className="space-y-1.5">
          <div className="text-xs font-medium">Recent jobs</div>
          {jobs.data!.jobs.slice(0, 10).map((j) => <JobView key={j.id} job={j} open={j.state === "running"} />)}
        </div>
      )}
      <ConfirmDialog
        open={!!removing}
        title={`Remove the engine ${removing}?`}
        body={
          <div className="space-y-2">
            <p>The router stops it and removes its [engines.{removing}] entry. Models that use it fall back to their other engines.</p>
            <Check2 label="Also uninstall the files (venv, downloads, image)" checked={uninstall} onChange={setUninstall} testid="store-remove-uninstall" />
          </div>
        }
        confirm="Remove"
        destructive
        onCancel={() => setRemoving(null)}
        onConfirm={() => { const e = removing!; setRemoving(null); remove.mutate({ engine: e, uninstall }); }}
      />
    </div>
  );
}

// ── stores ───────────────────────────────────────────────────────────────────────────────────

function StoresPane() {
  const qc = useQueryClient();
  const stores = useQuery({ queryKey: STORES, queryFn: engineStoreApi.stores, refetchOnWindowFocus: false });
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  const [trusted, setTrusted] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [del, setDel] = useState<string | null>(null);
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: STORES });
    void qc.invalidateQueries({ queryKey: PKGS });
  };
  const add = useMutation({
    mutationFn: () => engineStoreApi.addStore(url.trim(), trusted, name.trim() || undefined),
    onSuccess: () => { setUrl(""); setName(""); setTrusted(false); refresh(); },
  });
  const remove = useMutation({ mutationFn: (id: string) => engineStoreApi.removeStore(id), onSuccess: refresh });
  const go = () => {
    const e = validStoreUrl(url);
    setErr(e);
    if (!e) add.mutate();
  };
  return (
    <div className="space-y-3" data-testid="store-stores">
      {stores.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {stores.error && <div className="text-xs text-destructive">{errText(stores.error)}</div>}
      <div className="divide-y rounded-md border">
        {(stores.data?.stores ?? []).map((s) => (
          <div key={s.id} className="flex flex-wrap items-center gap-2 px-3 py-2 text-xs" data-testid="store-row" data-store={s.id}>
            <Store className="size-4 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="font-medium">{s.name}</span>
                <span className="font-mono text-muted-foreground">{s.id}</span>
                {s.builtin && <Badge variant="secondary">built in</Badge>}
                {s.trusted ? <Badge variant="success">trusted</Badge> : <Badge variant="warning">untrusted</Badge>}
                <span className="text-muted-foreground">{s.packages} package{s.packages === 1 ? "" : "s"}</span>
              </div>
              {s.url && <div className="truncate font-mono text-muted-foreground">{s.url}</div>}
              {s.error && <div className="text-destructive">{s.error}</div>}
            </div>
            {!s.builtin && <Button size="sm" variant="ghost" onClick={() => setDel(s.id)} data-testid="store-remove" aria-label={`Remove ${s.name}`}><Trash2 /></Button>}
          </div>
        ))}
      </div>
      <div className="space-y-2 rounded-md border p-3" data-testid="store-add">
        <div className="text-xs font-medium">Add a store</div>
        <p className="text-xs text-muted-foreground">A static index.json over https, or a GitHub repo URL. Stores hold manifests only: every download is pinned and checked at install. Packages from an untrusted store need an explicit approval each time they are installed.</p>
        <div className="grid gap-2 sm:grid-cols-[2fr_1fr]">
          <Field label="URL"><Text value={url} onChange={setUrl} mono placeholder="https://github.com/owner/abhoth-store" testid="store-add-url" /></Field>
          <Field label="Name (optional)"><Text value={name} onChange={setName} placeholder="Community" /></Field>
        </div>
        <Check2 label="Trusted (only for a store you control or fully trust)" checked={trusted} onChange={setTrusted} testid="store-add-trusted" />
        {(err || add.error) && <div className="text-xs text-destructive">{err ?? errText(add.error)}</div>}
        <div className="flex justify-end"><Button size="sm" onClick={go} disabled={add.isPending} data-testid="store-add-go">{add.isPending ? <Loader2 className="animate-spin" /> : <Plus />} Add store</Button></div>
      </div>
      <ConfirmDialog
        open={!!del}
        title={`Remove the store ${del}?`}
        body="Its packages leave the list. Engines already installed from it keep working."
        confirm="Remove"
        destructive
        onCancel={() => setDel(null)}
        onConfirm={() => { const id = del!; setDel(null); remove.mutate(id); }}
      />
    </div>
  );
}
