// Migo Bridge on this PC (MACHINE.md, D155): its link to the Sothoth Router, what it shares,
// changes a Sothoth Router admin asked for that wait for approval, and the changes they made
// (each undoable). Shown on This Machine (Sharing) and in Settings → Migo Bridge.
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Check, History, Loader2, Undo2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/misc";
import { ago } from "@/features/router/routerModel";
import { machineApi, type MachineStatus } from "./machineApi";

export const MACHINE_STATUS = ["machine-status"];
const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function BridgeBadge({ s }: { s: MachineStatus }) {
  if (!s.bridgeMode) return <Badge variant="outline">runs its engines itself</Badge>;
  if (s.bridge?.connected) return <Badge variant="success" data-testid="machine-bridge-online">Migo Bridge to {s.bridge.router ?? s.bridgeRouter} · online</Badge>;
  return <Badge variant="warning" title={s.bridge?.lastError ?? ""}>Migo Bridge · offline{s.bridge?.lastError ? `: ${s.bridge.lastError}` : ""}</Badge>;
}

/** Settings → Migo Bridge: the live part (status, sharing, approvals, changes) under its settings. */
export function MigoPanel() {
  const st = useQuery({ queryKey: MACHINE_STATUS, queryFn: machineApi.status, refetchInterval: 4000 });
  return (
    <section className="space-y-2 border-t py-3" data-testid="migo-panel">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Status, sharing and approvals</h3>
        {st.data && <BridgeBadge s={st.data} />}
      </div>
      {st.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {st.isError && <div className="text-xs text-destructive">{errText(st.error)}</div>}
      {st.data && <Sharing s={st.data} settings={false} />}
    </section>
  );
}

/** `settings`: shown on This Machine, where the Migo Bridge settings are a tab away (it says where). */
export function Sharing({ s, settings = true }: { s: MachineStatus; settings?: boolean }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const done = { onSuccess: () => { setError(null); void qc.invalidateQueries({ queryKey: MACHINE_STATUS }); }, onError: (e: unknown) => setError(errText(e)) };
  const share = useMutation({ mutationFn: machineApi.share, ...done });
  const decide = useMutation({ mutationFn: (v: { id: string; approve: boolean }) => machineApi.decide(v.id, v.approve), ...done });
  const undo = useMutation({ mutationFn: machineApi.undo, ...done });
  const engines = Object.entries(s.share.engines);
  const models = Object.entries(s.share.models);
  return (
    <div className="space-y-4 text-xs" data-testid="machine-sharing">
      <section className="space-y-1">
        <h3 className="text-sm font-semibold">Migo Bridge</h3>
        {!s.bridgeMode && (
          <p className="text-muted-foreground">
            Migo Bridge is off: this app runs these engines for itself only.{" "}
            {settings ? "Turn it on in Settings → Migo Bridge" : "Turn it on above"} to offer them to your Sothoth Router (your requests still go through it then).
          </p>
        )}
        {s.bridgeMode && (
          <div className="space-y-0.5" data-testid="machine-bridge-status">
            <div>Sothoth Router: <span className="font-mono">{s.bridge?.router ?? s.bridgeRouter ?? "not set"}</span> · {s.bridge?.connected ? <span className="text-success">connected as {s.bridge.server}</span> : <span className="text-warning">not connected</span>}{s.bridge?.enabled === false && " · a Sothoth Router admin has turned it off"}</div>
            {s.bridge?.lastError && <div className="text-destructive">{s.bridge.lastError}</div>}
            <div className="text-muted-foreground">Remote config: {s.remoteConfig ? "a Sothoth Router admin may change this inventory (each change is logged below and can be undone)" : "off: your Sothoth Router sees this machine read-only"}.</div>
            {s.advancedFile && <div className="text-warning">Advanced: Migo Bridge runs from {s.advancedFile}, not this inventory.</div>}
          </div>
        )}
      </section>
      {error && <div className="text-destructive">{error}</div>}
      <section className="space-y-1">
        <h3 className="text-sm font-semibold">Share with your Sothoth Router</h3>
        <p className="text-muted-foreground">What your Sothoth Router may use through Migo Bridge. New engines and models are shared by default while Migo Bridge is on.</p>
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
        <h3 className="flex items-center gap-1 text-sm font-semibold"><History className="size-4" /> Changes by a Sothoth Router admin</h3>
        {s.remoteChanges.length === 0 && <div className="text-muted-foreground">None yet.</div>}
        <div className="divide-y rounded-md border">
          {s.remoteChanges.map((c) => (
            <div key={c.id} className="flex flex-wrap items-center gap-2 px-2 py-1.5" data-testid="machine-change">
              <span>Changed by Sothoth Router admin <b>{c.who.replace(/^admin:/, "")}</b> {ago(c.at * 1000)}{c.what ? `: ${c.what}` : ""}</span>
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
