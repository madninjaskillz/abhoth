// Folder browsing (D152 follow-up): the client's path rule, breadcrumbs, new-folder names, sizes
// and what a typed path's check says (plain node, no DOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import { cleanPath, crumbs, fmtBytes, joinPath, statLine, validFolderName } from "../src/features/router/forms/inventoryModel.ts";

test("paths are normalised; relative, .. and odd characters are refused", () => {
  assert.deepEqual(cleanPath(" /srv//models/./qwen/ "), { ok: true, path: "/srv/models/qwen" });
  assert.deepEqual(cleanPath("/"), { ok: true, path: "/" });
  assert.deepEqual(cleanPath("/a b/$(x)"), { ok: true, path: "/a b/$(x)" });
  for (const bad of ["", "srv/models", "~/m", "/srv/../etc", "/a\\b", "/a\nb", "C:\\models"]) assert.equal(cleanPath(bad).ok, false, bad);
});

test("breadcrumbs and joining", () => {
  assert.deepEqual(crumbs("/"), [{ label: "/", path: "/" }]);
  assert.deepEqual(crumbs("/srv/models"), [{ label: "/", path: "/" }, { label: "srv", path: "/srv" }, { label: "models", path: "/srv/models" }]);
  assert.equal(joinPath("/srv/models/", " new "), "/srv/models/new");
  assert.equal(joinPath("/", "srv"), "/srv");
});

test("new folder names are one plain component", () => {
  assert.equal(validFolderName("qwen3"), null);
  assert.match(validFolderName("a/b") ?? "", /one folder/);
  assert.match(validFolderName("..") ?? "", /dot/);
  assert.match(validFolderName(".cache") ?? "", /dot/);
  assert.match(validFolderName("  ") ?? "", /needed/);
});

test("sizes and the typed path's check", () => {
  assert.equal(fmtBytes(512), "512 B");
  assert.equal(fmtBytes(5 * 2 ** 30), "5.0 GiB");
  assert.equal(fmtBytes(null), "");
  assert.deepEqual(statLine({ path: "/x", allowed: false, exists: false }, "folder"), { ok: false, text: "outside the folders this server allows" });
  assert.equal(statLine({ path: "/x/new", allowed: true, exists: false, parentExists: "/x", parentWritable: true }, "folder").ok, true);
  assert.equal(statLine({ path: "/x/new", allowed: true, exists: false, parentExists: "/x", parentWritable: false }, "folder").ok, false);
  assert.equal(statLine({ path: "/x/f", allowed: true, exists: true, dir: false, size: 10 }, "folder").text, "a file, not a folder");
  assert.equal(statLine({ path: "/x", allowed: true, exists: true, dir: true, writable: true, free: 2 ** 40 }, "folder").text, "folder, writable · 1.0 TiB free");
  assert.equal(statLine({ path: "/x/m.gguf", allowed: true, exists: true, dir: false, size: 2048 }, "file").text, "file, 2.0 KiB");
  assert.equal(statLine({ path: "/x", allowed: true, exists: true, dir: true }, "file").ok, false);
});
