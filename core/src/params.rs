//! Timelock ordering for the two legs of a swap.
//!
//! The whole safety of an HTLC swap rests on this: the leg that must be claimed
//! *second* has to expire *first*, so the party acting first still has time to
//! act after the other side reveals the preimage.

use bitcoin::absolute::LockTime;

/// Minimum gap, in blocks, between the two refund timeouts.
///
/// The gap has to cover the counterparty claiming their leg plus confirmation.
/// Conservative on purpose, and a floor rather than a target.
pub const MIN_MARGIN_BLOCKS: u32 = 12;

/// Final CLTV delta the maker asks for on its hold invoice, in blocks.
///
/// Chosen rather than inherited from the node's default, because the on-chain
/// refund has to outlive it: see [`LegTimelocks`].
pub const DEFAULT_CLTV_DELTA: u32 = 40;

/// Blocks a counterparty is given to act in at the last moment either side may
/// still act, which is the moment its window is smallest.
///
/// Folded into the locktime margin below, so an action taken at the very last
/// moment still leaves a payment that can be claimed.
pub const PAYMENT_WINDOW_BLOCKS: u32 = 30;

/// Blocks from the last height a leg may be started at to the refund.
///
/// The payment's CLTV delta plus the minimum margin plus the window above: any
/// less and a payment made at the last moment would expire too close to the
/// refund to be claimed.
pub const DEFAULT_LOCKTIME_MARGIN: u32 =
    DEFAULT_CLTV_DELTA + MIN_MARGIN_BLOCKS + PAYMENT_WINDOW_BLOCKS;

/// Whether `initiator` outlives `counterparty` by at least `margin` blocks.
///
/// The same rule as [`LegTimelocks::new`], for a caller that has plain heights
/// rather than timeout types.
pub fn has_margin(initiator: u32, counterparty: u32, margin: u32) -> bool {
    initiator > counterparty && initiator - counterparty >= margin
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimelockError {
    #[error("initiator timeout {initiator} is not after counterparty timeout {counterparty}")]
    WrongOrder { initiator: u32, counterparty: u32 },

    #[error(
        "margin too small: initiator {initiator} minus counterparty {counterparty} \
         is {actual} blocks, need at least {required}"
    )]
    MarginTooSmall {
        initiator: u32,
        counterparty: u32,
        actual: u32,
        required: u32,
    },

    #[error("refund timeout must be a block height, not a time")]
    NonHeight,
}

/// A validated pair of refund timeouts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LegTimelocks {
    initiator: LockTime,
    counterparty: LockTime,
}

impl LegTimelocks {
    /// Validate ordering, requiring a gap of at least `margin` blocks.
    pub fn new(
        initiator: LockTime,
        counterparty: LockTime,
        margin: u32,
    ) -> Result<Self, TimelockError> {
        if initiator.is_block_time() || counterparty.is_block_time() {
            return Err(TimelockError::NonHeight);
        }

        let initiator_h = initiator.to_consensus_u32();
        let counterparty_h = counterparty.to_consensus_u32();

        if initiator_h <= counterparty_h {
            return Err(TimelockError::WrongOrder {
                initiator: initiator_h,
                counterparty: counterparty_h,
            });
        }

        let actual = initiator_h - counterparty_h;
        if actual < margin {
            return Err(TimelockError::MarginTooSmall {
                initiator: initiator_h,
                counterparty: counterparty_h,
                actual,
                required: margin,
            });
        }

        Ok(Self {
            initiator,
            counterparty,
        })
    }

    /// Same, from plain heights, which is how a swap is described before it is
    /// built.
    pub fn from_heights(
        initiator: u32,
        counterparty: u32,
        margin: u32,
    ) -> Result<Self, TimelockError> {
        let to_locktime =
            |height: u32| LockTime::from_height(height).map_err(|_| TimelockError::NonHeight);
        Self::new(to_locktime(initiator)?, to_locktime(counterparty)?, margin)
    }

    /// Same, using [`MIN_MARGIN_BLOCKS`].
    pub fn with_default_margin(
        initiator: LockTime,
        counterparty: LockTime,
    ) -> Result<Self, TimelockError> {
        Self::new(initiator, counterparty, MIN_MARGIN_BLOCKS)
    }

    pub fn initiator(&self) -> LockTime {
        self.initiator
    }

    pub fn counterparty(&self) -> LockTime {
        self.counterparty
    }

    /// Gap in blocks between the two timeouts.
    pub fn margin_blocks(&self) -> u32 {
        self.initiator.to_consensus_u32() - self.counterparty.to_consensus_u32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(height: u32) -> LockTime {
        LockTime::from_height(height).unwrap()
    }

    #[test]
    fn accepts_a_healthy_margin() {
        let t = LegTimelocks::new(h(800_144), h(800_100), 12).unwrap();
        assert_eq!(t.margin_blocks(), 44);
        assert_eq!(t.initiator(), h(800_144));
        assert_eq!(t.counterparty(), h(800_100));
    }

    #[test]
    fn accepts_exactly_the_minimum_margin() {
        assert!(LegTimelocks::new(h(112), h(100), 12).is_ok());
    }

    #[test]
    fn heights_are_checked_the_same_way_as_timeouts() {
        assert!(LegTimelocks::from_heights(112, 100, 12).is_ok());
        assert_eq!(
            LegTimelocks::from_heights(111, 100, 12).unwrap_err(),
            TimelockError::MarginTooSmall {
                initiator: 111,
                counterparty: 100,
                actual: 11,
                required: 12,
            }
        );
        // The same rule without the types, for callers that only have heights.
        assert!(has_margin(112, 100, 12));
        assert!(!has_margin(111, 100, 12));
    }

    #[test]
    fn rejects_inverted_timeouts() {
        assert!(matches!(
            LegTimelocks::new(h(100), h(800_144), 12),
            Err(TimelockError::WrongOrder { .. })
        ));
    }

    #[test]
    fn rejects_equal_timeouts() {
        assert!(matches!(
            LegTimelocks::new(h(500), h(500), 12),
            Err(TimelockError::WrongOrder { .. })
        ));
    }

    #[test]
    fn rejects_margin_that_is_one_block_short() {
        assert!(matches!(
            LegTimelocks::new(h(111), h(100), 12),
            Err(TimelockError::MarginTooSmall {
                actual: 11,
                required: 12,
                ..
            })
        ));
    }

    #[test]
    fn rejects_time_based_timeouts() {
        let time = LockTime::from_time(1_700_000_000).unwrap();
        assert!(matches!(
            LegTimelocks::new(h(800_144), time, 12),
            Err(TimelockError::NonHeight)
        ));
        assert!(matches!(
            LegTimelocks::new(time, h(800_000), 12),
            Err(TimelockError::NonHeight)
        ));
    }
}
