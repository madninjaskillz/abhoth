# Rust port status (Abhoth)

Updated 2026-11-25. Plan and decisions: [`PORTING.md`](PORTING.md).

## Done

### Phase 1: foundation

- Workspace (8 crates), pinned dependencies, CI job (`rust` in `.github/workflows/ci.yml`).
- Config: `OrcaConfig` model, load/save round-trip byte-compatible with C# (fixtures,
  BOM, first run, ported `ConfigManager` tests).
- OpenAI-compatible client: streaming SSE and non-streaming, tool-call assembly, sampling
  presets and thinking levels (`RequestParameters`), extra body and session field, served
  model tracking, timeouts; wire format matches the C# SDK byte for byte in the cross-check.
- Tools: read_file, write_file, edit_file (fuzzy matching), list_directory, glob, grep,
  bash, get_process_output, stop_process.
- CLI: argument parsing as `CliArgumentParser`, `--print` JSON output, `--version`.

### Phase 2: agent loop parity (most of it)

- **Prompt system** (`config::prompt_manager`, `config::project_instructions`,
  `config::memory_manager`, `agent::system_prompt`, `agent::prompt_constants`): the shipped
  template and `prompts/default.md` with its version stamp and hash (byte-identical to C#),
  promptProfile / model slug / default.md resolution, legacy prompt hashes, project
  instructions (ORCA.md, AGENTS.md, CLAUDE.md), opt-in memory, native vs text tool protocol,
  model notes, peer / orchestrator / plan / sandbox / directory / thinking budget / Context7
  additions, `PLATFORM` as .NET prints it. The system prompt is compared in the cross-check
  (only the tool-derived lines are masked).
- **Turn loop** (`agent::turn`, `AgentLoopRunner` + `ToolCallDispatcher`): native and text
  tool calls (`agent::tool_call_parser`, all tag formats, JSON repair), truncated-call and
  code-block nudges, sticky native-tools downgrade with prompt rebuild, doom-loop guard,
  identical-failure warnings / STOP and the retry-loop redirect, failing-test gate, harness
  notes, dangling-call closing, task reminder, budget advisory, budget extension while the
  turn progresses (`agent::long_task`), stuck-probe steering, iteration-limit handoff with
  harness facts, same-model retry with backoff and `lmStudio.models` failover on idle
  timeout / empty replies, quality-based failover (`agent::response_quality`), verification
  and summary wind-down. `--print` reports `LastAssistantResponse` as C# does.
- **Context management** (`chat::*`, `agent::context`, `client::context_overflow`,
  `client::context_window_detector`): token estimates with server usage, pruning, squeezing,
  thinking-aware pass, planned/hard compaction with the LLM summary (Pi prompts), compact and
  retry once on a context overflow, repeat-call notes.
- **Policies and permissions** (`agent::policies`, `agent::permissions`): input size, plan
  mode, sandbox mode (`--sandbox`), orchestrator-Lead write ban; read-only tool lists in plan /
  sandbox mode; the permission prompter trait with session "yes to all" scopes.
- **Sessions, checkpoints, hooks** (`session::*`, `hooks::*`, `agent::tool_hooks`): session
  files compatible both ways with C#, `-c` / `-r` in `--print`, fork / tree / list, file
  checkpoints before modifications, pre/post tool hooks and the auto-formatter.

### Phase 2 leftovers (done 2026-11-25, second run)

- Stream guards (`agent::streaming`, client `stream_with_control`): thinking-runaway cancel with
  strikes and the low-temperature retry, per-step reasoning cap and re-ask, budget note,
  tool-less retry of a content-free stream; mid-stream salvage (`ChatError::Interrupted`);
  adaptive reasoning; the server error probe after empty replies; `MaxTokensNegotiator`; the
  startup context-window probe and `ServerIdentity` (gateway `/health` skipped).

### Phase 3: tools, slots, engines, profiles (most of it)

- Tools (C# plugin order, optional groups as Cargo features, all on by default): the whole
  FileSystem, Shell, Utility, Interactive (ask_user), Archive, Network, Git, GitHub, Web,
  ImageGeneration, Lsp and Mcp plugins (`orca-tools-*`); `ToolExposure` (advertised subset)
  and the C# wire schema shape.
- Pi profile (`orca-tools-pi`): all Pi tools, the Pi system prompt, Pi loop wording; subagent /
  progress / lead_mode run through the orchestrator.
- `orca-core::{slots, engines, profiles, live}`: slot registries, `SlotAllocator` balanced
  placement, copy models / served models, the slotted client with the Lead's Strata pin, lazy
  Lead lease; Claude Code / Codex bridges; profiles v3, agent modes; `--profile`, `--mode`,
  `--engine` in `--print`.

### Phases 3-5 (merged 2026-11-25/26)

- Orchestration (`orca-core::orchestration`, `progress`), agent tools (`orca-tools-agent`:
  spawn_agent, spawn_agents, report_progress), planner phase, pair voting, early read-only
  runs, delegated checks, background helper collection, auto lead mode, read-only spawns in
  plan mode.
- Interactive REPL (`orca-cli::repl`, crossterm): input editor, status panel, Ctrl+C cancel,
  console permission prompter, auto-save, all slash commands and `/config` (`ConfigSchema`),
  custom markdown commands, attachments and tool-result images.
- WebUi host (`orca-webui`, `--hostui`): websocket hub with resume, client transport adapter,
  git/files/projects/terminal/preview/profiles/settings endpoints, bench (queue, runner,
  results.jsonl compatible).
- Usage logging: `Slot usage` / `Provider usage` lines in the C# log file, `/stats` usage rows,
  bench token metrics.

### Tests

- Rust: 1946 tests (`cargo test --workspace --all-features --no-fail-fast`), clippy `-D warnings` and fmt
  clean.
- Cross-check: 149 scenarios (bridges 119-123, number digits 124, parity sweep 200-234), all identical between the C# and Rust builds, two runs (phase 1
  and 2 sets, stream guards 30-39, tools 40-45, git/web 50-55, Pi 60-69 strict, MCP/LSP/image
  70-76, profiles/slots/engines 80-84, agents 85-94d (slots spawn, Pi subagent strict), interactive PTY 95-107, tool images
  108-109, usage lines 110-114, tool-argument digits 119). 110/110 on 2026-11-25 (`rust-port/ghapp`). Bench API: 42 calls identical against both hosts (one-off
  script).
- Live smoke 2026-11-25 (qwen3.8-27b via LiteLLM, `--print`): task_list, edit_file, git_diff,
  git_commit on a temp repo; bug fixed and committed.

### Merged 2026-11-25 (third run)

- `rust-port/rest`: `/benchmark`, UncensoredGate and provider clients, web UI settings schema,
  browser ask_user, project profile/mode switching, generated images, TOTP sign-in, session
  `engine` fields (D81-D87).
- **Rebrand to Abhoth** (D88-D90): binary `abhoth`, `--help`, TUI/web/desktop names and mascot
  (`assets/abhoth/`), the React client is Abhoth, state in `~/.abhoth` (one-time copy of
  `~/.openorca`), `ABHOTH_*` env vars first. See [`README.md`](README.md) and
  [`CUTOVER.md`](CUTOVER.md).

### `rust-port/ghapp` (2026-11-25)

- GitHub App "Clone from GitHub" (D91): device flow with interval / slow_down, token refresh
  (5-minute margin, single flight, rotation), sign-out, repositories across installations
  (60 s cache, search, paging), branches, clone linked to the sign-in so fetch/pull/push get a
  fresh token through the credential helper. C# tests ported against a fake GitHub (API and
  `git http-backend`).
- Anthropic Messages API and Ollama clients (D92, D93): named providers of those types and
  `lmStudio.provider: "anthropic"`; prompt caching, effort, signed thinking, tool results with
  images, cache read/write usage; tested against fake local servers only.
- Tool-call arguments keep the model's number digits in history and `--print` records (D94).

### Bridge engines (`rust-port/bridge`, 2026-11-25)

- Claude Code / Codex bridges in the REPL (D100-D102): `EngineHost` (switching, startup check with
  native fallback and the login offer, `/engine`, `/clear` ends the CLI session, `--resume` of the
  CLI session from a saved bridged session, which also selects its engine at startup),
  `SessionBridgeSink` + the REPL view, progress.js bars on the info row, the engine on the status
  line. Cross-check 119-122 with the fake Claude CLI.
- Bridges under `--hostui` and in bench runs (D103): `SetEngine`, the project's preferred
  engine, the login offer, bridge turns on the bus, the Lead bar and the CLI's bars.
- Foreground sub-agents ask the user for approvals in the REPL and the browser (D104).
- `lead_mode` for the OpenOrca profile in `leadMode: auto` (D105, scenario 123).
- Coloured agent badges in chat lines and the `/agents` picker (D106).
- The REPL agent footer: Lead bar, a bridge's progress.js bars, agent bars (D107); scenario 235
  and the `--hostui` bridge integration test (D108).

### Phase 7: desktop app (2026-11-25, D110)

- `abhoth --desktop` / `abhoth-desktop.exe` (feature `desktop`): in-process host on loopback with
  a per-launch token, WebView2 window titled Abhoth with the mascot icon, single instance (a
  second launch focuses the first), tray Show/Quit, native folder dialog in the folder picker,
  external links in the system browser. Built, launched and screenshotted on Windows 11.
- Close-to-tray (D113, user decision): closing hides the window to the tray by default and
  agents keep running; `desktop.closeToTray: false` (settings, Desktop section) quits on close.
  Checked on Windows 11: close hides, a second launch shows it again, with the setting off close
  quits.
- CI builds and unit-tests the desktop feature on Linux, macOS and Windows (job `rust-desktop`,
  D114); on Linux the web view goes into tao's GTK box (Wayland and X11).
- Windows installer (D115): a per-user MSI (WiX v5, `installer/build.ps1`), no admin: both
  binaries, a Start-menu shortcut, optional user PATH, uninstall. Workflow
  `abhoth-installer.yml` on `abhoth-v*` tags or by hand. Built locally (abhoth-0.15.0-x64.msi,
  25 MB); a silent per-user install and uninstall on Windows 11 put in and took out the files,
  shortcut (with icon), PATH entry and HKCU keys.

### Phase 6: parity sweep, coverage (2026-11-25, D120-D128)

- 34 new cross-check scenarios (200-234): permission allow/deny patterns, disabled tools, a
  malformed deny rule, `--allow`, `alwaysApprove`, `--allow-dir`; wildcard pre-hooks and
  post-hooks with `ORCA_TOOL_*`; memory and ORCA.md in the prompt; `--demo`; backup model on a
  refusal / short answer; `promptProfile` and model-slug prompts; `--mode orchestrator` / `peer`;
  the advertised tool set compared strictly (core, `extendedTools`, `extraTools`);
  `modelSampling`; thinking budget; `verificationMode` always / off with `repeatCallNotes` and
  `earlyReadOnlyTools` off; file tool and bash error paths; the planner phase on a named provider;
  `--print -r` with autoSave; REPL `/checkpoint`, `/diff`, `/undo`, `/memory`, auto memory at exit,
  `/export`, `/init`, `/model`, `/retry` (no backup), `/context`, `/stats`, `-r` with engine fields.
- Rust fixes found by them: spawn tool order, raw-argument records, write_file error text, the
  REPL showing the wrap-up and the summary both, `/model` not reaching the request, `/init` line
  endings, planner tool calls missing from `--print`, provider usage log category, `/retry` model.

## Next

1. Bench parity run (user-scheduled; the gate for the cutover, CUTOVER.md step 0).
2. Fresh install: the desktop app must start without a configured model and show why; an icon
   in the `.exe` files.
3. Phase 7 follow-ups: code signing for the MSI; a GitHub Release with the MSI (CUTOVER.md).
4. Cutover (user-triggered): the abhoth repo split (`tools/split-abhoth-repo.sh`, tested), the
   web container, the local install (CUTOVER.md).

### Live smoke 2026-11-25 (third run)

- `abhoth --output json -p` with `leadMode: auto` and auto-approve, against LiteLLM
  `qwen3.8-27b`: spawn an explore sub-agent to read a file, fix a bug, and run the test. Before
  D111, 0 of 4 runs succeeded: the model's first reply was a text `{"name", "args"}` call, and
  the turn ended. The C# build failed 1 of 2 runs the same way. After D111, 3 of 3 runs
  succeeded: a sub-agent answered, the edit was made and the test passed. In one run a sub-agent
  hit a LiteLLM 500, and the Lead retried it.
- The React client was checked against the Rust `--hostui` (token, sign-in and main pages) and
  the `--desktop` window. Screenshots are in the scratchpad (`shots/abhoth-*.png`).

## Bench parity run (to be scheduled with the user; not run by agents)

What it must show: on the same model, slots and tasks, `abhoth` is no slower per task and no
worse at passing than the C# `openorca` it replaces (CUTOVER.md gate 3).

- **Binaries:** `dotnet publish src/OpenOrca.Cli -c Release -o bench/.bin/csharp` and
  `cargo build --release -p orca-cli` copied to `bench/.bin/rust/abhoth.exe`, both built from
  the same `feature/rust-port` commit.
- **Homes:** one template, `bench/.homes/parity` (the user's usual config: slots, leadMode,
  prompt). Both variants use the same template. The runner sets `OPENORCA_HOME` per run, and
  Abhoth reads it as the fallback for `ABHOTH_HOME`.
- **Runs:** interleave variants so slot load and time of day hit both alike. Use
  `python bench/run.py run --variant csharp --orca-exe bench/.bin/csharp/openorca.exe --home
  bench/.homes/parity --tier easy,hard,multi --reps 3 --resume --wait-registry <registry>`,
  then the same with `--variant rust --orca-exe bench/.bin/rust/abhoth.exe`. Do it in two halves
  (C# then Rust, then Rust then C#). That is about 1 h per variant at the "Bench sizing" scale.
  Add `--tier t3 --tasks t3-long-horizon,t3-multi-file --reps 3` for compaction and long-context
  parity.
- **Web host and bench page:** one queue item per host
  (`--hostui-bench`, same kit, `realtasks` suite, 3 reps). This checks the in-process Rust bench
  runner against the C# worker-process runner (D74). It also checks results.jsonl compatibility
  on real runs.
- **Compare:** `python bench/run.py report --variant csharp,rust --by-mode`.
  - Pass rate per tier: equal or better.
  - Median wall-clock per task: Rust no more than 5% slower.
  - Median requests and total tokens per task: within about 10%.
  - Loop events and failovers: no new kinds.
  - Rerun only gaps that the noise does not explain ("Bench sizing").
- **What it can't show:** the bridges (Claude Code / Codex) and the desktop window. Those are
  covered by the cross-check and the smoke tests.

## Known gaps

- REPL footer: no bar selection (↓ / Ctrl+G) or agent view (D107).
- Anthropic: a turn with several signed thinking blocks keeps no signature (its reasoning is not
  sent back, D92).
- Bench runs: kit timeouts kill only the process.
- Desktop: the Linux and macOS builds are CI-only (never run with a window); no first-hide
  tray notification (D113); the MSI is unsigned, so SmartScreen warns.
- Desktop with no model configured (fresh install): the host refuses to start ("no model
  configured") and `abhoth-desktop.exe`, having no console, shows nothing.
- grep: files in sorted order; regex error texts differ from .NET (lookaround/backreferences work, D112).
- C# REPL bugs kept out of the cross-check (D127): custom markdown commands ("Unknown command" in
  C#), `/memory <unknown>` (crashes C#), `/retry` on a second model (C# drops the prompt).
- Not cross-checked (need a live service or are rendering-only): `/docs` (Context7), `/doctor`,
  `/help` layout, `/copy` (clipboard), the `/config` editor, `/workflow` and `/review` runs,
  tool-output spill files (temp path and time stamp in the result).
- Console output decoding is lossy UTF-8 rather than the OEM code page; hook stdin is UTF-8.
- list_directory ordering approximates .NET culture-aware sorting.
- Error texts from OS / transport / JSON parser failures (web, network, MCP, slots) differ from
  .NET's wording in places; profile JSON property names are case-sensitive.
- Not ported by design: `job screenshot` (needs `unsafe` Win32 capture, D37). C# bugs not
  reproduced: LSP PascalCase wire fields (D42), MCP image base64 (D43).
- Archive sizes/timestamps, network ping/DNS output, env ordering differ (unit-tested only).
- Live smoke (LiteLLM, qwen3.8-27b, read a file and answer) passed 2026-11-25 for phase 1.
