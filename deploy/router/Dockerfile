# abhoth-sothoth: Sothoth, the router (LiteLLM + qfn-lb replacement), as a small image. Build
# from the repo root (the old abhoth-router tag is the same image; docs/NAMES.md):
#   docker build -f deploy/router/Dockerfile -t local/abhoth-sothoth:1 -t local/abhoth-router:1 .
# One static-ish binary (rustls, no OpenSSL) on debian slim; runs as uid 568 (TrueNAS "apps").
# openssh-client: the control plane reaches the GPU host over SSH (hostctl, ROUTER-CONFIG.md 6).
ARG RUST_VERSION=1.100.0
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
ENV CARGO_TERM_COLOR=never
# FEATURES=claude-engine builds the Claude engine in (ROUTER.md 12).
ARG FEATURES=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orca-router --bin abhoth-sothoth ${FEATURES:+--features $FEATURES} \
 && install -D -m 755 target/release/abhoth-sothoth /out/abhoth-sothoth

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tzdata openssh-client \
 && rm -rf /var/lib/apt/lists/*
# CLAUDE_CLI=@anthropic-ai/claude-code@<version> adds the Claude Code CLI (Node) for the Claude
# engine; pin the version. Its login is a `claude setup-token` token mounted as a file.
ARG CLAUDE_CLI=""
RUN if [ -n "$CLAUDE_CLI" ]; then \
      apt-get update && apt-get install -y --no-install-recommends nodejs npm \
      && npm install -g --no-fund --no-audit "$CLAUDE_CLI" && npm cache clean --force \
      && rm -rf /var/lib/apt/lists/* && claude --version; \
    fi
COPY --from=build /out/abhoth-sothoth /app/abhoth-sothoth
# The old name: compose files and scripts that run /app/abhoth-router keep working.
RUN ln -s abhoth-sothoth /app/abhoth-router \
 && /app/abhoth-sothoth --help >/dev/null && /app/abhoth-router --help >/dev/null
USER 568:568
WORKDIR /state
# Default: serve /state/sothoth.json (or /state/router.json when only that is there). The shadow
# compose overrides this per service.
ENTRYPOINT ["/app/abhoth-sothoth"]
CMD ["--config", "/state/sothoth.json", "--listen", "0.0.0.0:8020"]
