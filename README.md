# swapsite

Trustless swaps between BTC, Liquid (L-BTC) and Lightning.

The website is a **non-custodial coordinator**: it matches counterparties and
relays swap state, but every value transfer is enforced by hashlocked scripts
and Lightning HTLCs. The coordinator cannot move funds.

## Docs

- [docs/plan.md](docs/plan.md) - design, trust model, wallet requirements, roadmap
- [docs/regtest.md](docs/regtest.md) - local regtest environment (BTC + Lightning)
- [docs/m1.md](docs/m1.md) - protocol core: HTLC leg, timeouts, state machine

## Status

M0 through M1c are done. Both legs are implemented in Rust, a complete swap runs
end to end on regtest, and a maker daemon settles and refunds swaps on its own
from observed chain and Lightning state. Next is the WASM taker signer.

```bash
cargo test                       # unit tests, includes real script verification
docker compose -f docker/docker-compose.yml up -d
./scripts/regtest-init.sh        # fund nodes, open a balanced channel
./scripts/regtest-onchain.sh     # fund, claim and refund an HTLC on-chain
./scripts/regtest-swap.sh        # one complete swap, both legs, plus the failure path
./scripts/regtest-maker.sh       # the daemon: cold-start settle and timed refund
```

## Layout

| Crate | Contents |
| ----- | -------- |
| `core/` | the domain: HTLC, Lightning leg, swap state, maker decision logic. No I/O, so it compiles to WASM. |
| `adapters/` | the I/O: Bitcoin Core JSON-RPC, `lncli`, swap records on disk. |
| `cli/` | `swapx`, a stateless leg driver, and `maker`, the daemon. |
