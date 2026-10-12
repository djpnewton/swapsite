//! What a taker can still do about a swap, and how long it has to do it.
//!
//! Recovery is not something anyone can do *for* a taker: nobody else holds the
//! claim key or the preimage. What can be done is to say, from the swap's public
//! terms and the state of the chain, whether a move is still available - and to
//! say it early enough to matter.
//!
//! Two things this has to get right. The hashlock path is not timelocked, so
//! passing the deadline is not the same as losing: it turns a safe claim into a
//! race with the maker's refund. And a spent deposit does not mean the maker
//! took it - the taker may have claimed it itself - so the verdict reports the
//! fact rather than apportioning blame.

use serde::{Deserialize, Serialize};

use crate::params::{has_margin, MIN_MARGIN_BLOCKS};
use crate::state::SwapState;

/// A deadline this close, in blocks, counts as urgent.
pub const URGENT_BLOCKS: u32 = 12;

/// What the taker knows when it asks for a verdict.
///
/// `has_preimage` and `deposit_unspent` are the taker's own findings, which is
/// why the verdict is computed on the taker's side rather than asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TakerView {
    /// The swap's state as last seen. Used only to tell "not funded yet" from
    /// "funded but unpaid"; the chain decides the rest.
    pub state: SwapState,
    pub has_preimage: bool,
    /// Whether the HTLC output is still unspent, as the chain reports it.
    pub deposit_unspent: bool,
    /// The Lightning leg's final CLTV delta: a payment made now expires at
    /// `height + cltv_delta`, and the refund has to open after that.
    pub cltv_delta: u32,
    /// The height at which the maker's refund path opens.
    pub deadline: u32,
    /// The height this verdict is being decided at.
    pub height: u32,
}

/// What the taker has left to do about one swap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TakerAction {
    /// The maker has not funded the HTLC yet. Nothing to lose, nothing to do.
    WaitForDeposit,
    /// The HTLC is funded and the preimage has not reached this page. Either
    /// the payment is still in flight or it was never made.
    WaitForPayment,
    /// Paying now would leave the payment expiring too close to the maker's
    /// refund opening, so a maker could refund and then settle and take both
    /// legs. Nothing to claim yet, and nothing to pay.
    TooLateToPay { blocks_left: u32 },
    /// The swap settled, so the preimage is public, but this page does not have
    /// it: it has to come from the wallet that paid, before the refund opens.
    FindPreimage { blocks_left: u32 },
    /// The preimage is here and the refund path is still closed, so claiming now
    /// cannot lose a race.
    Claim { blocks_left: u32 },
    /// The refund path is open, so the maker can withdraw the deposit at any
    /// moment. A claim still wins, but only if it confirms first.
    Racing { blocks_since: u32 },
    /// The deposit has been spent, so there is nothing left to claim. Either the
    /// maker refunded it or this taker already claimed it. A claim signed after
    /// this point cannot be mined.
    Gone,
}

/// The verdict for one swap, with the heights it was reached from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Recovery {
    pub action: TakerAction,
    /// The height at which the maker's refund path opens.
    pub deadline: u32,
    /// The height the verdict was decided against.
    pub height: u32,
    /// Whether the taker should act now rather than later.
    pub urgent: bool,
    /// Whether there is anything to act on at all.
    ///
    /// A field rather than a method on purpose: the page reads the verdict as
    /// JSON, and a method would not cross that boundary - it would arrive as
    /// `undefined` and quietly skip every swap.
    pub actionable: bool,
}

impl Recovery {
    /// Decide what the taker can still do.
    pub fn assess(view: TakerView) -> Self {
        let TakerView {
            state,
            has_preimage,
            deposit_unspent,
            cltv_delta,
            deadline,
            height,
        } = view;

        // The refund path opens *at* `deadline`, not after it: a transaction
        // carrying that nLockTime can be mined in that very block.
        let left = if height < deadline {
            Some(deadline - height)
        } else {
            None
        };
        let since = height.saturating_sub(deadline);

        let action = if state == SwapState::Refunded || !deposit_unspent {
            TakerAction::Gone
        } else if has_preimage {
            match left {
                Some(blocks_left) => TakerAction::Claim { blocks_left },
                None => TakerAction::Racing {
                    blocks_since: since,
                },
            }
        } else {
            match state {
                // A swap the taker might still pay into. If the refund would
                // open before the payment expires plus a margin, paying is the
                // one move that can lose the taker money.
                SwapState::Created | SwapState::InitiatorFunded | SwapState::CounterpartyLocked
                    if !has_margin(
                        deadline,
                        height.saturating_add(cltv_delta),
                        MIN_MARGIN_BLOCKS,
                    ) =>
                {
                    TakerAction::TooLateToPay {
                        blocks_left: left.unwrap_or(0),
                    }
                }
                SwapState::Created => TakerAction::WaitForDeposit,
                // Settled means the payment went through and the maker has it,
                // so the deposit is the taker's and only the secret is missing.
                // Chasing it is still worth doing: the hashlock path has no
                // timelock, so a claim is valid at any height.
                SwapState::Settled => match left {
                    Some(blocks_left) => TakerAction::FindPreimage { blocks_left },
                    None => TakerAction::Racing {
                        blocks_since: since,
                    },
                },
                _ => TakerAction::WaitForPayment,
            }
        };

        let urgent = match action {
            TakerAction::Claim { blocks_left } | TakerAction::FindPreimage { blocks_left } => {
                blocks_left <= URGENT_BLOCKS
            }
            TakerAction::Racing { .. } => true,
            TakerAction::WaitForDeposit
            | TakerAction::WaitForPayment
            | TakerAction::TooLateToPay { .. }
            | TakerAction::Gone => false,
        };

        let actionable = matches!(
            action,
            TakerAction::Claim { .. }
                | TakerAction::FindPreimage { .. }
                | TakerAction::Racing { .. }
        );

        Self {
            action,
            deadline,
            height,
            urgent,
            actionable,
        }
    }
}

/// Names match the serde representation, so a verdict reads the same in a log
/// line, a status output and the page.
impl std::fmt::Display for TakerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            TakerAction::WaitForDeposit => "wait_for_deposit",
            TakerAction::WaitForPayment => "wait_for_payment",
            TakerAction::TooLateToPay { .. } => "too_late_to_pay",
            TakerAction::FindPreimage { .. } => "find_preimage",
            TakerAction::Claim { .. } => "claim",
            TakerAction::Racing { .. } => "racing",
            TakerAction::Gone => "gone",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SwapState::*;

    const DEADLINE: u32 = 500;

    /// A taker whose deposit is still there, which is the normal case.
    fn view(state: SwapState, has_preimage: bool, height: u32) -> TakerView {
        TakerView {
            state,
            has_preimage,
            deposit_unspent: true,
            cltv_delta: 40,
            deadline: DEADLINE,
            height,
        }
    }

    fn action(state: SwapState, has_preimage: bool, height: u32) -> TakerAction {
        Recovery::assess(view(state, has_preimage, height)).action
    }

    #[test]
    fn an_unfunded_swap_asks_nothing_of_the_taker() {
        let recovery = Recovery::assess(view(Created, false, 100));
        assert_eq!(recovery.action, TakerAction::WaitForDeposit);
        assert!(!recovery.urgent);
        assert!(!recovery.actionable);
    }

    #[test]
    fn a_funded_swap_without_a_payment_waits() {
        for state in [InitiatorFunded, CounterpartyLocked] {
            assert_eq!(action(state, false, 100), TakerAction::WaitForPayment);
        }
    }

    #[test]
    fn paying_into_a_window_that_is_too_short_is_refused() {
        // 449 + 40 + 12 puts the payment's expiry at the deadline itself, so the
        // maker could refund and still settle before the taker could claim.
        for state in [Created, InitiatorFunded, CounterpartyLocked] {
            assert_eq!(
                action(state, false, 449),
                TakerAction::TooLateToPay { blocks_left: 51 },
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_settled_swap_still_wants_the_preimage_when_the_window_is_short() {
        // The payment already happened, so there is nothing to refuse: a claim
        // is valid at any height and racing the refund is the taker's only move.
        assert_eq!(
            action(Settled, false, 449),
            TakerAction::FindPreimage { blocks_left: 51 }
        );
    }

    #[test]
    fn a_settled_swap_wants_the_preimage_from_wherever_it_went() {
        // Settled with no preimage is not "nothing to do": the maker has been
        // paid and the deposit is waiting for whoever holds H.
        let recovery = Recovery::assess(view(Settled, false, 100));
        assert_eq!(
            recovery.action,
            TakerAction::FindPreimage { blocks_left: 400 }
        );
        assert!(recovery.actionable);
    }

    #[test]
    fn a_claim_counts_the_blocks_it_has() {
        assert_eq!(
            action(Settled, true, 480),
            TakerAction::Claim { blocks_left: 20 }
        );
        // The last safe block: at DEADLINE the refund path opens.
        assert_eq!(
            action(Settled, true, 499),
            TakerAction::Claim { blocks_left: 1 }
        );
    }

    #[test]
    fn urgency_starts_a_margin_before_the_deadline_not_at_it() {
        let just_too_early = Recovery::assess(view(Settled, true, 500 - URGENT_BLOCKS - 1));
        assert!(!just_too_early.urgent, "{just_too_early:?}");

        let on_the_line = Recovery::assess(view(Settled, true, 500 - URGENT_BLOCKS));
        assert!(on_the_line.urgent, "{on_the_line:?}");
    }

    #[test]
    fn passing_the_deadline_is_a_race_not_a_loss() {
        // The hashlock path has no CLTV, so a claim is still valid at DEADLINE
        // and beyond. What changed is that the maker can now refund too.
        assert_eq!(
            action(Settled, true, DEADLINE - 1),
            TakerAction::Claim { blocks_left: 1 }
        );
        assert_eq!(
            action(Settled, true, DEADLINE),
            TakerAction::Racing { blocks_since: 0 }
        );
        assert_eq!(
            action(Settled, true, DEADLINE + 7),
            TakerAction::Racing { blocks_since: 7 }
        );

        let recovery = Recovery::assess(view(Settled, true, DEADLINE + 7));
        assert!(recovery.urgent);
        assert!(recovery.actionable);
    }

    #[test]
    fn a_spent_deposit_ends_it_whatever_the_clock_says() {
        // The deposit can be spent well before the deadline, and the deadline
        // can pass with the deposit still sitting there. The output decides.
        for height in [100, DEADLINE, DEADLINE + 100] {
            let recovery = Recovery::assess(TakerView {
                deposit_unspent: false,
                ..view(Settled, true, height)
            });
            assert_eq!(recovery.action, TakerAction::Gone, "at height {height}");
            assert!(!recovery.actionable);
            assert!(!recovery.urgent);
        }
    }

    #[test]
    fn a_refund_nobody_reported_is_still_noticed() {
        // The maker's own bookkeeping cannot see a refund it did not build, so
        // a spent output has to be enough on its own.
        let recovery = Recovery::assess(TakerView {
            deposit_unspent: false,
            ..view(Settled, true, 100)
        });
        assert_eq!(recovery.action, TakerAction::Gone);
    }

    #[test]
    fn only_a_recorded_refund_or_a_spent_output_is_an_ending() {
        assert_eq!(action(Refunded, true, 100), TakerAction::Gone);
        assert_eq!(action(Refunded, false, 100), TakerAction::Gone);

        let recovery = Recovery::assess(view(Refunded, true, 100));
        assert!(!recovery.urgent);
        assert!(!recovery.actionable);
    }

    #[test]
    fn the_verdict_reports_the_heights_it_used() {
        let recovery = Recovery::assess(view(Settled, true, 480));
        assert_eq!(recovery.deadline, DEADLINE);
        assert_eq!(recovery.height, 480);
    }

    #[test]
    fn a_height_well_past_the_deadline_does_not_underflow() {
        // Silly input, but the arithmetic should not wrap into a false alarm.
        let recovery = Recovery::assess(view(Settled, true, 1_000_000));
        assert_eq!(
            recovery.action,
            TakerAction::Racing {
                blocks_since: 1_000_000 - DEADLINE
            }
        );
    }
}
