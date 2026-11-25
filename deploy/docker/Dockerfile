# Abhoth web UI in a container (Linux). Build from the abhoth repo root (rust/ in the openorca
# repo, with the React client at client/; see tools/split-abhoth-repo.sh for that layout):
#   docker build -f deploy/docker/Dockerfile -t local/abhoth-web:<tag> .
# The web client is the checked-in client/dist, embedded in the binary (no Node in the build).
# The runtime keeps the .NET SDK (the agent builds and tests .NET projects) and Node (LTS) for
# the Claude Code and Codex CLIs, which the bridge engines and the web terminal run.
ARG RUST_VERSION=1.100.0
ARG NODE_VERSION=26.13.1
ARG CLAUDE_CODE_VERSION=2.1.321
ARG CODEX_VERSION=0.167.0
FROM node:${NODE_VERSION}-bookworm-slim AS node

# Headless release build of `abhoth` (crate orca-cli, default features, no `desktop`).
FROM rust:${RUST_VERSION}-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV ABHOTH_CLIENT_DIST=/src/client/dist \
    CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    test -f client/dist/index.html \
 && cargo build --release --locked -p orca-cli --bin abhoth \
 && install -D -m 755 target/release/abhoth /out/abhoth

# Runtime: the SDK image, so the agent can build and test .NET projects (plus the .NET 8 runtimes,
# for net8.0 test projects); and the tools a coding agent expects. Runs as uid 568 (TrueNAS "apps");
# the only writable paths are /projects, /state and the /tmp tmpfs (the compose file makes the root
# filesystem read-only).
FROM mcr.microsoft.com/dotnet/sdk:10.0
ARG CLAUDE_CODE_VERSION
ARG CODEX_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash git openssh-client python3 python3-venv procps util-linux less ripgrep tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 568 apps && useradd -u 568 -g 568 -d /state/home -M -s /bin/bash orca
COPY --from=mcr.microsoft.com/dotnet/aspnet:8.0 /usr/share/dotnet/shared /usr/share/dotnet/shared
# Node LTS + the two CLIs, pinned (global installs live in /usr/local, read-only at run time;
# their logins and settings live in the persistent HOME: /state/home/.claude, /state/home/.codex).
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && npm install -g --no-fund --no-audit "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" "@openai/codex@${CODEX_VERSION}" \
 && npm cache clean --force \
 && node --version && claude --version && codex --version
COPY --from=build /out/abhoth /app/abhoth
COPY deploy/docker/entrypoint.sh /app/entrypoint.sh
COPY deploy/docker/home /app/home-seed
# benchkit.py next to the binary: abhoth looks for <exe dir>/bench/benchkit.py.
COPY deploy/bench/benchkit.py /app/bench/benchkit.py
# /app/openorca stays (a symlink), so `docker exec … /app/openorca --hostui-set-password` still works.
RUN sed -i 's/\r$//' /app/entrypoint.sh && chmod 755 /app/entrypoint.sh /app/abhoth \
 && ln -s abhoth /app/openorca \
 && /app/abhoth --version
ENV HOME=/state/home \
    OPENORCA_HOME=/state/openorca \
    OPENORCA_PROJECTS_ROOT=/projects \
    DOTNET_CLI_HOME=/state/home \
    NUGET_PACKAGES=/state/nuget \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DISABLE_AUTOUPDATER=1 \
    SHELL=/bin/bash \
    PORT=8093
USER 568:568
WORKDIR /projects
ENTRYPOINT ["/app/entrypoint.sh"]
