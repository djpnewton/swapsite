//! Swap state machine.
//!
//! Deliberately small and total: every (state, event) pair is either a defined
//! transition or an error. The important properties are that a leg can never be
//! claimed before the other side committed, and that every funded state has a
//! refund escape hatch.

use serde::{Deserialize, Serialize};

/// Where a swap is in its lifecycle.
///
/// `Initiator` is the party who commits value first; `counterparty` is the
/// other side. For a submarine swap the initiator is the taker funding the
/// on-chain HTLC.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapState {
    /// Terms agreed, nothing committed yet.
    Created,
    /// The initiator's leg is locked (on-chain HTLC funded, LN hold in flight).
    InitiatorFunded,
    /// Both legs are locked. The preimage can now be revealed.
    CounterpartyLocked,
    /// The preimage path was used and both legs resolved.
    Settled,
    /// The initiator reclaimed their leg after the timeout.
    Refunded,
}

/// Something that moves a swap forward.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// Initiator locks their leg.
    Fund,
    /// Counterparty locks their leg.
    Lock,
    /// Preimage revealed; the swap completes.
    Settle,
    /// Initiator reclaims after the timeout.
    Refund,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    #[error("cannot {event:?} while {state:?}")]
    Illegal { state: SwapState, event: Event },
    #[error("swap already finished in state {0:?}")]
    Finished(SwapState),
    #[error("cannot lock the counterparty leg before the initiator funds")]
    NotFundedYet,
    #[error("cannot settle before both legs are locked")]
    NotLockedYet,
    #[error("cannot refund: the initiator leg is not funded")]
    NothingToRefund,
    #[error("refusing to adopt non-terminal state {0:?} from outside")]
    NotTerminal(SwapState),
}

impl SwapState {
    pub fn is_terminal(self) -> bool {
        matches!(self, SwapState::Settled | SwapState::Refunded)
    }
    /// Whether the initiator can still recover their funds from here.
    pub fn can_refund(self) -> bool {
        matches!(
            self,
            SwapState::InitiatorFunded | SwapState::CounterpartyLocked
        )
    }

    /// Adopt a state observed from outside, for crash recovery.
    ///
    /// Only terminal states can be adopted. A swap that is settled or refunded
    /// on the network must not be un-finished by a stale local record.
    pub fn restore(self, observed: SwapState) -> Result<SwapState, StateError> {
        if observed.is_terminal() {
            Ok(observed)
        } else {
            Err(StateError::NotTerminal(observed))
        }
    }

    /// Apply `event`, returning the next state or explaining the refusal.
    pub fn on(self, event: Event) -> Result<SwapState, StateError> {
        use Event::*;
        use SwapState::*;

        if self.is_terminal() {
            return Err(StateError::Finished(self));
        }

        match (self, event) {
            (Created, Fund) => Ok(InitiatorFunded),
            (InitiatorFunded, Lock) => Ok(CounterpartyLocked),
            (CounterpartyLocked, Settle) => Ok(Settled),
            (InitiatorFunded, Refund) | (CounterpartyLocked, Refund) => Ok(Refunded),

            (Created, Lock) => Err(StateError::NotFundedYet),
            (Created, Settle) | (InitiatorFunded, Settle) => Err(StateError::NotLockedYet),
            (Created, Refund) => Err(StateError::NothingToRefund),

            (InitiatorFunded, Fund) | (CounterpartyLocked, Fund) | (CounterpartyLocked, Lock) => {
                Err(StateError::Illegal { state: self, event })
            }

            // Unreachable: guarded by the terminal check above, but the match
            // still has to be total.
            (Settled, _) | (Refunded, _) => Err(StateError::Finished(self)),
        }
    }
}

/// Names match the serde representation, so a state can travel between a
/// record on disk, a log line and a status output without being reinterpreted.
impl std::fmt::Display for SwapState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            SwapState::Created => "created",
            SwapState::InitiatorFunded => "initiator_funded",
            SwapState::CounterpartyLocked => "counterparty_locked",
            SwapState::Settled => "settled",
            SwapState::Refunded => "refunded",
        };
        f.write_str(name)
    }
}

/// A swap plus its current state, with no way to set the state directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Swap {
    state: SwapState,
}

impl Default for Swap {
    fn default() -> Self {
        Self {
            state: SwapState::Created,
        }
    }
}

impl Swap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> SwapState {
        self.state
    }

    /// Advance the swap, or leave it untouched on an illegal event.
    pub fn apply(&mut self, event: Event) -> Result<SwapState, StateError> {
        self.state = self.state.on(event)?;
        Ok(self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Event::*;
    use SwapState::*;

    #[test]
    fn happy_path() {
        let mut swap = Swap::new();
        assert_eq!(swap.state(), Created);
        assert_eq!(swap.apply(Fund).unwrap(), InitiatorFunded);
        assert_eq!(swap.apply(Lock).unwrap(), CounterpartyLocked);
        assert_eq!(swap.apply(Settle).unwrap(), Settled);
        assert!(swap.state().is_terminal());
    }

    #[test]
    fn refund_from_initiator_funded() {
        let mut swap = Swap::new();
        swap.apply(Fund).unwrap();
        assert_eq!(swap.apply(Refund).unwrap(), Refunded);
    }

    #[test]
    fn refund_after_counterparty_locked() {
        // The counterparty locked but never revealed: the initiator still exits.
        let mut swap = Swap::new();
        swap.apply(Fund).unwrap();
        swap.apply(Lock).unwrap();
        assert!(swap.state().can_refund());
        assert_eq!(swap.apply(Refund).unwrap(), Refunded);
    }

    #[test]
    fn cannot_settle_before_both_legs_are_locked() {
        let mut swap = Swap::new();
        assert_eq!(swap.apply(Settle), Err(StateError::NotLockedYet));
        swap.apply(Fund).unwrap();
        assert_eq!(swap.apply(Settle), Err(StateError::NotLockedYet));
        assert_eq!(
            swap.state(),
            InitiatorFunded,
            "state must not move on error"
        );
    }

    #[test]
    fn cannot_lock_counterparty_leg_before_funding() {
        let mut swap = Swap::new();
        assert_eq!(swap.apply(Lock), Err(StateError::NotFundedYet));
    }

    #[test]
    fn cannot_refund_what_was_never_funded() {
        let mut swap = Swap::new();
        assert_eq!(swap.apply(Refund), Err(StateError::NothingToRefund));
    }

    #[test]
    fn cannot_double_fund() {
        let mut swap = Swap::new();
        swap.apply(Fund).unwrap();
        assert!(matches!(
            swap.apply(Fund),
            Err(StateError::Illegal {
                state: InitiatorFunded,
                event: Fund
            })
        ));
    }

    #[test]
    fn terminal_states_absorb_nothing() {
        for finish in [Fund, Lock, Settle, Refund] {
            let mut settled = Swap::new();
            settled.apply(Fund).unwrap();
            settled.apply(Lock).unwrap();
            settled.apply(Settle).unwrap();
            assert_eq!(settled.apply(finish), Err(StateError::Finished(Settled)));

            let mut refunded = Swap::new();
            refunded.apply(Fund).unwrap();
            refunded.apply(Refund).unwrap();
            assert_eq!(refunded.apply(finish), Err(StateError::Finished(Refunded)));
        }
    }

    #[test]
    fn every_funded_state_can_refund() {
        for state in [InitiatorFunded, CounterpartyLocked] {
            assert!(state.can_refund(), "{state:?} should be refundable");
            assert_eq!(state.on(Refund).unwrap(), Refunded);
        }
        for state in [Created, Settled, Refunded] {
            assert!(!state.can_refund(), "{state:?} should not be refundable");
        }
    }

    #[test]
    fn restore_only_adopts_terminal_states() {
        // Recovery from a crash may jump straight to a finished state.
        assert_eq!(Created.restore(Settled).unwrap(), Settled);
        assert_eq!(Created.restore(Refunded).unwrap(), Refunded);
        assert_eq!(InitiatorFunded.restore(Settled).unwrap(), Settled);

        // It must not be usable to walk somewhere non-terminal by hand.
        for wanted in [Created, InitiatorFunded, CounterpartyLocked] {
            assert_eq!(
                Created.restore(wanted),
                Err(StateError::NotTerminal(wanted))
            );
        }
    }
}
