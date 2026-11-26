// The router's own admin keys (D151): list, make (shown once) and revoke. Only on routers that
// keep keys of their own (a router.db); the Abhoth web server's keys are managed there.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, KeyRound, Loader2, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { routerApi } from "../routerApi";
import { ago } from "../routerModel";
import type { AdminKey, NewAdminKey } from "./formModel";
import { Field, SectionHead, Text, errText } from "./formKit";

export function AdminKeys() {
  const qc = useQueryClient();
  const keys = useQuery({ queryKey: ["router-admin-keys"], queryFn: routerApi.adminKeys });
  const [name, setName] = useState("");
  const [made, setMade] = useState<NewAdminKey | null>(null);
  const [revoking, setRevoking] = useState<AdminKey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const create = useMutation({
    mutationFn: () => routerApi.createAdminKey(name),
    onSuccess: (k) => {
      setMade(k);
      setName("");
      setError(null);
      void qc.invalidateQueries({ queryKey: ["router-admin-keys"] });
    },
    onError: (e) => setError(errText(e)),
  });
  const revoke = useMutation({
    mutationFn: (id: number) => routerApi.revokeAdminKey(id),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["router-admin-keys"] }),
    onError: (e) => setError(errText(e)),
  });
  if (keys.isLoading) return <Loader2 className="size-4 animate-spin text-muted-foreground" />;
  if (keys.isError) return <div className="text-xs text-destructive">{errText(keys.error)}</div>;
  if (!keys.data?.local) return <div className="text-xs text-muted-foreground">This router keeps no admin keys of its own.</div>;
  return (
    <section className="space-y-3" data-testid="admin-keys">
      <SectionHead title="Admin keys" blurb="Keys that sign in to these pages and call the admin API. Each is shown once when it is made; the router keeps only its hash. Revoking ends its sessions." />
      <div className="flex items-end gap-2">
        <Field label="New key's name"><Text value={name} onChange={setName} placeholder="e.g. ops laptop" testid="admin-key-name" /></Field>
        <Button size="sm" disabled={!name.trim() || create.isPending} onClick={() => create.mutate()} data-testid="admin-key-create">
          {create.isPending ? <Loader2 className="animate-spin" /> : <KeyRound />} Make key
        </Button>
      </div>
      {error && <div className="text-xs text-destructive">{error}</div>}
      {made && (
        <div className="space-y-1 rounded-md border border-warning/50 p-2 text-xs" data-testid="admin-key-made">
          <div className="font-medium">Key {made.name}: copy it now, it is not shown again.</div>
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 break-all rounded bg-muted px-2 py-1">{made.key}</code>
            <Button size="sm" variant="ghost" onClick={() => void navigator.clipboard?.writeText(made.key)} aria-label="Copy"><Copy /></Button>
            <Button size="sm" variant="ghost" onClick={() => setMade(null)}>Done</Button>
          </div>
        </div>
      )}
      <div className="divide-y rounded-md border">
        {keys.data.keys.map((k) => (
          <div key={k.id} className="flex items-center gap-2 px-3 py-2 text-xs" data-testid="admin-key-row">
            <span className="font-medium">{k.name}</span>
            <span className="font-mono text-muted-foreground">{k.prefix}…</span>
            {k.revoked ? <Badge variant="outline">revoked</Badge> : null}
            <span className="ml-auto text-muted-foreground">made {ago(k.created * 1000)} by {k.createdBy}{k.lastUsed ? ` · used ${ago(k.lastUsed * 1000)}` : ""}</span>
            {!k.revoked && <Button size="sm" variant="ghost" onClick={() => setRevoking(k)} aria-label="Revoke"><Trash2 /></Button>}
          </div>
        ))}
        {keys.data.keys.length === 0 && <div className="px-3 py-2 text-xs text-muted-foreground">No keys yet.</div>}
      </div>
      {revoking && (
        <ConfirmDialog
          open
          title={`Revoke ${revoking.name}?`}
          body="It stops working at once, and anyone signed in with it is signed out."
          confirm="Revoke"
          destructive
          onConfirm={() => {
            revoke.mutate(revoking.id);
            setRevoking(null);
          }}
          onCancel={() => setRevoking(null)}
        />
      )}
    </section>
  );
}
