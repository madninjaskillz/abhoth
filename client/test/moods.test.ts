// The mascot's moods (D150): the outcome-to-mood table, the live and empty-state faces, the
// on/off setting, and the session store's turn signals they are read from. Plain node.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  emptyStateMood, lastMood, liveMood, moodReason, moodsEnabled, setMoodsEnabled, turnMood, turnMoods,
  LONG_TURN_MS, MOODS, MOODS_PREF, TURN_MOODS, type TurnSignals,
} from "../src/lib/moods.ts";
import { sessionStore, type ChatItem, type LiveEvent, type LiveSnapshot } from "../src/store/session.ts";

const done: TurnSignals = { outcome: "done", reason: null, denied: 0, errors: 0, warnings: 0, pending: 0 };
const t = (extra: Partial<TurnSignals>): TurnSignals => ({ ...done, ...extra });

test("a finished turn is happy", () => {
  assert.equal(turnMood(done), "happy");
});

test("blocked turns are angry: refused keys, rate limits, quota, no model, engine unavailable, a denied permission", () => {
  for (const reason of ["auth", "rateLimited", "quota", "noModel", "unavailable"]) {
    assert.equal(turnMood(t({ outcome: "error", reason })), "angry", reason);
  }
  // The bridge reports an unavailable engine as outcome "done" with the reason.
  assert.equal(turnMood(t({ reason: "unavailable" })), "angry");
  assert.equal(turnMood(t({ denied: 1 })), "angry");
});

test("failed turns are sad: server errors, a failed engine, the iteration limit, error notices", () => {
  assert.equal(turnMood(t({ outcome: "error" })), "sad");
  assert.equal(turnMood(t({ outcome: "error", reason: "server" })), "sad");
  assert.equal(turnMood(t({ outcome: "error", reason: "failed" })), "sad");
  assert.equal(turnMood(t({ reason: "iterationLimit" })), "sad");
  assert.equal(turnMood(t({ errors: 1 })), "sad");
});

test("blocked beats failed, failed beats a warning", () => {
  assert.equal(turnMood(t({ outcome: "error", reason: "rateLimited", errors: 2, warnings: 1 })), "angry");
  assert.equal(turnMood(t({ outcome: "error", warnings: 3 })), "sad");
});

test("a turn that needs the user or warned is surprised", () => {
  assert.equal(turnMood(t({ pending: 1 })), "surprised");
  assert.equal(turnMood(t({ warnings: 1 })), "surprised");
});

test("a success right after a failure winks, once; otherwise it is happy", () => {
  assert.equal(turnMood(done, "sad"), "wink");
  assert.equal(turnMood(done, "angry"), "wink");
  assert.equal(turnMood(done, "wink"), "happy");
  assert.equal(turnMood(done, "happy"), "happy");
  assert.equal(turnMood(done, null), "happy");
});

test("a cancelled turn has no face", () => {
  assert.equal(turnMood(t({ outcome: "cancelled", errors: 1 })), null);
});

test("the table names every mood but sleepy, and each has words for the tooltip", () => {
  const inTable = new Set(TURN_MOODS.map((r) => r.mood).filter(Boolean));
  for (const m of MOODS) if (m !== "sleepy") assert.ok(inTable.has(m), m);
  for (const m of MOODS) assert.ok(moodReason(m).length > 0, m);
});

const end = (key: number, extra: Partial<TurnSignals> = {}): ChatItem => ({ kind: "turnEnd", key, elapsedMs: 1, tokens: 1, ...done, ...extra });

test("turnMoods walks the transcript: each turn sees the one before", () => {
  const items: ChatItem[] = [
    { kind: "user", key: 1, text: "a", source: "web" },
    end(2, { outcome: "error" }),
    end(3),
    end(4),
    end(5, { outcome: "cancelled" }),
    end(6),
  ];
  const m = turnMoods(items);
  assert.deepEqual([...m.entries()], [[2, "sad"], [3, "wink"], [4, "happy"], [6, "happy"]]);
  assert.equal(lastMood(items), "happy");
  assert.equal(lastMood(items.slice(0, 5)), null, "the last turn was cancelled");
  assert.equal(lastMood([]), null);
});

test("live: surprised while the user is asked, sleepy on a long turn, else neutral", () => {
  assert.equal(liveMood({ busy: true, pending: 1, elapsedMs: 0 }), "surprised");
  assert.equal(liveMood({ busy: true, pending: 0, elapsedMs: LONG_TURN_MS }), "sleepy");
  assert.equal(liveMood({ busy: true, pending: 0, elapsedMs: 1000 }), null);
  assert.equal(liveMood({ busy: false, pending: 0, elapsedMs: LONG_TURN_MS * 2 }), null);
});

test("the empty chat dozes unless a turn already had a face", () => {
  assert.equal(emptyStateMood(null), "sleepy");
  assert.equal(emptyStateMood("angry"), "angry");
});

test("the setting: on by default, off once turned off, on again; storage errors keep it on", () => {
  const data = new Map<string, string>();
  const storage = { getItem: (k: string) => data.get(k) ?? null, setItem: (k: string, v: string) => void data.set(k, v) };
  assert.equal(moodsEnabled(storage), true);
  setMoodsEnabled(storage, false);
  assert.equal(data.get(MOODS_PREF), "0");
  assert.equal(moodsEnabled(storage), false);
  setMoodsEnabled(storage, true);
  assert.equal(moodsEnabled(storage), true);
  assert.equal(moodsEnabled(undefined), true);
  const broken = { getItem: () => { throw new Error("blocked"); }, setItem: () => { throw new Error("blocked"); } };
  assert.equal(moodsEnabled(broken), true);
  assert.doesNotThrow(() => setMoodsEnabled(broken, false));
});

// The session store fills a turn's signals from its events.

const snapshot = (): LiveSnapshot => ({
  workingDirectory: "C:/p", mode: "headless", busy: false, queued: 0, lastSeq: 0, history: [], currentTurn: [],
  agents: { agents: [] }, pendingPermissions: [],
});

let seq = 0;
const ev = (type: string, data: object = {}): LiveEvent => ({ seq: ++seq, type, at: "2026-11-26T12:00:00Z", data });

function lastTurnEnd(id: string): Extract<ChatItem, { kind: "turnEnd" }> {
  const items = sessionStore(id).getState().items;
  const e = items.filter((i) => i.kind === "turnEnd").pop();
  assert.ok(e && e.kind === "turnEnd");
  return e;
}

test("turnEnd carries the server's reason and the turn's denied permissions and notice levels", () => {
  const s = sessionStore("moods-1");
  s.getState().loadSnapshot(snapshot());
  s.getState().applyEvents([
    ev("turnStart", { source: "web" }),
    ev("notice", { level: "warning", text: "Compacted the conversation" }),
    ev("permission", { id: "p1", tool: "bash", risk: "high" }),
    ev("permissionResolved", { id: "p1", decision: "no" }),
    ev("notice", { level: "error", text: "Error: HTTP 429" }),
    ev("turnEnd", { elapsedMs: 5, tokens: 1, outcome: "error", reason: "rateLimited" }),
  ]);
  const e = lastTurnEnd("moods-1");
  assert.deepEqual(
    { outcome: e.outcome, reason: e.reason, denied: e.denied, errors: e.errors, warnings: e.warnings, pending: e.pending },
    { outcome: "error", reason: "rateLimited", denied: 1, errors: 1, warnings: 1, pending: 0 },
  );
  assert.equal(turnMood(e), "angry");
  // The next turn starts with a clean tally.
  s.getState().applyEvents([ev("turnStart", { source: "web" }), ev("turnEnd", { elapsedMs: 5, tokens: 1, outcome: "done" })]);
  const next = lastTurnEnd("moods-1");
  assert.deepEqual([next.denied, next.errors, next.warnings, next.reason], [0, 0, 0, null]);
  assert.deepEqual([...turnMoods(s.getState().items).values()], ["angry", "wink"]);
});

test("an approved permission is not a denial; a question still waiting at the end is pending", () => {
  const s = sessionStore("moods-2");
  s.getState().loadSnapshot(snapshot());
  s.getState().applyEvents([
    ev("turnStart", { source: "web" }),
    ev("permission", { id: "p1", tool: "bash", risk: "high" }),
    ev("permissionResolved", { id: "p1", decision: "yes" }),
    ev("question", { id: "q1", question: "Which?", options: [] }),
    ev("turnEnd", { elapsedMs: 5, tokens: 1, outcome: "done" }),
  ]);
  const e = lastTurnEnd("moods-2");
  assert.equal(e.denied, 0);
  assert.equal(e.pending, 1);
  assert.equal(turnMood(e), "surprised");
});

test("the running turn's start time comes from its turnStart event", () => {
  const s = sessionStore("moods-3");
  s.getState().loadSnapshot(snapshot());
  s.getState().applyEvents([ev("turnStart", { source: "web" })]);
  assert.equal(s.getState().turnStartedAt, Date.parse("2026-11-26T12:00:00Z"));
  s.getState().applyEvents([ev("turnEnd", { elapsedMs: 5, tokens: 1, outcome: "done" })]);
  assert.equal(s.getState().turnStartedAt, 0);
});
