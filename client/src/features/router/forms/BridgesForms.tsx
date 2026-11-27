// Bridges (ROUTER-MEDIA-PLAN.md 11): machines that dial this router's /bridge/ws and become
// servers it manages. Each describes itself (cards, engines); here an admin mints its token
// (shown once), turns it off or on, sets its priority, and looks at what it runs. A bridge's
// server, cards and engines are read-only here: they come from the bridge.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cable, Copy, Cpu, KeyRound, Loader2, MemoryStick, Play, RefreshCw, Square, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { routerApi, type BridgeCard, type BridgeEngineState, type BridgeView } from "../routerApi";
import { ago } from "../routerModel";
import { Field, SectionHead, Text, errText, input } from "./formKit";

export function BridgesForms() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["router-bridges"], queryFn: routerApi.bridges, refetchInterval: 5000 });
  const [id, setId] = useState("");
  const [made, setMade] = useState<{ id: string; token: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);
  const refresh = () => void qc.invalidateQueries({ queryKey: ["router-bridges"] });
  const mint = useMutation({
    mutationFn: (v: { id: string; rotate: boolean }) => routerApi.mintBridgeToken(v.id, v.rotate),
    onSuccess: (r) => { setMade(r); setId(""); setError(null); refresh(); },
    onError: (e) => setError(errText(e)),
  });
  const revoke = useMutation({ mutationFn: (b: string) => routerApi.revokeBridge(b), onSuccess: refresh, onError: (e) => setError(errText(e)) });
  if (q.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (q.isError) return <div className="text-xs text-destructive">{errText(q.error)}</div>;
  const bridges = q.data?.bridges ?? [];
  const idOk = /^[a-z0-9_-]{1,32}$/.test(id);
  return (
    <section className="space-y-3" data-testid="bridges">
      <SectionHead
        title="Bridges"
        blurb="A bridge makes another machine a server this router manages: it dials this router (no port opens on it), describes its cards and engines, and starts and stops engines for it. Run `abhoth bridge --config bridge.toml`, the abhoth-bridge image, or turn on Bridge mode in the Abhoth desktop app."
      />
      <div className="flex flex-wrap items-end gap-2">
        <Field label="New bridge's id" hint="a-z, 0-9, - and _ (it becomes the server's id)">
          <Text value={id} onChange={(v) => setId(v.trim().toLowerCase())} placeholder="e.g. truenas or pc" testid="bridge-new-id" />
        </Field>
        <Button size="sm" disabled={!idOk || mint.isPending} onClick={() => mint.mutate({ id, rotate: false })} data-testid="bridge-mint">
          {mint.isPending ? <Loader2 className="animate-spin" /> : <KeyRound />} Make token
        </Button>
      </div>
      {error && <div className="text-xs text-destructive">{error}</div>}
      {made && (
        <div className="space-y-1 rounded-md border border-warning/50 p-2 text-xs" data-testid="bridge-token-made">
          <div className="font-medium">Token for bridge {made.id}: copy it now, it is not shown again. Put it in the bridge's token file (mode 600) or the desktop app's Bridge token setting.</div>
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 break-all rounded bg-muted px-2 py-1">{made.token}</code>
            <Button size="sm" variant="ghost" onClick={() => void navigator.clipboard?.writeText(made.token)} aria-label="Copy"><Copy /></Button>
            <Button size="sm" variant="ghost" onClick={() => setMade(null)}>Done</Button>
          </div>
        </div>
      )}
      {bridges.length === 0 && <div className="rounded-md border px-3 py-2 text-xs text-muted-foreground">No bridges yet. Make a token above, then start the bridge with it.</div>}
      {bridges.map((b) => (
        <BridgeCardView key={b.id} b={b} onRotate={() => mint.mutate({ id: b.id, rotate: true })} onRevoke={() => setRevoking(b.id)} onChanged={refresh} />
      ))}
      {revoking && (
        <ConfirmDialog
          open
          title={`Remove bridge ${revoking}?`}
          body="Its token stops working at once and it disconnects. Its server and cards leave the inventory."
          confirm="Remove"
          destructive
          onConfirm={() => { revoke.mutate(revoking); setRevoking(null); }}
          onCancel={() => setRevoking(null)}
        />
      )}
    </section>
  );
}

function BridgeCardView({ b, onRotate, onRevoke, onChanged }: { b: BridgeView; onRotate: () => void; onRevoke: () => void; onChanged: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [priority, setPriority] = useState(String(b.priority));
  const settings = useMutation({
    mutationFn: (s: { enabled?: boolean; priority?: number }) => routerApi.bridgeSettings(b.id, s),
    onSuccess: () => { setError(null); onChanged(); },
    onError: (e) => setError(errText(e)),
  });
  const status = useQuery({
    queryKey: ["router-bridge-status", b.id],
    queryFn: () => routerApi.bridgeCmd<{ engines: BridgeEngineState[]; free_ram_gib?: number | null }>(b.id, "status"),
    enabled: b.online,
    refetchInterval: b.online ? 10000 : false,
  });
  const cmd = useMutation({
    mutationFn: (v: { op: string; engine: string; card: string }) => routerApi.bridgeCmd(b.id, v.op, { engine: v.engine, card: v.card }),
    onSuccess: () => { setError(null); void status.refetch(); },
    onError: (e) => setError(errText(e)),
  });
  return (
    <div className="space-y-2 rounded-md border p-3 text-xs" data-testid={`bridge-${b.id}`}>
      <div className="flex flex-wrap items-center gap-2">
        <Cable className="size-4 text-muted-foreground" />
        <span className="font-medium">{b.title || b.id}</span>
        <span className="font-mono text-muted-foreground">{b.id}</span>
        {b.online ? <Badge>online</Badge> : <Badge variant="outline">{b.seen ? "offline" : "never connected"}</Badge>}
        {!b.enabled && <Badge variant="outline">disabled</Badge>}
        {!b.token && <Badge variant="outline">no token</Badge>}
        <span className="text-muted-foreground">
          {b.online && b.since ? `connected ${ago(b.since * 1000)}` : b.lastSeen ? `last seen ${ago(b.lastSeen * 1000)}` : ""}
          {b.version ? ` · abhoth ${b.version}` : ""}
          {b.os ? ` · ${b.os}/${b.arch}` : ""}
          {b.online && b.streams ? ` · ${b.streams} open` : ""}
        </span>
        <span className="ml-auto flex items-center gap-1">
          <label className="flex items-center gap-1">
            <input type="checkbox" checked={b.enabled} onChange={(e) => settings.mutate({ enabled: e.target.checked })} data-testid={`bridge-enabled-${b.id}`} /> enabled
          </label>
          <label className="flex items-center gap-1" title="Higher goes first among bridges that can take a request">
            priority
            <input className={`${input} h-7 max-w-16`} value={priority} onChange={(e) => setPriority(e.target.value)}
              onBlur={() => { const n = Number(priority); if (Number.isFinite(n) && n !== b.priority) settings.mutate({ priority: Math.round(n) }); }} />
          </label>
          <Button size="sm" variant="ghost" onClick={onRotate} title="Make a new token (the old one stops working)"><RefreshCw /></Button>
          <Button size="sm" variant="ghost" onClick={onRevoke} aria-label="Remove"><Trash2 /></Button>
        </span>
      </div>
      <div className="text-muted-foreground">Managed by bridge {b.id}: its server, cards and engines come from the bridge and are read-only here.</div>
      {error && <div className="text-destructive">{error}</div>}
      {(b.issues ?? []).map((i) => <div key={i} className="text-warning">{i}</div>)}
      {b.cards && b.cards.length > 0 && (
        <div className="grid gap-2 sm:grid-cols-2">
          {b.cards.map((c) => <CardBox key={c.id} c={c} />)}
        </div>
      )}
      {b.engines && b.engines.length > 0 && (
        <div className="divide-y rounded-md border">
          {b.engines.map((e) => {
            const states = (status.data?.engines ?? []).filter((s) => s.engine === e.id);
            return (
              <div key={e.id} className="flex flex-wrap items-center gap-2 px-2 py-1.5">
                <span className="font-medium">{e.id}</span>
                <span className="text-muted-foreground">{e.kinds.join(", ")} · {e.method ?? "manual"} · {e.lifecycle.replace("_", " ")}{e.idle_stop_seconds ? `, stops after ${Math.round(e.idle_stop_seconds / 60)} min idle` : ""}</span>
                <span className="ml-auto flex flex-wrap items-center gap-1">
                  {Object.entries(e.ports).map(([card, port]) => {
                    const s = states.find((x) => x.card === card);
                    const running = s?.state === "running";
                    return (
                      <span key={card} className="flex items-center gap-1 rounded border px-1.5 py-0.5">
                        <span className="font-mono">{card}:{port}</span>
                        <span className={running ? "text-success" : s?.state === "failed" ? "text-destructive" : "text-muted-foreground"} title={s?.error ?? ""}>{e.method === "external" ? "runs outside the bridge" : s?.state ?? "?"}</span>
                        {b.online && e.method !== "external" && (running
                          ? <Button size="sm" variant="ghost" className="h-5 px-1" onClick={() => cmd.mutate({ op: "stop", engine: e.id, card })} aria-label="Stop"><Square className="size-3" /></Button>
                          : <Button size="sm" variant="ghost" className="h-5 px-1" disabled={cmd.isPending} onClick={() => cmd.mutate({ op: "start", engine: e.id, card })} aria-label="Start"><Play className="size-3" /></Button>)}
                      </span>
                    );
                  })}
                </span>
              </div>
            );
          })}
        </div>
      )}
      {b.online && status.data?.free_ram_gib != null && <div className="text-muted-foreground">RAM free on the host: {status.data.free_ram_gib.toFixed(1)} GiB · {b.files ?? 0} model files listed</div>}
    </div>
  );
}

function CardBox({ c }: { c: BridgeCard }) {
  const Icon = c.device === "cpu" ? Cpu : MemoryStick;
  return (
    <div className="space-y-0.5 rounded-md border px-2 py-1.5">
      <div className="flex items-center gap-1.5">
        <Icon className="size-3.5 text-muted-foreground" />
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
