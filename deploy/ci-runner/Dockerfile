# The GitHub Actions runner image for abhoth on deepthought (deploy/ci-runner/README.md).
# Modelled on the fissure-ci and ezmuze-ci runner images on the same box. The jobs build
# IN this container, so it carries everything ci.yml's Linux jobs need:
#
# - the Rust toolchain pinned by rust-toolchain.toml (clippy, rustfmt) plus the
#   x86_64-pc-windows-msvc target, and cargo-xwin with clang/lld/llvm to cross-compile
#   abhoth.exe and abhoth-desktop.exe (the MSVC CRT and Windows SDK are downloaded by
#   cargo-xwin on first use into XWIN_CACHE_DIR, on the Storage pool, not baked in);
# - the desktop feature's system libraries (WebKitGTK 4.1, GTK 3, D-Bus, Wayland);
# - Node 22 for client/;
# - .NET 10 and vpk (Velopack), which packs the Windows Setup.exe and the Linux AppImage
#   from Linux. zstd (deltas, 1.5.7 built below) and squashfs-tools (AppImage) are required by vpk.
# - gh, to attach the packages to a GitHub Release on tags.
FROM ubuntu:24.04

ARG RUNNER_VERSION=2.338.0
ARG RUST_TOOLCHAIN=1.100.0
ARG CARGO_XWIN_VERSION=0.24.1
ARG NODE_VERSION=22.23.5
ARG VPK_VERSION=1.2.161

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl wget git jq python3 openssh-client gzip tar xz-utils zip unzip \
      file procps sudo zstd squashfs-tools libicu74 libatomic1 \
      build-essential pkg-config clang lld llvm \
      libwebkit2gtk-4.1-dev libgtk-3-dev libdbus-1-dev libwayland-dev \
 && rm -rf /var/lib/apt/lists/*

# gh, from its vendor's apt repository.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=amd64 signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# Rust. The toolchain lives in the image; CARGO_HOME (the registry and git caches) is
# moved to the Storage pool at run time (compose.yaml), and the rustup proxies in
# /usr/local/cargo/bin work with any CARGO_HOME.
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs -o /tmp/rustup.sh \
 && CARGO_HOME=/usr/local/cargo sh /tmp/rustup.sh -y --no-modify-path --profile minimal \
      --default-toolchain "${RUST_TOOLCHAIN}" -c clippy -c rustfmt -t x86_64-pc-windows-msvc \
 && rm /tmp/rustup.sh \
 && chmod -R a+rX /usr/local/rustup /usr/local/cargo \
 && rustc --version

# cargo-xwin, the release binary.
RUN curl -fsSL "https://github.com/rust-cross/cargo-xwin/releases/download/v${CARGO_XWIN_VERSION}/cargo-xwin-v${CARGO_XWIN_VERSION}.x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin cargo-xwin \
 && cargo-xwin --version

# Node, the official tarball.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
 && node --version && npm --version

# .NET 10 and vpk (a pinned dotnet tool, installed to a fixed path rather than ~/.dotnet).
ENV DOTNET_ROOT=/usr/share/dotnet \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1
RUN curl -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh \
 && bash /tmp/dotnet-install.sh --channel 10.0 --install-dir /usr/share/dotnet \
 && rm /tmp/dotnet-install.sh \
 && ln -s /usr/share/dotnet/dotnet /usr/bin/dotnet \
 && dotnet tool install --tool-path /opt/dotnet-tools vpk --version "${VPK_VERSION}" \
 && ln -s /opt/dotnet-tools/vpk /usr/local/bin/vpk \
 && vpk --help > /dev/null

# zstd 1.5.7 from source, in /usr/local/bin ahead of Ubuntu's /usr/bin/zstd. Ubuntu 24.04's
# zstd 1.5.5 fails vpk's delta call (`zstd --patch-from OLD -o OUT --force --single-thread NEW
# --long=27`) with "error: missing command argument"; 1.5.7 takes it.
ARG ZSTD_VERSION=1.5.7
ARG ZSTD_SHA256=eb33e51f49a15e023950cd7825ca74a4a2b43db8354825ac24fc1b7ee09e6fa3
RUN curl -fsSL -o /tmp/zstd.tar.gz "https://github.com/facebook/zstd/releases/download/v${ZSTD_VERSION}/zstd-${ZSTD_VERSION}.tar.gz" \
 && echo "${ZSTD_SHA256}  /tmp/zstd.tar.gz" | sha256sum -c - \
 && mkdir /tmp/zstd && tar -xzf /tmp/zstd.tar.gz -C /tmp/zstd --strip-components=1 \
 && make -C /tmp/zstd/programs -j"$(nproc)" zstd \
 && install -m 0755 /tmp/zstd/programs/zstd /usr/local/bin/zstd \
 && rm -rf /tmp/zstd /tmp/zstd.tar.gz \
 && head -c 100000 /dev/urandom > /tmp/a && { cat /tmp/a; echo x; } > /tmp/b \
 && zstd -q --patch-from /tmp/a -o /tmp/b.zsdiff --force --single-thread /tmp/b --long=27 \
 && rm /tmp/a /tmp/b /tmp/b.zsdiff \
 && zstd --version

# The runner is unpacked into each runner's own directory on first start (entrypoint.sh),
# so its self-updates persist; the image only carries the tarball and its dependencies.
RUN mkdir -p /opt/runner-dist \
 && curl -fsSL -o /opt/runner-dist/runner.tar.gz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz" \
 && mkdir /tmp/r && tar -xzf /opt/runner-dist/runner.tar.gz -C /tmp/r \
 && apt-get update && /tmp/r/bin/installdependencies.sh && rm -rf /tmp/r /var/lib/apt/lists/*

COPY entrypoint.sh job-started.sh /opt/abhoth-ci/
ENTRYPOINT ["bash", "/opt/abhoth-ci/entrypoint.sh"]
