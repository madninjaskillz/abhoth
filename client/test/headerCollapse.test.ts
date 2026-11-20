// When the app bar's buttons fold into the menu, and the menu's arrow keys (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import { PHONE_WIDTH, UNFOLD_SLACK, nextMenuIndex, shouldCollapse } from "../src/lib/headerCollapse.ts";

const bar = (m: Partial<Parameters<typeof shouldCollapse>[0]>) =>
  shouldCollapse({ viewport: 1600, room: 1400, tabs: 300, actions: 320, collapsed: false, ...m });

test("a wide desktop with a few tabs keeps the buttons in the bar", () => {
  assert.equal(bar({}), false);
  assert.equal(bar({ collapsed: true }), false);
});

test("many tabs: the buttons fold when tabs and buttons don't both fit", () => {
  assert.equal(bar({ tabs: 1080 }), false); // exactly fits
  assert.equal(bar({ tabs: 1081 }), true);
  assert.equal(bar({ tabs: 3000 }), true);
});

test("a narrower window folds them with the same tabs", () => {
  assert.equal(bar({ viewport: 700, room: 600, tabs: 300 }), true);
  assert.equal(bar({ viewport: 700, room: 640, tabs: 300 }), false);
});

test("a phone always uses the menu, even with one tab", () => {
  assert.equal(bar({ viewport: PHONE_WIDTH, room: 400, tabs: 80, actions: 150 }), true);
  assert.equal(bar({ viewport: 360, room: 300, tabs: 80, actions: 0 }), true);
  assert.equal(bar({ viewport: PHONE_WIDTH + 1, room: 400, tabs: 80, actions: 150 }), false);
});

test("folded, it unfolds only with some room to spare (no flicker at the edge)", () => {
  const need = 300 + 320;
  assert.equal(bar({ room: need, collapsed: true }), true);
  assert.equal(bar({ room: need + UNFOLD_SLACK - 1, collapsed: true }), true);
  assert.equal(bar({ room: need + UNFOLD_SLACK, collapsed: true }), false);
  // ... and once unfolded at that width it stays unfolded.
  assert.equal(bar({ room: need + UNFOLD_SLACK, collapsed: false }), false);
});

test("folded without a measured width for the buttons: unfold to measure", () => {
  assert.equal(bar({ actions: 0, collapsed: true, tabs: 5000 }), false);
});

test("arrow keys walk the menu's rows and wrap; Home and End jump", () => {
  assert.equal(nextMenuIndex(0, 4, "ArrowDown"), 1);
  assert.equal(nextMenuIndex(3, 4, "ArrowDown"), 0);
  assert.equal(nextMenuIndex(0, 4, "ArrowUp"), 3);
  assert.equal(nextMenuIndex(2, 4, "ArrowUp"), 1);
  assert.equal(nextMenuIndex(-1, 4, "ArrowDown"), 0);
  assert.equal(nextMenuIndex(-1, 4, "ArrowUp"), 3);
  assert.equal(nextMenuIndex(2, 4, "Home"), 0);
  assert.equal(nextMenuIndex(1, 4, "End"), 3);
  assert.equal(nextMenuIndex(2, 4, "a"), 2);
  assert.equal(nextMenuIndex(0, 0, "ArrowDown"), -1);
});
