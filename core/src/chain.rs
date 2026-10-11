//! The on-chain side of a swap, as a participant sees it.
//!
//! A trait rather than a client: the maker daemon and the browser-side taker
//! signer need very different transports for the same handful of operations.

use bitcoin::{Address, Amount, Transaction, Txid};

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("chain request failed: {0}")]
    Rpc(String),
    #[error("{method} failed ({code}): {message}")]
    Node {
        method: String,
        code: i64,
        message: String,
    },
}

/// The on-chain operations a swap participant needs.
pub trait Chain {
    /// Current block height.
    fn height(&self) -> Result<u32, ChainError>;

    /// Confirmations of a transaction. Zero while it is unconfirmed or not
    /// known at all, which is the normal state for something just broadcast.
    fn confirmations(&self, txid: &Txid) -> Result<u32, ChainError>;

    /// Broadcast a signed transaction.
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, ChainError>;

    /// Build and sign a transaction paying `amount` to `to`, funded from this
    /// participant's own wallet. Returns the signed transaction so the caller
    /// can persist it before broadcasting.
    fn fund(&self, to: &Address, amount: Amount) -> Result<Transaction, ChainError>;

    /// A fresh address controlled by this participant, for refunds.
    fn new_address(&self) -> Result<Address, ChainError>;
}
