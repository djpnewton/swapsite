//! Building and signing spends of an on-chain HTLC leg.

use bitcoin::{
    absolute::LockTime,
    ecdsa::Signature as EcdsaSignature,
    hashes::Hash,
    secp256k1::{Message, Secp256k1, SecretKey},
    sighash::{EcdsaSighashType, SighashCache},
    transaction::Version,
    Address, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};

use crate::{hash::Preimage, htlc::Htlc};

/// Sequence that leaves locktime enabled for the refund path.
const REFUND_SEQUENCE: u32 = 0xFFFF_FFFE;

#[derive(Debug, thiserror::Error)]
pub enum SpendError {
    #[error("value {value} sat does not cover the {fee} sat fee")]
    FeeExceedsValue { value: u64, fee: u64 },

    #[error("could not compute sighash: {0}")]
    Sighash(String),

    #[error("claim path needs a preimage")]
    MissingPreimage,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    Claim,
    Refund,
}

/// Spend the HTLC with the preimage.
///
/// `key` must be the claimant key from the HTLC, `preimage` must hash to the
/// HTLC's payment hash.
pub fn claim(
    htlc: &Htlc,
    outpoint: OutPoint,
    value: Amount,
    to: &Address,
    fee: Amount,
    key: &SecretKey,
    preimage: &Preimage,
) -> Result<Transaction, SpendError> {
    build(
        htlc,
        outpoint,
        value,
        to,
        fee,
        Path::Claim,
        key,
        Some(preimage),
    )
}

/// Spend the HTLC through the timelocked path.
///
/// Only valid from [`Htlc::locktime_height`] onwards. `key` must be the
/// refunder key from the HTLC.
pub fn refund(
    htlc: &Htlc,
    outpoint: OutPoint,
    value: Amount,
    to: &Address,
    fee: Amount,
    key: &SecretKey,
) -> Result<Transaction, SpendError> {
    build(htlc, outpoint, value, to, fee, Path::Refund, key, None)
}

#[allow(clippy::too_many_arguments)]
fn build(
    htlc: &Htlc,
    outpoint: OutPoint,
    value: Amount,
    to: &Address,
    fee: Amount,
    path: Path,
    key: &SecretKey,
    preimage: Option<&Preimage>,
) -> Result<Transaction, SpendError> {
    let spendable = value.checked_sub(fee).ok_or(SpendError::FeeExceedsValue {
        value: value.to_sat(),
        fee: fee.to_sat(),
    })?;

    // The refund path must carry a locktime at or after the HTLC timeout, with
    // a sequence that does not disable it.
    let (sequence, lock_time) = match path {
        Path::Claim => (Sequence::MAX, LockTime::ZERO),
        Path::Refund => (
            Sequence::from_consensus(REFUND_SEQUENCE),
            htlc.refund_locktime,
        ),
    };

    let mut tx = Transaction {
        version: Version::TWO,
        lock_time,
        input: vec![TxIn {
            previous_output: outpoint,
            script_sig: ScriptBuf::new(),
            sequence,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: spendable,
            script_pubkey: to.script_pubkey(),
        }],
    };

    let script = htlc.script();
    let mut cache = SighashCache::new(&tx);
    let sighash = cache
        .p2wsh_signature_hash(0, &script, value, EcdsaSighashType::All)
        .map_err(|e| SpendError::Sighash(e.to_string()))?;

    let secp = Secp256k1::new();
    let msg = Message::from_digest(sighash.to_byte_array());
    let mut signature = secp.sign_ecdsa(&msg, key);
    // Low-S, as standardness rules require.
    signature.normalize_s();

    let sig_bytes = EcdsaSignature {
        signature,
        sighash_type: EcdsaSighashType::All,
    }
    .serialize();

    tx.input[0].witness = match path {
        Path::Claim => htlc.claim_witness(&sig_bytes, preimage.ok_or(SpendError::MissingPreimage)?),
        Path::Refund => htlc.refund_witness(&sig_bytes),
    };

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Preimage;
    use bitcoin::{
        secp256k1::{Secp256k1, SecretKey},
        Address, Network, PublicKey, ScriptBuf, Txid,
    };
    use bitcoinconsensus::Error as ConsensusError;

    const VALUE: u64 = 100_000;
    const FEE: u64 = 500;

    struct Fixture {
        htlc: Htlc,
        claimant: SecretKey,
        refunder: SecretKey,
        preimage: Preimage,
        outpoint: OutPoint,
        destination: Address,
        network: Network,
    }

    fn key(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).unwrap()
    }

    fn fixture(height: u32) -> Fixture {
        let secp = Secp256k1::new();
        let claimant = key(1);
        let refunder = key(2);
        let preimage = Preimage::from_bytes([42u8; 32]);

        let htlc = Htlc::from_height(
            preimage.payment_hash(),
            PublicKey::new(claimant.public_key(&secp)),
            PublicKey::new(refunder.public_key(&secp)),
            height,
        )
        .unwrap();

        Fixture {
            htlc,
            claimant,
            refunder,
            preimage,
            outpoint: OutPoint {
                txid: Txid::from_byte_array([9u8; 32]),
                vout: 0,
            },
            // Any valid scriptPubKey will do: the destination is never spent.
            destination: Address::p2wsh(&ScriptBuf::new(), Network::Regtest),
            network: Network::Regtest,
        }
    }

    /// Run the real script interpreter over a spend of the HTLC output.
    fn verify_spend(htlc: &Htlc, tx: &Transaction, network: Network) -> Result<(), ConsensusError> {
        let spk = htlc.script_pubkey(network);
        bitcoinconsensus::verify(
            spk.as_bytes(),
            VALUE,
            &bitcoin::consensus::serialize(tx),
            None,
            0,
        )
    }

    #[test]
    fn claim_spend_passes_script_verification() {
        let f = fixture(800_000);
        let tx = claim(
            &f.htlc,
            f.outpoint,
            Amount::from_sat(VALUE),
            &f.destination,
            Amount::from_sat(FEE),
            &f.claimant,
            &f.preimage,
        )
        .unwrap();

        assert_eq!(tx.output[0].value.to_sat(), VALUE - FEE);
        assert_eq!(tx.input[0].witness.len(), 4);
        assert_eq!(verify_spend(&f.htlc, &tx, f.network), Ok(()));
    }

    #[test]
    fn refund_spend_passes_script_verification() {
        let f = fixture(800_000);
        let tx = refund(
            &f.htlc,
            f.outpoint,
            Amount::from_sat(VALUE),
            &f.destination,
            Amount::from_sat(FEE),
            &f.refunder,
        )
        .unwrap();

        assert_eq!(tx.lock_time, f.htlc.refund_locktime);
        assert_eq!(
            tx.input[0].sequence,
            Sequence::from_consensus(REFUND_SEQUENCE)
        );
        assert_eq!(tx.input[0].witness.len(), 3);
        assert_eq!(verify_spend(&f.htlc, &tx, f.network), Ok(()));
    }

    #[test]
    fn claim_with_the_wrong_preimage_fails_verification() {
        let f = fixture(800_000);
        let wrong = Preimage::from_bytes([43u8; 32]);
        let tx = claim(
            &f.htlc,
            f.outpoint,
            Amount::from_sat(VALUE),
            &f.destination,
            Amount::from_sat(FEE),
            &f.claimant,
            &wrong,
        )
        .unwrap();

        assert_eq!(
            verify_spend(&f.htlc, &tx, f.network),
            Err(ConsensusError::ERR_SCRIPT)
        );
    }

    #[test]
    fn claim_with_the_wrong_key_fails_verification() {
        let f = fixture(800_000);
        let tx = claim(
            &f.htlc,
            f.outpoint,
            Amount::from_sat(VALUE),
            &f.destination,
            Amount::from_sat(FEE),
            &f.refunder,
            &f.preimage,
        )
        .unwrap();

        assert_eq!(
            verify_spend(&f.htlc, &tx, f.network),
            Err(ConsensusError::ERR_SCRIPT)
        );
    }

    #[test]
    fn fee_larger_than_value_is_rejected() {
        let f = fixture(800_000);
        let err = claim(
            &f.htlc,
            f.outpoint,
            Amount::from_sat(FEE),
            &f.destination,
            Amount::from_sat(VALUE),
            &f.claimant,
            &f.preimage,
        )
        .unwrap_err();
        assert!(matches!(err, SpendError::FeeExceedsValue { .. }));
    }
}
