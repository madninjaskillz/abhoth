// The router admin forms (D151): router.toml sections <-> form fields and edits, router.json's
// model list, and the page's relative URLs (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  cardForm, cardRuns, cardValue, claudeForm, claudeValue, engineForm, engineValue, layoutForm, layoutValue, linesToMap, modelBadges, modelForm, modelValue,
  sectionEdits, serverForm, serverValue, upstreamForm, upstreamValue, usedBy, validateCard, validateClaude, validateEngine, validateLayout, validateModel,
  validateServer, validateUpstream, withDeployment, type Obj,
} from "../src/features/router/forms/formModel.ts";
import { pageUrl } from "../src/features/router/admin/session.ts";

const data: Obj = {
  version: 1,
  servers: { gpu: { title: "GPU host", host: "gpu-host", exec: "ssh", ssh: { user: "abhoth-exec", key: "secret:ssh_gpu", host_key: "SHA256:x" }, ready: [{ builtin: "docker-up" }] } },
  cards: {
    A: { server: "gpu", gpu_uuid: "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", vram_gib: 32.0, cpus: "0-23" },
    B: { server: "gpu", gpu_uuid: "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", vram_gib: 32.0 },
  },
  engines: { ninfer: { driver: "docker", port: { A: 8094, B: 8096 }, run: { docker: ["x"] }, profiles: { "4lane": { lanes: 4, vram_gib: 7.5 }, "2lane": { lanes: 2 } } } },
  models: { dense: { title: "Dense" } },
  layouts: { x2: { title: "Both", cards: { A: { engine: "ninfer", profile: "4lane", model: "dense" }, B: "same" } } },
};

test("a server form keeps what it does not show and refuses a literal key", () => {
  const f = serverForm("gpu", data.servers && (data.servers as Obj).gpu);
  assert.equal(f.sshUser, "abhoth-exec");
  const v = serverValue({ ...f, host: "gpu-2", sshPort: "2222" }, (data.servers as Obj).gpu);
  assert.equal(v.host, "gpu-2");
  assert.deepEqual(v.ssh, { user: "abhoth-exec", key: "secret:ssh_gpu", host_key: "SHA256:x", port: 2222 });
  assert.deepEqual(v.ready, [{ builtin: "docker-up" }], "hooks the form doesn't show are kept");
  assert.ok(validateServer({ ...f, sshKey: "-----BEGIN OPENSSH" }, ["gpu"], "gpu").sshKey);
  assert.ok(validateServer({ ...f, id: "gpu" }, ["gpu"], null).id, "duplicate id");
  assert.deepEqual(validateServer(f, ["gpu"], "gpu"), {});
  assert.equal(serverValue({ ...f, exec: "local" }, undefined).ssh, undefined);
});

test("cards: uuid, server, numbers; what each layout runs on them with lanes", () => {
  const f = cardForm("A", (data.cards as Obj).A);
  assert.equal(f.vramGib, "32");
  assert.deepEqual(validateCard(f, ["A", "B"], "A", ["gpu"]), {});
  assert.ok(validateCard({ ...f, gpuUuid: "nope" }, [], null, ["gpu"]).gpuUuid);
  assert.ok(validateCard({ ...f, server: "other" }, [], null, ["gpu"]).server);
  assert.ok(validateCard({ ...f, cpus: "a-b" }, [], null, ["gpu"]).cpus);
  assert.equal(validateCard({ ...f, managed: false, gpuUuid: "" }, [], null, ["gpu"]).gpuUuid, undefined, "unmanaged cards need no uuid");
  const v = cardValue({ ...f, vramGib: "24", onDemand: true, managed: false }, (data.cards as Obj).A);
  assert.equal(v.vram_gib, 24);
  assert.equal(v.on_demand, true);
  assert.equal(v.managed, false);
  const runs = cardRuns(data, "B");
  assert.deepEqual(runs, [{ layout: "x2", title: "Both", engine: "ninfer", profile: "4lane", model: "dense", lanes: 4 }], "same resolves to the card before");
  assert.deepEqual(usedBy(data, "servers", "gpu"), ["card A", "card B"]);
  assert.deepEqual(usedBy(data, "engines", "ninfer"), ["layout x2"]);
});

test("engines: per-card ports, profiles (lanes) checked and kept", () => {
  const f = engineForm("ninfer", (data.engines as Obj).ninfer);
  assert.equal(f.port, "A = 8094\nB = 8096");
  assert.equal(f.profiles.length, 2);
  assert.deepEqual(validateEngine(f, ["ninfer"], "ninfer"), {});
  const bad = { ...f, port: "A = x", profiles: [...f.profiles, { id: "4lane", title: "", lanes: "0", ctx: "", vramGib: "", build: "" }] };
  const e = validateEngine(bad, [], null);
  assert.ok(e.port && e["profiles.2.id"] && e["profiles.2.lanes"]);
  const v = engineValue({ ...f, profiles: [{ ...f.profiles[0], lanes: "8" }] }, (data.engines as Obj).ninfer);
  assert.deepEqual(v.profiles, { "4lane": { lanes: 8, vram_gib: 7.5 } });
  assert.deepEqual(v.run, { docker: ["x"] }, "the start command is kept");
  assert.deepEqual(engineValue({ ...f, port: "8080" }, undefined).port, 8080);
});

test("layouts: off / same / runs, checked against engines, profiles and models", () => {
  const f = layoutForm("x2", (data.layouts as Obj).x2, ["A", "B", "C"]);
  assert.equal(f.cards.B.kind, "same");
  assert.equal(f.cards.C.kind, "unset");
  assert.deepEqual(validateLayout(f, ["x2"], "x2", data), {});
  const bad = { ...f, cards: { ...f.cards, A: { kind: "spec" as const, engine: "ninfer", profile: "9lane", model: "dense" } } };
  assert.match(validateLayout(bad, [], null, data)["cards.A"], /profiles/);
  const sameFirst = { ...f, cards: { A: { kind: "same" as const, engine: "", profile: "", model: "" } } };
  assert.ok(validateLayout(sameFirst, [], null, data)["cards.A"]);
  assert.deepEqual(layoutValue({ ...f, cards: { ...f.cards, C: { kind: "off", engine: "", profile: "", model: "" } } }, undefined).cards, {
    A: { engine: "ninfer", profile: "4lane", model: "dense" },
    B: "same",
    C: "off",
  });
});

test("a rename is a removal and a set", () => {
  assert.deepEqual(sectionEdits("cards", "C", "B", { server: "gpu" }), [
    { path: ["cards", "B"], value: null },
    { path: ["cards", "C"], value: { server: "gpu" } },
  ]);
  assert.deepEqual(sectionEdits("cards", "B", "B", {}), [{ path: ["cards", "B"], value: {} }]);
});

const gw: Obj = {
  upstreams: { A: { url: "http://gpu-host:8094", engine: "ninfer", lanes: 4, keyFile: "/run/secrets/k" }, B: { url: "http://gpu-host:8096", lanes: 4 } },
  deployments: [
    { name: "dense", cards: ["A", "B"], servedId: "dense-27b", modelInfo: { max_input_tokens: 131072, supports_vision: true, engine: "ninfer" } },
    { name: "flash", cards: ["B"], servedId: "moe", params: { reasoning_effort: "low" }, modelInfo: { effort_levels: ["low", "high"] } },
  ],
  claude: { enabled: false, access: "admins", routes: { "claude-opus": "opus" }, accessNote: "kept" },
};

test("the model list: capabilities into model_info, other keys kept, checked", () => {
  const f = modelForm((gw.deployments as Obj[])[0]);
  assert.equal(f.contextTokens, "131072");
  assert.ok(f.vision && !f.tools);
  assert.deepEqual(validateModel(f, ["A", "B"], false), {});
  assert.ok(validateModel({ ...f, cards: ["Z"] }, ["A", "B"], false).cards);
  assert.ok(validateModel({ ...f, cards: [] }, ["A", "B"], false).cards);
  assert.ok(validateModel({ ...f, params: "{nope" }, ["A"], false).params);
  assert.ok(validateModel({ ...f, thinkingOffEffort: "max" }, ["A"], false).thinkingOffEffort);
  const v = modelValue({ ...f, tools: true, effortLevels: "low, medium", thinkingOffEffort: "low" }, (gw.deployments as Obj[])[0]);
  assert.deepEqual(v.modelInfo, { max_input_tokens: 131072, supports_vision: true, engine: "ninfer", supports_function_calling: true, effort_levels: ["low", "medium"], thinking_off_effort: "low" });
  assert.deepEqual(modelBadges(v), ["128k ctx", "vision", "tools", "reasoning"]);
  const f2 = modelForm((gw.deployments as Obj[])[1]);
  assert.equal(JSON.parse(f2.params).reasoning_effort, "low");
  const text = withDeployment(gw, -1, modelValue({ ...modelForm(undefined), name: "new", servedId: "n", cards: ["A"] }, undefined));
  const parsed = JSON.parse(text);
  assert.equal(parsed.deployments.length, 3);
  assert.ok(text.endsWith("\n"));
  assert.equal(JSON.parse(withDeployment(gw, 0, null)).deployments[0].name, "flash");
  assert.equal((gw.deployments as Obj[]).length, 2, "the loaded document is not changed");
});

test("upstreams and the Claude block", () => {
  const u = upstreamForm("A", (gw.upstreams as Obj).A);
  assert.deepEqual(validateUpstream(u, ["A", "B"], "A"), {});
  assert.ok(validateUpstream({ ...u, url: "gpu-host:8094" }, [], null).url);
  assert.ok(validateUpstream({ ...u, keyFile: "sk-live" }, [], null).keyFile);
  assert.ok(validateUpstream({ ...u, card: "B" }, ["A", "B"], "A").card);
  assert.deepEqual(upstreamValue({ ...u, url: "http://x:1/", online: false }, (gw.upstreams as Obj).A), { url: "http://x:1", engine: "ninfer", lanes: 4, keyFile: "/run/secrets/k", online: false });
  const c = claudeForm(gw.claude);
  assert.deepEqual(linesToMap(c.routes).map, { "claude-opus": "opus" });
  assert.deepEqual(validateClaude(c), {});
  assert.ok(validateClaude({ ...c, access: "everyone" }).access);
  const cv = claudeValue({ ...c, enabled: true, access: "keys:bench" }, gw.claude);
  assert.equal(cv.enabled, true);
  assert.equal(cv.access, "keys:bench");
  assert.equal(cv.accessNote, "kept");
});

test("the page's URLs are relative to it (any proxy prefix)", () => {
  assert.equal(pageUrl("session", "https://h/admin/"), "/admin/session");
  assert.equal(pageUrl("api", "https://h/router/admin/#keys"), "/router/admin/api");
  assert.equal(pageUrl("login", "http://h/api/settings/connection/router-admin/ui/"), "/api/settings/connection/router-admin/ui/login");
});
