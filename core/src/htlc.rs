//! The on-chain leg: a P2WSH HTLC with a hashlock path and a timelocked refund.
//!
//! ```text
//! OP_IF
//!     OP_SHA256 <payment_hash> OP_EQUALVERIFY <claimant> OP_CHECKSIG
//! OP_ELSE
//!     <refund_locktime> OP_CHECKLOCKTIMEVERIFY OP_DROP <refunder> OP_CHECKSIG
//! OP_ENDIF
//! ```
//!
//! The preimage path belongs to whoever is owed value on this leg; the
//! timelocked path belongs to whoever funded it. A funder can always exit, but
//! only after the timeout - that asymmetry is what makes the swap atomic.

use bitcoin::{
    absolute::LockTime, opcodes::all as op, script::Builder, Address, Network, PublicKey,
    ScriptBuf, Witness,
};

use crate::hash::{PaymentHash, Preimage};

/// Error constructing an [`Htlc`].
#[derive(Debug, thiserror::Error)]
pub enum HtlcError {
    #[error("claimant and refunder keys must differ")]
    DuplicateKeys,
    #[error("refund locktime must be a block height, not a time")]
    NonHeightLocktime,
    #[error("refund locktime must not be zero")]
    ZeroLocktime,
}

/// Parameters of an on-chain HTLC leg.
#[derive(Clone, PartialEq, Eq)]
pub struct Htlc {
    /// Hash unlocked by the preimage on the claim path.
    pub payment_hash: PaymentHash,
    /// Key allowed to claim with the preimage.
    pub claimant: PublicKey,
    /// Key allowed to refund after `refund_locktime`.
    pub refunder: PublicKey,
    /// Block height at which the refund path opens.
    pub refund_locktime: LockTime,
}

impl Htlc {
    pub fn new(
        payment_hash: PaymentHash,
        claimant: PublicKey,
        refunder: PublicKey,
        refund_locktime: LockTime,
    ) -> Result<Self, HtlcError> {
        if claimant == refunder {
            return Err(HtlcError::DuplicateKeys);
        }
        if refund_locktime.is_block_time() {
            return Err(HtlcError::NonHeightLocktime);
        }
        if refund_locktime == LockTime::ZERO {
            return Err(HtlcError::ZeroLocktime);
        }
        Ok(Self {
            payment_hash,
            claimant,
            refunder,
            refund_locktime,
        })
    }

    /// Convenience constructor from a block height.
    pub fn from_height(
        payment_hash: PaymentHash,
        claimant: PublicKey,
        refunder: PublicKey,
        height: u32,
    ) -> Result<Self, HtlcError> {
        let locktime = LockTime::from_height(height).map_err(|_| HtlcError::NonHeightLocktime)?;
        Self::new(payment_hash, claimant, refunder, locktime)
    }

    /// The witness script (the redeem script) for this HTLC.
    pub fn script(&self) -> ScriptBuf {
        Builder::new()
            .push_opcode(op::OP_IF)
            .push_opcode(op::OP_SHA256)
            .push_slice(self.payment_hash.as_bytes())
            .push_opcode(op::OP_EQUALVERIFY)
            .push_key(&self.claimant)
            .push_opcode(op::OP_CHECKSIG)
            .push_opcode(op::OP_ELSE)
            .push_int(self.locktime_height() as i64)
            .push_opcode(op::OP_CLTV)
            .push_opcode(op::OP_DROP)
            .push_key(&self.refunder)
            .push_opcode(op::OP_CHECKSIG)
            .push_opcode(op::OP_ENDIF)
            .into_script()
    }

    /// The P2WSH address the funder pays into.
    pub fn address(&self, network: Network) -> Address {
        Address::p2wsh(&self.script(), network)
    }

    /// The scriptPubKey locking the funding output.
    pub fn script_pubkey(&self, network: Network) -> ScriptBuf {
        self.address(network).script_pubkey()
    }

    /// Refund height as a `u32`.
    pub fn locktime_height(&self) -> u32 {
        self.refund_locktime.to_consensus_u32()
    }

    /// Witness for the preimage path: `<sig> <preimage> <1> <script>`.
    pub fn claim_witness(&self, signature: &[u8], preimage: &Preimage) -> Witness {
        Witness::from_slice(&[
            signature.to_vec(),
            preimage.as_bytes().to_vec(),
            vec![1u8],
            self.script().into_bytes(),
        ])
    }

    /// Witness for the timelocked path: `<sig> <> <script>`.
    pub fn refund_witness(&self, signature: &[u8]) -> Witness {
        Witness::from_slice(&[signature.to_vec(), Vec::new(), self.script().into_bytes()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bitcoin::Network;

    fn pubkey(seed: u8) -> PublicKey {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[seed; 32]).unwrap();
        PublicKey::new(sk.public_key(&secp))
    }

    fn sample(height: u32) -> Htlc {
        let preimage = Preimage::from_bytes([7u8; 32]);
        Htlc::from_height(preimage.payment_hash(), pubkey(1), pubkey(2), height).unwrap()
    }

    #[test]
    fn script_has_expected_shape() {
        let htlc = sample(800_000);
        let script = htlc.script();
        let asm = script.to_asm_string();

        assert!(asm.starts_with("OP_IF OP_SHA256"));
        assert!(asm.contains("OP_EQUALVERIFY"));
        assert!(asm.contains("OP_CHECKSIG"));
        assert!(asm.contains("OP_ELSE"));
        // rust-bitcoin renders the opcode by its modern name.
        assert!(asm.contains("OP_CLTV"), "asm was: {asm}");
        assert!(asm.ends_with("OP_ENDIF"));
    }

    #[test]
    fn script_embeds_the_payment_hash_bytes() {
        let htlc = sample(800_000);
        let script = htlc.script();
        let bytes = script.as_bytes();
        let needle = htlc.payment_hash.as_bytes();
        assert!(
            bytes.windows(32).any(|w| w == needle),
            "payment hash not present in script"
        );
    }

    #[test]
    fn address_is_p2wsh_and_stable() {
        let htlc = sample(800_000);
        let addr = htlc.address(Network::Regtest);
        assert_eq!(addr.address_type(), Some(bitcoin::AddressType::P2wsh));
        assert_eq!(addr, htlc.address(Network::Regtest));
        assert_eq!(addr.script_pubkey(), htlc.script_pubkey(Network::Regtest));
    }

    #[test]
    fn distinct_hashes_give_distinct_addresses() {
        let a = sample(800_000);
        let mut b = sample(800_000);
        b.payment_hash = Preimage::from_bytes([9u8; 32]).payment_hash();
        assert_ne!(a.address(Network::Regtest), b.address(Network::Regtest));
    }

    #[test]
    fn witness_layout_is_correct() {
        let htlc = sample(800_000);
        let preimage = Preimage::from_bytes([7u8; 32]);

        let claim = htlc.claim_witness(&[0xaa, 0xbb], &preimage);
        assert_eq!(claim.len(), 4);
        assert_eq!(claim[1], *preimage.as_bytes());
        assert_eq!(claim[2], vec![1u8]);
        assert_eq!(&claim[3], htlc.script().as_bytes());

        let refund = htlc.refund_witness(&[0xaa, 0xbb]);
        assert_eq!(refund.len(), 3);
        assert!(refund[1].is_empty());
    }

    #[test]
    fn rejects_bad_parameters() {
        let preimage = Preimage::from_bytes([7u8; 32]);
        assert!(matches!(
            Htlc::from_height(preimage.payment_hash(), pubkey(1), pubkey(1), 100),
            Err(HtlcError::DuplicateKeys)
        ));
        assert!(matches!(
            Htlc::from_height(preimage.payment_hash(), pubkey(1), pubkey(2), 0),
            Err(HtlcError::ZeroLocktime)
        ));
        assert!(matches!(
            Htlc::new(
                preimage.payment_hash(),
                pubkey(1),
                pubkey(2),
                LockTime::from_time(1_700_000_000).unwrap()
            ),
            Err(HtlcError::NonHeightLocktime)
        ));
    }
}
