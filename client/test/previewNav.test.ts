// The HTML preview's history and address bar (previewNav.ts has no imports, so plain node runs it).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  canBack,
  canForward,
  displayPath,
  emptyNav,
  isHtmlPath,
  navReport,
  navStep,
  previewMessage,
  rebase,
  resolveAddress,
} from "../src/features/editor/previewNav.ts";

const BASE = "/preview/tok.sig/";
const at = (p: string) => `http://localhost:8192${BASE}${p}`;

test("only .html and .htm files preview", () => {
  assert.ok(isHtmlPath("site/index.html"));
  assert.ok(isHtmlPath("A.HTM"));
  assert.ok(!isHtmlPath("notes.md"));
  assert.ok(!isHtmlPath("page.html.txt"));
});

test("the frame's reports build a history; back and forward walk it", () => {
  let nav = navReport(emptyNav, at("index.html"));
  nav = navReport(nav, at("index.html")); // a reload: no new entry
  assert.deepEqual(nav.entries, [at("index.html")]);
  assert.ok(!canBack(nav));
  nav = navReport(nav, at("about.html"));
  nav = navReport(nav, at("about.html#team"));
  assert.equal(nav.index, 2);
  assert.ok(canBack(nav) && !canForward(nav));

  const back = navStep(nav, -1)!;
  assert.equal(back.href, at("about.html"));
  nav = navReport(back.nav, back.href); // arrived
  assert.equal(nav.index, 1);
  assert.equal(nav.entries.length, 3);
  assert.ok(canForward(nav));
  assert.equal(navStep(nav, 1)!.href, at("about.html#team"));

  // A new page from the middle drops what was ahead.
  nav = navReport(nav, at("contact.html"));
  assert.deepEqual(nav.entries, [at("index.html"), at("about.html"), at("contact.html")]);
  assert.ok(!canForward(nav));
  assert.equal(navStep(nav, 1), null);
});

test("a new token keeps the history, under the new base", () => {
  const nav = navReport(navReport(emptyNav, at("index.html")), at("b.html"));
  const moved = rebase(nav, BASE, "/preview/new.sig/");
  assert.deepEqual(moved.entries, ["http://localhost:8192/preview/new.sig/index.html", "http://localhost:8192/preview/new.sig/b.html"]);
  assert.equal(moved.index, 1);
});

test("the address bar shows project paths, and takes them", () => {
  assert.equal(displayPath(at("index.html"), BASE, "site"), "site/index.html");
  assert.equal(displayPath(at("sub/my%20page.html?q=1#x"), BASE, "site"), "site/sub/my page.html?q=1#x");
  assert.equal(displayPath(at("index.html"), BASE, ""), "index.html");
  assert.equal(displayPath("https://example.com/x", BASE, "site"), "https://example.com/x");

  assert.equal(resolveAddress("site/sub/my page.html#x", BASE, "site"), `${BASE}sub/my%20page.html#x`);
  assert.equal(resolveAddress("/site/b.html", BASE, "site"), `${BASE}b.html`);
  assert.equal(resolveAddress("b.html?x=1", BASE, ""), `${BASE}b.html?x=1`);
  assert.equal(resolveAddress("https://example.com", BASE, "site"), "https://example.com");
  // Outside the previewed folder, upwards, or hidden: refused.
  assert.equal(resolveAddress("other/b.html", BASE, "site"), null);
  assert.equal(resolveAddress("site/../secret.txt", BASE, "site"), null);
  assert.equal(resolveAddress(".env", BASE, ""), null);
  assert.equal(resolveAddress("   ", BASE, ""), null);
});

test("only the preview script's messages count", () => {
  assert.deepEqual(previewMessage({ type: "openorca-preview", href: at("a.html"), title: "A" }), { href: at("a.html"), title: "A" });
  assert.equal(previewMessage({ type: "other", href: "x" }), null);
  assert.equal(previewMessage({ type: "openorca-preview", href: 42 }), null);
  assert.equal(previewMessage("openorca-preview"), null);
  assert.equal(previewMessage(null), null);
});
