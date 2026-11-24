import { Cpu, Server } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useSession, type LiveAgent } from "@/store/session";
import { api, type LiveLayout } from "@/lib/api";
import { useProfiles } from "@/features/profiles/Profiles";
import { modelLabel } from "@/features/agents/modelLabel";
import { agentColor, cn } from "@/lib/utils";
import { Tip } from "@/components/ui/misc";
import { MascotThinking } from "@/components/Mascot";

/** The footer bars of the TUI, as a strip above the chat: the Lead and each helper with slot and model. */
export function AgentBars() {
  const agents = useSession((s) => s.agents);
  const slots = useSession((s) => s.slots);
  const busy = useSession((s) => s.busy);
  const helpers = agents.filter((a) => !a.isLead);
  const lead = agents.find((a) => a.isLead);
  // The live server layout names the model behind the config's alias until a reply says.
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const live = useProfiles(!!info.data?.profiles).data?.live ?? null;
  const localModel = lead?.requestedModel ?? null;

  return (
    <div className="border-b bg-panel/60 px-3 py-2 space-y-1.5">
      {lead && <Bar agent={{ ...lead, status: busy ? "running" : lead.status }} live={live} localModel={localModel} />}
      {helpers.map((a) => (
        <Bar key={a.id} agent={a} live={live} localModel={localModel} />
      ))}
      {slots && (
        <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Server className="size-3" />
          <span>
            slots {slots.leases.length}/{slots.total}
          </span>
          <span className="truncate">
            {slots.leases.map((l) => `${l.slot ?? "?"}·${l.label || l.owner}`).join("  ")}
          </span>
        </div>
      )}
    </div>
  );
}

function Bar({ agent, live, localModel }: { agent: LiveAgent; live: LiveLayout | null; localModel: string | null }) {
  const color = agentColor(agent.letter);
  const running = agent.status === "running" || agent.status === "pending";
  const pct = Math.max(0, Math.min(1, agent.progress ?? (running ? 0 : agent.status === "idle" ? 0 : 1)));
  const detail = agent.activity || agent.note;
  const model = modelLabel(agent, live, localModel);
  return (
    <div className="flex items-center gap-2 text-xs min-w-0" data-testid="agent-bar">
      <span
        className="inline-flex h-5 min-w-5 items-center justify-center rounded px-1 font-mono text-[11px] font-bold text-black"
        style={{ background: color }}
      >
        {agent.letter || "·"}
      </span>
      <span className="font-medium truncate max-w-[9rem]">{agent.label}</span>
      <div className="relative h-1.5 flex-1 min-w-[3rem] overflow-hidden rounded-full bg-muted">
        <div
          className={cn("absolute inset-y-0 left-0 rounded-full transition-[width] duration-500", running && pct === 0 && "animate-pulse w-1/4")}
          style={{ width: running && pct === 0 ? undefined : `${pct * 100}%`, background: color, opacity: running ? 1 : 0.5 }}
        />
      </div>
      {agent.isLead && running && <MascotThinking />}
      <span className={cn("w-14 text-right tabular-nums", running ? "text-foreground" : "text-muted-foreground")}>
        {running && agent.progress != null ? `${Math.round(pct * 100)}%` : agent.status}
      </span>
      <Tip label={<span>slot {agent.slot ?? "—"} · {model.tip}{detail ? <><br />{detail}</> : null}</span>}>
        <span className="hidden sm:inline-flex items-center gap-1 text-muted-foreground max-w-[14rem] truncate">
          <Cpu className="size-3 shrink-0" />
          <span className="truncate">
            {agent.slot ? `${agent.slot} · ` : ""}
            <span data-testid="agent-model" data-source={model.source}>{model.text}</span>
          </span>
        </span>
      </Tip>
    </div>
  );
}
