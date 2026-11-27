import { useEffect, useRef, useState } from "react";
import { Loader2, ShieldAlert, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ssoApi, type SsoAsk } from "@/lib/auth";
import { brandName } from "@/lib/brand";
import { AuthPage } from "./SignIn";
import { errorText } from "./parts";

/**
 * `/oauth/authorize` (Sign in with Abhoth, D154): a router's admin pages want to sign the user in
 * with this account. Admins allow or deny once (remembered; revoked in Account > Sessions), and
 * go straight back after that; anyone else is refused here, whatever they would choose.
 */
export function Consent() {
  const query = location.search;
  const [ask, setAsk] = useState<SsoAsk | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const auto = useRef(false);

  const answer = async (allow: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const r = await ssoApi.answer(query, allow);
      location.replace(r.redirect);
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  };

  useEffect(() => {
    ssoApi.ask(query).then(setAsk, (e) => setError(errorText(e)));
  }, [query]);
  // Consent given before: straight back to the router.
  useEffect(() => {
    if (ask?.consented && !auto.current) {
      auto.current = true;
      void answer(true);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ask]);

  const product = brandName();
  if (!ask) {
    return (
      <AuthPage subtitle="Sign in to a router">
        {error ? (
          <div className="rounded-md border border-destructive/50 p-3 text-sm text-destructive" role="alert" data-testid="consent-error">{error}</div>
        ) : (
          <Loader2 className="mx-auto size-5 animate-spin text-muted-foreground" />
        )}
      </AuthPage>
    );
  }
  if (!ask.admin) {
    return (
      <AuthPage subtitle="Sign in to a router">
        <div className="space-y-3 rounded-lg border bg-card p-5 text-sm" data-testid="consent-refused">
          <div className="flex items-center gap-2 font-medium"><ShieldAlert className="size-4 text-destructive" /> Not allowed</div>
          <p><b>{ask.client.name}</b> asked to sign you in to its admin pages, but {ask.refused ?? "only admins may."}</p>
          <p className="text-xs text-muted-foreground">Signed in as {ask.user}.</p>
          <Button variant="outline" className="w-full" onClick={() => location.assign("/")}>Back to {product}</Button>
        </div>
      </AuthPage>
    );
  }
  if (ask.consented) {
    return (
      <AuthPage subtitle="Sign in to a router">
        <div className="flex items-center justify-center gap-2 text-sm text-muted-foreground" data-testid="consent-auto">
          <Loader2 className="size-4 animate-spin" /> Signing you in to {ask.client.name}…
        </div>
        {error && <div className="mt-3 text-center text-sm text-destructive" role="alert">{error}</div>}
      </AuthPage>
    );
  }
  return (
    <AuthPage subtitle="Sign in to a router">
      <div className="space-y-4 rounded-lg border bg-card p-5 text-sm" data-testid="consent">
        <div className="flex items-center gap-2 font-medium"><ShieldCheck className="size-4 text-primary" /> {ask.client.name}</div>
        <p>
          <b>{ask.client.name}</b> wants to use your {product} account to sign you in to its admin pages.
        </p>
        <ul className="list-disc space-y-1 pl-5 text-xs text-muted-foreground">
          <li>It learns your username ({ask.user}) and that you are an admin, nothing else.</li>
          <li>You go back to <span className="font-mono">{ask.redirectOrigin}</span>.</li>
          <li>{product} remembers your answer; you can take it back in Account &gt; Sessions.</li>
        </ul>
        {error && <div className="text-sm text-destructive" role="alert">{error}</div>}
        <div className="flex gap-2">
          <Button variant="outline" className="flex-1" disabled={busy} onClick={() => void answer(false)} data-testid="consent-deny">Deny</Button>
          <Button className="flex-1" disabled={busy} onClick={() => void answer(true)} data-testid="consent-allow">
            {busy && <Loader2 className="animate-spin" />} Allow
          </Button>
        </div>
        <p className="text-center text-[11px] text-muted-foreground">Signed in as {ask.user}</p>
      </div>
    </AuthPage>
  );
}
