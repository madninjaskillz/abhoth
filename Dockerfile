# Abhoth in one image (D148): the web UI, Sothoth (the router: gateway + control plane) and
# Nodens (the supervisor: telemetry hub, Home Assistant, Glance feed) in one container and one
# process (docs/NAMES.md for the names; the old ones all still work).
# Build from the repo root:
#   docker build -t abhoth .
#   docker build -t abhoth --build-arg FEATURES=claude-engine .   # the router's Claude engine
# Run: docker-compose.yml beside this file, and README.md "Docker". Only the web UI is on by
# default; SOTHOTH_ARGS and NODENS_CONFIG (or the old ROUTER_ARGS and SUPERVISOR_CONFIG) turn
# the other two on (deploy/docker/entrypoint.sh).
# The separate images (deploy/docker for the web UI, deploy/router for Sothoth, deploy/bridge
# for Migo) still build.
#
# Mounts (one volume at /data, or one per folder; the image declares no VOLUME, so nothing
# anonymous is created next to bind mounts):
#   /data/web         web UI state: HOME (Claude/Codex logins), abhoth.db and sessions, bench/
#   /data/router      Sothoth's state: sothoth.json / sothoth.toml (or router.json / router.toml),
#                     router.db, requests.jsonl, ssh/
#   /data/supervisor  nodens.toml (or supervisor.toml) and the files it names
#   /data/projects    the projects the agent works in (OPENORCA_PROJECTS_ROOT)
#   /run/secrets      read-only secret files (entrypoint.sh lists the ones it reads)
ARG RUST_VERSION=1.100.0
ARG NODE_VERSION=26.13.1
ARG CLAUDE_CODE_VERSION=2.1.321
ARG CODEX_VERSION=0.167.0
FROM node:${NODE_VERSION}-bookworm-slim AS node

# Headless release build of `abhoth` (crate orca-cli: default features, router and supervisor
# included; no `desktop`). The web client is the checked-in client/dist (no Node in the build).
FROM rust:${RUST_VERSION}-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV ABHOTH_CLIENT_DIST=/src/client/dist \
    CARGO_TERM_COLOR=never
# FEATURES=claude-engine builds the router's Claude engine in (ROUTER.md 12).
ARG FEATURES=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    test -f client/dist/index.html \
 && cargo build --release --locked -p orca-cli --bin abhoth ${FEATURES:+--features $FEATURES} \
 && install -D -m 755 target/release/abhoth /out/abhoth

# Runtime: the .NET SDK image (the agent builds and tests .NET projects; plus the .NET 8
# runtimes), Node LTS with the Claude Code and Codex CLIs (the bridge engines, the web terminal
# and the router's Claude engine), openssh-client (the router's control plane reaches GPU hosts
# over SSH) and tini as PID 1 (reaps orphaned tool processes, forwards SIGTERM).
FROM mcr.microsoft.com/dotnet/sdk:10.0
ARG CLAUDE_CODE_VERSION
ARG CODEX_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash git openssh-client python3 python3-venv procps util-linux less ripgrep tzdata curl tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 568 apps && useradd -u 568 -g 568 -d /data/web/home -M -s /bin/bash orca
COPY --from=mcr.microsoft.com/dotnet/aspnet:8.0 /usr/share/dotnet/shared /usr/share/dotnet/shared
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && npm install -g --no-fund --no-audit "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" "@openai/codex@${CODEX_VERSION}" \
 && npm cache clean --force \
 && node --version && claude --version && codex --version
COPY --from=build /out/abhoth /app/abhoth
COPY deploy/docker/entrypoint.sh /app/entrypoint.sh
COPY deploy/abhoth/healthcheck.sh /app/healthcheck.sh
COPY deploy/docker/home /app/home-seed
COPY deploy/bench/benchkit.py /app/bench/benchkit.py
RUN sed -i 's/\r$//' /app/entrypoint.sh /app/healthcheck.sh \
 && chmod 755 /app/entrypoint.sh /app/healthcheck.sh /app/abhoth \
 && ln -s abhoth /app/openorca && ln -s abhoth /app/abhoth-router && ln -s abhoth /app/abhoth-supervisor \
 && ln -s abhoth /app/abhoth-sothoth && ln -s abhoth /app/abhoth-nodens \
 && mkdir -p /data/web /data/router /data/supervisor /data/projects && chown 568:568 /data /data/web /data/router /data/supervisor /data/projects \
 && /app/abhoth --version
ENV STATE_DIR=/data/web \
    HOME=/data/web/home \
    ABHOTH_HOME=/data/web/abhoth \
    OPENORCA_PROJECTS_ROOT=/data/projects \
    OPENORCA_WORKDIR=/data/projects/workspace \
    DOTNET_CLI_HOME=/data/web/home \
    NUGET_PACKAGES=/data/web/nuget \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DISABLE_AUTOUPDATER=1 \
    SHELL=/bin/bash \
    PORT=8093
# The web UI; Sothoth's (the router's) usual ports (gateway, compat lb/slots/profiles); the Nodens (supervisor) feed
# listens where its config says (127.0.0.1:9890 in the example: inside the container only).
EXPOSE 8093 8010 8092 8098 8099
USER 568:568
WORKDIR /data
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 CMD ["/app/healthcheck.sh"]
ENTRYPOINT ["/usr/bin/tini", "--", "/app/entrypoint.sh"]
