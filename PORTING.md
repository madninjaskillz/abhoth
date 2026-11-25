# Porting OpenOrca to Rust

The C# code (`src/`, `tests/`) stays the reference implementation and keeps working. The
Rust port lives in `rust/`, is a separate Cargo workspace, and is invisible to .NET: the
solution file lists its projects explicitly, and nothing under `rust/` is a .NET project.
Progress and gaps are tracked in [`STATUS.md`](STATUS.md).

## Building and testing

```sh
cd rust
cargo build                         # debug build; target/debug/openorca(.exe)
cargo test --workspace              # unit, ported C# tests, end-to-end --print tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml` (rustup picks it up). CI runs the same four
commands in the `rust` job of `.github/workflows/ci.yml`, on Linux and Windows.

Cross-check against the C# build (needs a Release build of the C# CLI):

```sh
dotnet build ../OpenOrca.sln -c Release
cargo build -p orca-cli -p orca-crosscheck
target/debug/orca-crosscheck --csharp ../src/OpenOrca.Cli/bin/Release/net10.0/openorca.exe \
    --rust target/debug/openorca.exe [--only <scenario>] [--out <dir>] [--strict]
```

## Crate map

| C# project / area                        | Rust crate            | State     |
|------------------------------------------|-----------------------|-----------|
| OpenOrca.Abstractions (`IOrcaTool`, `ToolResult`, capabilities, risk) | `orca-abstractions` | done |
| OpenOrca.Tools.Common (truncation, paths, text helpers) | `orca-tools-common` | done for current tools |
| OpenOrca.Core/Configuration (`OrcaConfig`, `ConfigManager`) | `orca-core::config` | done, round-trip compatible |
| OpenOrca.Core/Client (OpenAI client, extra body, served models, request parameters, thinking) | `orca-core::client` | done for LM Studio / OpenAI-compatible |
| OpenOrca.Cli/Repl agent loop (`AgentLoopRunner`, `ToolCallDispatcher`, `ToolCallExecutor`, `ToolExecutionPipeline`, `RunEvidence`) | `orca-core::agent` | loop parity done (see STATUS gaps) |
| OpenOrca.Core/Permissions | `orca-core::agent::permissions` | done; prompter trait, CLI installs none yet |
| plugins/tools FileSystem (read, write, edit, list, glob, grep) | `orca-tools-fs` | done |
| plugins/tools Shell (bash, get_process_output, stop_process) | `orca-tools-shell` | done |
| plugins/tools (git, web, image, LSP, MCP proxy, archive, multi_edit, apply_patch, agents, ...) | `orca-tools-*` | phase 3 |
| plugins/commands (slash commands) | `orca-cli::commands` | phase 4 |
| OpenOrca.Tools.Pi (Pi profile toolset) | `orca-tools-pi` | phase 3 |
| OpenOrca.Core Session, Chat (compaction), Hooks | `orca-core::{session,chat,hooks}` | done |
| OpenOrca.Core Orchestration, Slots, Engines, Profiles, Mcp, Live, Attachments | `orca-core::*` | phase 3 |
| OpenOrca.Cli (Program, REPL, rendering, TUI, custom commands, `--print`) | `orca-cli` | `--print` done; REPL/TUI phase 4 |
| OpenOrca.WebUi (host, SignalR hub, bench, git, files, PTY, projects) | `orca-webui` | phase 5 |
| tests/OpenOrca.TestKit (fake model server) | `orca-testkit` | done |
| (new) C#/Rust cross-check | `orca-crosscheck` | done |

## Library mapping

| .NET                                | Rust                                  | Notes / risk |
|-------------------------------------|---------------------------------------|--------------|
| System.Text.Json + JsonDefaults     | serde_json (`preserve_order`) + `orca_core::config::dotnet_json` | .NET escaping (JavaScriptEncoder: `<>&'+` and non-ASCII escaped in records; relaxed encoder for history echo) and indented CRLF output are reproduced by hand. Low risk; covered by byte-compat fixtures. |
| Microsoft.Extensions.AI + OpenAI SDK | reqwest + own SSE parser             | Wire format (key order, float form, message shapes) is checked by the cross-check. Medium risk: the SDK has undocumented quirks (see below). |
| Spectre.Console                     | crossterm + ratatui (phase 4)         | Rendering will not be byte-identical; TUI harness tests compare semantics. |
| ASP.NET Core + SignalR              | axum + tokio-tungstenite websockets   | See "SignalR". |
| Microsoft.Extensions.FileSystemGlobbing | globset (case-insensitive, literal separator) | Done. |
| .NET Regex                          | regex                                 | No lookaround/backreferences: grep with such patterns fails with a clear error; internal patterns are rewritten by hand (`run_evidence` splitter). |
| Process + ConPTY                    | tokio::process; portable-pty for PTY (phase 5) | Kill trees via `taskkill /T /F` / process groups. |
| xUnit                               | cargo test (+ tokio::test)            | Tests are ported with each part. |
| MCP client (ModelContextProtocol)   | rmcp or a small JSON-RPC client (phase 3) | Decide when phase 3 starts; prefer the official crate if mature. |
| Plugin assemblies loaded at runtime | compile-time registration             | See "Plugins". |

Dependencies are pinned exactly in the workspace `Cargo.toml` and `Cargo.lock` is
committed. New crates must be well-known; upgrades are deliberate.

## SignalR to websockets

The React client (`src/OpenOrca.WebUi/client`) stays. It talks to the hub through
`@microsoft/signalr`. Plan: the Rust host speaks plain JSON messages over one websocket
(`/hub/ws`), with the same method names and payloads as the hub's methods and client
callbacks (`{"type": "invoke", "id", "method", "args"}` / `{"type": "result", ...}` /
`{"type": "event", "method", "args"}`). The client gets a thin adapter with the
`HubConnection` surface it uses (`invoke`, `on`, `off`, `start`, `stop`, reconnect events)
so components don't change. Reconnect with resume (the current hub's reconnect handling)
is kept by the server keeping per-connection state for a grace period. Risk: medium
(streams and reconnect semantics); mitigated by porting the WebUi hub tests first.

## Plugins

C# loads `plugins/tools` and `plugins/commands` assemblies at runtime. Rust has no stable
ABI, so tools and commands are compiled in: each tool crate exposes `all_tools(...)` and
the CLI registers them (`orca_cli::build_registry`). Optional groups are Cargo features
(e.g. `image`, `lsp`) so a slimmer binary is possible. External extension stays available
through MCP servers and custom commands (markdown), which C# also supports. Third-party
DLL plugins are not supported by the port (decision D3).

## Phase plan

Sizes are rough (S < 1 day, M 1-3 days, L 3-7 days, XL > 1 week of agent time).

1. **Foundation** (done): workspace, config, OpenAI client, agent loop core, file and
   shell tools, `--print`, e2e tests, cross-check harness, CI job. M-L.
2. **Agent loop parity**: full prompt system (`prompts/default.md`, project instructions,
   model notes), text tool calling (parser, nudges), retries and error recovery, doom-loop
   and retry-loop detection, context management and compaction, sessions (save, resume,
   `-c`/`-r`), interactive permission prompter, hooks, checkpoints, plan mode and sandbox
   policies, test-failure gate, alias repair. L-XL.
3. **Tools**: the rest of plugins/tools (git, web, image, process, archive, multi_edit,
   apply_patch, LSP, MCP proxy, ask_user, task list, think), sub-agents and orchestration
   (spawn_agent(s), slots, engines, profiles), Tools.Pi. XL.
4. **CLI and TUI**: REPL, slash commands, rendering, TUI, custom commands, `--demo`. L-XL.
5. **WebUi host**: axum host, websocket hub + client adapter, bench, git, files, PTY,
   projects, profiles, settings, serving the built React client. XL.
6. **Parity sweep**: run the whole cross-check set and the ported test suites, bench runs
   side by side, then a decision on replacing the C# build (not part of this port). M.

## Parity strategy

1. **Ported tests**: each part brings its C# tests (same names in snake_case, same inputs
   and expected values). A test that cannot be ported says why in a comment.
2. **Byte-compat fixtures**: config files written by C# load and save back byte-identical.
3. **Cross-check harness** (`orca-crosscheck`): scripted sessions (`crosscheck/scenarios`)
   run through both binaries against the same fake server; the `--print` JSON, every
   request body and the resulting files must match after normalising temp paths and
   durations. Known gaps (system prompt text, advertised tool list) are masked unless
   `--strict`. New behaviour gets a scenario before or with its port.
4. **Live smoke** (optional): a real OpenAI-compatible server, only with a key already in
   the environment.

## C# behaviours to preserve (found while porting)

- **Wire format** (OpenAI .NET SDK): body key order is `temperature, top_p, messages,
  max_completion_tokens, presence_penalty, stream, stream_options, tools, tool_choice`, then
  the extra-body fields (top_k, additional properties, session field), then `model`. Whole
  floats are written without a fraction (`0`, not `0.0`). `max_completion_tokens`, not
  `max_tokens`.
- **History echo**: assistant tool-call arguments are re-serialized indented with platform
  line breaks (relaxed encoder: only non-BMP characters escaped as surrogate pairs);
  unparseable arguments are echoed as `null`. A tool-call-only assistant turn has content
  `""`. A streamed reply whose first content delta was empty is sent back as a two-part
  content array (`[{"type":"text","text":""}, {"type":"text","text":...}]`).
- **Tool records** (`--print` `tool_calls`): arguments indented with JavaScriptEncoder
  escaping (`&` as `&`), except the first of several calls in one response, which is
  recorded compact. Prelude failures (unknown tool, denied, missing arguments, read-before-
  edit) are not recorded; denied/blocked results have no `ERROR: ` prefix, executed-tool
  errors do.
- **Read-before-edit**: write/edit of an existing file not read this session (or changed on
  disk since) is refused with the exact C# message.
- **Empty-args downgrade**: a native tool call with empty arguments for a tool with required
  parameters drops that response and switches to text tool calling (no `tools` sent).
- **Wind-down**: after tools ran, a verification step is requested when files changed and
  no passing build/test run came after (`agent.verificationMode`), a retry when the
  verification reply used no tools, and a summary request only when the final message is
  not a substantive wrap-up (40+ chars, 8+ words); summary continuations on `length`.
- **Prompt constants** are raw string literals: they carry the source checkout's line
  breaks (CRLF on Windows), so the port sends them with platform line breaks.
- Text output uses `Environment.NewLine` wherever C# uses `AppendLine`; lengths and limits
  count UTF-16 units; `--print` omits null `tool_calls` / `files_modified`.
- `nativeToolCalling` defaults to false in config; cmd.exe fed through stdin prints its
  banner and exits 0 (same in both).

## Decisions log

- **D1** Separate Cargo workspace under `rust/`, edition 2024, toolchain pinned. Crates
  follow the C# project split so tests and parity checks map one to one.
- **D2** Exact version pins and a committed lockfile; only well-known crates.
- **D3** Tools and commands are compiled in (Cargo features for optional groups) instead of
  runtime plugin loading; MCP and markdown custom commands remain the extension points.
- **D4** SignalR is replaced by a JSON-over-websocket protocol with a client-side adapter
  that keeps the `HubConnection` surface; the React client stays.
- **D5** Wire and output quirks of the C# build are reproduced, not fixed, while the C#
  build is the reference: parity first, improvements later behind explicit decisions.
- **D6** Grep uses the `regex` crate: patterns needing lookaround/backreferences fail with
  an error instead of pulling in a backtracking engine (fancy-regex can be added later).
- **D7** The cross-check masks the system prompt text and the tools list until phase 2
  ports the prompt system and phase 3 the remaining tools.
- **D8** The run-evidence command splitter is hand-written (the C# regex uses lookaround).
- **D9** The turn loop lives in `agent::turn` (one module for `AgentLoopRunner` +
  `ToolCallDispatcher` + `ToolCallExecutor`), with the per-turn state in a `LoopState` struct;
  the session keeps the cross-turn state (failover index, sticky native-tools support,
  retry and doom histories). Retry/failover delays are session fields so tests can zero them.
- **D10** Stream-start errors other than a context overflow end the turn with an error, as in
  C#; only idle timeouts and empty replies go through retry and failover.
- **D11** Tool policies (input size, plan, sandbox, orchestrator Lead) run before execution in
  the C# order; a blocked call returns its message as the result and is neither recorded nor
  counted for retry detection.
- **D12** Permissions: `evaluate_permission` returns Allow / Deny / Ask, and a
  `PermissionPrompter` trait answers Ask (approve / deny / approve-all, the scope stored
  lowercase for the session). With no prompter, calls needing approval fail closed, matching
  C# `--print`.
- **D13** `--print` reports `LastAssistantResponse` (the last reply text), not the wrap-up
  buffer, matching C#.
- **D14** The text tool-call parser's file-path regex and single-quote JSON repair are
  hand-written (the C# patterns use lookaround).
- **D15** Cross-check scenarios can pass extra CLI args (`args`) and plant config-home files
  (`home_files`); scenario numbers are unique prefixes.
- **D16** New pinned deps: `sha2 =0.10.8` (prompt hashes), `windows-version =0.1.1`
  (PLATFORM string as .NET prints it), `async-trait =0.1.92` (the compactor trait object).
- **D17** Loop messages re-export `PromptConstants` from `agent::prompt_constants` instead of
  keeping copies.
- **D18** Context management keeps the session's `Vec<ChatMessage>`; each compaction check
  works on a `Conversation` copy and writes the result back, so a cancelled compaction loses
  nothing. Usage reported for a history changed in place since the request is discarded.
- **D19** Session files keep the C# JSON shape (read and written both ways); `--print` does not
  auto-save, as in C#.
- **D20** The client's stop hook is a per-chunk `UpdateEnd` event with a Stop return
  (`stream_with_control`); `agent::streaming` ports `StreamingResponseProcessor` (runaway cancel,
  per-step reasoning cap, `ThinkTagFilter`, tool-less retry of a content-free stream).
- **D21** A stream that dies mid-response returns `ChatError::Interrupted` carrying the partial
  reply; tool calls from an unfinished stream are dropped, as in C#.
- **D22** "Empty reply" means no content items at all (usage included), matching C#; the
  tool-less retry ignores the retry's usage, as C# does.
- **D23** The fake server records non-chat requests (compared by the cross-check) and can stream
  reasoning in chunks, open with a null-content role chunk, and die mid-stream. Runaway scenarios
  use the null role chunk (C# counts the usual empty first chunk as text).
- **D24** `ServerIdentity` keeps one cache entry per server with a single in-flight probe; the
  startup probes (`AgentSession.startup_probes`) are off for library use and on in the CLI.
- **D30** Git and GitHub tools share `orca-tools-git`; `github` has a `program` field (default
  `gh`) so tests can point it at a missing program.
- **D31** Child command lines are built as C# builds them: on Windows the argument string is
  passed through unchanged, elsewhere it is split with .NET's rules
  (`orca_tools_common::process`).
- **D32** HTML-to-text is a direct port of the C# regex code; the script/style stripper (a
  backreference in C#) is hand-written; `HtmlDecode` uses the 253-entity table.
- **D33** New pinned deps: `hyper =1.13.0` (already in the tree; reason phrases in HTTP
  errors) and reqwest's `charset` feature (`encoding_rs` pinned at 0.8.34).
- **D34** Connection errors are worded like .NET (OS message plus `(host:port)`).
- **D25** Utility and Interactive tools share `orca-tools-utility`; Archive and Network are
  separate crates. Optional tool groups are CLI Cargo features, all on by default (`archive`,
  `network`, `mcp`, `lsp`, `image`). The registry keeps the C# order: plugin assemblies by file
  name, tool classes by name within each.
- **D26** `cd` changes the session's working directory (not the process's): its result carries
  the new directory (`ToolResult::with_cwd_change`) and the session applies it. Tools that wait
  on the user (ask_user) get no timeout. ask_user goes through a `UserPrompter` trait; without a
  terminal it fails with the C# `--print` message.
- **D27** `TaskStore` is one shared store per process (like the C# static class); the tool holds
  an `Arc` so tests can use their own.
- **D28** Archive uses `zip =9.8.2` (deflate only). Network runs the system `ping` and the OS
  resolver (raw ICMP needs `unsafe`, which the workspace forbids).
- **D29** `agent::tool_exposure` ports `ToolExposure.FilterAdvertised` (core list, extraTools,
  extendedTools, mcp_* always, lsp_* with `lsp.enabled`, image tools with `imageGen.enabled`);
  every tool stays executable. Tool schemas are rewritten on the wire as the C# OpenAI adapter
  does (fixed key order, `additionalProperties: false`).
- **D40** MCP uses the official SDK `rmcp =3.11.0` (protocol 2025-11-25); on Windows server
  commands are wrapped in `cmd.exe /c` as in C#.
- **D41** Image tools use `image =0.25.10` (PNG, JPEG, WebP, GIF, BMP only); format detected
  from content; resize uses Catmull-Rom.
- **D42** The C# LSP client sends PascalCase field names (`TextDocument`), so against a real
  server hover/definition/diagnostics never work (a C# bug, left unchanged). Rust speaks
  correct LSP; the cross-check compares only the LSP error paths.
- **D43** MCP image content is base64-decoded in Rust (C# keeps the base64 text as bytes, a C#
  bug).
- **D44** Cross-check extensions: fake MCP/LSP server placeholders, binary input files,
  pixel-hash image comparison, recorded images-API requests. The recorded-arguments rule for
  the first of several calls applies only to calls C# starts early (read-only tools).
- **D35** `Tool::prepare_arguments` lets a tool validate its own arguments (Pi's validation
  texts); `affected_file_paths` takes the `ToolContext`, so tools can return absolute paths.
- **D36** Pi's subagent, progress and lead_mode tools take their host behaviour from optional
  hooks; without a host they answer C#'s "not available in this session" errors. The CLI wires
  progress to the Lead's "Progress recorded: N%." text.
- **D37** `job screenshot` is not ported (Win32 window capture needs `unsafe`); it validates its
  arguments and returns an error. The pure helpers are ported.
- **D38** `orca-tools-pi` depends only on `orca-abstractions`; the Pi profile registers its
  tools over same-named ones and `tool_exposure` uses `FilterPiProfile` for the advertised set
  (Pi tools, agent tools with `pi.agentTools`, mcp_*, extraTools). Cross-check scenarios can set
  `"strict": true` to compare tools and the whole system prompt (all Pi scenarios do).
- **D39** New pinned dep: `unicode-normalization =0.1.23` (Pi edit fuzzy matching).
- **D45** A `ChatBackend` trait plus an optional router on `OpenAiClient`: the slot runtime
  (`slots::SlotRuntime`, lazy Lead lease, Strata prefix pin, idle release) sits behind the
  existing client, so `AgentSession` is unchanged.
- **D46** A tokio task-local replaces C#'s AsyncLocal ambient client
  (`SlotRuntime::with_ambient`), so an agent's compaction call stays on its own copy.
- **D47** A monotonic clock replaces `DateTimeOffset` in the allocator and idle release; tests
  inject a clock.
- **D48** Engines (Claude Code / Codex bridges) use a small `CancelToken` and `EngineError`
  (Unavailable, Failed, Cancelled) instead of adding `tokio-util`.
- **D49** Cross-check config placeholders `<SERVER>`, `<SERVER_ROOT>`, `<HOME_JSON>`,
  `<WORK_JSON>` (no fixed ports or paths); profile/mode/engine errors on stderr are compared.
- **D65** `orca-webui` (axum) hosts `--hostui`; chat sessions sit behind a `LiveSession` trait
  and sub-agent features behind an `AgentHost` trait (wired after the orchestration merge).
- **D66** Hub protocol: JSON over one websocket (`/hub/ws`) with the C# method names, numbered
  messages, acks and pings; reconnect resumes (`?resume=&last=`) and replays missed events
  within a 30 s grace period.
- **D67** The React client picks its transport from `/api/info` (`"hub":"ws"` selects the
  websocket adapter `hubConnection.ts`), so one client build works with both hosts.
- **D68** The built client is embedded with rust-embed (an optional folder can be served
  instead); SPA fallback and token auth (header, cookie, query token redirect) as in C#.
- **D69** The file-change watcher polls.
- **D70** The settings page takes its schema from the caller (the config schema comes with the
  REPL port).
- **D71** Extra projects run as in-process sessions (C# uses worker processes).
- **D72** The native session runs on its own thread and sends approvals to the browser; only
  headless mode is supported (`--tui`, `--hostui-login` are refused).
- **D57** The REPL uses crossterm only (no ratatui): the C# UI is a scrolling transcript with a
  bottom status panel, not a full-screen app.
- **D58** Slash commands are compiled in behind an async `Command` trait with a
  `CommandContext` and C#'s sentinels (`__clear__`, `__ask_toggle__`, `__plan_toggle__`).
- **D59** `/review` and `/workflow` go through an `AgentHost` trait (wired with
  orchestration); `/engine` switches only to native in interactive mode.
- **D60** Custom markdown commands send the rendered template and run a turn for both `/name`
  and `/customcommand name` (C# drops the template for `/name` and never runs a turn for
  `/customcommand`): a deliberate fix of a C# bug, so no cross-check scenario.
- **D61** Cancel drops the turn future when the console Ctrl+C signal or the raw-mode key
  watcher fires; open tool calls are closed afterwards. The unfinished markdown block streams
  as plain text and is replaced by the rendered block when complete.
- **D62** Images live in `ChatMessage.parts`; the image rewrite is in `OpenAiClient`, keyed on
  `lmStudio.vision`, with a vision probe and a resend without images. The Windows clipboard is
  read through PowerShell (no `unsafe`).
- **D63** `orca-crosscheck` (test tooling only) uses `unsafe_code = "deny"` with one audited
  `SetConsoleCtrlHandler(NULL, FALSE)` call so Ctrl+C reaches child processes in PTY
  scenarios. Shipped crates keep `forbid`. Interactive scenarios (95-107) run in a PTY
  (`portable-pty` + `vt100`) and compare screens, requests and session files.
- **D64** `/config` saves from a fresh load plus the profile overlay (profile values are not
  written into config.json); `/profile switch` saves `profiles.active` and restarts with
  `--profile` and `--resume`.
- **D73** Bench runs are kept by id in one state object; changes go out on the C#
  `/api/bench/events` stream (the React client is unchanged); cancelling a run drops its task.
- **D74** Bench times: UK summer time computed by hand, .NET timestamp text and rounding.
- **D75** `results.jsonl` rows keep the C# keys and order; Rust writes `+`/non-ASCII unescaped
  and whole floats as `200.0` (both builds read either form).
- **D76** Each bench run is an in-process Rust session configured from its own `config.json`
  (C# starts a worker process per run).
