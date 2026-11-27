// The Engine store and Create package wizard helpers: filtering, fit lines, the approvals an
// install needs, install params, and the wizard's form <-> manifest conversion (plain node).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  allTicked, applyGuesses, approvalItems, approvalsFrom, changedParams, emptyForm, emptyMethod, filterPackages, fitLine, formProblems, formToManifest,
  installedByServer, manifestJson, manifestToForm, manualEngines, methodTypes, parseApproval, parseManifest, parseMounts, parsePorts, rejectionLines,
  validStoreUrl, type CardFit, type Fit, type InstalledPkg, type Manifest, type PackageSummary,
} from "../src/features/router/forms/engineStoreModel.ts";

const pkg = (id: string, kinds: string[], extra: Partial<PackageSummary> = {}): PackageSummary => ({
  store: "default", trusted: true, id, name: id, version: "1", kinds, summary: "", api: "openai-chat", licence: "MIT", source: "", private: false,
  methods: [{ type: "docker", label: "", platforms: [], gpu: [] }], needsApproval: [], installed: [], update: null, ...extra,
});

test("packages filter by kind and by every word of the search", () => {
  const all = [
    pkg("kokoro", ["tts"], { name: "Kokoro TTS", summary: "Text to speech" }),
    pkg("llamacpp-bin", ["llm", "embedding"], { name: "llama.cpp", methods: [{ type: "binary", label: "", platforms: [], gpu: [] }, { type: "existing", label: "", platforms: [], gpu: [] }] }),
    pkg("whisper", ["stt"], { store: "community", trusted: false }),
  ];
  assert.deepEqual(filterPackages(all, "", "").map((p) => p.id), ["kokoro", "llamacpp-bin", "whisper"]);
  assert.deepEqual(filterPackages(all, "embedding", "").map((p) => p.id), ["llamacpp-bin"]);
  assert.deepEqual(filterPackages(all, "", "speech kokoro").map((p) => p.id), ["kokoro"]);
  assert.deepEqual(filterPackages(all, "", "binary").map((p) => p.id), ["llamacpp-bin"]);
  assert.deepEqual(filterPackages(all, "", "COMMUNITY").map((p) => p.id), ["whisper"]);
  assert.deepEqual(filterPackages(all, "tts", "llama"), []);
  assert.deepEqual(methodTypes({ methods: [{ type: "docker" }, { type: "pip" }, { type: "docker" }] }), ["docker", "pip"]);
});

test("store URLs must be http(s)", () => {
  assert.equal(validStoreUrl("https://example.org/index.json"), null);
  assert.match(validStoreUrl("")!, /URL/);
  assert.match(validStoreUrl("file:///x")!, /https/);
});

const fit = (f: Partial<Fit>): Fit => ({ ok: false, method: null, method_label: null, method_type: null, rejections: [], warnings: [], approvals: [], ...f });
const card = (f: Partial<Fit>): CardFit => ({ card: "A", device: "gpu", label: "RTX 3090", vramGib: 24, fit: fit(f) });

test("fit lines say which method fits, or the first reason it does not", () => {
  assert.equal(fitLine(card({ ok: true, method: 0, method_label: "Kokoro-FastAPI (GPU)", method_type: "docker" })), "Fits: Kokoro-FastAPI (GPU) (docker)");
  assert.equal(fitLine(card({ ok: true, method: 2, method_type: "pip", warnings: ["slow"] })), "Fits: pip · 1 warning");
  const no = fit({ rejections: [{ method: 0, label: "GPU", reasons: ["needs 8 GiB VRAM, the card has 6"] }, { method: 1, label: "pip (host)", reasons: ["host processes are off", "no python"] }] });
  assert.equal(fitLine({ ...card({}), fit: no }), "Does not fit: needs 8 GiB VRAM, the card has 6");
  assert.equal(fitLine(card({})), "Does not fit");
  assert.deepEqual(rejectionLines(no), ["1. GPU: needs 8 GiB VRAM, the card has 6", "2. pip (host): host processes are off; no python"]);
});

test("approval strings read as kinds with values; unknown ones stay as their own tick", () => {
  assert.deepEqual(parseApproval("host_mounts:/srv/x"), { key: "host_mounts:/srv/x", kind: "host_mounts", value: "/srv/x", label: "Mount /srv/x from the host" });
  assert.equal(parseApproval("device=/dev/kfd").kind, "devices");
  assert.equal(parseApproval("cap_add:SYS_NICE").value, "SYS_NICE");
  assert.equal(parseApproval("privileged").kind, "privileged");
  assert.equal(parseApproval("devices").kind, "other");
  assert.deepEqual(parseApproval("something new"), { key: "other:something new", kind: "other", label: "something new" });
});

test("the approvals list has every item once, host-run and untrusted first", () => {
  const items = approvalItems({
    fit: fit({ ok: true, method_type: "pip", approvals: ["host_run", "host_mounts:/data"] }),
    needsApproval: ["host_mounts:/data", "privileged", "device:/dev/dri"],
    trusted: false,
  });
  assert.deepEqual(items.map((i) => i.key), ["untrusted", "host_run", "host_mounts:/data", "privileged", "devices:/dev/dri"]);
  assert.match(items[1].warning!, /not sandboxed/);
  // A trusted docker package with nothing to approve needs no ticks.
  assert.deepEqual(approvalItems({ fit: fit({ ok: true, method_type: "docker" }), needsApproval: [], trusted: true }), []);
  // Host-run from the method type even when the router's list leaves it out.
  assert.deepEqual(approvalItems({ fit: fit({ ok: true, method_type: "binary" }), trusted: true }).map((i) => i.kind), ["host_run"]);
});

test("ticked items become the install's approvals; all must be ticked", () => {
  const items = approvalItems({ fit: fit({ approvals: ["host_mounts:/a", "host_mounts:/b", "cap_add:NET_ADMIN", "host_network"] }), trusted: false, methodType: "docker" });
  const ticked = new Set(["untrusted", "host_mounts:/a", "cap_add:NET_ADMIN", "host_network"]);
  assert.equal(allTicked(items, ticked), false);
  assert.deepEqual(approvalsFrom(items, ticked), { host_run: false, privileged: false, host_network: true, host_mounts: ["/a"], devices: [], cap_add: ["NET_ADMIN"], untrusted: true });
  ticked.add("host_mounts:/b");
  assert.equal(allTicked(items, ticked), true);
  assert.deepEqual(approvalsFrom(items, ticked).host_mounts, ["/a", "/b"]);
});

test("install params send only what changed, typed", () => {
  const params = { ctx: { type: "int" as const, default: 8192 }, voice: { type: "string" as const, default: "af_bella" }, fast: { type: "bool" as const } };
  assert.deepEqual(changedParams(params, { ctx: "8192", voice: "af_bella" }), {});
  assert.deepEqual(changedParams(params, { ctx: "16384", voice: "bf_emma", fast: "true" }), { ctx: 16384, voice: "bf_emma", fast: true });
});

test("installed packages group by server; engines without a package stay listed", () => {
  const row = (server: string, id: string, engines: string[]): InstalledPkg => ({ server, id, version: "1", method: 0, method_type: "docker", pins: [], at: 0, store: "default", engines });
  const rows = [row("pc", "kokoro", ["kokoro"]), row("dt", "comfyui", ["comfyui", "comfyui-2"]), row("dt", "sa3", [])];
  assert.deepEqual(installedByServer(rows).map(([s, r]) => [s, r.map((x) => x.id)]), [["dt", ["comfyui", "sa3"]], ["pc", ["kokoro"]]]);
  assert.deepEqual(manualEngines([{ id: "kokoro" }, { id: "llamacpp" }, { id: "comfyui-2" }, { id: "vllm" }], rows).map((e) => e.id), ["llamacpp", "vllm"]);
});

const kokoro: Manifest = {
  schema: 1, id: "kokoro", name: "Kokoro TTS", version: "0.9.4", kinds: ["tts"], caps: { inherits: "kokoro" },
  hardware: { gpu: ["cuda"], cpu_ok: true, min_vram_gib: 1.5, min_ram_gib: 2 },
  params: { voice: { type: "string", default: "af_bella", description: "The default voice." } },
  api: "openai-speech", probe: "http", endpoints: { health: "/health" },
  models: { layout: "hf", env: "HF_HOME", mount: "/models", read_only: false },
  methods: [
    { type: "docker", label: "GPU", platforms: ["linux/amd64"], gpu: ["cuda"], image: "ghcr.io/x/k@sha256:" + "a".repeat(64), ports: [{ id: "http", container: 8880 }], health: { http: "/health", start_seconds: 300 } },
    { type: "pip", label: "pip (host)", platforms: ["linux/amd64", "windows/amd64"], gpu: ["cuda", "cpu"], python: "3.12", requirements: ["kokoro==0.9.4", "soundfile==0.13.1"], files: ["server.py"], run: ["{{python}}", "{{install}}/server.py", "--port", "{{port}}"], ports: [{ id: "http" }], health: { http: "/health" } },
  ],
  security: { host_mounts: [] },
  licence: "Apache-2.0", source: "https://github.com/hexgrad/kokoro", notes: ["One.", "Two."],
};

test("a manifest survives the form unchanged (fields the form does not show are kept)", () => {
  assert.deepEqual(formToManifest(manifestToForm(kokoro)), kokoro);
  const llama = JSON.parse(manifestJson({
    schema: 1, id: "llamacpp-bin", name: "llama.cpp", version: "b1", kinds: ["llm"], api: "openai-chat", also_speaks: ["openai-embeddings"],
    methods: [
      { type: "binary", label: "CPU", platforms: ["linux/amd64"], gpu: ["cpu"], assets: [{ platform: "linux/amd64", url: "https://x/y.tar.gz", sha256: "b".repeat(64), strip: 1, size: 9 }], run: ["{{install}}/llama-server{{exe}}"] },
      { type: "existing", label: "PATH", platforms: ["*"], detect: { command: "llama-server", paths: ["~/a"], version_args: ["--version"], version_regex: "version: (\\d+)" }, run: ["{{bin}}"] },
      { type: "git", repo: "https://github.com/a/b", commit: "c".repeat(40), build: [{ run: ["make"] }] },
    ],
  })) as Manifest;
  assert.deepEqual(formToManifest(manifestToForm(llama)), llama);
});

test("form edits become the manifest; a changed method type drops the old type's fields", () => {
  const f = manifestToForm(kokoro);
  f.kinds = ["tts", "llm"];
  f.minVram = "";
  f.params.push({ id: "speed", type: "float", def: "1.0", description: "", per_model: true, raw: {} });
  f.methods[0] = { ...f.methods[0], type: "compose", compose: "services: {}", env: "A=1\nB=x=y", ports: "http:8880, ctl:9000/tcp", mounts: "/srv/m:/models:ro\nC:\\data:/data" };
  f.private = true;
  const m = formToManifest(f);
  assert.deepEqual(m.kinds, ["llm", "tts"]);
  assert.equal(m.hardware?.min_vram_gib, undefined);
  assert.deepEqual(m.params?.speed, { type: "float", default: 1, per_model: true });
  const c = m.methods[0];
  assert.equal(c.type, "compose");
  assert.equal(c.image, undefined);
  assert.equal(c.compose, "services: {}");
  assert.deepEqual(c.env, { A: "1", B: "x=y" });
  assert.deepEqual(c.ports, [{ id: "http", container: 8880 }, { id: "ctl", container: 9000, protocol: "tcp" }]);
  assert.deepEqual(c.mounts, [{ source: "/srv/m", target: "/models", read_only: true }, { source: "C:\\data", target: "/data" }]);
  assert.equal(m.private, true);
  assert.deepEqual(Object.keys(m).slice(0, 4), ["schema", "id", "name", "version"]);
});

test("a package from scratch: problems first, then a minimal manifest", () => {
  const f = emptyForm();
  assert.equal(formProblems(f).length, 6);
  f.id = "my-engine";
  f.name = "Mine";
  f.version = "1.0";
  f.kinds = ["image"];
  f.api = "http";
  f.methods = [{ ...emptyMethod("docker"), image: "ghcr.io/me/x:1" }];
  assert.deepEqual(formProblems(f), []);
  assert.deepEqual(formToManifest(f), {
    schema: 1, id: "my-engine", name: "Mine", version: "1.0", kinds: ["image"], api: "http",
    methods: [{ type: "docker", platforms: ["linux/amd64"], gpu: ["cuda"], ports: [{ id: "http" }], health: { http: "/health" }, image: "ghcr.io/me/x:1" }],
  });
  f.params = [{ id: "Bad", type: "int", def: "", description: "", per_model: false, raw: {} }, { id: "Bad", type: "int", def: "", description: "", per_model: false, raw: {} }];
  assert.equal(formProblems(f).length, 3);
});

test("test-run guesses fill the API and the docker health path", () => {
  const f = applyGuesses(manifestToForm(kokoro), "openai-chat", "/v1/models");
  assert.equal(f.api, "openai-chat");
  assert.equal(f.methods[0].healthHttp, "/v1/models");
  assert.equal(f.methods[1].healthHttp, "/health");
  assert.equal(applyGuesses(f, null, null).api, "openai-chat");
});

test("small parsers", () => {
  assert.deepEqual(parsePorts("http"), [{ id: "http" }]);
  assert.deepEqual(parseMounts("/a:/b"), [{ source: "/a", target: "/b" }]);
  assert.equal(parseManifest("{").error !== undefined, true);
  assert.equal(parseManifest("[]").error, "The manifest must be a JSON object.");
  assert.equal(parseManifest('{"id":"x"}').manifest?.id, "x");
});
