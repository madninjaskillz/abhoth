// Local Engines' model (LOCAL-ENGINES.md, D156): ids, models, local URLs and tool choices.
import { test } from "node:test";
import assert from "node:assert/strict";
import { CLOUD_PRESETS, KIND_LABELS, idFrom, isLocalUrl, parseModels, toolChoices, type LocalEngines } from "../src/features/machine/localEnginesModel.ts";

test("ids are made from names as the server makes them", () => {
  assert.equal(idFrom("LM Studio", new Set()), "lm-studio");
  assert.equal(idFrom("LM Studio", new Set(["lm-studio"])), "lm-studio-2");
  assert.equal(idFrom("  ComfyUI (NAS)! ", new Set()), "comfyui-nas");
  assert.equal(idFrom("!!!", new Set()), "engine");
});

test("models are one per line or comma-separated; none is Engine default", () => {
  assert.deepEqual(parseModels("a\n b ,c\n\na"), ["a", "b", "c"]);
  assert.deepEqual(parseModels("  "), []);
});

test("only this PC's own addresses are local; anything else is relayed through this PC", () => {
  for (const u of ["http://127.0.0.1:1234/v1", "http://localhost:11434", "tcp://127.0.0.1:10300"]) assert.ok(isLocalUrl(u), u);
  for (const u of ["http://nas:8188", "https://api.anthropic.com/v1", "http://10.0.0.5:8000", "nonsense"]) assert.ok(!isLocalUrl(u), u);
});

test("a tool's choices are the exposed engines of its kind, managed first", () => {
  const le = {
    managed: [
      { id: "comfyui", title: "ComfyUI", kinds: ["image"], tool: true },
      { id: "ninfer", title: "ninfer", kinds: ["llm"], tool: true },
      { id: "off", title: "Off", kinds: ["image"], tool: false },
    ],
    endpoints: [
      { id: "dalle", title: "DALL-E", kind: "image", tool: true },
      { id: "kokoro", title: "Kokoro", kind: "tts", tool: true },
    ],
  } as unknown as LocalEngines;
  assert.deepEqual(toolChoices(le, "image").map((c) => c.id), ["comfyui", "dalle"]);
  assert.deepEqual(toolChoices(le, "tts").map((c) => c.id), ["kokoro"]);
  assert.deepEqual(toolChoices(le, "music"), []);
});

test("every kind has a label and the Cloud presets are https", () => {
  for (const k of ["llm", "tts", "stt", "image", "music", "embedding", "video"]) assert.ok(KIND_LABELS[k as keyof typeof KIND_LABELS], k);
  for (const p of CLOUD_PRESETS) assert.match(p.url, /^https:\/\//);
});
