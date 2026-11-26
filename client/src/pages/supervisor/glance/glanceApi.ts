import { csrfHeaders } from "@/lib/auth";

export interface CardActionResult {
  text: string;
  done: boolean;
  ok?: boolean;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Takes a 32 GB card online or offline through the web server's admin endpoint (it forwards to
 * the control plane; the old dash POSTed from the browser to an unauthenticated LAN port), then
 * follows the job until it settles.
 */
export async function cardAction(card: string, action: "online" | "offline", progress: (r: CardActionResult) => void): Promise<void> {
  const say = (text: string, done = false, ok?: boolean) => progress({ text: `${action} ${card}: ${text}`, done, ok });
  say("sending…");
  try {
    const r = await fetch(`/api/glance/cards/${encodeURIComponent(card)}/${action}`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", ...csrfHeaders() },
      body: JSON.stringify({ reason: "Glance button" }),
    });
    const j = (await r.json().catch(() => ({}))) as { id?: string; already?: boolean; error?: string };
    if (r.status !== 202 || !j.id) {
      say(j.already ? "already " + action : j.error || "HTTP " + r.status, true, !!j.already);
      return;
    }
    say(`queued (${j.id})`);
    for (let i = 0; i < 120; i++) {
      await sleep(3000);
      const jr = (await (await fetch(`/api/glance/jobs/${encodeURIComponent(j.id)}`, { credentials: "same-origin", cache: "no-store" })).json()) as {
        state?: string;
        steps?: { msg?: string }[];
      };
      const last = (jr.steps || []).slice(-1)[0];
      const running = ["queued", "running"].includes(jr.state ?? "");
      say(`${jr.state}${last?.msg ? " · " + last.msg : ""}`, !running, jr.state === "done" || jr.state === "succeeded");
      if (!running) return;
    }
  } catch (e) {
    say(String((e as Error).message || e), true, false);
  }
}
