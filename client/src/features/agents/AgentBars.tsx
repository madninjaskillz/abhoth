import { useEffect, useRef } from "react";
import { Check, Clock, Cpu, Loader2, Rows3, Server, PanelsTopLeft, X } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useProject, useSession, type LiveAgent } from "@/store/session";
import { api, type LiveLayout } from "@/lib/api";
import { useProfiles } from "@/features/profiles/Profiles";
import { modelLabel } from "@/features/agents/modelLabel";
import { agentColor, cn } from "@/lib/utils";
import { Tip } from "@/components/ui/misc";
import { MascotThinking } from "@/components/Mascot";
import { buildTabs, selectedTab, type AgentTab } from "./agentTabs";
import { useAgentUi } from "./agentStore";

/**
 * The footer bars of the TUI, as a strip above the chat: the Lead and each helper with slot and
 * model. Or, in the tabs view, one tab per agent: a helper's tab shows its own conversation.
 */
export function AgentBars() {
  const agents = useSession((s) => s.agents);
  const slots = useSession((s) => s.slots);
  const busy = useSession((s) => s.busy);
  const view = useAgentUi((s) => s.view);
  const helpers = agents.filter((a) => !a.isLead);
  const lead = agents.find((a) => a.isLead);
  // The live server layout names the model behind the config's alias until a reply says.
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const live = useProfiles(!!info.data?.profiles).data?.live ?? null;
  const localModel = lead?.requestedModel ?? null;

  if (view === "tabs") return <AgentTabs agents={agents} busy={busy} />;

  return (
    <div className="border-b bg-panel/60 px-3 py-2 space-y-1.5">
      {lead && (
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1">
            <Bar agent={{ ...lead, status: busy ? "running" : lead.status }} live={live} localModel={localModel} />
          </div>
          <ViewToggle />
        </div>
      )}
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

/** Bars | Tabs, remembered in this browser. */
function ViewToggle() {
  const view = useAgentUi((s) => s.view);
  const setView = useAgentUi((s) => s.setView);
  const opt = (v: "bars" | "tabs", Icon: typeof Rows3, label: string) => (
    <button
      type="button"
      role="radio"
      aria-checked={view === v}
      title={v === "bars" ? "Progress bars" : "One tab per agent: open one to see its conversation"}
      onClick={() => setView(v)}
      className={cn(
        "inline-flex items-center gap-1 px-1.5 py-0.5 text-[11px]",
        view === v ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground",
      )}
      data-testid={`agent-view-${v}`}
    >
      <Icon className="size-3" />
      <span className="hidden sm:inline">{label}</span>
    </button>
  );
  return (
    <div role="radiogroup" aria-label="Agent view" className="inline-flex shrink-0 overflow-hidden rounded border">
      {opt("bars", Rows3, "Bars")}
      {opt("tabs", PanelsTopLeft, "Tabs")}
    </div>
  );
}

/** The helper whose tab is open in the tabs view (null: the main chat). */
export function useViewedAgent(): LiveAgent | null {
  const { id: pid } = useProject();
  const view = useAgentUi((s) => s.view);
  const chosen = useAgentUi((s) => s.selected[pid]);
  const agents = useSession((s) => s.agents);
  if (view !== "tabs" || !chosen) return null;
  const id = selectedTab(buildTabs(agents, false), chosen);
  return id ? agents.find((a) => a.id === id) ?? null : null;
}

function AgentTabs({ agents, busy }: { agents: LiveAgent[]; busy: boolean }) {
  const { id: pid } = useProject();
  const chosen = useAgentUi((s) => s.selected[pid]);
  const select = useAgentUi((s) => s.select);
  const tabs = buildTabs(agents, busy);
  const current = selectedTab(tabs, chosen);
  return (
    <div className="flex items-end gap-2 border-b bg-panel/60 pl-2 pr-3 pt-1.5">
      <div role="tablist" aria-label="Agents" className="flex min-w-0 flex-1 gap-0.5 overflow-x-auto [scrollbar-width:thin]" data-testid="agent-tabs">
        {tabs.map((t) => (
          <TabButton key={t.id} tab={t} active={t.isLead ? current === null : current === t.id} onClick={() => select(pid, t.isLead ? null : t.id)} />
        ))}
      </div>
      <div className="pb-1.5">
        <ViewToggle />
      </div>
    </div>
  );
}

function TabButton({ tab, active, onClick }: { tab: AgentTab; active: boolean; onClick: () => void }) {
  const color = agentColor(tab.letter);
  const ref = useRef<HTMLButtonElement>(null);
  // On a phone the strip scrolls: keep the open tab in sight.
  useEffect(() => {
    if (active) ref.current?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [active]);
  return (
    <button
      ref={ref}
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      title={`${tab.isLead ? "Lead (main chat)" : tab.label} · ${tab.state}`}
      className={cn(
        "group relative flex shrink-0 items-center gap-1.5 rounded-t-md border border-b-0 px-2 py-1 text-xs",
        active ? "bg-background text-foreground" : "border-transparent text-muted-foreground hover:bg-muted/60 hover:text-foreground",
      )}
      data-testid="agent-tab"
      data-agent={tab.id}
      data-state={tab.state}
    >
      <span
        className="inline-flex h-4 min-w-4 items-center justify-center rounded px-0.5 font-mono text-[10px] font-bold text-black"
        style={{ background: color }}
      >
        {tab.letter}
      </span>
      <span className="max-w-[6.5rem] truncate sm:max-w-[9rem]">{tab.label}</span>
      <TabStatus tab={tab} />
      {active && <span className="absolute inset-x-1 bottom-0 h-0.5 rounded-full" style={{ background: color }} />}
    </button>
  );
}

function TabStatus({ tab }: { tab: AgentTab }) {
  switch (tab.state) {
    case "running":
      return tab.pct != null ? (
        <span className="tabular-nums text-[10px] text-foreground">{tab.pct}%</span>
      ) : (
        <Loader2 className="size-3 animate-spin" aria-label="running" />
      );
    case "waiting":
      return <Clock className="size-3" aria-label="waiting" />;
    case "done":
      return <Check className="size-3 text-success" aria-label="done" />;
    case "failed":
      return <X className="size-3 text-destructive" aria-label="failed" />;
    default:
      return null;
  }
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
