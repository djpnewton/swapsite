# The services, built once and run from one image.
#
# The runtime base is the node's own image rather than a slim Debian, because
# three of the four services shell out to `lncli`: the maker and the panel drive
# LND, and the coordinator's dev payer pays an invoice. `lncli` does not survive
# being copied across distributions, so the cheapest correct thing is to share
# the image the node already uses. The relay is the one that does not need it.
#
# That image's own entrypoint creates a user and drops privileges, which leaves a
# container unable to read the admin macaroon. Every service here therefore sets
# its own `entrypoint` to skip it.

FROM rust:1.89-bookworm AS builder

WORKDIR /src

# The whole tree: `.dockerignore` keeps `target/` out, and that is the only thing
# here measured in gigabytes.
COPY . .

# One build for every binary, so what runs is what is in the tree and nothing
# else. Release, because these run as long-lived services.
RUN cargo build --release --bin relay --bin maker --bin coordinator --bin regtestd

# The taker is a separate target: the page loads it, so it is built for wasm
# rather than for the machine doing the building.
#
# `cc-rs` compiles secp256k1's C sources for that target, so the build needs a
# clang that can emit wasm. This is the one place the build differs from a plain
# `cargo build`, and the failure without it is a `cc-rs: failed to find tool
# "clang"` from somewhere inside a dependency.
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
RUN cargo build --release --target wasm32-unknown-unknown -p swapsite-wasm

# --- runtime ----------------------------------------------------------------

FROM polarlightning/lnd:0.18.3-beta AS runtime

WORKDIR /app

COPY --from=builder /src/target/release/relay /app/relay
COPY --from=builder /src/target/release/maker /app/maker
COPY --from=builder /src/target/release/coordinator /app/coordinator
COPY --from=builder /src/target/release/regtestd /app/regtestd

# The page and the module it loads, served straight off disk by the coordinator.
COPY --from=builder /src/web /app/web
COPY --from=builder /src/target/wasm32-unknown-unknown/release/swapsite_wasm.wasm /app/swapsite_wasm.wasm
