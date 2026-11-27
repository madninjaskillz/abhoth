// Bridges (ROUTER-MEDIA-PLAN.md 11): the tokens of machines that dial this router's /bridge/ws.
// Here an admin mints a token (shown once), rotates or removes it. What each machine offers,
// whether the router uses it and its priority are under Inventory → Bridged machines
// (BridgedMachines.tsx, MACHINE.md 9), apart from the router's own servers.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cable, Copy, KeyRound, Loader2, RefreshCw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { routerApi, type BridgeView } from "../routerApi";
import { Field, SectionHead, Text, errText } from "./formKit";
import { BridgeStatusBadges, BridgedMachineDialog, bridgeSeen } from "./BridgedMachines";

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
        blurb="A bridge brings another machine's cards and engines to this router: it dials in (no port opens on it), says what it has, and starts and stops its engines for the router. Make its token here, then run `abhoth bridge --config bridge.toml`, the abhoth-bridge image, or turn on Bridge mode in the Abhoth desktop app. The machines themselves are under Inventory → Bridged machines."
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
        <BridgeTokenRow key={b.id} b={b} onRotate={() => mint.mutate({ id: b.id, rotate: true })} onRevoke={() => setRevoking(b.id)} />
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

function BridgeTokenRow({ b, onRotate, onRevoke }: { b: BridgeView; onRotate: () => void; onRevoke: () => void }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-md border p-3 text-xs" data-testid={`bridge-${b.id}`}>
      <Cable className="size-4 text-muted-foreground" />
      <span className="font-medium">{b.title || b.id}</span>
      <span className="font-mono text-muted-foreground">{b.id}</span>
      {b.token ? <Badge variant="secondary">token made {new Date(b.token.created * 1000).toLocaleDateString()}</Badge> : <Badge variant="outline">no token</Badge>}
      <BridgeStatusBadges b={b} />
      <span className="text-muted-foreground">{bridgeSeen(b)}</span>
      <span className="ml-auto flex items-center gap-1">
        {b.seen && <Button size="sm" variant="ghost" onClick={() => setOpen(true)} data-testid={`bridge-open-${b.id}`}>Details…</Button>}
        <Button size="sm" variant="ghost" onClick={onRotate} title="Make a new token (the old one stops working)"><RefreshCw /></Button>
        <Button size="sm" variant="ghost" onClick={onRevoke} aria-label="Remove"><Trash2 /></Button>
      </span>
      {open && <BridgedMachineDialog b={b} onClose={() => setOpen(false)} />}
    </div>
  );
}
