# The services, built once and run from one image.
#
# The runtime base is the node's own image rather than a slim Debian, because three
# of the four services shell out to `lncli`, which does not survive being copied
# across distributions. That image's entrypoint drops privileges, which leaves a
# container unable to read the admin macaroon, so each service sets its own.

# Dependencies first, keyed only on the manifests. Otherwise any source edit
# recompiles the whole graph in release mode, which with `lto = true` is the
# slowest part of this build.

FROM rust:1.89-bookworm AS deps

WORKDIR /src

# For the wasm target only: `cc-rs` compiles secp256k1's C sources, and without a
# wasm-capable clang the failure comes from inside a dependency.
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown

COPY Cargo.toml Cargo.lock ./
COPY core/Cargo.toml core/
COPY adapters/Cargo.toml adapters/
COPY cli/Cargo.toml cli/
COPY wasm/Cargo.toml wasm/

# Cargo needs the files the manifests name to exist. What gets compiled here is
# the dependencies; the real sources arrive below and only they are rebuilt.
RUN mkdir -p core/src adapters/src wasm/src cli/src/bin \
    && : > core/src/lib.rs \
    && : > adapters/src/lib.rs \
    && : > wasm/src/lib.rs \
    && for bin in swapx maker relay coordinator regtestd; do \
         echo 'fn main() {}' > "cli/src/bin/$bin.rs"; \
       done

# Separate layers, so a wasm failure still commits the native dependencies.
RUN cargo build --release --bin relay --bin maker --bin coordinator --bin regtestd
RUN cargo build --release --target wasm32-unknown-unknown -p swapsite-wasm

FROM deps AS builder

# `.dockerignore` keeps `target/` out, which is the only thing here in gigabytes.
COPY . .

# `COPY` keeps the context's mtimes, so the stubs can look newer than the sources
# replacing them and cargo would ship the stubs.
RUN find core adapters cli wasm -name '*.rs' -exec touch {} +

RUN cargo build --release --bin relay --bin maker --bin coordinator --bin regtestd
RUN cargo build --release --target wasm32-unknown-unknown -p swapsite-wasm

FROM polarlightning/lnd:0.18.3-beta AS runtime

WORKDIR /app

COPY --from=builder \
     /src/target/release/relay \
     /src/target/release/maker \
     /src/target/release/coordinator \
     /src/target/release/regtestd \
     /app/

# The page and the module it loads, served straight off disk by the coordinator.
COPY --from=builder /src/web /app/web
COPY --from=builder /src/target/wasm32-unknown-unknown/release/swapsite_wasm.wasm /app/swapsite_wasm.wasm
