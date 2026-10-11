//! Core primitives for trustless BTC <-> Lightning swaps.
//!
//! This crate is deliberately free of network and node dependencies so the same
//! code compiles natively for the maker daemon and to WASM for the browser-side
//! taker signer.
//!
//! The design is one primitive - a hashlocked leg - instantiated on an on-chain
//! backend or a Lightning backend. See `docs/plan.md`.

#![forbid(unsafe_code)]

pub mod chain;
pub mod hash;
pub mod htlc;
pub mod lightning;
pub mod maker;
pub mod params;
pub mod spend;
pub mod state;

pub use chain::{Chain, ChainError};
pub use hash::{PaymentHash, Preimage};
pub use htlc::Htlc;
pub use lightning::{check_joined, Invoice, InvoiceState, LightningNode, NodeInfo};
pub use maker::{plan, reconcile, Action, Observed, SwapRecord};
pub use params::{LegTimelocks, TimelockError, MIN_MARGIN_BLOCKS};
pub use spend::{claim, refund, SpendError};
pub use state::{Event, StateError, SwapState};
