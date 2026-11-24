// The agent bars' model name: what served the agent, not the config's alias (modelLabel.ts has no imports).
import { test } from "node:test";
import assert from "node:assert/strict";
import { cardModel, cardOfSlot, modelLabel } from "../src/features/agents/modelLabel.ts";

const inkling = { engine: "ninfer", model: "inkling2-mini-q8.ninfer", options: { alias: "inkling-2-mini" } };
const layout = { layout: "merged-inkling-x2", cards: { A: inkling, B: inkling } };
const mixed = { layout: "split", cards: { A: inkling, B: { engine: "strata", model: "qwen3.8-27b-q4.gguf", options: { alias: "qwen3.8-27b" } } } };

test("slots map to the card that has them", () => {
  assert.equal(cardOfSlot("B3", layout), "B");
  assert.equal(cardOfSlot("a", layout), "A");
  assert.equal(cardOfSlot("C1", layout), null);
  assert.equal(cardOfSlot("waiting", layout), null);
  assert.equal(cardOfSlot("—", layout), null);
  assert.equal(cardOfSlot(null), null);
  assert.equal(cardOfSlot("A2"), "A");
});

test("a card's name is its alias, else its model file; off cards have none", () => {
  assert.equal(cardModel(inkling), "inkling-2-mini");
  assert.equal(cardModel({ engine: "ninfer", model: "x.ninfer" }), "x.ninfer");
  assert.equal(cardModel({ engine: "off", model: "x" }), null);
  assert.equal(cardModel(null), null);
});

test("the served model wins, with the alias in the tooltip", () => {
  const l = modelLabel({ slot: "B2", model: "inkling-2-mini", requestedModel: "qwen3.8-27b", servedModel: "inkling-2-mini" }, layout, "qwen3.8-27b");
  assert.equal(l.text, "inkling-2-mini");
  assert.equal(l.source, "served");
  assert.equal(l.tip, "requested qwen3.8-27b → served by inkling-2-mini (card B)");
});

test("served without a card (through the gateway)", () => {
  const l = modelLabel({ slot: "—", requestedModel: "qwen3.8-27b", servedModel: "inkling-2-mini" }, layout, "qwen3.8-27b");
  assert.equal(l.tip, "requested qwen3.8-27b → served by inkling-2-mini");
  assert.equal(l.card, null);
});

test("before a reply, the live layout's card stands in", () => {
  const l = modelLabel({ slot: "B1", model: "qwen3.8-27b", requestedModel: "qwen3.8-27b" }, mixed, "qwen3.8-27b");
  assert.equal(l.text, "qwen3.8-27b");
  assert.equal(l.source, "layout");
  const a = modelLabel({ slot: "A1", model: "qwen3.8-27b", requestedModel: "qwen3.8-27b" }, mixed, "qwen3.8-27b");
  assert.equal(a.text, "inkling-2-mini");
  assert.match(a.tip, /requested qwen3\.8-27b → live layout split: inkling-2-mini \(card A\)/);
});

test("no card known: the layout's model, or each one when the cards differ", () => {
  assert.equal(modelLabel({ slot: null, requestedModel: "qwen3.8-27b" }, layout, "qwen3.8-27b").text, "inkling-2-mini");
  assert.equal(modelLabel({ slot: "waiting", requestedModel: "qwen3.8-27b" }, mixed, "qwen3.8-27b").text, "inkling-2-mini / qwen3.8-27b");
});

test("a helper on another provider keeps its own model", () => {
  const l = modelLabel({ slot: null, model: "claude-haiku-4-5", requestedModel: "claude-haiku-4-5" }, layout, "qwen3.8-27b");
  assert.equal(l.text, "claude-haiku-4-5");
  assert.equal(l.source, "config");
});

test("a bridge (no requested model) shows its own label; no layout = the config's name", () => {
  assert.equal(modelLabel({ slot: "claude", model: "Claude Code" }, layout, null).text, "Claude Code");
  const l = modelLabel({ slot: "A1", model: "qwen3.8-27b", requestedModel: "qwen3.8-27b" }, null, "qwen3.8-27b");
  assert.equal(l.text, "qwen3.8-27b");
  assert.equal(l.source, "config");
  assert.equal(modelLabel({}, null, null).tip, "model —");
});
