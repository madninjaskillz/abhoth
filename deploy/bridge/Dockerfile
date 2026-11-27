# abhoth-bridge (ROUTER-MEDIA-PLAN.md 11): the headless bridge on its own. Build from the repo root:
#   docker build -f deploy/bridge/Dockerfile -t local/abhoth-bridge:1 .
# It dials the router (no inbound port), describes this host's GPUs and CPU, and starts and
# stops engine containers through the host's docker socket. No router, no web UI, and never the
# Claude engine or a Claude token. GPU hosts: run it with the nvidia runtime (compose.yaml), so
# nvidia-smi is there for detection.
ARG RUST_VERSION=1.100.0
# The docker CLI only (engines run as sibling containers on the host's daemon).
ARG DOCKER_CLI_IMAGE=docker:28.5.2-cli
FROM ${DOCKER_CLI_IMAGE} AS dockercli

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
ENV CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orca-bridge --bin abhoth-bridge \
 && install -D -m 755 target/release/abhoth-bridge /out/abhoth-bridge

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini tzdata \
 && rm -rf /var/lib/apt/lists/*
COPY --from=dockercli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=build /out/abhoth-bridge /app/abhoth-bridge
RUN /app/abhoth-bridge --version && /app/abhoth-bridge --help >/dev/null
# The nvidia runtime mounts nvidia-smi and the driver's utility libraries in.
ENV NVIDIA_DRIVER_CAPABILITIES=utility \
    ABHOTH_BRIDGE_STATE_DIR=/state
WORKDIR /state
ENTRYPOINT ["/usr/bin/tini", "--", "/app/abhoth-bridge"]
CMD ["--config", "/config/bridge.toml"]
