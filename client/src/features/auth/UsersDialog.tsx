import { useState, type FormEvent } from "react";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, MoreHorizontal, Plus, ScrollText, ShieldCheck, Users } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { adminApi, authState, type AuditEntry, type UserRow } from "@/lib/auth";
import { cn } from "@/lib/utils";
import { MIN_PASSWORD, ago, when, eventLabel, passwordProblem, usernameProblem } from "./authHelpers";
import { Field, errorText } from "./parts";

const small = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/** Admins: the users of this server (create, disable, roles, password and 2FA resets, delete) and the audit log. */
export function UsersDialog({ onClose }: { onClose: () => void }) {
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="flex h-[min(86vh,780px)] max-w-3xl flex-col gap-0 p-0" data-testid="users-dialog">
        <div className="space-y-1 border-b p-4 pb-3">
          <DialogTitle className="flex items-center gap-2"><Users className="size-4 text-muted-foreground" /> Users</DialogTitle>
          <DialogDescription>
            Each user has their own projects, sessions and API keys. Admins also manage users and the server's settings.
            Everyone's agents and terminals run as this server's operating-system account: only add people you trust with it.
          </DialogDescription>
        </div>
        <Tabs defaultValue="users" className="flex min-h-0 flex-1 flex-col">
          <div className="border-b px-4 py-2">
            <TabsList>
              <TabsTrigger value="users" data-testid="users-tab-users"><Users /> Users</TabsTrigger>
              <TabsTrigger value="audit" data-testid="users-tab-audit"><ScrollText /> Audit log</TabsTrigger>
            </TabsList>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-4">
            <TabsContent value="users">
              <UsersTab />
            </TabsContent>
            <TabsContent value="audit">
              <AuditTab />
            </TabsContent>
          </div>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

type Pending =
  | { kind: "delete"; user: UserRow }
  | { kind: "totp"; user: UserRow }
  | { kind: "password"; user: UserRow }
  | null;

function UsersTab() {
  const qc = useQueryClient();
  const users = useQuery({ queryKey: ["admin-users"], queryFn: adminApi.users });
  const [creating, setCreating] = useState(false);
  const [pending, setPending] = useState<Pending>(null);
  const [menu, setMenu] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const me = authState().userId;
  const refresh = () => void qc.invalidateQueries({ queryKey: ["admin-users"] });
  const act = useMutation({
    mutationFn: async (fn: () => Promise<unknown>) => fn(),
    onSuccess: () => refresh(),
    onError: (e) => setError(errorText(e)),
  });
  const run = (fn: () => Promise<unknown>, done?: string) => {
    setError(null);
    setNotice(null);
    setMenu(null);
    act.mutate(fn, { onSuccess: () => done && setNotice(done) });
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <span className="text-xs text-muted-foreground">{users.data ? `${users.data.length} user${users.data.length === 1 ? "" : "s"}` : ""}</span>
        <span className="ml-auto" />
        <Button size="sm" onClick={() => setCreating(true)} data-testid="user-create-open">
          <Plus /> Add user
        </Button>
      </div>
      {error && <div className="text-xs text-destructive" role="alert" data-testid="users-error">{error}</div>}
      {notice && <div className="text-xs text-success" role="status">{notice}</div>}
      <div className="rounded-md border">
        {users.isLoading && <div className="p-3 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>}
        {users.isError && <div className="p-3 text-xs text-destructive">{errorText(users.error)}</div>}
        {!!users.data?.length && (
          <table className="w-full text-xs" data-testid="users-table">
            <thead className="text-left text-muted-foreground">
              <tr className="border-b">
                <th className="px-3 py-1.5 font-medium">User</th>
                <th className="px-2 font-medium">2FA</th>
                <th className="px-2 font-medium">Last sign-in</th>
                <th className="px-2 text-right font-medium">Projects</th>
                <th className="px-2" />
              </tr>
            </thead>
            <tbody>
              {users.data.map((u) => (
                <tr key={u.id} className={cn("border-b last:border-0", u.disabled && "text-muted-foreground")} data-testid="user-row" data-user={u.username}>
                  <td className="px-3 py-1.5">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className={cn("font-medium", u.disabled && "line-through")}>{u.username}</span>
                      {u.id === me && <span className="text-[10px] text-muted-foreground">(you)</span>}
                      {u.admin && <Badge>admin</Badge>}
                      {u.disabled && <Badge variant="destructive">disabled</Badge>}
                    </div>
                  </td>
                  <td className="px-2">{u.totp ? <ShieldCheck className="size-3.5 text-success" aria-label="2FA on" /> : <span className="text-muted-foreground">off</span>}</td>
                  <td className="px-2 text-muted-foreground" title={when(u.lastLogin)}>{u.lastLogin ? ago(u.lastLogin) : "never"}</td>
                  <td className="px-2 text-right tabular-nums">{u.projects}</td>
                  <td className="relative px-2 text-right">
                    <Button size="icon-sm" variant="ghost" onClick={() => setMenu(menu === u.id ? null : u.id)} aria-label={`Actions for ${u.username}`} data-testid="user-actions">
                      <MoreHorizontal />
                    </Button>
                    {menu === u.id && (
                      <div className="absolute right-2 top-full z-20 mt-1 flex w-44 flex-col rounded-md border bg-panel p-1 text-left shadow-lg" role="menu">
                        <MenuItem disabled={u.id === me} onClick={() => run(() => adminApi.update(u.id, { disabled: !u.disabled }))} testId="user-toggle-disabled">
                          {u.disabled ? "Enable" : "Disable"}
                        </MenuItem>
                        <MenuItem disabled={u.id === me} onClick={() => run(() => adminApi.update(u.id, { admin: !u.admin }))} testId="user-toggle-admin">
                          {u.admin ? "Remove admin" : "Make admin"}
                        </MenuItem>
                        <MenuItem onClick={() => { setMenu(null); setPending({ kind: "password", user: u }); }} testId="user-reset-password">
                          Reset password…
                        </MenuItem>
                        <MenuItem disabled={!u.totp} onClick={() => { setMenu(null); setPending({ kind: "totp", user: u }); }} testId="user-reset-totp">
                          Reset 2FA…
                        </MenuItem>
                        <MenuItem destructive disabled={u.id === me} onClick={() => { setMenu(null); setPending({ kind: "delete", user: u }); }} testId="user-delete">
                          Delete…
                        </MenuItem>
                      </div>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {creating && <CreateUser onClose={() => setCreating(false)} onCreated={(u) => { setCreating(false); setNotice(`Created ${u.username}.`); refresh(); }} />}
      {pending?.kind === "password" && (
        <ResetPassword user={pending.user} onClose={() => setPending(null)} onDone={() => { setNotice(`New password set for ${pending.user.username}; their sessions were signed out.`); setPending(null); }} />
      )}
      <ConfirmDialog
        open={pending?.kind === "totp"}
        title={`Reset 2FA for ${pending?.user.username}?`}
        body="Their authenticator and recovery codes stop working; they sign in with the password alone (or set 2FA up again)."
        confirm="Reset 2FA"
        destructive
        onConfirm={() => {
          const u = pending!.user;
          setPending(null);
          run(() => adminApi.resetTotp(u.id), `2FA reset for ${u.username}.`);
        }}
        onCancel={() => setPending(null)}
      />
      <ConfirmDialog
        open={pending?.kind === "delete"}
        title={`Delete ${pending?.user.username}?`}
        body="The account, its sessions and API keys are deleted, and its projects leave the project list (their folders are not touched). This can't be undone."
        confirm="Delete user"
        destructive
        onConfirm={() => {
          const u = pending!.user;
          setPending(null);
          run(() => adminApi.remove(u.id), `Deleted ${u.username}.`);
        }}
        onCancel={() => setPending(null)}
      />
    </div>
  );
}

function MenuItem({ children, onClick, disabled, destructive, testId }: { children: React.ReactNode; onClick: () => void; disabled?: boolean; destructive?: boolean; testId?: string }) {
  return (
    <button
      role="menuitem"
      disabled={disabled}
      onClick={onClick}
      className={cn("rounded px-2 py-1.5 text-xs hover:bg-accent disabled:pointer-events-none disabled:opacity-40", destructive && "text-destructive")}
      data-testid={testId}
    >
      {children}
    </button>
  );
}

function CreateUser({ onClose, onCreated }: { onClose: () => void; onCreated: (u: UserRow) => void }) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [admin, setAdmin] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const problem = usernameProblem(username) ?? passwordProblem(password);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (problem) return setError(problem);
    setBusy(true);
    setError(null);
    try {
      onCreated(await adminApi.create(username.trim(), password, admin));
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-sm" data-testid="user-create-dialog">
        <DialogTitle>Add a user</DialogTitle>
        <DialogDescription>Give them the password; they can change it under Account.</DialogDescription>
        <form onSubmit={(e) => void submit(e)} className="space-y-3">
          <Field label="Username">
            <input className={small} autoComplete="off" value={username} onChange={(e) => setUsername(e.target.value)} autoFocus data-testid="user-create-name" />
          </Field>
          <Field label="Password" hint={`At least ${MIN_PASSWORD} characters.`}>
            <input className={small} type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} data-testid="user-create-password" />
          </Field>
          <label className="flex items-center gap-2 text-xs">
            <input type="checkbox" checked={admin} onChange={(e) => setAdmin(e.target.checked)} data-testid="user-create-admin" />
            Admin (manages users and server settings, sees every project)
          </label>
          {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button type="button" size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
            <Button type="submit" size="sm" disabled={busy} data-testid="user-create-submit">
              {busy && <Loader2 className="animate-spin" />} Add user
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function ResetPassword({ user, onClose, onDone }: { user: UserRow; onClose: () => void; onDone: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const problem = passwordProblem(password);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (problem) return setError(problem);
    setBusy(true);
    setError(null);
    try {
      await adminApi.resetPassword(user.id, password);
      onDone();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-sm" data-testid="user-password-dialog">
        <DialogTitle>New password for {user.username}</DialogTitle>
        <DialogDescription>Their sessions are signed out; API keys keep working.</DialogDescription>
        <form onSubmit={(e) => void submit(e)} className="space-y-3">
          <Field label="New password" hint={`At least ${MIN_PASSWORD} characters.`}>
            <input className={small} type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus data-testid="user-password-input" />
          </Field>
          {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button type="button" size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
            <Button type="submit" size="sm" disabled={busy} data-testid="user-password-submit">
              {busy && <Loader2 className="animate-spin" />} Set password
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

const PAGE = 100;

function AuditTab() {
  const audit = useInfiniteQuery({
    queryKey: ["admin-audit"],
    queryFn: ({ pageParam }) => adminApi.audit(pageParam, PAGE),
    initialPageParam: undefined as number | undefined,
    getNextPageParam: (last: AuditEntry[]) => (last.length < PAGE ? undefined : last[last.length - 1].id),
  });
  const rows = audit.data?.pages.flat() ?? [];
  return (
    <div className="space-y-2" data-testid="audit-log">
      {audit.isLoading && <div className="text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>}
      {audit.isError && <div className="text-xs text-destructive">{errorText(audit.error)}</div>}
      {audit.isSuccess && rows.length === 0 && <div className="text-xs text-muted-foreground">Nothing logged yet.</div>}
      {rows.length > 0 && (
        <table className="w-full text-xs">
          <thead className="text-left text-muted-foreground">
            <tr className="border-b">
              <th className="py-1.5 pr-2 font-medium">When</th>
              <th className="px-2 font-medium">Event</th>
              <th className="px-2 font-medium">By</th>
              <th className="px-2 font-medium">User</th>
              <th className="px-2 font-medium">Address</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((e) => (
              <tr key={e.id} className="border-b align-top last:border-0" data-testid="audit-row">
                <td className="whitespace-nowrap py-1 pr-2 text-muted-foreground" title={when(e.time)}>{ago(e.time)}</td>
                <td className="px-2">
                  <span className={cn(/fail|lock|reject|denied/.test(e.event) && "text-destructive")}>{eventLabel(e.event)}</span>
                  {e.detail && Object.keys(e.detail).length > 0 && (
                    <div className="break-all font-mono text-[10px] text-muted-foreground">
                      {Object.entries(e.detail).map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`).join(" ")}
                    </div>
                  )}
                </td>
                <td className="px-2">{e.actor ?? <span className="text-muted-foreground">—</span>}</td>
                <td className="px-2">{e.target ?? <span className="text-muted-foreground">—</span>}</td>
                <td className="px-2 font-mono text-muted-foreground">{e.ip ?? ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {audit.hasNextPage && (
        <Button size="sm" variant="outline" onClick={() => void audit.fetchNextPage()} disabled={audit.isFetchingNextPage} data-testid="audit-more">
          {audit.isFetchingNextPage && <Loader2 className="animate-spin" />} Load more
        </Button>
      )}
    </div>
  );
}
