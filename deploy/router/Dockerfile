# abhoth router (LiteLLM + qfn-lb replacement) as a small image. Build from rust/:
#   docker build -f deploy/router/Dockerfile -t local/abhoth-router:1 .
# One static-ish binary (rustls, no OpenSSL) on debian slim; runs as uid 568 (TrueNAS "apps").
# openssh-client: the control plane reaches the GPU host over SSH (hostctl, ROUTER-CONFIG.md 6).
ARG RUST_VERSION=1.100.0
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
ENV CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orca-router --bin abhoth-router \
 && install -D -m 755 target/release/abhoth-router /out/abhoth-router

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tzdata openssh-client \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/abhoth-router /app/abhoth-router
RUN /app/abhoth-router --help >/dev/null
USER 568:568
WORKDIR /state
# Default: serve /state/router.json. The shadow compose overrides this per service.
ENTRYPOINT ["/app/abhoth-router"]
CMD ["--config", "/state/router.json", "--listen", "0.0.0.0:8020"]
