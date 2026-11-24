// The add-project picker's "New folder" name check (folderName.ts has no imports, so plain node runs it).
import { test } from "node:test";
import assert from "node:assert/strict";
import { folderNameError } from "../src/features/projects/folderName.ts";

test("plain names are fine", () => {
  for (const n of ["my-project", "Project 2", ".config", "a.b", "CONSOLE", "com10", "über"]) assert.equal(folderNameError(n), null, n);
});

test("empty, dot and path names are refused", () => {
  for (const n of ["", "   ", ".", "..", "a/b", "a\\b", "../x", "..\\x"]) assert.ok(folderNameError(n), n);
});

test("characters Windows forbids are refused", () => {
  for (const n of ["a<b", "a>b", "a:b", 'a"b', "a|b", "a?b", "a*b", "a\u0001b", "tab\there"]) assert.ok(folderNameError(n), n);
});

test("a trailing dot or space is refused", () => {
  assert.match(folderNameError("name.")!, /end with/);
  assert.match(folderNameError("name ")!, /end with/);
});

test("Windows reserved names are refused, with or without an extension", () => {
  for (const n of ["CON", "con", "Nul", "aux.txt", "COM1", "lpt9.log", "COM¹"]) assert.match(folderNameError(n)!, /reserved/, n);
});

test("over-long names are refused", () => {
  assert.equal(folderNameError("a".repeat(255)), null);
  assert.match(folderNameError("a".repeat(256))!, /too long/);
});
