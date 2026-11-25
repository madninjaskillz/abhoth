// Sign-in / setup / account page helpers (authHelpers.ts has no imports, so plain node runs it).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MIN_PASSWORD, ago, codeInput, codesText, describeAgent, eventLabel, passwordProblem, recoveryInput, totpInput, usernameProblem,
} from "../src/features/auth/authHelpers.ts";

test("passwords need the minimum length and nothing else", () => {
  assert.match(passwordProblem("")!, /Enter/);
  assert.match(passwordProblem("short")!, /At least 12/);
  assert.equal(passwordProblem("a".repeat(MIN_PASSWORD)), null);
  assert.equal(passwordProblem("all lower case words"), null);
  assert.match(passwordProblem("x".repeat(1025))!, /At most/);
});

test("password length counts characters, not UTF-16 units", () => {
  assert.match(passwordProblem("😀".repeat(11))!, /1 more/);
  assert.equal(passwordProblem("😀".repeat(12)), null);
});

test("the confirmation must match", () => {
  assert.match(passwordProblem("long enough password", "long enough passwork")!, /differ/);
  assert.equal(passwordProblem("long enough password", "long enough password"), null);
});

test("usernames", () => {
  for (const u of ["admin", "j.doe", "a_b-c", "me@example", "über"]) assert.equal(usernameProblem(u), null, u);
  assert.match(usernameProblem("  ")!, /Enter/);
  assert.match(usernameProblem("has space")!, /only/);
  assert.match(usernameProblem("a/b")!, /only/);
  assert.match(usernameProblem("x".repeat(65))!, /64/);
});

test("authenticator codes keep six digits", () => {
  assert.equal(totpInput("12 34 56"), "123456");
  assert.equal(totpInput("1234567"), "123456");
  assert.equal(totpInput("abc"), "");
  assert.deepEqual(codeInput("123 456", false), { kind: "totp", value: "123456", complete: true });
  assert.equal(codeInput("12345", false).complete, false);
});

test("recovery codes are normalised to xxxxx-xxxxx", () => {
  assert.equal(recoveryInput("ABCDE12345"), "abcde-12345");
  assert.equal(recoveryInput("abcde-12345"), "abcde-12345");
  assert.equal(recoveryInput(" ab cd e1 2345 99"), "abcde-12345");
  assert.equal(recoveryInput("abc"), "abc");
  assert.deepEqual(codeInput("ABCDE-12345", true), { kind: "recovery", value: "abcde-12345", complete: true });
  assert.equal(codeInput("abcde-1234", true).complete, false);
});

test("user agents read as browser on OS", () => {
  assert.equal(
    describeAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36"),
    "Chrome on Windows",
  );
  assert.equal(describeAgent("Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0"), "Firefox on Linux");
  assert.equal(describeAgent("Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/131 Safari/537.36 Edg/131"), "Edge on Windows");
  assert.equal(describeAgent("curl/8.4.0"), "curl");
  assert.equal(describeAgent(null), "");
});

test("relative times", () => {
  const now = Date.parse("2026-11-25T12:00:00Z");
  assert.equal(ago("2026-11-25T11:59:30Z", now), "just now");
  assert.equal(ago("2026-11-25T11:57:00Z", now), "3 minutes ago");
  assert.equal(ago("2026-11-25T11:00:00Z", now), "1 hour ago");
  assert.equal(ago("2026-11-23T12:00:00Z", now), "2 days ago");
  assert.equal(ago(Date.parse("2026-11-25T11:00:00Z") / 1000, now), "1 hour ago", "Unix seconds");
  assert.equal(ago(null, now), "");
  assert.equal(ago("nonsense", now), "");
});

test("codes text and event labels", () => {
  assert.equal(codesText(["a", "b"], "Abhoth"), "Abhoth recovery codes (each works once):\na\nb\n");
  assert.equal(eventLabel("login.fail"), "login fail");
  assert.equal(eventLabel("api_key.created"), "api key created");
});
