//! Node transports and on-disk state for the swap tooling.
//!
//! Everything here does real I/O: it spawns docker, opens sockets and touches
//! the filesystem. That is why it lives outside `swapsite-core`, which stays
//! pure so the same domain code can compile to WASM for the browser-side taker
//! signer.

pub mod bitcoind;
pub mod lncli;
pub mod store;

pub use bitcoind::{BitcoinRpc, RawTransaction};
pub use lncli::LnCli;
pub use store::SwapStore;
