/**
 * The mascot's moods (D150, docs/design/MOODS.md): which face Abhoth shows at the end of a
 * response, and while a turn runs. Pure (no imports but types, no DOM), so plain node can test
 * it (test/moods.test.ts).
 *
 * A mood comes only from structured turn state: the `turnEnd` event's `outcome` and `reason`
 * (the server's `orca_core::live::reasons`), the level of the turn's notices, and the permission
 * and question events. Never from the text of a message.
 */
import type { ChatItem } from "@/store/session";

export type Mood = "happy" | "sad" | "angry" | "surprised" | "wink" | "sleepy";

export const MOODS: Mood[] = ["happy", "sad", "angry", "surprised", "wink", "sleepy"];

/** What a finished turn's mood is picked from (the session store fills it from the turn's events). */
export interface TurnSignals {
  /** "done", "error" or "cancelled". */
  outcome: string;
  /** Why it ended, when the server says (`auth`, `rateLimited`, `quota`, `noModel`, `unavailable`, `server`, `failed`, `iterationLimit`). */
  reason?: string | null;
  /** Permission prompts answered "no" in the turn. */
  denied: number;
  /** Error-level notices in the turn. */
  errors: number;
  /** Warning-level notices in the turn (compaction, the context nearly full, retries, failover). */
  warnings: number;
  /** Questions and permission prompts still waiting for the user when the turn ended. */
  pending: number;
}

/** Turn-end reasons meaning something refused to go on: the face is angry. */
export const BLOCKED_REASONS = ["auth", "rateLimited", "quota", "noModel", "unavailable"];
/** Turn-end reasons meaning the turn failed: the face is sad. */
export const FAILED_REASONS = ["server", "failed", "iterationLimit"];

export interface MoodRule {
  /** null: no face for this turn (the footer shows none). */
  mood: Mood | null;
  /** In words, for the design note and tooltips. */
  when: string;
  test: (t: TurnSignals, previous: Mood | null) => boolean;
}

/**
 * The mapping, in one table: the first rule that matches wins. Change moods here.
 * `previous` is the mood of the turn before (null when there was none, or it had no face).
 */
export const TURN_MOODS: MoodRule[] = [
  { mood: null, when: "you stopped the turn", test: (t) => t.outcome === "cancelled" },
  {
    mood: "angry",
    when: "blocked: the server refused the key (401/403), rate limited (429), out of quota (402), no model, the engine unavailable, or a permission was denied",
    test: (t) => BLOCKED_REASONS.includes(t.reason ?? "") || t.denied > 0,
  },
  {
    mood: "sad",
    when: "the turn failed: a model or server error, the engine failed, it gave up at the iteration limit, or an error notice",
    test: (t) => t.outcome === "error" || FAILED_REASONS.includes(t.reason ?? "") || t.errors > 0,
  },
  {
    mood: "surprised",
    when: "it needs you (a question or permission still waiting) or something unexpected came up (a warning: compaction, context nearly full)",
    test: (t) => t.pending > 0 || t.warnings > 0,
  },
  {
    mood: "wink",
    when: "fixed: it finished right after a turn that failed or was blocked",
    test: (t, previous) => t.outcome === "done" && (previous === "sad" || previous === "angry"),
  },
  { mood: "happy", when: "the turn finished", test: (t) => t.outcome === "done" },
];

/** The mood of one finished turn. */
export function turnMood(t: TurnSignals, previous: Mood | null = null): Mood | null {
  for (const r of TURN_MOODS) if (r.test(t, previous)) return r.mood;
  return null;
}

/** When a face shows, in words (the badge's tooltip). */
export function moodReason(mood: Mood): string {
  return TURN_MOODS.find((r) => r.mood === mood)?.when ?? (mood === "sleepy" ? "waiting, or a long turn" : "");
}

/** Each turn-end item's mood, by item key (turns without a face are left out). */
export function turnMoods(items: ChatItem[]): Map<number, Mood> {
  const out = new Map<number, Mood>();
  let previous: Mood | null = null;
  for (const it of items) {
    if (it.kind !== "turnEnd") continue;
    const m = turnMood(it, previous);
    if (m) out.set(it.key, m);
    previous = m;
  }
  return out;
}

/** The last finished turn's mood in a transcript (null: none yet, or it had no face). */
export function lastMood(items: ChatItem[]): Mood | null {
  for (let i = items.length - 1; i >= 0; i--) {
    const it = items[i];
    if (it.kind === "turnEnd") return turnMoods(items).get(it.key) ?? null;
  }
  return null;
}

/** A turn running this long is long-running background work: the working mascot dozes. */
export const LONG_TURN_MS = 2 * 60_000;

/**
 * The working mascot's face while a turn runs: surprised while a question or permission waits
 * for the user, sleepy once the turn has run for a long time, else none (the neutral mascot).
 */
export function liveMood(s: { busy: boolean; pending: number; elapsedMs: number }): Mood | null {
  if (s.pending > 0) return "surprised";
  if (s.busy && s.elapsedMs >= LONG_TURN_MS) return "sleepy";
  return null;
}

/** The empty chat ("type a prompt to start"): waiting, so sleepy, unless a turn already had a face. */
export function emptyStateMood(last: Mood | null): Mood {
  return last ?? "sleepy";
}

/** The browser setting (localStorage) that turns moods off; on unless it says "0". */
export const MOODS_PREF = "abhoth.moods";

export function moodsEnabled(storage: Pick<Storage, "getItem"> | undefined): boolean {
  try {
    return storage?.getItem(MOODS_PREF) !== "0";
  } catch {
    return true;
  }
}

export function setMoodsEnabled(storage: Pick<Storage, "setItem"> | undefined, on: boolean): void {
  try {
    storage?.setItem(MOODS_PREF, on ? "1" : "0");
  } catch {
    /* a private window: this session only */
  }
}

/** Alt text for a face. */
export function moodLabel(mood: Mood): string {
  return { happy: "happy", sad: "sad", angry: "cross", surprised: "surprised", wink: "winking", sleepy: "sleepy" }[mood];
}
