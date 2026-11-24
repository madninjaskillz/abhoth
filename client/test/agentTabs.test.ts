// The agent strip's view toggle, the tab model and a sub-agent's transcript as chat items
// (agentTabs.ts has only type imports, so plain node runs it).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  VIEW_KEY, applyDelta, applyEntry, buildTabs, fromWire, loadView, saveView, selectedTab, tabState, transcriptItems,
  type AgentEntry, type AgentTranscriptWire, type ViewStorage,
} from "../src/features/agents/agentTabs.ts";

const memory = (): ViewStorage & { data: Map<string, string> } => {
  const data = new Map<string, string>();
  return { data, getItem: (k) => data.get(k) ?? null, setItem: (k, v) => void data.set(k, v) };
};
const throwing: ViewStorage = {
  getItem: () => { throw new Error("blocked"); },
  setItem: () => { throw new Error("blocked"); },
};

test("the view defaults to bars and remembers tabs", () => {
  const s = memory();
  assert.equal(loadView(s), "bars");
  saveView("tabs", s);
  assert.equal(s.data.get(VIEW_KEY), "tabs");
  assert.equal(loadView(s), "tabs");
  saveView("bars", s);
  assert.equal(loadView(s), "bars");
  s.data.set(VIEW_KEY, "garbage");
  assert.equal(loadView(s), "bars");
});

test("blocked storage falls back to bars and never throws", () => {
  assert.equal(loadView(throwing), "bars");
  assert.doesNotThrow(() => saveView("tabs", throwing));
  assert.equal(loadView(null), "bars");
});

const agent = (id: string, letter: string, status: string, extra: Record<string, unknown> = {}) => ({
  id, letter, label: `task ${letter}`, type: "general", status, isLead: false, ...extra,
});
const lead = { id: "lead", letter: "L", label: "Lead", type: "main", status: "idle", isLead: true };

test("tabs: Lead first, one per helper, with compact states", () => {
  const tabs = buildTabs([agent("a1", "A", "running", { progress: 0.42 }), lead, agent("b1", "B", "completed"),
    agent("c1", "C", "failed"), agent("d1", "D", "pending"), agent("e1", "E", "running")], true);
  assert.deepEqual(tabs.map((t) => t.id), ["lead", "a1", "b1", "c1", "d1", "e1"]);
  // By letter, whatever order the bars list them in (running first), so tabs don't jump.
  assert.deepEqual(buildTabs([agent("b1", "B", "running"), lead, agent("a1", "A", "completed")], false).map((t) => t.id), ["lead", "a1", "b1"]);
  assert.equal(tabs[0].state, "running");
  assert.equal(tabs[0].label, "Lead");
  assert.equal(tabs[1].pct, 42);
  assert.deepEqual(tabs.slice(1).map((t) => t.state), ["running", "done", "failed", "waiting", "running"]);
  assert.equal(tabs[5].pct, null);
  assert.equal(buildTabs([lead], false)[0].state, "idle");
});

test("status words map to tab states", () => {
  assert.equal(tabState("completedpartial"), "done");
  assert.equal(tabState("cancelled"), "failed");
  assert.equal(tabState("idle"), "idle");
});

test("the chosen tab holds while listed; the Lead or a vanished agent means the main chat", () => {
  const tabs = buildTabs([lead, agent("a1", "A", "completed")], false);
  assert.equal(selectedTab(tabs, "a1"), "a1");
  assert.equal(selectedTab(tabs, "lead"), null);
  assert.equal(selectedTab(tabs, "gone"), null);
  assert.equal(selectedTab(tabs, null), null);
});

const entry = (n: number, kind: AgentEntry["kind"], text: string, extra: Partial<AgentEntry> = {}): AgentEntry =>
  ({ agentId: "a1", n, at: "", kind, text, isError: false, ...extra });

const wire = (extra: Partial<AgentTranscriptWire> = {}): AgentTranscriptWire => ({
  agentId: "a1", letter: "A", label: "task A", type: "general", status: "running", task: "do it",
  entries: [
    entry(0, "task", "Do the thing"),
    entry(1, "assistant", "Looking."),
    entry(2, "tool", "read a.cs", { tool: "read_file", args: '{"path":"a.cs"}' }),
    entry(3, "toolResult", "class A {}"),
  ],
  dropped: 0, streaming: "Half a", streamEnd: 20, canMessage: true, canCancel: true,
  ...extra,
});

test("a transcript becomes chat items: the Lead's prompt, replies, tool cards, the streaming reply", () => {
  const s = fromWire(wire());
  const items = transcriptItems(s);
  assert.deepEqual(items.map((i) => i.kind), ["user", "assistant", "tool", "assistant"]);
  assert.equal(items[0].kind === "user" && items[0].source, "lead");
  const tool = items[2];
  assert.ok(tool.kind === "tool" && tool.done && tool.name === "read_file" && tool.result === "class A {}");
  assert.ok(items[3].kind === "assistant" && items[3].streaming && items[3].text === "Half a");
  assert.equal(s.nextN, 4);
});

test("live entries already in the fetched transcript are skipped; new ones append", () => {
  let s = fromWire(wire());
  const before = s.items.length;
  s = applyEntry(s, entry(3, "toolResult", "again"));
  assert.equal(s.items.length, before);
  s = applyEntry(s, entry(4, "message", "use B instead", { from: "user" }));
  const last = s.items.at(-1);
  assert.ok(last?.kind === "user" && last.text === "use B instead" && last.source === "web");
  s = applyEntry(s, entry(5, "message", "fix the test", { from: "lead" }));
  const lead2 = s.items.at(-1);
  assert.ok(lead2?.kind === "user" && lead2.source === "lead");
});

test("deltas take only what is past the fetched stream; an entry ends the streaming reply", () => {
  let s = fromWire(wire());
  s = applyDelta(s, { agentId: "a1", text: "Half a", end: 20 }); // already in the snapshot
  assert.equal(s.streaming, "Half a");
  s = applyDelta(s, { agentId: "a1", text: "a sentence", end: 29 }); // 9 new characters
  assert.equal(s.streaming, "Half a sentence");
  s = applyEntry(s, entry(4, "assistant", "Half a sentence."));
  assert.equal(s.streaming, "");
  assert.equal(transcriptItems(s).at(-1)?.kind, "assistant");
  s = applyDelta(s, { agentId: "a1", text: "Next", end: 33 });
  assert.equal(s.streaming, "Next");
});

test("progress markers are dropped from the agent's text", () => {
  let s = fromWire(wire({ entries: [entry(0, "assistant", "[progress 40%] Reading the store.")], streaming: "[progress 50%] Now hub", streamEnd: 5 }));
  const items = transcriptItems(s);
  assert.ok(items[0].kind === "assistant" && items[0].text === "Reading the store.");
  assert.ok(items[1].kind === "assistant" && items[1].text === "Now hub");
  s = { ...s, streaming: "[progress 60%] " };
  assert.equal(transcriptItems(s).length, 1);
});

test("a status entry is a notice; errors show as errors; dropped entries are mentioned", () => {
  let s = fromWire(wire({ entries: [], dropped: 12, streaming: "", streamEnd: 0 }));
  assert.equal(s.nextN, 12);
  assert.ok(s.items[0].kind === "notice" && /12 earlier/.test(s.items[0].text));
  s = applyEntry(s, entry(12, "status", "failed: boom", { isError: true }));
  const n = s.items.at(-1);
  assert.ok(n?.kind === "notice" && n.level === "error");
});
