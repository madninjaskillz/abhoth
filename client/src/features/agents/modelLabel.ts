// The model an agent bar shows: the one that actually served it, not the alias in the config.
// No imports, so plain node runs the tests.

/** The parts of an agent the label needs (LiveAgent from the session store). */
export interface ModelAgent {
  slot?: string | null;
  model?: string | null;
  /** The model id it sends (a gateway alias such as "qwen3.8-27b"); null for a bridge. */
  requestedModel?: string | null;
  /** What answered its last request; null = not known yet (or the server only echoed the alias). */
  servedModel?: string | null;
}

/** The parts of the live server layout the label needs (LiveLayout from the profiles API). */
export interface ModelLayout {
  layout?: string | null;
  cards?: Record<string, { engine?: string | null; model?: string | null; options?: { alias?: string | null } | null } | null> | null;
}

export interface ModelLabel {
  /** Shown on the bar. */
  text: string;
  /** Explains where the name comes from (the tooltip). */
  tip: string;
  /** served = its last reply said; layout = the live server layout; config = only the config's name. */
  source: "served" | "layout" | "config";
  /** The card it ran on, when the slot says. */
  card: string | null;
}

/** "B3" → "B"; "A" → "A"; "waiting", "—", "claude" → null (only when the layout has that card). */
export function cardOfSlot(slot: string | null | undefined, layout?: ModelLayout | null): string | null {
  const m = /^([A-Za-z])\d*$/.exec((slot ?? "").trim());
  if (!m) return null;
  const card = m[1].toUpperCase();
  return layout?.cards && !layout.cards[card] ? null : card;
}

/** The name a live card serves: its engine's alias, else its model file. */
export function cardModel(c: { engine?: string | null; model?: string | null; options?: { alias?: string | null } | null } | null | undefined): string | null {
  if (!c || c.engine === "off") return null;
  return c.options?.alias || c.model || null;
}

/**
 * What the bar shows. <paramref name="localModel"/> is the model the Lead asks for: an agent asking
 * for the same one runs on the local server, so the live layout may stand in for a reply not seen yet.
 */
export function modelLabel(agent: ModelAgent, layout?: ModelLayout | null, localModel?: string | null): ModelLabel {
  const requested = agent.requestedModel ?? null;
  const card = cardOfSlot(agent.slot, layout);
  const onCard = card ? ` (card ${card})` : "";
  const served = agent.servedModel || null;
  if (served) {
    const tip = requested && requested !== served ? `requested ${requested} → served by ${served}${onCard}` : `served by ${served}${onCard}`;
    return { text: served, tip, source: "served", card };
  }
  const local = !!requested && (card !== null || !localModel || requested === localModel);
  if (local && layout?.cards) {
    let live: string | null = null;
    if (card) live = cardModel(layout.cards[card]);
    else {
      const all = [...new Set(Object.values(layout.cards).map(cardModel).filter((m): m is string => !!m))];
      live = all.length ? all.join(" / ") : null;
    }
    if (live) {
      const where = layout.layout ? `live layout ${layout.layout}` : "live layout";
      return { text: live, tip: `requested ${requested} → ${where}: ${live}${onCard} (no reply seen yet)`, source: "layout", card };
    }
  }
  const name = agent.model || requested || "";
  return { text: name, tip: name ? `model ${name}` : "model —", source: "config", card };
}
