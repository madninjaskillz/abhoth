// The session store's handling of the /uncensored switch (snapshot field and live event), run by
// plain node: session.ts needs react and zustand from node_modules, nothing bundled.
import { test } from "node:test";
import assert from "node:assert/strict";
import { sessionStore, type LiveSnapshot } from "../src/store/session.ts";

const snapshot = (extra: Partial<LiveSnapshot> = {}): LiveSnapshot => ({
  workingDirectory: "C:/p",
  mode: "headless",
  busy: false,
  queued: 0,
  lastSeq: 10,
  history: [],
  currentTurn: [],
  agents: { agents: [] },
  pendingPermissions: [],
  ...extra,
});

test("uncensored comes from the snapshot and is off when the server doesn't say", () => {
  const s = sessionStore("unc-snapshot");
  assert.equal(s.getState().uncensored, false);
  s.getState().loadSnapshot(snapshot({ uncensored: true }));
  assert.equal(s.getState().uncensored, true);
  s.getState().loadSnapshot(snapshot());
  assert.equal(s.getState().uncensored, false);
});

test("the uncensored event flips the marker, and the command's answer shows as a notice", () => {
  const s = sessionStore("unc-events");
  s.getState().loadSnapshot(snapshot());
  s.getState().applyEvents([
    { seq: 11, type: "notice", at: "", data: { level: "info", text: "Uncensored on\nImages: generate_image now uses qwen-image-uncensored" } },
    { seq: 12, type: "uncensored", at: "", data: { on: true } },
  ]);
  assert.equal(s.getState().uncensored, true);
  const notice = s.getState().items.at(-1);
  assert.equal(notice?.kind, "notice");
  assert.match(notice?.kind === "notice" ? notice.text : "", /qwen-image-uncensored/);

  s.getState().applyEvents([{ seq: 13, type: "uncensored", at: "", data: { on: false } }]);
  assert.equal(s.getState().uncensored, false);
  // An event already covered by the snapshot is ignored.
  s.getState().applyEvents([{ seq: 5, type: "uncensored", at: "", data: { on: true } }]);
  assert.equal(s.getState().uncensored, false);
});
