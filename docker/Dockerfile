# syntax=docker/dockerfile:1

# Deterministic, credential-free image build for Latch.
#
# Stage layout:
#   builder -> compiles the workspace and the deterministic mock provider
#   runtime -> minimal non-root image that runs the `latch` binary
#   test    -> runtime plus the mock provider and the dogfood test entrypoint
#
# The default (last) stage is `test` so the deterministic harness builds with a
# plain `docker build`. The developer-facing runner targets `runtime`.
#
# Provenance: LATCH_SOURCE_REVISION is passed by the harness as the source Git
# SHA (with a `-dirty` suffix for a dirty tree) and is embedded both as the
# `org.opencontainers.image.revision` label and as an ENV, so a built image can
# be traced back to the source revision it came from.
#
# Multi-arch: `rust:1-bookworm` and `debian:bookworm-slim` both publish arm64
# and amd64, so the same file works on a Raspberry Pi 5 and on amd64 Linux.

ARG LATCH_SOURCE_REVISION=unknown

FROM rust:1-bookworm AS builder
# Optional cap on parallel codegen for memory-constrained hosts (Raspberry Pi).
# Passing `--build-arg CARGO_BUILD_JOBS=2` limits concurrency; unset keeps the
# cargo default. Cargo reads CARGO_BUILD_JOBS from the environment.
ARG CARGO_BUILD_JOBS
WORKDIR /src

# Resolve and cache dependencies first so source-only rebuilds stay cheap.
COPY Cargo.toml Cargo.lock ./
COPY crates/latch-protocol/Cargo.toml crates/latch-protocol/
COPY crates/latch-kernel/Cargo.toml crates/latch-kernel/
COPY crates/latch-tui/Cargo.toml crates/latch-tui/
COPY crates/latch-cli/Cargo.toml crates/latch-cli/
# Cargo resolves the workspace before sources exist, so give each member a
# placeholder target; the real sources are copied below.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    mkdir -p crates/latch-protocol/src crates/latch-kernel/src \
        crates/latch-tui/src crates/latch-cli/src \
 && touch crates/latch-protocol/src/lib.rs crates/latch-kernel/src/lib.rs \
        crates/latch-tui/src/lib.rs \
 && printf 'fn main() {}\n' > crates/latch-cli/src/main.rs \
 && cargo fetch --locked \
 && rm -rf crates/latch-protocol/src crates/latch-kernel/src \
        crates/latch-tui/src crates/latch-cli/src

COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p latch-cli --bin latch --example mock_provider \
 && mkdir -p /out \
 && cp target/release/latch /out/latch \
 && cp target/release/examples/mock_provider /out/mock_provider \
 && strip /out/latch /out/mock_provider

FROM debian:bookworm-slim AS runtime
ARG DEBIAN_FRONTEND=noninteractive
ARG LATCH_SOURCE_REVISION
LABEL org.opencontainers.image.revision=$LATCH_SOURCE_REVISION \
      org.opencontainers.image.title="latch-dogfood" \
      org.opencontainers.image.description="Latch dogfood runtime; source revision label for run provenance"
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bubblewrap \
      ca-certificates \
      git \
      python3 \
      ripgrep \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 1000 --shell /bin/bash latch

COPY --from=builder /out/latch /usr/local/bin/latch

# The sandbox and search tooling are mandatory; make the expectations explicit.
ENV HOME=/home/latch \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    LATCH_SOURCE_REVISION=$LATCH_SOURCE_REVISION
WORKDIR /home/latch

# Non-root by default. The dogfood runner additionally matches the host uid/gid
# so host-mounted files keep clear ownership.
USER latch
ENTRYPOINT ["latch"]
CMD ["--help"]

FROM runtime AS test
ARG LATCH_SOURCE_REVISION
LABEL org.opencontainers.image.revision=$LATCH_SOURCE_REVISION
COPY --from=builder /out/mock_provider /usr/local/bin/latch-mock-provider
COPY docker/test-entrypoint.sh /usr/local/bin/latch-dogfood-test
# The runtime stage already dropped to the non-root latch user.
USER root
RUN chmod 0755 /usr/local/bin/latch-dogfood-test
USER latch
ENTRYPOINT ["/usr/local/bin/latch-dogfood-test"]
