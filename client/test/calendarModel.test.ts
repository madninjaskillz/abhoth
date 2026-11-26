// The Supervisor calendar page's model: week grid placement across DST, lanes, the effect rows (plain node).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  effectRow, effectValue, hhmm, keyState, localMidnight, phaseOf, placeWeek, tidy, weekDays, whenKind,
  type Occurrence,
} from "../src/features/calendar/calendarModel.ts";

const TZ = "Europe/London";
const utc = (s: string) => Date.parse(s) / 1000;

const occ = (entry: string, start: string, end: string, lead = 0, over: Partial<Occurrence> = {}): Occurrence => ({
  entry, id: `${entry}@${utc(start)}`, title: entry, priority: "normal", nominal: utc(start),
  start: utc(start), end: utc(end), block_start: utc(start) - lead, block_end: utc(end), tail_end: utc(end),
  unplanned: false, extended: false, skipped: false, ...over,
});

test("weeks start on Monday at local midnight, also across the clocks going back", () => {
  // Sunday 25 October 2026 is 25 hours long in London.
  const days = weekDays(utc("2026-10-22T12:00:00Z"), TZ);
  assert.equal(days.length, 8);
  assert.equal(days[0], utc("2026-10-18T23:00:00Z"), "Mon 19 Oct 00:00 BST");
  assert.equal(days[6], utc("2026-10-24T23:00:00Z"), "Sun 25 Oct 00:00 BST");
  assert.equal(days[7], utc("2026-10-26T00:00:00Z"), "Mon 26 Oct 00:00 GMT");
  assert.equal(days[7] - days[6], 25 * 3600);
  assert.equal(localMidnight(utc("2026-03-29T12:00:00Z"), TZ), utc("2026-03-29T00:00:00Z"));
  assert.equal(hhmm(utc("2026-07-01T06:00:00Z"), TZ), "07:00");
});

test("blocks are split at midnight and overlapping ones get lanes", () => {
  const days = weekDays(utc("2026-11-25T12:00:00Z"), TZ); // Mon 23 Nov
  const p = placeWeek(
    [
      occ("ai-news", "2026-11-25T07:00:00Z", "2026-11-25T08:30:00Z", 900),
      occ("bench", "2026-11-25T06:30:00Z", "2026-11-25T09:30:00Z"),
      occ("night", "2026-11-25T22:00:00Z", "2026-11-26T06:00:00Z"),
    ],
    days,
  );
  const ai = p.find((x) => x.occ.entry === "ai-news")!;
  const bench = p.find((x) => x.occ.entry === "bench")!;
  assert.equal(ai.day, 2);
  assert.equal(ai.lanes, 2);
  assert.notEqual(ai.lane, bench.lane);
  assert.ok(ai.coreTop > ai.top, "the lead is drawn above the planned start");
  const night = p.filter((x) => x.occ.entry === "night");
  assert.equal(night.length, 2);
  assert.ok(night[0].clippedBottom && night[1].clippedTop);
  assert.equal(night[1].top, 0);
});

test("effect rows round-trip and the entry is tidied", () => {
  for (const v of [
    { block_switch: "server:deepthought" },
    { reserve_card: "B", on_reserve: "wait" },
    { reserve_slots: "card:B", n: 2 },
    { fan_profile: "quiet", fan: "gpu" },
    { mute_alerts: "hot_*" },
    "info",
  ]) assert.deepEqual(effectValue(effectRow(v)), v);
  assert.deepEqual(tidy({ title: "x", at: "", effects: [], exdates: [] }), { title: "x", effects: [] });
  assert.equal(whenKind({ title: "x", cron: "0 7 * * *" }), "cron");
  assert.equal(whenKind({ title: "x", after: "ai-news" }), "rrule");
});

test("phases and key states", () => {
  const o = occ("ai-news", "2026-11-25T07:00:00Z", "2026-11-25T08:30:00Z", 900);
  assert.equal(phaseOf(o, utc("2026-11-25T06:50:00Z")), "lead");
  assert.equal(phaseOf(o, utc("2026-11-25T07:10:00Z")), "active");
  assert.equal(phaseOf({ ...o, block_end: utc("2026-11-25T12:25:00Z"), extended: true }, utc("2026-11-25T12:00:00Z")), "extended");
  const k = { id: 1, event: "e", prefix: "abe_x", owner: "admin", created: 0 };
  assert.equal(keyState(k, 10), "live");
  assert.equal(keyState({ ...k, retire_at: 20 }, 10), "retiring");
  assert.equal(keyState({ ...k, revoked: 5 }, 10), "revoked");
});
