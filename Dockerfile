# syntax=docker/dockerfile:1.7
#
# SPG v7.0 production image.
#
# Multi-stage:
#   1. `builder` — Rust 1.96 on debian-slim; statically links to
#      musl libc so the runtime stage can stay minimal.
#   2. `runtime` — gcr.io/distroless/cc-debian12:nonroot, ~25 MiB
#      base. No shell, no package manager, no setuid binaries.
#      Listens on 5544; bind a volume at /data for db + WAL
#      persistence.
#
# Build:
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -t goliakk/spg:7.0.0 -t goliakk/spg:latest .
#
# Run:
#   docker run --rm -p 5544:5544 -v $PWD/data:/data goliakk/spg:7.0.0
#
# Build args:
#   - RUST_VERSION  pinned Rust toolchain. Defaults to 1.96 to
#     match `Cargo.toml` workspace `rust-version`.

ARG RUST_VERSION=1.96
# v7.0 — `builder` runs in the target platform (under QEMU when
# the host arch differs). Cross-musl-gcc tooling would be the
# fast alternative, but it'd pull a stack of native cross-
# compiler packages we don't ship anywhere else; native-arch
# builds keep the toolchain story simple and avoid wrong-arch
# linker invocations entirely.
FROM rust:${RUST_VERSION}-slim-bookworm AS builder
ARG TARGETPLATFORM

# musl-tools provides the musl-gcc wrapper rustc invokes when
# targeting *-unknown-linux-musl. pkg-config + perl satisfy any
# transitive build-script needs; the workspace itself has zero
# external deps but bench harnesses pull in criterion which
# touches both.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    perl \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pre-fetch dependencies in a separate layer so source edits don't
# bust the dep-build cache. Copy Cargo.toml + Cargo.lock + every
# member's manifest, then a stub source per crate so cargo will
# fetch + compile deps but not the project source.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtests ./xtests
COPY xbench ./xbench

# Pick the right musl target based on the requested platform.
# Cross-build is structurally simple here — SPG has zero external
# C dependencies, so `cargo build --release` with the target
# triple just works once the musl linker is in PATH.
# v7.0 — link to glibc (dynamic). The runtime stage is
# `distroless/cc-debian12` which ships glibc + libgcc +
# libstdc++ from the same Debian base — so a glibc-dynamic
# binary just works without bundling a C runtime. musl would
# need `+crt-static` for distroless compatibility, but
# crt-static landed inconsistent across Rust versions; glibc
# avoids the entire surface.
RUN case "$TARGETPLATFORM" in \
        "linux/amd64") TARGET=x86_64-unknown-linux-gnu ;; \
        "linux/arm64") TARGET=aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac \
 && rustup target add "$TARGET" \
 && cargo build --release --target "$TARGET" \
        --bin spg-server --bin spg --bin pg_isready \
 && mkdir -p /out \
 && cp "target/$TARGET/release/spg-server" /out/ \
 && cp "target/$TARGET/release/spg"        /out/ \
 && cp "target/$TARGET/release/pg_isready" /out/

# ----------------------------------------------------------------------
# Runtime stage. distroless/cc-debian12 ships glibc + libstdc++ +
# libgcc + the same trust store as Debian — small enough for a
# server image (~25 MiB), no shell so a compromised process has
# nowhere to escalate.
#
# We compiled with musl above which is statically linked — even
# `distroless/static-debian12` would suffice. `cc` is chosen for
# the friendlier debug surface (operators can `docker run --rm
# --entrypoint /usr/bin/strings goliakk/spg ...` if they need).
# ----------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

COPY --from=builder /out/spg-server /usr/local/bin/spg-server
COPY --from=builder /out/spg        /usr/local/bin/spg
# v7.13.0 (C5, mailrs round-5) — pg_isready-compatible health
# probe. Drops in next to the canonical PG binary name so
# docker-compose `healthcheck: test: ["CMD", "pg_isready", …]`
# blocks transparently.
COPY --from=builder /out/pg_isready /usr/local/bin/pg_isready

# Operator contract:
# - PG-wire on 5432 (default) — drop-in for Postgres clients.
# - Native wire on 5544 (default).
# - Persistent state under /data: catalog snapshot, WAL,
#   manifest, cold segments. Volume-mount it.
# - All other knobs via env vars; see DEPLOYMENT.md.
EXPOSE 5432
EXPOSE 5544
VOLUME ["/data"]
WORKDIR /data

# Non-root by default. `:nonroot` resolves to uid 65532; the
# /data volume the operator mounts must be writable by that uid.
USER nonroot:nonroot

# Sensible defaults for a containerised deployment:
#   - bind on every interface inside the container, port 5544
#   - place catalog snapshot + WAL under /data
#   - v7.13.0 (C1, mailrs round-5): PG-wire enabled by default
#     on 0.0.0.0:5432 so docker-compose stacks can drop the SPG
#     image into a service slot that previously ran postgres
#     without changing client URLs.
ENV SPG_PG_ADDR=0.0.0.0:5432
ENTRYPOINT ["/usr/local/bin/spg-server"]
CMD ["0.0.0.0:5544", "/data/spg.db", "-", "/data/wal.log"]
