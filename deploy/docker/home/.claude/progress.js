#!/usr/bin/env node
// Progress bars for the Claude Code footer (rendered by statusline.js).
//
//   node ~/.claude/progress.js set <key> <pct> "<label, max 4 words>"
//   node ~/.claude/progress.js done <key>
//   node ~/.claude/progress.js clear            (remove all bars for this session)
//
// Each key gets its own colour, picked on first `set` from those not in use
// by other active bars. Each colour has a fixed LETTER (blue A, green B, …),
// drawn as the first cell of the bar. `set` prints "🟦A blue": the tag is the
// colour square AND the letter, so it shows in colour where emoji render (chat,
// footer) and still reads "A" where they're dropped (the agent list). The main
// thread uses it for the agent's name, its tool-call descriptions and chat.
//
// State lives in ~/.claude/progress/<session id>/<key>.json. The session id
// is the first of:
//   --session <id>            explicit override
//   PROGRESS_SESSION          any tool or script that wants its own bucket
//   CLAUDE_CODE_SESSION_ID    Claude Code (main thread and subagents)
//   CODEX_THREAD_ID           OpenAI Codex CLI (set for its shell commands)
//   "default"                 anything else
// Claude Code's footer shows only its own session; progress-watch.js (a
// terminal pane, e.g. beside Codex) shows every session's live bars.

const fs = require("fs");
const os = require("os");
const path = require("path");

const args = process.argv.slice(2);
const env = process.env;
let session, source;
if (env.PROGRESS_SESSION) [session, source] = [env.PROGRESS_SESSION, "env"];
else if (env.CLAUDE_CODE_SESSION_ID) [session, source] = [env.CLAUDE_CODE_SESSION_ID, "claude"];
else if (env.CODEX_THREAD_ID) [session, source] = [env.CODEX_THREAD_ID, "codex"];
else [session, source] = ["default", "default"];
const si = args.indexOf("--session");
if (si !== -1) {
  session = args[si + 1];
  source = "explicit";
  args.splice(si, 2);
}
if (!session) {
  console.error("progress.js: --session needs an id");
  process.exit(1);
}
session = String(session).replace(/[^A-Za-z0-9_.-]/g, "_").replace(/^\.+/, "_").slice(0, 80);

// Primary store, read by the Claude Code footer. Sandboxed tools that cannot
// write under ~/.claude (Codex workspace-write allows only the workspace and
// %TEMP%) fall back to %TEMP%/claude-progress; progress-watch.js reads both.
const ROOTS = [path.join(os.homedir(), ".claude", "progress"), path.join(os.tmpdir(), "claude-progress")];
const safe = (k) => String(k).replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 60);
const [cmd, key, pct, ...labelWords] = args;

// Order matters: first unused colour wins. Names must match statusline.js.
const COLOURS = ["blue", "green", "yellow", "purple", "orange", "red", "brown", "white"];
const LETTER = { blue: "A", green: "B", yellow: "C", purple: "D", orange: "E", red: "F", brown: "G", white: "H" };
const EMOJI = { blue: "🟦", green: "🟩", yellow: "🟨", purple: "🟪", orange: "🟧", red: "🟥", brown: "🟫", white: "⬜" };

function set(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, safe(key) + ".json");
  let started = Date.now();
  let colour = null;
  try {
    const prev = JSON.parse(fs.readFileSync(file, "utf8"));
    started = prev.started || started;
    colour = prev.colour || null;
  } catch {}
  if (!colour) {
    const used = new Set();
    for (const f of fs.readdirSync(dir)) {
      try { used.add(JSON.parse(fs.readFileSync(path.join(dir, f), "utf8")).colour); } catch {}
    }
    colour = COLOURS.find((c) => !used.has(c)) || COLOURS[used.size % COLOURS.length];
  }
  const label = labelWords.join(" ").split(/\s+/).filter(Boolean).slice(0, 4).join(" ") || key;
  const value = Math.max(0, Math.min(100, Number(pct) || 0));
  fs.writeFileSync(file, JSON.stringify({ label, pct: value, colour, started, updated: Date.now(), source }));
  return colour;
}

if (cmd === "set" && key && pct != null) {
  let colour;
  try {
    colour = set(path.join(ROOTS[0], session));
  } catch (e) {
    if (e.code !== "EPERM" && e.code !== "EACCES" && e.code !== "EROFS") throw e;
    colour = set(path.join(ROOTS[1], session));
  }
  process.stdout.write(EMOJI[colour] + LETTER[colour] + " " + colour + "\n");
} else if (cmd === "done" && key) {
  for (const r of ROOTS) try { fs.unlinkSync(path.join(r, session, safe(key) + ".json")); } catch {}
} else if (cmd === "clear") {
  for (const r of ROOTS) try { fs.rmSync(path.join(r, session), { recursive: true, force: true }); } catch {}
} else {
  console.error('usage: progress.js set <key> <pct> "<label>" | done <key> | clear');
  process.exit(2);
}
