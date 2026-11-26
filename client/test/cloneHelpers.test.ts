// The "Clone from GitHub" wizard's helpers (cloneHelpers.ts has no imports, so plain node runs it).
import { test } from "node:test";
import assert from "node:assert/strict";
import { deviceEndMessage, ownerRepo, pageCount, pageRange, pollDelayMs, repoFolderName, timeAgo } from "../src/features/projects/cloneHelpers.ts";

test("the folder name is the repository's name", () => {
  assert.equal(repoFolderName("acme/my-repo"), "my-repo");
  assert.equal(repoFolderName("acme/site.github.io"), "site.github.io");
  assert.equal(repoFolderName("acme/x.git"), "x");
  assert.equal(repoFolderName(""), "repository");
});

test("time since the last push", () => {
  const now = Date.parse("2026-11-24T12:00:00Z");
  assert.equal(timeAgo("2026-11-24T11:59:30Z", now), "just now");
  assert.equal(timeAgo("2026-11-24T11:00:00Z", now), "1 hour ago");
  assert.equal(timeAgo("2026-11-21T12:00:00Z", now), "3 days ago");
  assert.equal(timeAgo("2026-10-01T12:00:00Z", now), "1 month ago");
  assert.equal(timeAgo("2023-11-01T12:00:00Z", now), "3 years ago");
  assert.equal(timeAgo("2026-11-25T12:00:00Z", now), "just now"); // clock skew: never "in the future"
  assert.equal(timeAgo(null, now), "");
  assert.equal(timeAgo("not a date", now), "");
});

test("page ranges and counts", () => {
  assert.equal(pageRange(1, 30, 131), "1–30 of 131");
  assert.equal(pageRange(5, 30, 131), "121–131 of 131");
  assert.equal(pageRange(1, 30, 0), "No repositories");
  assert.equal(pageCount(30, 131), 5);
  assert.equal(pageCount(30, 0), 1);
  assert.equal(pageCount(30, 30), 1);
});

test("polling follows GitHub's interval, never faster than a second", () => {
  assert.equal(pollDelayMs(5), 5000);
  assert.equal(pollDelayMs(10), 10000); // after slow_down
  assert.equal(pollDelayMs(0), 1000);
  assert.equal(pollDelayMs(undefined), 5000);
  assert.equal(pollDelayMs(Number.NaN), 5000);
});

test("messages when the device flow ends without a sign-in", () => {
  assert.match(deviceEndMessage("expired"), /expired/);
  assert.match(deviceEndMessage("denied"), /cancelled/);
  assert.equal(deviceEndMessage("error", "Device flow isn't enabled"), "Device flow isn't enabled");
  assert.equal(deviceEndMessage("pending"), "");
});

test("a repository typed by name or address", () => {
  assert.equal(ownerRepo("acme/tool"), "acme/tool");
  assert.equal(ownerRepo(" acme/tool.git "), "acme/tool");
  assert.equal(ownerRepo("https://github.com/acme/tool"), "acme/tool");
  assert.equal(ownerRepo("https://github.com/acme/tool.git/"), "acme/tool");
  assert.equal(ownerRepo("git@github.com:acme/tool.git"), "acme/tool");
  assert.equal(ownerRepo("acme"), null);
  assert.equal(ownerRepo("../etc/passwd"), null);
  assert.equal(ownerRepo("-x/y"), null);
  assert.equal(ownerRepo("https://gitlab.com/acme/tool"), null);
  assert.equal(ownerRepo("search words"), null);
});
