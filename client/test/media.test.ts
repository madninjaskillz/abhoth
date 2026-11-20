// Where a turn's media is previewed (each file once, outside the collapsible blocks) and what a
// collapsible block shows when it is opened. media.ts only imports types, so plain node runs it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { argPath, itemMedia, mediaPathsIn, planMedia } from "../src/features/chat/media.ts";
import type { ChatItem } from "../src/store/session.ts";

let key = 1;
const user = (text = "draw a whale"): ChatItem => ({ kind: "user", key: key++, text, source: "web" });
const thinking = (text: string, streaming = false): ChatItem => ({ kind: "thinking", key: key++, text, streaming });
const answer = (text: string, streaming = false): ChatItem => ({ kind: "assistant", key: key++, text, streaming });
const system = (text: string): ChatItem => ({ kind: "system", key: key++, text });
const tool = (name: string, args: object, result?: string, extra: { isError?: boolean } = {}): ChatItem => ({
  kind: "tool",
  key: key++,
  name,
  args: JSON.stringify(args),
  result,
  done: result !== undefined,
  ...extra,
});

/** Every preview the plan draws, in transcript order. */
const drawn = (items: ChatItem[]) => {
  const plan = planMedia(items);
  return items.flatMap((i) => (plan.get(i.key) ?? []).map((p) => `${i.kind}:${p}`));
};

const IMG = "C:/Users/me/.openorca/images/whale.png";

test("an image a turn mentions four times is drawn once, under the answer", () => {
  const items = [
    user(),
    thinking(`I'll call generate_image and save to ${IMG}`),
    tool("generate_image", { prompt: "a whale" }, `Saved ${IMG} (1024x1024)`),
    thinking(`The image is at ${IMG}; let me look at it`),
    tool("read", { path: IMG }, "(image)"),
    answer(`Here is your whale: \`${IMG}\``),
  ];
  assert.deepEqual(drawn(items), [`assistant:${IMG}`]);
});

test("a thought never gets a default preview; opened, it shows its own media", () => {
  const t = thinking(`saved ${IMG}`);
  assert.deepEqual(drawn([user(), t]), []);
  assert.deepEqual(itemMedia(t), [IMG]);
});

test("no answer mentions the image: it is drawn once, after the first tool call that has it", () => {
  const gen = tool("generate_image", { prompt: "a whale" }, `Saved ${IMG}`);
  const items = [user(), gen, tool("read", { path: IMG }, "(image)"), answer("Done, a whale for you.")];
  const plan = planMedia(items);
  assert.deepEqual(plan.get(gen.key), [IMG]);
  assert.equal(plan.size, 1);
});

test("while the turn runs the image is under its tool call, then under the finished answer", () => {
  const gen = tool("generate_image", { prompt: "a whale" }, `Saved ${IMG}`);
  const streaming = answer(`Here it is: ${IMG}`, true);
  assert.deepEqual(drawn([user(), gen, streaming]), [`tool:${IMG}`]);
  assert.deepEqual(drawn([user(), gen, { ...streaming, streaming: false } as ChatItem]), [`assistant:${IMG}`]);
});

test("a running or failed tool call shows no media", () => {
  const running = tool("generate_image", { path: "out/a.png" });
  const failed = tool("read", { path: "out/a.png" }, "no such file out/a.png", { isError: true });
  assert.deepEqual(itemMedia(running), []);
  assert.deepEqual(itemMedia(failed), []);
  assert.deepEqual(drawn([user(), running, failed]), []);
});

test("two answers in a turn: the first that mentions the file draws it; other files go where they are first mentioned", () => {
  const items = [
    user(),
    tool("generate_image", {}, "Saved out/a.png and out/b.png"),
    answer("First one is out/a.png."),
    tool("bash", { command: "ls out" }, "a.png\nb.png\nc.wav"),
    answer("Both: out/a.png and out/b.png."),
  ];
  // a.png and b.png (bare names in the ls output) are other spellings, so they count as other files.
  assert.deepEqual(drawn(items), ["assistant:out/a.png", "tool:a.png", "tool:b.png", "tool:c.wav", "assistant:out/b.png"]);
});

test("the same file written with other slashes, ./ or drive-letter case is one file", () => {
  const items = [
    user(),
    tool("generate_image", {}, "Saved C:\\Users\\me\\pics\\Whale.png and ./out/a.png"),
    answer("See c:/Users/me/pics/Whale.png and out/a.png"),
  ];
  assert.deepEqual(drawn(items), ["assistant:c:/Users/me/pics/Whale.png", "assistant:out/a.png"]);
});

test("each turn is planned on its own: a later turn shows the image again, once", () => {
  const items = [
    user(),
    tool("generate_image", {}, `Saved ${IMG}`),
    answer(`Here: ${IMG}`),
    user("make it bluer"),
    tool("generate_image", {}, `Saved ${IMG}`),
    answer(`Bluer now: ${IMG}`),
  ];
  assert.deepEqual(drawn(items), [`assistant:${IMG}`, `assistant:${IMG}`]);
});

test("history without a user message first, and a 'tool results' block, are handled", () => {
  const results = system(`[tool results]\nSaved ${IMG}`);
  const items = [results, answer(`It is at ${IMG}`)];
  assert.deepEqual(drawn(items), [`assistant:${IMG}`]);
  // The block itself only shows media when opened.
  assert.deepEqual(itemMedia(results), [IMG]);
  assert.deepEqual(drawn([results]), []);
});

test("a tool call's file argument counts only when it is media", () => {
  assert.equal(argPath(JSON.stringify({ path: "src/a.ts" })), "src/a.ts");
  assert.deepEqual(itemMedia(tool("read", { path: "src/a.ts" }, "export {}")), []);
  assert.deepEqual(itemMedia(tool("read", { file_path: "shots/a.png" }, "ok shots/a.png")), ["shots/a.png"]);
});

test("mediaPathsIn: links, code spans and bare paths, not URLs, no repeats", () => {
  assert.deepEqual(mediaPathsIn("![w](out/w.png) and `out/w.png`, also out/song.mp3; not https://x.org/a.png"), ["out/w.png", "out/song.mp3"]);
  assert.deepEqual(mediaPathsIn("![a blue whale](out/w.png)"), ["out/w.png"]);
  assert.deepEqual(mediaPathsIn(null), []);
});
