# syntax=docker/dockerfile:1.7
#
# Nodal server image: the web viewer (WASM) + tour server + tiler.
# Multi-arch without emulated compiles: everything is built on the build platform and the
# server is cross-compiled for the target architecture.

ARG RUST_IMAGE=rust:1-bookworm

# ---- Web viewer (WebAssembly, architecture independent) -------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS web
RUN apt-get update && apt-get install -y --no-install-recommends brotli && rm -rf /var/lib/apt/lists/* \
 && rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,id=nodal-web-registry,target=/usr/local/cargo/registry,sharing=locked \
    WBV=$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p') \
 && cargo install wasm-bindgen-cli --version "$WBV" --locked
COPY . .
RUN --mount=type=cache,id=nodal-web-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=nodal-web-target,target=/src/target,sharing=locked \
    ./scripts/build-web.sh /out/web

# ---- Server (cross-compiled for the target platform) ----------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS server
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /target ;; \
      arm64) echo aarch64-unknown-linux-gnu > /target \
             && apt-get update && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
             && rm -rf /var/lib/apt/lists/* ;; \
      *) echo "unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac \
 && rustup target add "$(cat /target)"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
WORKDIR /src
COPY . .
# Separate, locked cache mounts per stage: the web and server stages (and the per-architecture
# server builds) run in parallel, and cargo's registry lock lives outside the mounted directory,
# so a shared registry mount lets two cargo processes unpack the same crate at once.
RUN --mount=type=cache,id=nodal-server-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=nodal-server-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p nodal-server --target "$(cat /target)" \
 && mkdir -p /out && cp "target/$(cat /target)/release/nodal-server" /out/

# ---- Runtime --------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12
LABEL org.opencontainers.image.title="Nodal" \
      org.opencontainers.image.description="Fast, GPU-accelerated 360° panorama viewer and virtual tour server" \
      org.opencontainers.image.source="https://github.com/au12/Nodal" \
      org.opencontainers.image.licenses="MIT"
COPY --from=server /out/nodal-server /usr/local/bin/nodal-server
COPY --from=web /out/web /app/web
ENV NODAL_DATA=/data NODAL_PORT=8080 NODAL_WEB=/app/web RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/usr/local/bin/nodal-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/nodal-server"]
