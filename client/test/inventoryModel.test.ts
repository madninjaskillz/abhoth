// The router inventory forms (D152): the edits each form sends, ids, fit hints and the guided
// steps (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  cardEnginesEdit, enginesFor, fitSummary, mainFile, modelEngineEdits, modelIdFromRepo, moved, newCardEdits, newModelEdits, newServerEdits, nextCardId, nextStep, pct,
  placementLine, repoDir, toggled, validModelDir, validateServerId, ownServers, bridgedServers, cardLabel, serverLabel, type HfDetails, type Inventory, type InvEngine, type InvModel,
} from "../src/features/router/forms/inventoryModel.ts";

const engine = (id: string, kinds: InvEngine["kinds"], formats: string[], archs: string[] = [], base?: string): InvEngine => ({
  id, title: id, kinds, driver: "docker", cards: ["A"], profiles: [], caps: { kinds, formats, archs, quants: [], sources: [], base }, probeHow: "props", usedBy: [],
});

const inv: Inventory = {
  migrated: true,
  catalog: [],
  kinds: [{ id: "llm", label: "Language models" }, { id: "tts", label: "Text to speech" }],
  servers: [{ id: "gpu", exec: "ssh", host: "gpu-host", modelDir: "/srv/models", cards: [] }],
  engines: [engine("llamacpp", ["llm", "embedding"], ["gguf"]), engine("llamacpp-fork", ["llm", "embedding"], ["gguf"], [], "llamacpp"), engine("vllm", ["llm"], ["safetensors"]), engine("kokoro", ["tts"], ["onnx"], ["kokoro"])],
  models: [],
};

test("engines for a file go by type, format and architecture (the fork too)", () => {
  assert.deepEqual(enginesFor(inv, "llm", "gguf", "qwen3").map((e) => e.id), ["llamacpp", "llamacpp-fork"]);
  assert.deepEqual(enginesFor(inv, "llm", "safetensors").map((e) => e.id), ["vllm"]);
  assert.deepEqual(enginesFor(inv, "tts", "onnx", "Kokoro").map((e) => e.id), ["kokoro"]);
  assert.deepEqual(enginesFor(inv, "tts", "onnx", "piper"), []);
  assert.deepEqual(enginesFor(inv, "llm", "exl2"), []);
});

test("card engine ticks become one engines table", () => {
  assert.deepEqual(toggled(["llamacpp"], "vllm", true), ["llamacpp", "vllm"]);
  assert.deepEqual(toggled(["*"], "vllm", false), []);
  assert.deepEqual(cardEnginesEdit("A", { llm: ["llamacpp"], tts: [], stt: ["whisper"] }), { path: ["cards", "A", "engines"], value: { llm: ["llamacpp"], stt: ["whisper"] } });
});

test("detected GPUs become cards by UUID with the next free letter", () => {
  assert.equal(nextCardId([]), "A");
  assert.equal(nextCardId(["A", "B", "D"]), "C");
  const e = newCardEdits("C", "gpu", { index: 2, uuid: "GPU-cccc", name: "RTX 4090", vramGib: 24 }, { llm: ["llamacpp"] });
  assert.deepEqual(e, [{ path: ["cards", "C"], value: { server: "gpu", gpu_uuid: "GPU-cccc", label: "RTX 4090", managed: true, on_demand: true, engines: { llm: ["llamacpp"] }, vram_gib: 24 } }]);
});

test("a new server: ssh or local, with its model folder", () => {
  const f = { title: "", host: "gpu-host", exec: "ssh" as const, user: "", key: "secret:ssh_gpu", hostKey: "SHA256:x", modelDir: "/srv/models" };
  assert.deepEqual(newServerEdits("gpu2", f)[0].value, { title: "gpu2", exec: "ssh", host: "gpu-host", ssh: { user: "abhoth-exec", key: "secret:ssh_gpu", host_key: "SHA256:x" }, model_dir: "/srv/models" });
  assert.deepEqual(newServerEdits("box", { ...f, exec: "local", modelDir: "" })[0].value, { title: "box", exec: "local" });
  assert.equal(validateServerId("gpu", ["gpu"]), "there is a server with that name");
  assert.match(validateServerId("Bad Name", []) ?? "", /lower-case/);
  assert.equal(validateServerId("gpu-2", ["gpu"]), null);
  assert.match(validModelDir("models") ?? "", /absolute/);
  assert.match(validModelDir("/srv/../etc") ?? "", /\.\./);
  assert.equal(validModelDir("/srv/models"), null);
});

test("a model's engines: primary, preference order, allow and deny", () => {
  assert.deepEqual(modelEngineEdits("m", { primary: "ninfer", prefer: ["ninfer", "llamacpp"], allow: [], deny: ["vllm"] }), [
    { path: ["models", "m", "primary"], value: "ninfer" },
    { path: ["models", "m", "prefer"], value: ["llamacpp"] },
    { path: ["models", "m", "allow"], value: null },
    { path: ["models", "m", "deny"], value: ["vllm"] },
  ]);
  assert.deepEqual(moved(["a", "b", "c"], "c", -1), ["a", "c", "b"]);
  assert.deepEqual(moved(["a", "b"], "a", -1), ["a", "b"]);
});

const details: HfDetails = {
  repo: "example/Tiny-Chat-GGUF", revision: "abc", kind: "llm", arch: "qwen3", context: 40960, vision: true, licence: "apache-2.0", gated: false, title: "Tiny-Chat-GGUF",
  choices: [{ id: "gguf-Q4_K_M", label: "Q4_K_M", format: "gguf", quant: "Q4_K_M", bytes: 5e9, sizeGib: 5.4, needGib: 5.9, files: [{ path: "Tiny-Q4_K_M.gguf", size: 5e9 }, { path: "mmproj-F16.gguf", size: 8e8 }] }],
};

test("a Hugging Face choice becomes a catalog model with its source and file", () => {
  assert.equal(modelIdFromRepo("Qwen/Qwen3-8B-GGUF"), "qwen3-8b");
  assert.equal(modelIdFromRepo("example/Tiny-Chat-GGUF", ["tiny-chat"]), "tiny-chat-2");
  const dir = repoDir("/srv/models/", details.repo);
  assert.equal(dir, "/srv/models/example/Tiny-Chat-GGUF");
  const [e] = newModelEdits("tiny-chat", details, details.choices[0], "llamacpp", [], dir);
  assert.deepEqual(e.path, ["models", "tiny-chat"]);
  const v = e.value as Record<string, unknown>;
  assert.equal(v.kind, "llm");
  assert.equal(v.primary, "llamacpp");
  assert.equal(v.arch, "qwen3");
  assert.equal(v.quant, "Q4_K_M");
  assert.equal(v.vision, true);
  assert.deepEqual(v.servers, []);
  assert.deepEqual(v.source, { hf: "example/Tiny-Chat-GGUF", revision: "abc", files: ["Tiny-Q4_K_M.gguf", "mmproj-F16.gguf"] });
  assert.deepEqual(v.artifacts, { llamacpp: { file: `${dir}/Tiny-Q4_K_M.gguf`, vram_gib: 5.9, options: { mmproj: `${dir}/mmproj-F16.gguf` } } });
});

test("fit hints, progress and placement lines", () => {
  assert.deepEqual(fitSummary([{ card: "A", server: "gpu", fits: true }, { card: "B", server: "gpu", fits: false }]), { ok: true, text: "fits on A" });
  assert.deepEqual(fitSummary([{ card: "A", server: "gpu", fits: false }]), { ok: false, text: "too big for every card" });
  assert.equal(fitSummary(undefined).ok, false);
  assert.equal(pct(50, 200), 25);
  assert.equal(pct(5, 0), 0);
  const m = { placement: [{ card: "A", server: "gpu", engine: "ninfer", profile: "2lane", rank: 0, ok: true, why: [] }, { card: "B", server: "gpu", engine: "ninfer", profile: "2lane", rank: 0, ok: true, why: [] }] } as unknown as InvModel;
  assert.equal(placementLine(m), "ninfer (2lane) on A, B");
  assert.equal(placementLine({ placement: [{ ok: false }] } as unknown as InvModel), "nowhere to run it yet");
});

test("the guided steps", () => {
  assert.equal(nextStep({ ...inv, servers: [] }), "server");
  assert.equal(nextStep(inv), "cards");
  const withCard = { ...inv, servers: [{ ...inv.servers[0], cards: [{ id: "A", gpuUuid: "GPU-a", reserveGib: 0.5, managed: true, onDemand: true, explicit: true, engines: {} }] }] };
  assert.equal(nextStep({ ...withCard, engines: [] }), "engines");
  assert.equal(nextStep(withCard), "models");
});

test("bridged machines are a group of their own", () => {
  const card = { id: "NAS-GPU0", gpuUuid: "GPU-n", reserveGib: 0, managed: true, onDemand: true, explicit: true, engines: {} };
  const nas = { id: "nas", title: "truenas.local", exec: "bridge" as const, origin: "bridge:nas", cards: [card] };
  const both = { ...inv, servers: [...inv.servers, nas] };
  assert.deepEqual(ownServers(both).map((s) => s.id), inv.servers.map((s) => s.id));
  assert.deepEqual(bridgedServers(both).map((s) => s.id), ["nas"]);
  // a bridged machine with cards is not "a server with cards": the guide still asks for one
  assert.equal(nextStep({ ...both, servers: [nas] }), "server");
  assert.equal(nextStep(both), "cards");
  assert.equal(cardLabel(both, "NAS-GPU0"), "NAS-GPU0 (bridge nas)");
  assert.equal(cardLabel(both, "A"), "A");
  assert.equal(serverLabel(nas), "truenas.local (bridged)");
});

test("a model whose download has not finished says so", () => {
  assert.equal(placementLine({ servers: [], placement: [{ ok: false }] } as unknown as InvModel), "files not on a server yet");
});

test("the file an engine loads: first GGUF shard, the one weight file, else the folder", () => {
  const ch = (format: string, paths: [string, number][]) => ({ id: "x", label: "x", format, bytes: 0, sizeGib: 1, needGib: 1, files: paths.map(([path, size]) => ({ path, size })) });
  assert.equal(mainFile(ch("gguf", [["mmproj-F16.gguf", 9], ["M-Q8_0-00002-of-00002.gguf", 5], ["M-Q8_0-00001-of-00002.gguf", 5]])), "M-Q8_0-00001-of-00002.gguf");
  assert.equal(mainFile(ch("onnx", [["config.json", 1], ["kokoro-v1.onnx", 100], ["voices.bin", 20]])), "kokoro-v1.onnx");
  assert.equal(mainFile(ch("safetensors", [["config.json", 1], ["model-00001-of-00002.safetensors", 5], ["model-00002-of-00002.safetensors", 5]])), null);
  assert.equal(mainFile(ch("safetensors", [["config.json", 1], ["model.safetensors", 5]])), "model.safetensors");
  const [e] = newModelEdits("v", { ...details, kind: "tts", vision: false }, ch("safetensors", [["config.json", 1], ["a.safetensors", 5], ["b.safetensors", 5]]), "vllm", [], "/m/o/v");
  assert.equal(((e.value as Record<string, Record<string, Record<string, unknown>>>).artifacts.vllm).file, "/m/o/v");
});

test("D153: a CPU card leaves the host some RAM and is never switched", async () => {
  const { newCpuCardEdits } = await import("../src/features/router/forms/inventoryModel.ts");
  const [e] = newCpuCardEdits("DT-CPU", "deepthought", { model: "AMD EPYC 7K62", cores: 96, threads: 192, ramGib: 251 }, { music: ["sa3"] });
  assert.deepEqual(e.path, ["cards", "DT-CPU"]);
  const v = e.value as Record<string, unknown>;
  assert.equal(v.device, "cpu");
  assert.equal(v.managed, false);
  assert.equal(v.on_demand, false);
  assert.equal(v.cores, 96);
  assert.equal(v.ram_gib, 188.3);
  assert.equal(v.gpu_uuid, undefined);
  const [s] = newCpuCardEdits("S-CPU", "small", { ramGib: 16 });
  assert.equal((s.value as Record<string, unknown>).ram_gib, 8);
});

test("D153: presence and needs lines", async () => {
  const { presenceLine, needsLine } = await import("../src/features/router/forms/inventoryModel.ts");
  const m = { present: { deepthought: true, pc: false, lab: null } } as unknown as InvModel;
  assert.equal(presenceLine(m), "deepthought ✓ · pc ✗ · lab ?");
  const r = { needsMet: { deepthought: { ok: true, missing: [] }, pc: { ok: false, missing: ["qwen-clip"] } } } as unknown as InvModel;
  assert.equal(needsLine(r), "deepthought: ready · pc: missing qwen-clip");
  assert.equal(presenceLine({} as InvModel), "");
});
