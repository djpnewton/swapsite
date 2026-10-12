//! The submarine direction: a taker pays on chain and is paid over Lightning.
//!
//! The taker funds an HTLC whose hash is the payment hash of its own invoice,
//! so the deposit can only be taken by revealing the preimage over Lightning,
//! which is what settles that invoice. The maker pays second and therefore
//! carries the risk that the taker refunds the deposit after being paid; what
//! stops that is the same margin the other direction uses, applied to the
//! taker's locktime.
//!
//! There is no bond in this direction. The deposit is the collateral: a taker
//! that walks away has parked its own capital until the locktime, and a maker
//! that never pays costs the taker only its on-chain fees.

use std::str::FromStr;

use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::{Address, Network, OutPoint, PublicKey};
use serde::{Deserialize, Serialize};

use crate::hash::{PaymentHash, Preimage};
use crate::htlc::{Htlc, HtlcError};
use crate::params::{has_margin, LegTimelocks, TimelockError, MIN_MARGIN_BLOCKS};

/// Blocks the deposit waits for before the invoice is paid.
///
/// A reorg that removes the deposit after the payment would leave the maker
/// with nothing to claim, so the money is given a moment to settle.
pub const DEPOSIT_CONFIRMATIONS: u32 = 2;

/// A stored record is not usable.
#[derive(Debug, thiserror::Error)]
pub enum SubmarineError {
    #[error("{field} is not a valid {what}")]
    Malformed {
        field: &'static str,
        what: &'static str,
    },
    #[error("the taker has not reported a deposit yet")]
    NotFunded,
    #[error("the htlc was refused: {0}")]
    Htlc(#[from] HtlcError),
}

/// What the taker must deposit for an invoice, so the maker keeps its fee.
///
/// The invoice, the maker's fee, and the fee of the transaction that claims the
/// deposit: claiming pays the last one out of the deposit itself.
pub fn deposit_sat(receive_sat: u64, fee_sat: u64, claim_fee_sat: u64) -> u64 {
    receive_sat + fee_sat + claim_fee_sat
}

/// Check that a taker's locktime leaves a payment made inside the funding
/// window room to be claimed before the deposit can be refunded.
pub fn check_locktime(
    locktime: u32,
    height: u32,
    cltv_delta: u32,
    window_blocks: u32,
) -> Result<(), TimelockError> {
    LegTimelocks::from_heights(
        locktime,
        height
            .saturating_add(cltv_delta)
            .saturating_add(window_blocks),
        MIN_MARGIN_BLOCKS,
    )
    .map(|_| ())
}

/// One submarine swap, as the maker knows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmarineRecord {
    /// The invoice's payment hash, which the HTLC also pays to. The id.
    pub id: String,
    /// The taker's invoice, paid once the deposit has confirmed.
    pub invoice: String,
    /// What the invoice is for, as the taker's node reports it.
    pub receive_sat: u64,
    /// What the deposit has to be: see [`deposit_sat`].
    pub deposit_sat: u64,
    /// The maker's fee, on top of the invoice, in satoshis.
    pub fee_sat: u64,
    /// Fee the claim transaction pays out of the deposit, in satoshis.
    pub claim_fee_sat: u64,
    /// Key that may claim the deposit: the maker's, fresh per swap, hex.
    pub claim_secret: String,
    /// Key that may refund the deposit after the locktime: the taker's.
    pub refund_pubkey: String,
    /// Height from which the deposit may be refunded.
    pub locktime: u32,
    /// Blocks the invoice's payment expires over after the height it is made.
    pub cltv_delta: u32,
    /// Unix time the invoice expires, after which it cannot be paid.
    pub expires_at: u64,
    /// The deposit, once the taker has reported it and the maker has checked
    /// that it pays the HTLC the right amount.
    pub deposit: Option<OutPoint>,
    /// The preimage of the payment, once the invoice settles.
    pub preimage: Option<String>,
    /// The claim transaction, once it has been broadcast.
    pub claim_txid: Option<String>,
}

impl SubmarineRecord {
    fn malformed(field: &'static str, what: &'static str) -> SubmarineError {
        SubmarineError::Malformed { field, what }
    }

    pub fn claim_key(&self) -> Result<SecretKey, SubmarineError> {
        SecretKey::from_str(&self.claim_secret)
            .map_err(|_| Self::malformed("claim_secret", "secret key"))
    }

    pub fn claim_pubkey(&self) -> Result<PublicKey, SubmarineError> {
        Ok(PublicKey::new(
            self.claim_key()?.public_key(&Secp256k1::new()),
        ))
    }

    pub fn refund_key(&self) -> Result<PublicKey, SubmarineError> {
        PublicKey::from_str(&self.refund_pubkey)
            .map_err(|_| Self::malformed("refund_pubkey", "public key"))
    }

    pub fn payment_hash(&self) -> Result<PaymentHash, SubmarineError> {
        PaymentHash::from_hex(&self.id).map_err(|_| Self::malformed("id", "payment hash"))
    }

    /// The deposit outpoint, once it is known.
    pub fn outpoint(&self) -> Result<OutPoint, SubmarineError> {
        self.deposit.ok_or(SubmarineError::NotFunded)
    }

    pub fn preimage(&self) -> Result<Option<Preimage>, SubmarineError> {
        match &self.preimage {
            None => Ok(None),
            Some(hex) => Preimage::from_hex(hex)
                .map(Some)
                .map_err(|_| Self::malformed("preimage", "preimage")),
        }
    }

    /// The HTLC the taker has to fund: the maker claims it, the taker refunds.
    pub fn htlc(&self) -> Result<Htlc, SubmarineError> {
        Ok(Htlc::from_height(
            self.payment_hash()?,
            self.claim_pubkey()?,
            self.refund_key()?,
            self.locktime,
        )?)
    }

    pub fn address(&self, network: Network) -> Result<Address, SubmarineError> {
        Ok(self.htlc()?.address(network))
    }
}

/// Facts from outside, gathered once per tick.
#[derive(Debug, Clone, Copy)]
pub struct Observed {
    pub height: u32,
    /// Unix time now.
    pub unix_now: u64,
    /// Confirmations on the reported deposit, if one has been reported yet.
    pub confirmations: Option<u32>,
}

/// What the maker does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Wait(&'static str),
    /// Pay the invoice: the deposit is in, confirmed, and a payment made now
    /// would still expire with the margin the refund needs.
    Pay,
    /// The deposit is in but a payment now would expire too close to the
    /// refund, so the swap will not go ahead.
    TooLateToPay {
        blocks_left: u32,
    },
    /// The invoice expired before the deposit arrived.
    InvoiceExpired,
    /// Take the deposit: the preimage is known.
    Claim,
    Done,
}

impl Action {
    /// A one-line name for logs and status output.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Wait(_) => "wait",
            Action::Pay => "pay",
            Action::TooLateToPay { .. } => "too_late_to_pay",
            Action::InvoiceExpired => "invoice_expired",
            Action::Claim => "claim",
            Action::Done => "done",
        }
    }

    /// The last height at which a payment would still be safe to make.
    pub fn last_paying_height(locktime: u32, cltv_delta: u32) -> u32 {
        locktime.saturating_sub(MIN_MARGIN_BLOCKS.saturating_add(cltv_delta))
    }
}

/// Decide what to do with a submarine swap from observed reality.
pub fn plan(record: &SubmarineRecord, observed: Observed) -> Action {
    if record.claim_txid.is_some() {
        return Action::Done;
    }
    if record.preimage.is_some() {
        return Action::Claim;
    }
    let Some(confirmations) = observed.confirmations else {
        return Action::Wait("waiting for the taker to fund the deposit");
    };
    if confirmations < DEPOSIT_CONFIRMATIONS {
        return Action::Wait("waiting for the deposit to confirm");
    }
    if observed.unix_now >= record.expires_at {
        return Action::InvoiceExpired;
    }
    // A payment made at this height expires `cltv_delta` blocks later, and the
    // taker can refund from the locktime. Without the margin between them a
    // taker could take the deposit back as well as the payment.
    if !has_margin(
        record.locktime,
        observed.height.saturating_add(record.cltv_delta),
        MIN_MARGIN_BLOCKS,
    ) {
        let last = Action::last_paying_height(record.locktime, record.cltv_delta);
        return Action::TooLateToPay {
            blocks_left: last.saturating_sub(observed.height),
        };
    }
    Action::Pay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Preimage;
    use bitcoin::secp256k1::Secp256k1;

    fn key() -> SecretKey {
        let mut rng = bitcoin::secp256k1::rand::thread_rng();
        SecretKey::new(&mut rng)
    }

    /// A swap whose deposit has been reported, with a locktime `margin` blocks
    /// beyond the last height a payment would be safe at.
    fn record(margin: u32) -> SubmarineRecord {
        let secp = Secp256k1::new();
        let preimage = Preimage::generate();
        let claim = key();
        let refund = key();
        let cltv_delta = 40;
        let locktime = 1000 + MIN_MARGIN_BLOCKS + cltv_delta + margin;
        SubmarineRecord {
            id: preimage.payment_hash().to_hex(),
            invoice: "lnbcrt1invoice".to_owned(),
            receive_sat: 1_000,
            deposit_sat: deposit_sat(1_000, 10, 2_000),
            fee_sat: 10,
            claim_fee_sat: 2_000,
            claim_secret: hex::encode(claim.secret_bytes()),
            refund_pubkey: PublicKey::new(refund.public_key(&secp)).to_string(),
            locktime,
            cltv_delta,
            expires_at: 2_000_000_000,
            deposit: Some(OutPoint::from_str(&format!("{}{}:0", "00".repeat(31), "01")).unwrap()),
            preimage: None,
            claim_txid: None,
        }
    }

    fn observed(record: &SubmarineRecord, confirmations: u32) -> Observed {
        Observed {
            height: 1_000,
            unix_now: 1_700_000_000,
            confirmations: record.deposit.map(|_| confirmations),
        }
    }

    #[test]
    fn a_deposit_nobody_reported_is_waited_for() {
        assert_eq!(
            plan(
                &record(10),
                Observed {
                    height: 1_000,
                    unix_now: 1_700_000_000,
                    confirmations: None,
                }
            ),
            Action::Wait("waiting for the taker to fund the deposit")
        );
    }

    #[test]
    fn an_unconfirmed_deposit_is_not_paid_for_yet() {
        let record = record(10);
        assert_eq!(
            plan(&record, observed(&record, DEPOSIT_CONFIRMATIONS - 1)),
            Action::Wait("waiting for the deposit to confirm")
        );
        assert_eq!(
            plan(&record, observed(&record, DEPOSIT_CONFIRMATIONS)),
            Action::Pay
        );
    }

    #[test]
    fn a_payment_at_the_last_safe_height_still_leaves_the_margin() {
        let record = record(0);
        let last = Action::last_paying_height(record.locktime, record.cltv_delta);
        let mut at = observed(&record, DEPOSIT_CONFIRMATIONS);
        at.height = last;
        assert_eq!(
            plan(&record, at),
            Action::Pay,
            "the last safe height still pays"
        );

        at.height = last + 1;
        assert_eq!(
            plan(&record, at),
            Action::TooLateToPay { blocks_left: 0 },
            "one block later the payment would outlive the refund"
        );
    }

    #[test]
    fn a_payment_that_would_expire_too_close_to_the_refund_is_refused() {
        let record = record(0);
        let mut at = observed(&record, DEPOSIT_CONFIRMATIONS);
        at.height = 1_000;
        // The locktime is far off here, so the swap is still fine.
        assert_eq!(plan(&record, at), Action::Pay);

        let late = record.locktime - MIN_MARGIN_BLOCKS - record.cltv_delta + 5;
        at.height = late;
        assert_eq!(plan(&record, at), Action::TooLateToPay { blocks_left: 0 });
    }

    #[test]
    fn an_expired_invoice_is_not_paid() {
        let record = record(10);
        let mut at = observed(&record, DEPOSIT_CONFIRMATIONS);
        at.unix_now = record.expires_at;
        assert_eq!(plan(&record, at), Action::InvoiceExpired);
    }

    #[test]
    fn the_preimage_is_spent_on_claiming_the_deposit() {
        let mut record = record(10);
        record.preimage = Some(Preimage::generate().to_hex());
        assert_eq!(
            plan(&record, observed(&record, DEPOSIT_CONFIRMATIONS)),
            Action::Claim
        );

        record.claim_txid = Some("00".repeat(32));
        assert_eq!(
            plan(&record, observed(&record, DEPOSIT_CONFIRMATIONS)),
            Action::Done
        );
    }

    #[test]
    fn a_deposit_covers_the_invoice_the_fee_and_the_claim() {
        assert_eq!(deposit_sat(1_000, 10, 2_000), 3_010);
    }

    #[test]
    fn a_locktime_inside_the_funding_window_is_refused() {
        let height = 1_000;
        let cltv_delta = 40;
        let window = 30;
        // The earliest locktime that leaves a payment made in the window the
        // margin it needs.
        let earliest = height + cltv_delta + window + MIN_MARGIN_BLOCKS;
        assert!(check_locktime(earliest, height, cltv_delta, window).is_ok());
        assert!(check_locktime(earliest - 1, height, cltv_delta, window).is_err());
    }

    #[test]
    fn the_htlc_lets_the_maker_claim_and_the_taker_refund() {
        let record = record(10);
        let htlc = record.htlc().unwrap();
        assert_eq!(
            htlc.address(Network::Regtest),
            record.address(Network::Regtest).unwrap()
        );
        // The address commits to both keys, the locktime and the invoice's own
        // payment hash, so a taker that funds anything else is caught.
        assert_eq!(htlc.locktime_height(), record.locktime);
    }
}
