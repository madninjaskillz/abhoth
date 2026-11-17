import { useEffect, useState, type FormEvent } from "react";
import { Loader2, ShieldCheck } from "lucide-react";
import { Mascot } from "@/components/Mascot";
import { Button } from "@/components/ui/button";
import { enrollInfo, signIn, submitCode, type AuthState } from "@/lib/auth";

const input =
  "h-9 w-full rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring";

/** The sign-in page (sign-in mode): password, then a 2FA code or 2FA setup. */
export function SignIn({ state, onDone }: { state: AuthState; onDone: () => void }) {
  const [stage, setStage] = useState(state.stage);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (fn: () => Promise<AuthState>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await fn();
      if (s.stage === "ok") onDone();
      else setStage(s.stage);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <div className="w-full max-w-sm">
        <Mascot size={128} className="mb-6" />
        <h1 className="mb-1 text-center text-lg font-semibold">
          Open<span className="text-primary">Orca</span>
        </h1>
        <p className="mb-6 text-center text-sm text-muted-foreground">
          {stage === "enroll" ? "Set up two-factor sign-in" : stage === "totp" ? "Enter the code from your authenticator app" : "Sign in to continue"}
        </p>
        {stage === "signedOut" && <PasswordForm busy={busy} onSubmit={(u, p) => run(() => signIn(u, p))} />}
        {stage === "totp" && <CodeForm busy={busy} onSubmit={(c) => run(() => submitCode(c))} />}
        {stage === "enroll" && <Enroll busy={busy} onSubmit={(c) => run(() => submitCode(c))} />}
        {error && <div className="mt-3 text-center text-sm text-destructive" role="alert" data-testid="signin-error">{error}</div>}
      </div>
    </div>
  );
}

function PasswordForm({ busy, onSubmit }: { busy: boolean; onSubmit: (u: string, p: string) => void }) {
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (user && password) onSubmit(user, password);
  };
  return (
    <form onSubmit={submit} className="space-y-3" data-testid="signin-form">
      <input className={input} autoComplete="username" placeholder="Username" value={user} onChange={(e) => setUser(e.target.value)} autoFocus aria-label="Username" />
      <input className={input} type="password" autoComplete="current-password" placeholder="Password" value={password} onChange={(e) => setPassword(e.target.value)} aria-label="Password" />
      <Button type="submit" className="w-full" disabled={busy || !user || !password}>
        {busy && <Loader2 className="animate-spin" />} Sign in
      </Button>
    </form>
  );
}

function CodeForm({ busy, onSubmit }: { busy: boolean; onSubmit: (c: string) => void }) {
  const [code, setCode] = useState("");
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (code.length >= 6) onSubmit(code);
      }}
      className="space-y-3"
      data-testid="code-form"
    >
      <input
        className={`${input} text-center font-mono tracking-[0.4em]`}
        inputMode="numeric"
        autoComplete="one-time-code"
        maxLength={7}
        placeholder="123456"
        value={code}
        onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, ""))}
        autoFocus
        aria-label="Authentication code"
      />
      <Button type="submit" className="w-full" disabled={busy || code.length < 6}>
        {busy && <Loader2 className="animate-spin" />} Verify
      </Button>
    </form>
  );
}

function Enroll({ busy, onSubmit }: { busy: boolean; onSubmit: (c: string) => void }) {
  const [info, setInfo] = useState<{ qr: string; secret: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    enrollInfo().then(setInfo, (e) => setError(String(e)));
  }, []);
  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">
        <ShieldCheck className="mr-1 inline size-3.5 text-primary" />
        Scan this with an authenticator app (1Password, Google Authenticator, Aegis…), then type the 6-digit code it shows.
      </p>
      {info ? (
        <>
          <img src={info.qr} alt="2FA QR code" className="mx-auto size-48 rounded-md bg-white p-1" data-testid="enroll-qr" />
          <div className="break-all text-center font-mono text-[11px] text-muted-foreground" data-testid="enroll-secret">{info.secret}</div>
        </>
      ) : (
        <div className="flex justify-center p-8">{error ?? <Loader2 className="animate-spin" />}</div>
      )}
      <CodeForm busy={busy} onSubmit={onSubmit} />
    </div>
  );
}
