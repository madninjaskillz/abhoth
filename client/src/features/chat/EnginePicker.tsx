import { useEffect, useRef, useState } from "react";
import { KeyRound, Loader2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { setEngine } from "@/lib/hub";
import { useProject, useSession } from "@/store/session";
import { useTerminalRuns } from "@/store/terminalRuns";

const ENGINES = [
  { id: "native", label: "Native" },
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
] as const;

export const engineName = (id: string) => ENGINES.find((e) => e.id === id)?.label ?? id;

/** What the login flow is doing, for the banner. */
type Flow =
  | { step: "idle" }
  | { step: "checking"; engine: string }
  | { step: "login"; engine: string; command: string; nonce: number }
  | { step: "rechecking"; engine: string }
  | { step: "failed"; engine: string; message: string };

/**
 * Native / Claude / Codex for this project's next turns. A bridge whose CLI isn't logged in
 * runs its login command in the project's terminal (shown with a banner); when the command
 * exits the engine is asked for again, and on failure the project stays on its engine.
 */
export function useEngineFlow() {
  const { id: pid } = useProject();
  const engine = useSession((s) => s.engine);
  const busy = useSession((s) => s.busy);
  const sessionAuth = useSession((s) => s.engineAuth);
  const request = useTerminalRuns((s) => s.request);
  const [flow, setFlow] = useState<Flow>({ step: "idle" });
  const run = useTerminalRuns((s) => s.runs[pid]);
  const handled = useRef(0);

  const choose = async (wanted: string) => {
    if (wanted === engine || flow.step === "checking" || flow.step === "rechecking") return;
    setFlow({ step: "checking", engine: wanted });
    const r = await setEngine(pid, wanted);
    if (r.ok) return setFlow({ step: "idle" });
    if (r.auth?.loginCommand) {
      const nonce = request(pid, r.auth.loginCommand);
      return setFlow({ step: "login", engine: wanted, command: r.auth.loginCommand, nonce });
    }
    setFlow({ step: "failed", engine: wanted, message: r.message ?? "The engine isn't available." });
  };

  // The login command finished: ask for the engine again.
  useEffect(() => {
    if (flow.step !== "login" || !run || run.nonce !== flow.nonce || run.code === undefined || handled.current === run.nonce) return;
    handled.current = run.nonce;
    const wanted = flow.engine;
    setFlow({ step: "rechecking", engine: wanted });
    void setEngine(pid, wanted).then((r) => {
      if (r.ok) setFlow({ step: "idle" });
      else
        setFlow({
          step: "failed",
          engine: wanted,
          message:
            run.code === -1
              ? "The terminal closed before the login finished. Staying on " + engineName(r.engine) + "."
              : `${r.message ?? "Still not logged in."} Staying on ${engineName(r.engine)}.`,
        });
    });
  }, [flow, run, pid]);

  return { pid, engine, busy, sessionAuth, flow, choose, dismiss: () => setFlow({ step: "idle" }) };
}

export function EnginePicker({ flow }: { flow: ReturnType<typeof useEngineFlow> }) {
  const pending = flow.flow.step === "checking" || flow.flow.step === "rechecking" ? flow.flow.engine : null;
  return (
    <div className="inline-flex items-center rounded-md border p-0.5" role="radiogroup" aria-label="Engine" data-testid="engine-picker">
      {ENGINES.map((e) => (
        <button
          key={e.id}
          role="radio"
          aria-checked={flow.engine === e.id}
          disabled={flow.busy && flow.engine !== e.id}
          title={flow.busy && flow.engine !== e.id ? "Switch engines when the current turn has finished" : `Run the next turns on ${e.label}`}
          onClick={() => void flow.choose(e.id)}
          className={cn(
            "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] disabled:opacity-50",
            flow.engine === e.id ? "bg-primary/15 font-medium text-foreground" : "text-muted-foreground hover:bg-accent",
          )}
          data-testid={`engine-${e.id}`}
        >
          {pending === e.id && <Loader2 className="size-3 animate-spin" />}
          {e.label}
        </button>
      ))}
    </div>
  );
}

/** "Log in to Codex to use this engine", while its login runs in the terminal; or why it failed. */
export function EngineBanner({ flow }: { flow: ReturnType<typeof useEngineFlow> }) {
  const f = flow.flow;
  // A login the session asked for by itself (a saved engine that's logged out), not started from here.
  if (f.step === "idle" && flow.sessionAuth && flow.engine !== flow.sessionAuth.engine) {
    const a = flow.sessionAuth;
    return (
      <Banner tone="warning" testid="engine-banner">
        <KeyRound className="size-4 shrink-0" />
        <span className="min-w-0 flex-1">
          {engineName(a.engine)} isn't logged in, so this project runs on {engineName(flow.engine)}.
        </span>
        <Button size="sm" variant="outline" onClick={() => void flow.choose(a.engine)}>
          Log in to {engineName(a.engine)}
        </Button>
      </Banner>
    );
  }
  if (f.step === "login" || f.step === "rechecking")
    return (
      <Banner tone="warning" testid="engine-banner">
        <KeyRound className="size-4 shrink-0" />
        <span className="min-w-0 flex-1">
          Log in to {engineName(f.engine)} to use this engine: follow the steps in the terminal
          {f.step === "login" && (
            <>
              {" "}(<code className="font-mono text-[11px]">{f.command}</code>)
            </>
          )}
          .{f.step === "rechecking" && " Checking the login…"}
        </span>
        {f.step === "rechecking" ? <Loader2 className="size-4 animate-spin" /> : (
          <Button size="sm" variant="ghost" onClick={flow.dismiss} title="Stop waiting (the terminal command keeps running)">
            <X />
          </Button>
        )}
      </Banner>
    );
  if (f.step === "failed")
    return (
      <Banner tone="error" testid="engine-banner">
        <span className="min-w-0 flex-1">{engineName(f.engine)}: {f.message}</span>
        <Button size="sm" variant="ghost" onClick={flow.dismiss}>
          <X />
        </Button>
      </Banner>
    );
  return null;
}

function Banner({ tone, testid, children }: { tone: "warning" | "error"; testid: string; children: React.ReactNode }) {
  return (
    <div
      className={cn(
        "flex items-center gap-2 border-t px-4 py-2 text-sm sm:px-6",
        tone === "warning" ? "bg-warning/10 text-warning" : "bg-destructive/10 text-destructive",
      )}
      data-testid={testid}
    >
      {children}
    </div>
  );
}
