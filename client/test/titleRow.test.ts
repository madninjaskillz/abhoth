// The desktop title row: which presses move the window, the double-click, the window title (plain node).
import { test } from "node:test";
import assert from "node:assert/strict";
import { DOUBLE_CLICK_MS, DRAG_MIN, isDragHit, makeDoubleClick, windowTitle, type HitNode } from "../src/lib/titleRow.ts";

const row: HitNode = { tag: "HEADER", dragRegion: true };
const nav: HitNode = { tag: "NAV", role: "tablist", dragRegion: true };
const div = (extra: Partial<HitNode> = {}): HitNode => ({ tag: "DIV", ...extra });

test("empty space in the row, the tab strip and the spacer moves the window", () => {
  assert.equal(isDragHit([row]), true);
  assert.equal(isDragHit([nav, row]), true);
  assert.equal(isDragHit([div({ dragRegion: true }), row]), true);
  // The mascot (an image in a plain div) is not a control either.
  assert.equal(isDragHit([{ tag: "IMG" }, div(), row]), true);
});

test("tabs, buttons, menus and the status never move the window", () => {
  assert.equal(isDragHit([{ tag: "SPAN" }, div({ role: "tab" }), nav, row]), false);
  assert.equal(isDragHit([{ tag: "svg" }, { tag: "BUTTON" }, nav, row]), false); // "+" and the tab's close button
  assert.equal(isDragHit([{ tag: "BUTTON" }, div(), row]), false); // cog, account, window buttons
  assert.equal(isDragHit([{ tag: "BUTTON", role: "menuitem" }, div({ role: "menu" }), row]), false);
  assert.equal(isDragHit([{ tag: "SPAN" }, { tag: "SPAN", noDrag: true }, row]), false); // connection status
  assert.equal(isDragHit([div({ noDrag: true }), row]), false); // the resize strip
  assert.equal(isDragHit([{ tag: "INPUT" }, row]), false);
  assert.equal(isDragHit([{ tag: "A" }, row]), false);
});

test("outside a drag region, or outside the row (a dialog in a portal), nothing drags", () => {
  assert.equal(isDragHit([div(), div()]), false);
  assert.equal(isDragHit([], true), false);
  assert.equal(isDragHit([row], false), false);
});

test("a second press soon after and near the first is a double-click", () => {
  const dbl = makeDoubleClick();
  assert.equal(dbl(100, 10, 1000), false);
  assert.equal(dbl(102, 11, 1200), true);
  // A third press starts again.
  assert.equal(dbl(102, 11, 1300), false);
  assert.equal(dbl(102, 11, 1300 + DOUBLE_CLICK_MS), false); // too late
  assert.equal(dbl(200, 11, 1300 + DOUBLE_CLICK_MS + 100), false); // too far
  assert.equal(dbl(200, 11, 1300 + DOUBLE_CLICK_MS + 200), true);
});

test("the window title names the project, then the product", () => {
  assert.equal(windowTitle("Abhoth", "orca"), "orca · Abhoth");
  assert.equal(windowTitle("Abhoth", "  "), "Abhoth");
  assert.equal(windowTitle("Abhoth", ""), "Abhoth");
});

test("the row always keeps some drag space", () => {
  assert.ok(DRAG_MIN >= 40);
});
