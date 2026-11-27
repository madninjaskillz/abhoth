// Bridged machines (MACHINE.md 9): machines an `abhoth bridge` or a desktop app in Bridge mode
// brings in. They are a group of their own, apart from the router's servers: each describes its
// cards and engines, and a router admin turns it on or off, sets its priority and picks which
// of its shared engines this router uses. When the machine allows remote config, its own
// inventory is edited here too: the bridge checks, backs up and applies each change itself.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cable, Cpu, Loader2, Lock, MemoryStick, Play, Square, Unlock } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { RouterApiError, routerApi, type BridgeCard, type BridgeEngineState, type BridgeView } from "../routerApi";
import { ago } from "../routerModel";
import { area, errText, input } from "./formKit";

export const BRIDGES_KEY = ["router-bridges"];

/** Last seen / connected, in a few words. */
export function bridgeSeen(b: BridgeView): string {
  if (b.online && b.since) return `connected ${ago(b.since * 1000)}`;
  if (b.lastSeen) return `last seen ${ago(b.lastSeen * 1000)}`;
  return b.seen ? "" : "never connected";
}

export function BridgeStatusBadges({ b }: { b: BridgeView }) {
  return (
    <>
      {b.online ? <Badge variant="success">online</Badge> : <Badge variant="outline">{b.seen ? "offline" : "never connected"}</Badge>}
      {!b.enabled && <Badge variant="outline">disabled</Badge>}
      <Badge variant={b.remoteConfig ? "default" : "outline"} data-testid="bridge-managed-badge">
        {b.remoteConfig ? <Unlock className="mr-1 size-3" /> : <Lock className="mr-1 size-3" />}
        Managed by bridge · {b.remoteConfig ? "remote config allowed" : "read-only"}
      </Badge>
    </>
  );
}

/** Inventory → Bridged machines: one row per bridge that has a token, apart from the servers. */
export function BridgedMachinesPane({ onTokens }: { onTokens?: () => void }) {
  const q = useQuery({ queryKey: BRIDGES_KEY, queryFn: routerApi.bridges, refetchInterval: 5000 });
  const [open, setOpen] = useState<string | null>(null);
  if (q.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (q.isError) return <div className="text-xs text-destructive">{errText(q.error)}</div>;
  const bridges = (q.data?.bridges ?? []).filter((b) => b.token || b.seen);
  const shown = bridges.find((b) => b.id === open);
  return (
    <div className="space-y-3" data-testid="inv-bridged-pane">
      <div>
        <h3 className="text-sm font-semibold">Migo machines</h3>
        <p className="text-xs text-muted-foreground">
          Machines that dial Sothoth through Migo (formerly the bridge) and offer their cards and engines: a TrueNAS box running the abhoth-migo image, or a PC with Migo mode on in the desktop app. They are not servers this router configures: the machine says what it has, you choose whether and how much the router uses it.
        </p>
      </div>
      {bridges.length === 0 && (
        <div className="rounded-md border border-dashed p-4 text-center text-xs text-muted-foreground" data-testid="inv-bridged-empty">
          <div className="font-medium text-foreground">No Migo machines yet.</div>
          <div className="mt-1">
            How to bring one in: make a token in{" "}
            {onTokens ? <button className="underline" onClick={onTokens}>Migo</button> : "Migo"}
            , then run <code>abhoth migo --config migo.toml</code> (or the abhoth-migo image) with it, or paste it into the desktop app's Settings → This machine → Migo mode. The machine appears here when it connects.
          </div>
        </div>
      )}
      {bridges.map((b) => <BridgedRow key={b.id} b={b} onOpen={() => setOpen(b.id)} />)}
      {shown && <BridgedMachineDialog b={shown} onClose={() => setOpen(null)} />}
    </div>
  );
}

function BridgedRow({ b, onOpen }: { b: BridgeView; onOpen: () => void }) {
  const off = new Set(b.enginesOff ?? []);
  const problems = Object.keys(b.engineProblems ?? {}).length;
  return (
    <button className="block w-full rounded-md border text-left hover:border-primary/60" onClick={onOpen} data-testid="inv-bridged" data-bridge={b.id}>
      <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <Cable className="size-4 text-muted-foreground" />
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium">{b.title || b.id} <span className="font-mono text-xs text-muted-foreground">{b.id}</span></div>
          <div className="truncate text-xs text-muted-foreground">
            {bridgeSeen(b)}{b.version ? ` · abhoth ${b.version}` : ""} · priority {b.priority}
            {b.engines ? ` · ${b.engines.filter((e) => !off.has(e.id)).length} of ${b.engines.length} engines in use` : ""}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-1"><BridgeStatusBadges b={b} /></div>
      </div>
      <div className="flex flex-wrap gap-1.5 px-3 py-2">
        {(b.cards ?? []).length === 0 && <span className="text-xs text-muted-foreground">no cards described yet</span>}
        {(b.cards ?? []).map((c) => (
          <span key={c.id} className="flex items-center gap-1 rounded border px-1.5 py-0.5 text-xs">
            <Badge variant="secondary">{c.device === "cpu" ? "CPU" : "GPU"}</Badge>
            <span className="font-mono">{c.global}</span>
            <span className="text-muted-foreground">{c.device === "cpu" ? `${c.ram_gib ?? "?"} GiB RAM` : `${c.vram_gib ?? "?"} GiB`}</span>
          </span>
        ))}
        {(b.engines ?? []).map((e) => (
          <span key={e.id} className={cn("rounded border px-1.5 py-0.5 text-xs", off.has(e.id) && "text-muted-foreground line-through")}>{e.id}</span>
        ))}
        {problems > 0 && <Badge variant="warning">{problems} engine{problems > 1 ? "s" : ""} cannot start</Badge>}
      </div>
    </button>
  );
}

/** The server detail for a bridged machine: what it offers, what the router uses, and (when the
 * machine allows it) its own inventory. */
export function BridgedMachineDialog({ b, onClose }: { b: BridgeView; onClose: () => void }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const [priority, setPriority] = useState(String(b.priority));
  const refresh = () => void qc.invalidateQueries({ queryKey: BRIDGES_KEY });
  const settings = useMutation({
    mutationFn: (s: { enabled?: boolean; priority?: number; enginesOff?: string[] }) => routerApi.bridgeSettings(b.id, s),
    onSuccess: () => { setError(null); refresh(); void qc.invalidateQueries({ queryKey: ["router-inventory"] }); },
    onError: (e) => setError(errText(e)),
  });
  const status = useQuery({
    queryKey: ["router-bridge-status", b.id],
    queryFn: () => routerApi.bridgeCmd<{ engines: BridgeEngineState[]; free_ram_gib?: number | null; ram?: { text?: string } }>(b.id, "status"),
    enabled: b.online,
    refetchInterval: b.online ? 10000 : false,
  });
  const cmd = useMutation({
    mutationFn: (v: { op: string; engine: string; card: string }) => routerApi.bridgeCmd(b.id, v.op, { engine: v.engine, card: v.card }),
    onSuccess: () => { setError(null); void status.refetch(); },
    onError: (e) => setError(errText(e)),
  });
  const off = new Set(b.enginesOff ?? []);
  const toggle = (id: string) => {
    const next = new Set(off);
    if (next.has(id)) next.delete(id); else next.add(id);
    settings.mutate({ enginesOff: [...next].sort() });
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto" data-testid="inv-bridged-detail">
        <DialogTitle className="flex flex-wrap items-center gap-2">
          <Cable className="size-4" /> {b.title || b.id} <span className="font-mono text-xs text-muted-foreground">{b.id}</span>
        </DialogTitle>
        <DialogDescription>
          {bridgeSeen(b)}{b.version ? ` · abhoth ${b.version}` : ""}{b.os ? ` · ${b.os}/${b.arch}` : ""}{b.online && b.streams ? ` · ${b.streams} open` : ""}
        </DialogDescription>
        <div className="space-y-3 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <BridgeStatusBadges b={b} />
            <span className="ml-auto flex items-center gap-3">
              <label className="flex items-center gap-1">
                <input type="checkbox" checked={b.enabled} onChange={(e) => settings.mutate({ enabled: e.target.checked })} data-testid="bridged-enabled" /> enabled
              </label>
              <label className="flex items-center gap-1" title="Higher goes first among bridges that can take a request">
                priority
                <input className={`${input} h-7 max-w-16`} value={priority} onChange={(e) => setPriority(e.target.value)} data-testid="bridged-priority"
                  onBlur={() => { const n = Number(priority); if (Number.isFinite(n) && n !== b.priority) settings.mutate({ priority: Math.round(n) }); }} />
              </label>
            </span>
          </div>
          {error && <div className="text-destructive">{error}</div>}
          {(b.issues ?? []).map((i) => <div key={i} className="text-warning">{i}</div>)}
          <section className="space-y-1">
            <h4 className="font-semibold">Cards</h4>
            {(b.cards ?? []).length === 0 && <div className="text-muted-foreground">No cards described yet.</div>}
            <div className="grid gap-2 sm:grid-cols-2">{(b.cards ?? []).map((c) => <CardBox key={c.id} c={c} />)}</div>
            {b.online && (status.data?.ram?.text || status.data?.free_ram_gib != null) && (
              <div className="text-muted-foreground">RAM for engines: {status.data?.ram?.text ?? `${status.data?.free_ram_gib?.toFixed(1)} GiB`}</div>
            )}
          </section>
          <section className="space-y-1">
            <h4 className="font-semibold">Engines it shares</h4>
            <p className="text-muted-foreground">Untick one to keep this router off it (the machine still runs it for itself).</p>
            {(b.engines ?? []).length === 0 && <div className="text-muted-foreground">It shares no engines.</div>}
            <div className="divide-y rounded-md border">
              {(b.engines ?? []).map((e) => {
                const states = (status.data?.engines ?? []).filter((s) => s.engine === e.id);
                const problem = b.engineProblems?.[e.id];
                return (
                  <div key={e.id} className="space-y-0.5 px-2 py-1.5" data-testid="bridged-engine" data-engine={e.id}>
                    <div className="flex flex-wrap items-center gap-2">
                      <label className="flex items-center gap-1 font-medium">
                        <input type="checkbox" checked={!off.has(e.id)} disabled={settings.isPending} onChange={() => toggle(e.id)} data-testid="bridged-engine-use" /> {e.id}
                      </label>
                      <span className="text-muted-foreground">{e.kinds.join(", ")} · {e.package ?? e.method ?? "manual"} · {e.lifecycle.replace("_", " ")}</span>
                      <span className="ml-auto flex flex-wrap items-center gap-1">
                        {Object.entries(e.ports).map(([card, port]) => {
                          const s = states.find((x) => x.card === card);
                          const running = s?.state === "running";
                          return (
                            <span key={card} className="flex items-center gap-1 rounded border px-1.5 py-0.5">
                              <span className="font-mono">{card}:{port}</span>
                              <span className={running ? "text-success" : s?.state === "failed" ? "text-destructive" : "text-muted-foreground"} title={s?.error ?? ""}>{e.method === "external" ? "runs outside the bridge" : s?.state ?? "?"}</span>
                              {b.online && e.method !== "external" && !off.has(e.id) && (running
                                ? <Button size="sm" variant="ghost" className="h-5 px-1" onClick={() => cmd.mutate({ op: "stop", engine: e.id, card })} aria-label="Stop"><Square className="size-3" /></Button>
                                : <Button size="sm" variant="ghost" className="h-5 px-1" disabled={cmd.isPending} onClick={() => cmd.mutate({ op: "start", engine: e.id, card })} aria-label="Start"><Play className="size-3" /></Button>)}
                            </span>
                          );
                        })}
                      </span>
                    </div>
                    {problem && <div className="text-destructive" data-testid="bridged-engine-problem">Cannot start: {problem}</div>}
                  </div>
                );
              })}
            </div>
          </section>
          {(b.models ?? []).length > 0 && (
            <section className="space-y-1">
              <h4 className="font-semibold">Models it shares</h4>
              <div className="flex flex-wrap gap-1">
                {(b.models ?? []).map((m) => <span key={m.id} className="rounded border px-1.5 py-0.5"><span className="font-mono">{m.id}</span> <span className="text-muted-foreground">{m.kind} · {m.engines.join(", ")}</span></span>)}
              </div>
            </section>
          )}
          <MachineConfig b={b} />
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** The machine's own router.toml, edited through its bridge (remote config), or why not. */
function MachineConfig({ b }: { b: BridgeView }) {
  const allowed = !!b.remoteConfig && b.online;
  const doc = useQuery({ queryKey: ["bridge-machine-config", b.id], queryFn: () => routerApi.bridgeMachineConfig(b.id), enabled: allowed, refetchOnWindowFocus: false });
  const [text, setText] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const [note, setNote] = useState<{ tone: "ok" | "warn" | "err"; text: string } | null>(null);
  const save = useMutation({
    mutationFn: () => routerApi.saveBridgeMachineConfig(b.id, text ?? "", doc.data!.rev, reason),
    onSuccess: (r) => {
      if (r.pending) setNote({ tone: "warn", text: `Waiting for approval on the machine: ${(r.reasons ?? []).join("; ")}` });
      else setNote({ tone: "ok", text: "Saved on the machine (with a backup; its user can undo it)." });
      setText(null);
      void doc.refetch();
    },
    onError: (e) => {
      if (e instanceof RouterApiError && e.status === 412) setNote({ tone: "err", text: "The machine's config changed since you loaded it: reload and edit again." });
      else setNote({ tone: "err", text: errText(e) });
    },
  });
  if (!b.remoteConfig) {
    return (
      <section className="rounded-md border border-dashed p-2 text-muted-foreground" data-testid="bridged-readonly">
        <Lock className="mr-1 inline size-3" /> Read-only: this machine doesn't allow remote config. Its user can turn on <b>Migo: allow Sothoth to config</b> in the desktop app's settings.
      </section>
    );
  }
  if (!b.online) return <section className="text-muted-foreground">The machine is offline: its config can be edited when it connects.</section>;
  return (
    <section className="space-y-1" data-testid="bridged-config">
      <h4 className="font-semibold">Its inventory (router.toml on the machine)</h4>
      <p className="text-muted-foreground">Migo checks each change, keeps a backup and logs it as yours. Turning on anything that needs approval there (privileged containers, host network or folders, host processes) waits for its user to approve. The router address, the token and this setting cannot be changed from here.</p>
      {doc.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {doc.isError && <div className="text-destructive">{errText(doc.error)}</div>}
      {doc.data && (
        <>
          <textarea className={cn(area, "min-h-[220px]")} value={text ?? doc.data.text} onChange={(e) => setText(e.target.value)} spellCheck={false} data-testid="bridged-config-text" />
          <div className="flex flex-wrap items-center gap-2">
            <input className={`${input} max-w-sm`} placeholder="Why (for the machine's change log)" value={reason} onChange={(e) => setReason(e.target.value)} />
            <Button size="sm" disabled={text == null || text === doc.data.text || save.isPending} onClick={() => save.mutate()} data-testid="bridged-config-save">
              {save.isPending && <Loader2 className="animate-spin" />} Check and save on the machine
            </Button>
            <Button size="sm" variant="ghost" onClick={() => { setText(null); setNote(null); void doc.refetch(); }}>Reload</Button>
            <span className="font-mono text-muted-foreground">rev {doc.data.rev}</span>
          </div>
        </>
      )}
      {note && <div className={note.tone === "ok" ? "text-success" : note.tone === "warn" ? "text-warning" : "text-destructive"} data-testid="bridged-config-note">{note.text}</div>}
    </section>
  );
}

function CardBox({ c }: { c: BridgeCard }) {
  const Icon = c.device === "cpu" ? Cpu : MemoryStick;
  return (
    <div className="space-y-0.5 rounded-md border px-2 py-1.5">
      <div className="flex items-center gap-1.5">
        <Icon className="size-3.5 text-muted-foreground" />
        <Badge variant="secondary">{c.device === "cpu" ? "CPU" : "GPU"}</Badge>
        <span className="font-mono">{c.global}</span>
        <span>{c.label}</span>
        {c.shared && <Badge variant="outline">shared</Badge>}
      </div>
      <div className="text-muted-foreground">
        {c.device === "cpu" ? `${c.cores ?? "?"} cores · ${c.ram_gib ?? "?"} GiB RAM` : `${c.vram_gib ?? "?"} GiB VRAM`}
        {Object.keys(c.engines ?? {}).length > 0 && " · " + Object.entries(c.engines).map(([k, v]) => `${k}: ${v.join(", ")}`).join("; ")}
      </div>
    </div>
  );
}
