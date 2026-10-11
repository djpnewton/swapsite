//! The maker side of a reverse submarine swap.
//!
//! The maker deposits on chain first, then waits for the taker's Lightning
//! payment to be held. If the taker pays, the maker settles and so reveals the
//! preimage. If the taker never pays, the maker takes its deposit back after the
//! timeout. Both paths are driven from observed reality, never from what the
//! maker would like to be true.

use std::str::FromStr;

use bitcoin::{secp256k1::SecretKey, OutPoint, PublicKey, Txid};
use serde::{Deserialize, Serialize};

use crate::hash::{PaymentHash, Preimage};
use crate::htlc::{Htlc, HtlcError};
use crate::lightning::InvoiceState;
use crate::state::{Event, StateError, SwapState};

/// Everything the maker must remember about one swap.
///
/// Holds the preimage and the refund secret, so this record is a hot key. On
/// regtest that is fine; a real maker would keep these in a signer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapRecord {
    pub id: String,
    pub payment_hash: PaymentHash,
    pub preimage: Preimage,
    /// What the taker receives on chain.
    pub amount_sat: u64,
    /// What the maker charges for the swap. The taker pays this on top, so the
    /// Lightning invoice is for `amount_sat + fee_sat` and the taker still nets
    /// exactly `amount_sat` on chain.
    pub fee_sat: u64,
    /// What the maker deposits on chain: `amount_sat` plus `claim_fee_sat`.
    pub deposit_sat: u64,
    pub claim_fee_sat: u64,
    pub refund_fee_sat: u64,
    pub claim_pubkey: String,
    pub refund_secret: String,
    pub refund_pubkey: String,
    pub locktime: u32,
    pub htlc_address: String,
    pub bolt11: String,
    /// Signed deposit transaction, hex.
    ///
    /// Persisted before it is broadcast, so a crash can always be recovered
    /// from by rebroadcasting.
    pub funding_tx: Option<String>,
    pub funding: Option<Funding>,
    /// Signed refund transaction, hex, persisted for the same reason.
    pub refund_tx: Option<String>,
    pub state: SwapState,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Funding {
    pub txid: String,
    pub vout: u32,
    pub sats: u64,
}

/// What the world looks like right now, as far as this swap is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Observed {
    pub height: u32,
    pub funding_confirmations: u32,
    pub refund_confirmations: u32,
    /// Whether the deposit output is gone, whoever spent it.
    ///
    /// The record only ever holds the refund this daemon built, so a refund
    /// from anywhere else is invisible without this.
    pub deposit_spent: bool,
    pub invoice: InvoiceState,
}

/// The next thing to do about one swap.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Nothing useful to do yet.
    Wait(&'static str),
    /// Finished, one way or the other.
    Done,
    /// The deposit transaction still is not in a block: send it again.
    RebroadcastFunding,
    /// The taker paid and the deposit is safe: settle, revealing the preimage.
    Settle,
    /// The timeout passed with nothing settled: get the deposit back.
    Refund,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("bad `{field}` in swap record: {reason}")]
    Field { field: &'static str, reason: String },
    #[error("swap has no funding outpoint yet")]
    NotFunded,
    #[error(transparent)]
    Htlc(#[from] HtlcError),
    #[error(transparent)]
    State(#[from] StateError),
}

/// The shareable description of a swap.
///
/// Everything a taker or a coordinator needs, and nothing else. The preimage
/// and the refund secret are the maker's alone and must never appear here.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PublicSwap {
    pub id: String,
    pub state: SwapState,
    /// What the taker receives on chain.
    pub amount_sat: u64,
    /// The maker's fee, paid by the taker on top.
    pub fee_sat: u64,
    /// What the taker pays over Lightning.
    pub invoice_sat: u64,
    pub claim_fee_sat: u64,
    pub deposit_sat: u64,
    pub locktime: u32,
    pub payment_hash: PaymentHash,
    pub bolt11: String,
    pub claim_pubkey: String,
    pub refund_pubkey: String,
    pub htlc_address: String,
    pub deposit_txid: Option<String>,
    pub deposit_vout: Option<u32>,
    pub refund_txid: Option<String>,
}

impl SwapRecord {
    /// What the maker may tell anyone about this swap.
    pub fn public(&self) -> Result<PublicSwap, RecordError> {
        Ok(PublicSwap {
            id: self.id.clone(),
            state: self.state,
            amount_sat: self.amount_sat,
            fee_sat: self.fee_sat,
            invoice_sat: self.invoice_sat(),
            claim_fee_sat: self.claim_fee_sat,
            deposit_sat: self.deposit_sat,
            locktime: self.locktime,
            payment_hash: self.payment_hash,
            bolt11: self.bolt11.clone(),
            claim_pubkey: self.claim_pubkey.clone(),
            refund_pubkey: self.refund_pubkey.clone(),
            htlc_address: self.htlc_address.clone(),
            deposit_txid: self.funding.as_ref().map(|funding| funding.txid.clone()),
            deposit_vout: self.funding.as_ref().map(|funding| funding.vout),
            refund_txid: self.refund_txid()?.map(|txid| txid.to_string()),
        })
    }

    /// What the taker pays over Lightning.
    pub fn invoice_sat(&self) -> u64 {
        self.amount_sat + self.fee_sat
    }

    fn field<T: FromStr>(&self, name: &'static str, value: &str) -> Result<T, RecordError>
    where
        T::Err: std::fmt::Display,
    {
        T::from_str(value).map_err(|e| RecordError::Field {
            field: name,
            reason: e.to_string(),
        })
    }

    pub fn claim_pubkey(&self) -> Result<PublicKey, RecordError> {
        self.field("claim_pubkey", &self.claim_pubkey)
    }

    pub fn refund_pubkey(&self) -> Result<PublicKey, RecordError> {
        self.field("refund_pubkey", &self.refund_pubkey)
    }

    /// The maker's refund secret. Never leaves the maker.
    pub fn refund_key(&self) -> Result<SecretKey, RecordError> {
        self.field("refund_secret", &self.refund_secret)
    }

    /// Rebuild the HTLC this swap is locked to.
    pub fn htlc(&self) -> Result<Htlc, RecordError> {
        Ok(Htlc::from_height(
            self.payment_hash,
            self.claim_pubkey()?,
            self.refund_pubkey()?,
            self.locktime,
        )?)
    }

    pub fn funding(&self) -> Result<&Funding, RecordError> {
        self.funding.as_ref().ok_or(RecordError::NotFunded)
    }

    pub fn funding_txid(&self) -> Result<Txid, RecordError> {
        let funding = self.funding()?;
        self.field("funding.txid", &funding.txid)
    }

    pub fn funding_outpoint(&self) -> Result<OutPoint, RecordError> {
        let funding = self.funding()?;
        Ok(OutPoint {
            txid: self.field("funding.txid", &funding.txid)?,
            vout: funding.vout,
        })
    }

    /// Txid of the refund, if one has been built.
    pub fn refund_txid(&self) -> Result<Option<Txid>, RecordError> {
        self.refund_tx
            .as_deref()
            .map(|hex| {
                let bytes = hex::decode(hex).map_err(|e| RecordError::Field {
                    field: "refund_tx",
                    reason: e.to_string(),
                })?;
                let tx: bitcoin::Transaction =
                    bitcoin::consensus::deserialize(&bytes).map_err(|e| RecordError::Field {
                        field: "refund_tx",
                        reason: e.to_string(),
                    })?;
                Ok(tx.compute_txid())
            })
            .transpose()
    }

    /// Move the stored state forward.
    pub fn advance(&mut self, event: Event) -> Result<SwapState, RecordError> {
        self.state = self.state.on(event)?;
        Ok(self.state)
    }

    /// Adopt a terminal state seen from outside, for crash recovery.
    pub fn restore(&mut self, observed: SwapState) -> Result<SwapState, RecordError> {
        self.state = self.state.restore(observed)?;
        Ok(self.state)
    }
}

/// Bring the stored state in line with what is actually true on both legs.
///
/// Terminal facts win over the local record: a swap that is settled or refunded
/// on the network cannot be un-finished by a stale file.
pub fn reconcile(record: &mut SwapRecord, observed: &Observed) -> Result<(), RecordError> {
    if observed.invoice == InvoiceState::Settled {
        record.restore(SwapState::Settled)?;
        return Ok(());
    }

    // Nothing settled, yet the deposit is gone. Only the refund path can have
    // spent it: a claim needs the preimage, and the preimage is only revealed by
    // settling. So a spent output with an unsettled invoice is a refund, whether
    // or not this daemon is the one that sent it.
    if observed.refund_confirmations >= 1 || observed.deposit_spent {
        record.restore(SwapState::Refunded)?;
        return Ok(());
    }

    if observed.funding_confirmations >= 1 && record.state == SwapState::Created {
        record.advance(Event::Fund)?;
    }
    if observed.invoice.is_held() && record.state == SwapState::InitiatorFunded {
        record.advance(Event::Lock)?;
    }
    Ok(())
}

/// Decide what to do about one swap.
///
/// Run [`reconcile`] first, so `record.state` already reflects reality.
pub fn plan(record: &SwapRecord, observed: &Observed) -> Action {
    // Reality first: a settled invoice ends the swap wherever we thought we were.
    if observed.invoice == InvoiceState::Settled || record.state.is_terminal() {
        return Action::Done;
    }

    let deposited = observed.funding_confirmations >= 1;

    // A deposit we have not managed to get mined yet.
    if record.funding_tx.is_some() && !deposited {
        return Action::RebroadcastFunding;
    }

    // The taker paid and the deposit is safe: settle and reveal the preimage.
    // This comes before the timeout check on purpose. Settling always beats
    // refunding, because a held payment is a promise the taker already made.
    if observed.invoice.is_held() && deposited {
        return Action::Settle;
    }

    // Nothing settled and the timeout has passed: take the deposit back.
    if deposited && observed.height >= record.locktime {
        return Action::Refund;
    }

    if !deposited {
        return Action::Wait("deposit not confirmed");
    }
    Action::Wait("waiting for the taker to pay")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{secp256k1::Secp256k1, PublicKey};

    fn record(locktime: u32) -> SwapRecord {
        let secp = Secp256k1::new();
        let mut rng = bitcoin::secp256k1::rand::thread_rng();
        let refund = SecretKey::new(&mut rng);
        let claim = SecretKey::new(&mut rng);
        let preimage = Preimage::generate();
        let claim_pubkey = PublicKey::new(claim.public_key(&secp));
        let refund_pubkey = PublicKey::new(refund.public_key(&secp));
        let htlc = Htlc::from_height(
            preimage.payment_hash(),
            claim_pubkey,
            refund_pubkey,
            locktime,
        )
        .unwrap();

        SwapRecord {
            id: "test".to_owned(),
            payment_hash: preimage.payment_hash(),
            preimage,
            amount_sat: 30_000,
            fee_sat: 30,
            deposit_sat: 32_000,
            claim_fee_sat: 2_000,
            refund_fee_sat: 2_000,
            claim_pubkey: claim_pubkey.to_string(),
            refund_secret: hex::encode(refund.secret_bytes()),
            refund_pubkey: refund_pubkey.to_string(),
            locktime,
            htlc_address: htlc.address(bitcoin::Network::Regtest).to_string(),
            bolt11: "lnbcrt1...".to_owned(),
            funding_tx: Some("00".to_owned()),
            funding: Some(Funding {
                txid: "00".repeat(32),
                vout: 0,
                sats: 32_000,
            }),
            refund_tx: None,
            state: SwapState::Created,
        }
    }

    fn observed(height: u32, funding: u32, invoice: InvoiceState) -> Observed {
        Observed {
            height,
            funding_confirmations: funding,
            refund_confirmations: 0,
            deposit_spent: false,
            invoice,
        }
    }

    #[test]
    fn the_public_view_carries_no_secrets() {
        let mut record = record(500);
        record.state = SwapState::CounterpartyLocked;
        record.refund_tx = Some(bitcoin::consensus::encode::serialize_hex(
            &bitcoin::Transaction {
                version: bitcoin::transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: vec![],
                output: vec![],
            },
        ));

        let public = record.public().unwrap();
        assert_eq!(public.id, record.id);
        assert_eq!(public.state, SwapState::CounterpartyLocked);
        assert_eq!(public.invoice_sat, record.amount_sat + record.fee_sat);
        assert_eq!(
            public.deposit_txid.as_deref(),
            Some(record.funding().unwrap().txid.as_str())
        );
        assert!(public.refund_txid.is_some());

        // The whole point of this type: it cannot leak the maker's secrets.
        let json = serde_json::to_string(&public).unwrap();
        assert!(!json.contains(&record.preimage.to_hex()), "preimage leaked");
        assert!(
            !json.contains(&record.refund_secret),
            "refund secret leaked"
        );
        assert!(!json.contains("preimage"));
        assert!(!json.contains("secret"));
    }

    #[test]
    fn the_invoice_covers_the_payout_and_the_fee() {
        let record = record(500);
        assert_eq!(record.amount_sat, 30_000);
        assert_eq!(record.fee_sat, 30);
        assert_eq!(record.invoice_sat(), 30_030);
        // The taker's claim is covered by the deposit, not by the payout.
        assert_eq!(record.deposit_sat, record.amount_sat + record.claim_fee_sat);
    }

    #[test]
    fn rebuilds_the_same_htlc_address() {
        let record = record(500);
        assert_eq!(
            record
                .htlc()
                .unwrap()
                .address(bitcoin::Network::Regtest)
                .to_string(),
            record.htlc_address
        );
        assert_eq!(record.funding().unwrap().sats, 32_000);
    }

    #[test]
    fn waits_while_the_deposit_is_unconfirmed() {
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(100, 0, InvoiceState::Open)),
            Action::RebroadcastFunding
        );
    }

    #[test]
    fn waits_for_the_taker_once_deposited() {
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(100, 1, InvoiceState::Open)),
            Action::Wait("waiting for the taker to pay")
        );
    }

    #[test]
    fn does_not_settle_before_the_deposit_is_confirmed() {
        // The taker paid, but revealing the preimage now would be unsafe.
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(100, 0, InvoiceState::Accepted)),
            Action::RebroadcastFunding
        );
    }

    #[test]
    fn settles_once_the_taker_pays() {
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(100, 1, InvoiceState::Accepted)),
            Action::Settle
        );
    }

    #[test]
    fn finishing_wins_over_everything() {
        let mut record = record(500);
        record.state = SwapState::CounterpartyLocked;
        assert_eq!(
            plan(&record, &observed(100, 1, InvoiceState::Settled)),
            Action::Done
        );

        let mut refunded = record.clone();
        refunded.state = SwapState::Refunded;
        assert_eq!(
            plan(&refunded, &observed(100, 1, InvoiceState::Open)),
            Action::Done
        );
    }

    #[test]
    fn refunds_after_the_timeout_when_the_taker_never_paid() {
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(499, 1, InvoiceState::Open)),
            Action::Wait("waiting for the taker to pay")
        );
        assert_eq!(
            plan(&record, &observed(500, 1, InvoiceState::Open)),
            Action::Refund
        );
    }

    #[test]
    fn refunds_after_the_timeout_when_the_taker_canceled() {
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(500, 1, InvoiceState::Canceled)),
            Action::Refund
        );
    }

    #[test]
    fn settling_beats_refunding_past_the_timeout() {
        // The taker paid late. Taking the deposit back now would leave the
        // taker's held payment with nothing to unlock.
        let record = record(500);
        assert_eq!(
            plan(&record, &observed(600, 1, InvoiceState::Accepted)),
            Action::Settle
        );
    }

    #[test]
    fn reconcile_advances_on_what_it_sees() {
        let mut record = record(500);
        reconcile(&mut record, &observed(100, 1, InvoiceState::Open)).unwrap();
        assert_eq!(record.state, SwapState::InitiatorFunded);

        reconcile(&mut record, &observed(100, 1, InvoiceState::Accepted)).unwrap();
        assert_eq!(record.state, SwapState::CounterpartyLocked);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let mut record = record(500);
        for _ in 0..3 {
            reconcile(&mut record, &observed(100, 1, InvoiceState::Accepted)).unwrap();
        }
        assert_eq!(record.state, SwapState::CounterpartyLocked);
    }

    #[test]
    fn reconcile_adopts_terminal_facts() {
        // A crash right after settling must not leave the record behind.
        let mut settled = record(500);
        reconcile(&mut settled, &observed(100, 1, InvoiceState::Settled)).unwrap();
        assert_eq!(settled.state, SwapState::Settled);

        let mut refunded = record(500);
        let mut seen = observed(500, 1, InvoiceState::Open);
        seen.refund_confirmations = 1;
        reconcile(&mut refunded, &seen).unwrap();
        assert_eq!(refunded.state, SwapState::Refunded);
    }

    #[test]
    fn a_refund_nobody_recorded_still_finishes_the_swap() {
        // The record only holds the refund this daemon built, so a refund from
        // anywhere else used to leave the swap unfinished for good: the deposit
        // was gone, the timeout had passed, and every tick rebuilt and
        // rebroadcast a transaction that could never be mined.
        let mut record = record(500);
        let mut seen = observed(500, 1, InvoiceState::Open);
        seen.deposit_spent = true;
        reconcile(&mut record, &seen).unwrap();
        assert_eq!(record.state, SwapState::Refunded);

        assert_eq!(plan(&record, &seen), Action::Done);
    }

    #[test]
    fn a_spent_deposit_after_settling_is_not_a_refund() {
        // Once the invoice settles, the deposit being spent is the taker
        // claiming it, which is the swap working. Reading that as a refund
        // would report the maker as having taken back a deposit it was paid for
        // - the opposite of what happened.
        let mut record = record(500);
        let mut seen = observed(500, 1, InvoiceState::Settled);
        seen.deposit_spent = true;
        reconcile(&mut record, &seen).unwrap();
        assert_eq!(record.state, SwapState::Settled);
    }
}
