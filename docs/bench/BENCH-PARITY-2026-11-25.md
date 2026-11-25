# Bench parity run, 2026-11-25 (CUTOVER.md gate 3)

C# `openorca` against Rust `abhoth`, both built from `feature/rust-port` at **22791fc**
(`dotnet publish -c Release`, `cargo build --release -p orca-cli`).

## Setup

- **Model and slots:** the live layout `merged-inkling-x2` (Inkling 2 Mini on ninfer "merged",
  4 lanes per card). Copies A and B, Lead on A, `agentsPerCopy` 3, `maxConcurrent` 7,
  `leadMode` auto, reasoning medium. The servers' key comes from an environment variable
  named in the home's config (no key in the home).
- **Home:** one template, used by both builds. It has the profile's client settings and no
  keys. There is no custom prompt, so each build uses its shipped default.
- **Order:** C1 R1, R2 C2, C3 R3 (ABBA by rep, `--resume`, `--wait-registry`). Started at
  15:17 UTC, once other GPU jobs on the server were idle. The layout was checked every 30 s.
  The only bad sample (card B out, 15:12–15:16) was before the first run.
- **Trimmed for time:**
  - easy and hard: 3 reps;
  - multi: 2 reps;
  - t3: 2 reps.
  - `t3-multi-file` does not exist. `t3-cli-feature` (multi-file feature work) stood in for it.
  - The whole bench took about 3.5 h.
- **Not run:** the `--hostui-bench` / `realtasks` web-host comparison. Its kit
  (`realtasks.py`, REALTASKS_REPO) exists only in the web container's kit, not locally.

## Results (100 runs)

| tier | pass C# | pass Rust | median wall C# / Rust (s) | geo-mean Rust/C# wall per task | same, decode-normalised | median requests C# / Rust | geo-mean Rust/C# tokens |
|---|---|---|---|---|---|---|---|
| easy | 18/18 | 18/18 | 8 / 9 | 1.02 | 0.93 | 4 / 3 | 0.88 |
| hard | 24/24 | 24/24 | 53 / 69 | 1.08 | 1.00 | 6 / 6 | 1.00 |
| multi | 4/4 | 3/4 | 452 / 243 | 0.56 | 0.66 | 17 / 13 | 0.90 |
| t3 | 3/4 | 4/4 | 549 / 602 | 0.98 | 1.00 | 21 / 31 | 1.61 |
| all | 49/50 | 49/50 | 41 / 41 | 0.98 | n/a | 6 / 5 | 1.00 |

- **How the columns are counted:**
  - Requests and tokens come from the "Slot usage" lines (prompt + output), which both builds write.
  - The stock report's `requests` column was 0 for Rust, because it counts a C#-only log line
    (fixed afterwards, see "Log lines" below).
  - "Decode-normalised" puts every run's decode time at 76 tok/s.
- **Server speed changed during the bench.** The server's decode speed per 10-minute window
  ranged from 36 to 77 tok/s, and only the bench held leases.
  - C# ran 15:30–15:50 at about 37 tok/s.
  - Rust rep 1 and rep 2 ran at 50–65 tok/s.
  - Rep 3 (C3 then R3, back to back, both at 76 tok/s, easy and hard): C# 541 s, Rust 522 s,
    both 14/14.
- **Client-side time** (wall minus time inside LLM requests): Rust 330 s in total, C# 511 s.
- **Loop events and failovers:** none in either build. Compactions: C# 5, Rust 7, the same
  kinds (Light and Full).

## Gates

- **Pass rate: equal overall (49/50 each).**
  - Rust lost one multi run: multi-sales r1, a hidden edge case (`END` trailer not on the last line).
  - C# lost one t3 run: t3-long-horizon r1. One lead turn hit `maxTokens` with 32,766
    reasoning tokens in 525 s.
  - Both are model failures, not harness failures. The per-step reasoning cap is off in both builds.
- **Wall clock: pass.**
  - Overall per-task geo-mean 0.98. The hard tier's raw 1.08 comes from server decode speed:
    it is 1.00 normalised, and Rust was faster in the back-to-back rep 3.
  - Multi and t3 are faster or level.
- **Requests and tokens: pass for easy, hard, multi and overall.**
  - t3 is +61% on 2 reps. The low C# figure is its failed run, which stopped early. One Rust
    t3-cli-feature run used 34 requests against 20–22 in the others.
  - The runs that passed are comparable. t3-long-horizon: C# 28 requests / 898k tokens;
    Rust 28 / 760k and 48 / 1.29M.
  - The first-request prompt is identical (4,587 vs 4,586 tokens).
- **Loop and failover kinds: pass.** There are no new kinds. The event scan had a limit: Rust
  did not write several C# event lines, such as "requesting verification", nudges and tool
  errors as WRN. So only the patterns both builds share were compared.

## Task-level notes

- **Rust slower on these tasks:** kvstore (×1.36), ledger-bughunt (×1.34), logsummary (×1.25),
  git-commit (×1.29). Each falls within ±5% once the decode rate is normalised.
- **Logs:** `bench/runs/<variant>/<task>/r<n>/home/logs/` in the bench worktree (not
  committed), with the analysis (`bench/parity/analysis.txt`) and the stock report
  (`bench/parity/report.txt`).

## Log lines (follow-up, D141)

The missing C# log lines were ported afterwards (PORTING.md D141): the per-request
`AgentLoopRunner: Starting streaming request to LLM...` line, the iteration header, the
"Streaming complete" and "Executing N ... tool call(s)" lines, nudges, verification and
summary requests, the tool pipeline's lines (tool errors as WRN), the loop guards and the
rest of the inventory. Re-check on the same setup (fizzbuzz, 2 runs, 2/2 passed): `run.py
report` shows 4.5 median requests for Rust (5 and 4, matching the "Slot usage" lines), not 0.
Every event kind in C#'s fizzbuzz logs now has a Rust line with the same wording; the only
remaining differences were categories of lines Rust already wrote, which were aligned too, and
C#'s plugin-loading lines, which Rust has no equivalent for.
