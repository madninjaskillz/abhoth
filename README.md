# Abhoth

<img src="assets/abhoth/icons/abhoth-256.png" width="128" align="right" alt="Abhoth mascot">

**Abhoth** is the Rust successor of OpenOrca: the same autonomous coding agent for local models
(OpenAI-compatible servers, Strata/ninfer slots, LiteLLM), rewritten in Rust as one `abhoth`
binary. The C# build (`openorca`) stays the parity reference until it is retired; see
[`CUTOVER.md`](CUTOVER.md).

```sh
cd rust
cargo build --release                # target/release/abhoth(.exe)
abhoth                               # interactive session in the current folder
abhoth -p "fix the failing test"     # one prompt, then exit
abhoth --hostui                      # the web UI (prints a URL with a token)
abhoth --desktop                     # the web UI in its own window (feature "desktop")
abhoth --help                        # the common options
```

## Name, state and compatibility

- **Binary and product name:** `abhoth` / "Abhoth" (`orca_core::brand`). The TUI header,
  `--version`, `--help`, the web UI (title, favicon, mascot, sign-in and token pages) and the
  desktop window all say Abhoth. The system prompt introduces the agent as Abhoth; prompt files
  on disk keep their shared text.
- **State:** `~/.abhoth` (config.json, sessions, prompts, projects, web UI accounts). On the first
  run without `ABHOTH_HOME`/`OPENORCA_HOME`, Abhoth copies `~/.openorca` there once (logs,
  checkpoints and `config.json.bak*` stay behind) and leaves the old folder untouched, so the C#
  build keeps working until it is retired. The file formats are unchanged.
- **Environment:** `ABHOTH_*` variables are read first, `OPENORCA_*` as the fallback
  (`ABHOTH_HOME`, `ABHOTH_PROJECTS_ROOT`, ...). Wire names the web client and bridges use
  (`X-OpenOrca-Token`, the `openorca` MCP server name of the bridges, log categories
  `OpenOrca.Usage`) keep their names for compatibility.
- **Mascot:** `assets/abhoth/abhoth.svg` (vector original) and `abhoth-reference.webp`; the
  PNGs, the multi-size `abhoth.ico` and the web client's `brand/` files are rendered from the SVG
  (resvg, then Pillow for the .ico and .webp files).

Porting notes, decisions and the test setup: [`PORTING.md`](PORTING.md); progress and gaps:
[`STATUS.md`](STATUS.md).
