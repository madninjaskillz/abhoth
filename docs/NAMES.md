# Names

Abhoth is named after a being in Lovecraftian lore, and three of its parts now have names from the same lore:

- **Sothoth** (the router): from Yog-Sothoth, "the Gate and the Key". It is the gateway every request passes through.
- **Nodens** (the supervisor): the guardian Elder God. It watches fans, thermals, clocks and GPU health.
- **Migo** (the bridge): the Mi-Go ferry things between worlds. The bridge brings other machines in.

The old plain names still work everywhere a person or a script types them. The new name wins when both are set.

## Aliases

| What | New name | Old name (still works) |
|---|---|---|
| CLI | `abhoth sothoth ...` | `abhoth router ...` |
| | `abhoth nodens run\|check --config F` | `abhoth supervisor ...` |
| | `abhoth migo ...` | `abhoth bridge ...` |
| Single-process flags | `--with-sothoth "<flags>"`, `--with-nodens <config>` | `--with-router`, `--with-supervisor` |
| Standalone binaries | `abhoth-sothoth`, `abhoth-nodens`, `abhoth-migo` | `abhoth-router`, `abhoth-supervisor`, `abhoth-bridge` (the same programs) |
| Single image (`/app`) | `abhoth-sothoth`, `abhoth-nodens` symlinks | `abhoth-router`, `abhoth-supervisor` symlinks |
| Images | `local/abhoth-sothoth:1` (deploy/router), `local/abhoth-migo:1` (deploy/bridge) | tagged `local/abhoth-router:1`, `local/abhoth-bridge:1` too |
| Compose services | `sothoth` (deploy/router/compose.yaml), `abhoth-migo` (deploy/bridge/compose.yaml) | `router`, `abhoth-bridge` (`docker compose up -d --remove-orphans` replaces the old container) |
| Config files | `sothoth.toml`, `sothoth.json`, `nodens.toml`, `migo.toml` | `router.toml`, `router.json`, `supervisor.toml`, `bridge.toml` |
| Migo variables | `ABHOTH_MIGO_*` (ID, TITLE, ROUTER, TOKEN_FILE, STATE_DIR, MODEL_DIR, DOCKER) | `ABHOTH_BRIDGE_*` |
| Sothoth variables | `ABHOTH_SOTHOTH_URL`, `ABHOTH_SOTHOTH_TOKEN_FILE`, `ABHOTH_SOTHOTH_NAME` | `ABHOTH_ROUTER_URL`, `ABHOTH_ROUTER_TOKEN_FILE`, `ABHOTH_ROUTER_NAME` |
| Nodens variable | `ABHOTH_NODENS_URL` | `ABHOTH_SUPERVISOR_URL` |
| Image entrypoint | `SOTHOTH_ARGS`, `NODENS_CONFIG`, `SOTHOTH_HEALTH_URL` | `ROUTER_ARGS`, `SUPERVISOR_CONFIG`, `ROUTER_HEALTH_URL` |
| Bridge endpoint | `/migo/ws` | `/bridge/ws` |

Config files: when the file named after `--config` / `--control` (or a Nodens or Migo config) is missing, the same folder's file under the other name is read, in either direction. Nothing is renamed, so live setups keep their file names. `abhoth sothoth init` writes `sothoth.toml` and `sothoth.json`, unless the folder already has `router.*` files; then it keeps the old names, so one folder never mixes them.

Bridge endpoint: routers accept both paths. Bridges still dial `/bridge/ws` by default, so a new bridge reaches an old router. A `router` URL that ends in `/migo/ws` is used as it is.

The router's default display name is now "Sothoth on <Host>" (plain "Sothoth" in a container). `--name` and `ABHOTH_SOTHOTH_NAME` still set it.

## Unchanged on purpose

These stay as they are, so old and new routers and bridges keep working together:

- `/admin/` and every API path.
- Wire messages, descriptors and the protocol version.
- State files: `router.db`, `bridges.json`, `bridge.token`, `~/.abhoth/machine/router.toml` and the rest of the desktop app's state.
- The `abhoth-bridge-` prefix of engine containers. A restarted Migo still finds the containers it started.
- mDNS `_abhoth-router._tcp` and `/.well-known/abhoth-router`.
- The parts' keys in `/api/parts` (`router`, `supervisor`) and their restart log lines.
- The Rust crates and modules: `orca-router`, `orca-bridge`, `orca-supervisor`, `crates/orca-router/src/bridges/...`.

Later, optional: rename the crates and modules once the media-routing work has landed.
