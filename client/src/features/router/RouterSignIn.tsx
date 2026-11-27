// Sign in with Abhoth (D154), the admins' side: the routers whose admin pages sign people in with
// this server's accounts, and the routers asking to be registered (`abhoth router sso link`).
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, LogIn, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { ssoAdminApi, type SsoClientInfo } from "./routerApi";

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function RouterSignIn() {
  const qc = useQueryClient();
  const list = useQuery({ queryKey: ["sso-admin"], queryFn: ssoAdminApi.list, refetchInterval: 5000 });
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [remove, setRemove] = useState<SsoClientInfo | null>(null);
  const refresh = () => void qc.invalidateQueries({ queryKey: ["sso-admin"] });
  const ok = (text: string) => { setError(null); setDone(text); refresh(); };
  const approve = useMutation({ mutationFn: (id: string) => ssoAdminApi.approve(id), onSuccess: (r) => ok(`Approved: ${r.client.name} is registered and collects its credentials on its next check.`), onError: (e) => setError(errText(e)) });
  const deny = useMutation({ mutationFn: (id: string) => ssoAdminApi.deny(id), onSuccess: () => ok("Denied."), onError: (e) => setError(errText(e)) });
  const del = useMutation({ mutationFn: (id: string) => ssoAdminApi.remove(id), onSuccess: () => ok("Removed; its sign-ins and consents are gone."), onError: (e) => setError(errText(e)) });
  const requests = list.data?.requests ?? [];
  const clients = list.data?.clients ?? [];
  return (
    <div className="space-y-2 rounded-md border p-3" data-testid="router-signin-admin">
      <div className="flex items-center gap-2 text-sm font-medium">
        <LogIn className="size-4 text-muted-foreground" /> Router sign-in
        {!!requests.length && <Badge variant="warning">{requests.length} waiting</Badge>}
      </div>
      <p className="text-xs text-muted-foreground">
        Routers whose admin pages offer <i>Sign in with Abhoth</i>: admins of this server sign in there with their account (after a one-time consent). A linked router registers itself; another asks with <code className="rounded bg-muted px-1">abhoth router sso link</code> and shows a code to compare here.
      </p>
      {list.isLoading && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      {requests.map((r) => (
        <div key={r.id} className="space-y-1.5 rounded-md border border-primary/40 bg-primary/5 p-2.5" data-testid="sso-link-request">
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-xs">
            <span className="font-mono text-base font-semibold tracking-wider" data-testid="sso-link-code">{r.code}</span>
            <span className="font-medium">{r.name}</span>
            <span className="text-muted-foreground">from {r.ip} · {Math.max(1, Math.round(r.expiresIn / 60))} min left</span>
          </div>
          <div className="text-[11px] text-muted-foreground">Comes back to: {r.redirectUris.join(", ")}</div>
          <div className="flex justify-end gap-1">
            <Button size="sm" variant="outline" disabled={deny.isPending || approve.isPending} onClick={() => deny.mutate(r.id)} data-testid="sso-link-deny">Deny</Button>
            <Button size="sm" disabled={deny.isPending || approve.isPending} onClick={() => approve.mutate(r.id)} data-testid="sso-link-approve">
              {approve.isPending && <Loader2 className="animate-spin" />} Approve
            </Button>
          </div>
        </div>
      ))}
      {list.data && !clients.length && <div className="text-xs text-muted-foreground" data-testid="sso-clients-none">No routers registered yet.</div>}
      {!!clients.length && (
        <div className="divide-y rounded-md border" data-testid="sso-clients">
          {clients.map((c) => (
            <div key={c.id} className="flex items-center gap-3 px-3 py-2 text-xs" data-testid="sso-client">
              <div className="min-w-0 flex-1">
                <div className="font-medium">{c.name} <span className="font-normal text-muted-foreground">· {c.id}</span></div>
                <div className="truncate text-muted-foreground" title={c.redirectUris.join("\n")}>{c.redirectUris.join(", ")}</div>
              </div>
              <Button size="icon-sm" variant="ghost" aria-label={`Remove ${c.name}`} onClick={() => setRemove(c)} data-testid="sso-client-remove"><Trash2 /></Button>
            </div>
          ))}
        </div>
      )}
      {done && <div className="text-xs text-success" role="status">{done}</div>}
      {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
      <ConfirmDialog
        open={!!remove}
        title={`Remove ${remove?.name ?? "the router"}?`}
        body="Its Sign in with Abhoth stops working at once (sessions it already made last until they expire on the router), and everyone's consent for it is removed. A linked router registers itself again at its next sign-in."
        confirm="Remove"
        destructive
        onConfirm={() => { if (remove) del.mutate(remove.id); setRemove(null); }}
        onCancel={() => setRemove(null)}
      />
    </div>
  );
}
