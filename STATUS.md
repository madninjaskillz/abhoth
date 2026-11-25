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

- Rust: 1784+ tests (`cargo test --workspace --no-fail-fast`), clippy `-D warnings` and fmt
  clean.
- Cross-check: 105 scenarios, all identical between the C# and Rust builds, two runs (phase 1
  and 2 sets, stream guards 30-39, tools 40-45, git/web 50-55, Pi 60-69 strict, MCP/LSP/image
  70-76, profiles/slots/engines 80-84, agents 85-94d (slots spawn, Pi subagent strict), interactive PTY 95-107, tool images
  108-109, usage lines 110-114). Bench API: 42 calls identical against both hosts (one-off
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

## Next

1. Bridge engines under `--hostui` and in the REPL; sub-agent approval prompter; `lead_mode` for
   `leadMode: auto`.
2. Phase 6: parity sweep, live smoke on agents and the REPL; bench parity run (user-scheduled).
3. Phase 7: `abhoth --desktop`.

## Known gaps

- Sub-agent tool calls that need approval are refused, also in foreground agents (C# asks the
  user): the sub-agent pipeline has no prompter.
- `lead_mode` is not registered for the OpenOrca profile with `leadMode: auto`; agent badges are
  not coloured in note lines; `/agents` is a plain list, not the interactive picker.
- WebUi: no settings schema passed yet (settings page empty), profile/mode switching answers
  409, ask_user questions not forwarded, generated images only via the raw-file endpoint.
- Bench runs in the Rust host are native-engine only; kit timeouts kill only the process.
- Sessions: check `engine` / `engineSessionId` round-trip in the parity sweep (dropped on save before the engines port).
- grep: Rust regex syntax (no lookaround/backreferences); files in sorted order.
- Console output decoding is lossy UTF-8 rather than the OEM code page; hook stdin is UTF-8.
- list_directory ordering approximates .NET culture-aware sorting.
- Error texts from OS / transport / JSON parser failures (web, network, MCP, slots) differ from
  .NET's wording in places; profile JSON property names are case-sensitive.
- Not ported by design: `job screenshot` (needs `unsafe` Win32 capture, D37). C# bugs not
  reproduced: LSP PascalCase wire fields (D42), MCP image base64 (D43).
- Archive sizes/timestamps, network ping/DNS output, env ordering differ (unit-tested only).
- Live smoke (LiteLLM, qwen3.8-27b, read a file and answer) passed 2026-11-25 for phase 1.
