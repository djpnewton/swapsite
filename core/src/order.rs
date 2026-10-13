//! The order book: one line per swap, whatever mechanism carried it.
//!
//! Every swap writes one file under `orders/`, holding what was agreed and
//! nothing that moves. The mechanism's own record stays the source of truth for
//! acting on a swap, so an order that is missing, stale or half-written is a gap
//! in a listing and never a swap anybody loses track of. Anything that changes
//! as the swap runs - its state, its deadlines, whether it was paid - belongs in
//! the record the order points at, because two copies of a moving value drift.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Which mechanism carries a swap, and therefore where its record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapKind {
    /// The taker pays over Lightning and is paid on chain.
    Ln2btc,
    /// The taker pays on chain and is paid over Lightning.
    Btc2ln,
}

impl SwapKind {
    /// The directory its records live in, under the swap root.
    ///
    /// The one place a directory name is spelled, so a store cannot be opened
    /// on another mechanism's files.
    pub fn dir(self) -> &'static str {
        match self {
            SwapKind::Ln2btc => "ln2btc",
            SwapKind::Btc2ln => "btc2ln",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.dir()
    }

    /// Whether both sides of this mechanism are satoshi, which is what makes an
    /// order's two amounts comparable at all.
    pub fn both_sides_are_sats(self) -> bool {
        match self {
            SwapKind::Ln2btc | SwapKind::Btc2ln => true,
        }
    }
}

impl fmt::Display for SwapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SwapKind {
    type Err = OrderError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "ln2btc" => Ok(SwapKind::Ln2btc),
            "btc2ln" => Ok(SwapKind::Btc2ln),
            other => Err(OrderError::UnknownKind(other.to_owned())),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OrderError {
    #[error("no such kind of swap: {0}")]
    UnknownKind(String),
    #[error("a swap that gives {from} satoshi and returns {to} is not a swap")]
    NotASwap { from: u64, to: u64 },
}

/// One line of the order book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// The payment hash, which is also the record's file name.
    pub id: String,
    /// The mechanism, which says which directory the record is in.
    pub kind: SwapKind,
    /// Unix time the swap was agreed.
    pub at: u64,
    /// What the taker gives, in satoshis on every mechanism there is today: see
    /// [`SwapKind::both_sides_are_sats`].
    pub from: u64,
    /// What the taker gets, on the same terms.
    pub to: u64,
}

impl Order {
    /// Record an agreed swap, refusing one that returns more than it takes: a
    /// swap nobody can profit from is a fee bug, not a price.
    pub fn new(
        id: impl Into<String>,
        kind: SwapKind,
        at: u64,
        from: u64,
        to: u64,
    ) -> Result<Self, OrderError> {
        // Only comparable while both sides are the same asset, which is the
        // question the kind above has to answer.
        if kind.both_sides_are_sats() && from < to {
            return Err(OrderError::NotASwap { from, to });
        }
        Ok(Self {
            id: id.into(),
            kind,
            at,
            from,
            to,
        })
    }

    /// What the swap costs the taker, in satoshis.
    ///
    /// Nothing for a mechanism whose sides are not both satoshi: those two
    /// amounts are not comparable, let alone subtractable.
    pub fn cost_sat(&self) -> Option<u64> {
        self.kind.both_sides_are_sats().then(|| self.from - self.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_names_its_own_directory_and_nothing_elses() {
        assert_eq!(SwapKind::Ln2btc.dir(), "ln2btc");
        assert_eq!(SwapKind::Btc2ln.dir(), "btc2ln");
        assert_ne!(SwapKind::Ln2btc.dir(), SwapKind::Btc2ln.dir());
    }

    #[test]
    fn a_kind_survives_the_round_trip_it_is_stored_by() {
        for kind in [SwapKind::Ln2btc, SwapKind::Btc2ln] {
            assert_eq!(kind.to_string(), kind.dir());
            assert_eq!(SwapKind::from_str(kind.as_str()), Ok(kind));
            // And the spelling the file carries, since that is the half of the
            // link a reader gets from the order rather than from the path.
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.dir()));
            assert_eq!(serde_json::from_str::<SwapKind>(&json).unwrap(), kind);
        }
        assert!(SwapKind::from_str("lbtc").is_err());
    }

    #[test]
    fn an_order_says_what_the_swap_costs() {
        let order = Order::new("ab", SwapKind::Ln2btc, 1_700_000_000, 30_030, 30_000).unwrap();
        assert_eq!(order.cost_sat(), Some(30));
        assert_eq!(order.from - order.to, 30);
    }

    /// The question a new mechanism has to answer, because the match in
    /// `both_sides_are_sats` will not compile until it does.
    #[test]
    fn every_mechanism_says_whether_it_is_satoshi_on_both_sides() {
        for kind in [SwapKind::Ln2btc, SwapKind::Btc2ln] {
            assert!(kind.both_sides_are_sats(), "{kind}");
        }
    }

    #[test]
    fn an_order_that_returns_more_than_it_takes_is_refused() {
        let wrong = Order::new("ab", SwapKind::Btc2ln, 0, 1_000, 1_001);
        assert_eq!(
            wrong,
            Err(OrderError::NotASwap {
                from: 1_000,
                to: 1_001
            })
        );
        // A swap with no fee is legitimate, if pointless.
        assert!(Order::new("ab", SwapKind::Btc2ln, 0, 1_000, 1_000).is_ok());
    }

    #[test]
    fn an_order_round_trips_through_the_file_it_is_stored_in() {
        let order = Order::new("cd", SwapKind::Btc2ln, 1_700_000_001, 22_100, 20_000).unwrap();
        let json = serde_json::to_string(&order).unwrap();
        assert_eq!(serde_json::from_str::<Order>(&json).unwrap(), order);
    }
}
