//! Payment hashes and preimages - the `H` shared by both legs of a swap.

use std::fmt;

use bitcoin::hashes::{sha256, Hash};
use serde::{Deserialize, Serialize};

/// Error decoding a [`PaymentHash`] or [`Preimage`] from hex.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("expected 32 bytes, got {0}")]
    Length(usize),
    #[error("bad hex: {0}")]
    Hex(#[from] hex::FromHexError),
}

/// SHA256 payment hash. Both legs of a swap are locked to the same value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaymentHash([u8; 32]);

/// The 32-byte secret whose SHA256 is a [`PaymentHash`].
///
/// Must be random and single-use. Reusing one across swaps links them and
/// breaks the timeout guarantees. Not `Copy` on purpose.
#[derive(Clone, PartialEq, Eq)]
pub struct Preimage([u8; 32]);

impl Preimage {
    /// Fresh random preimage from the OS CSPRNG.
    #[cfg(feature = "rand")]
    pub fn generate() -> Self {
        use bitcoin::secp256k1::rand::RngCore;
        let mut bytes = [0u8; 32];
        bitcoin::secp256k1::rand::thread_rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(s: &str) -> Result<Self, HashError> {
        Ok(Self(decode32(s)?))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The hash this preimage unlocks.
    pub fn payment_hash(&self) -> PaymentHash {
        let hash = sha256::Hash::hash(&self.0);
        PaymentHash(hash.to_byte_array())
    }
}

impl PaymentHash {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(s: &str) -> Result<Self, HashError> {
        Ok(Self(decode32(s)?))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Same value in the form rust-bitcoin expects for script building.
    pub fn to_sha256(self) -> sha256::Hash {
        sha256::Hash::from_byte_array(self.0)
    }

    /// True when `preimage` unlocks this hash.
    pub fn matches(&self, preimage: &Preimage) -> bool {
        sha256::Hash::hash(preimage.as_bytes()).to_byte_array() == self.0
    }
}

/// Never print a preimage, not even by accident in a log line.
impl fmt::Debug for Preimage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Preimage(<redacted>)")
    }
}

impl fmt::Debug for PaymentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PaymentHash({})", self.to_hex())
    }
}

impl fmt::Display for PaymentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn decode32(s: &str) -> Result<[u8; 32], HashError> {
    let bytes = hex::decode(s)?;
    let len = bytes.len();
    let array: [u8; 32] = bytes.try_into().map_err(|_| HashError::Length(len))?;
    Ok(array)
}

// Serialize as hex so swap state is readable across the wire and in JSON files.
impl Serialize for PaymentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for PaymentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Preimage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Preimage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_preimage_matches() {
        let p = Preimage::generate();
        let h = p.payment_hash();
        assert!(h.matches(&p));
        assert_eq!(h, p.payment_hash());
    }

    #[test]
    fn different_preimages_do_not_collide_on_match() {
        let a = Preimage::generate();
        let b = Preimage::generate();
        assert_ne!(a.payment_hash(), b.payment_hash());
        assert!(!a.payment_hash().matches(&b));
    }

    #[test]
    fn hex_round_trip() {
        let p = Preimage::generate();
        assert_eq!(Preimage::from_hex(&p.to_hex()).unwrap(), p);

        let h = p.payment_hash();
        assert_eq!(PaymentHash::from_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn rejects_bad_hex_and_wrong_length() {
        assert!(matches!(
            PaymentHash::from_hex("00"),
            Err(HashError::Length(1))
        ));
        assert!(matches!(
            PaymentHash::from_hex("zz"),
            Err(HashError::Hex(_))
        ));
    }

    #[test]
    fn debug_redacts_preimage() {
        let p = Preimage::generate();
        let rendered = format!("{p:?}");
        assert_eq!(rendered, "Preimage(<redacted>)");
        assert!(!rendered.contains(&p.to_hex()));
    }
}
