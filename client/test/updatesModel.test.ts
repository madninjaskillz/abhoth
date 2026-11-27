// Settings → Updates' model: the update service's state in words, and when to poll.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ago, busy, canCheck, statusLine, type UpdateState } from "../src/features/settings/updatesModel.ts";

const base: UpdateState = { phase: "idle", current: "0.16.37", channel: "stable" };

test("each phase reads as a sentence", () => {
  assert.equal(statusLine(base), "Not checked since the app started.");
  assert.equal(statusLine({ ...base, phase: "checking" }), "Checking GitHub for a new release…");
  assert.equal(statusLine({ ...base, phase: "downloading", available: "0.16.38", progress: 42 }), "Downloading 0.16.38… 42%");
  assert.equal(statusLine({ ...base, phase: "ready", available: "0.16.38" }), "0.16.38 is downloaded. Restart to finish updating.");
  assert.equal(statusLine({ ...base, phase: "upToDate" }), "You're on the latest release (0.16.37).");
  assert.equal(statusLine({ ...base, phase: "error", error: "Couldn't reach GitHub" }), "Couldn't reach GitHub");
  assert.match(statusLine({ ...base, phase: "unavailable", reason: "a dev build" }), /dev build/);
});

test("the check button: only when nothing is under way, nothing is ready, and the copy is installed", () => {
  assert.deepEqual(canCheck(base), { ok: true });
  assert.deepEqual(canCheck({ ...base, phase: "error" }), { ok: true });
  assert.deepEqual(canCheck({ ...base, phase: "upToDate" }), { ok: true });
  assert.equal(canCheck({ ...base, phase: "checking" }).ok, false);
  assert.equal(canCheck({ ...base, phase: "downloading" }).ok, false);
  assert.equal(canCheck({ ...base, phase: "ready" }).ok, false);
  assert.equal(canCheck({ ...base, phase: "unavailable" }).why, "Not an installed copy");
  assert.ok(busy({ ...base, phase: "checking" }) && busy({ ...base, phase: "downloading" }) && !busy(base) && !busy(undefined));
});

test("the last check reads relative to now", () => {
  const now = 1_800_000_000_000;
  assert.equal(ago(null, now), "never");
  assert.equal(ago(now / 1000 - 10, now), "just now");
  assert.equal(ago(now / 1000 - 60, now), "1 minute ago");
  assert.equal(ago(now / 1000 - 5 * 3600, now), "5 hours ago");
  assert.equal(ago(now / 1000 - 3 * 86400, now), "3 days ago");
});
