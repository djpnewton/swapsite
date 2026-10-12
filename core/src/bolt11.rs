//! Just enough of BOLT-11 to read a taker's invoice.
//!
//! The taker has to build the deposit HTLC itself, so it has to know the hash
//! and the CLTV delta its own invoice commits to. WebLN hands back a bolt11
//! string and nothing else, and taking a server's word for the hash would let a
//! maker build the HTLC against a hash that invoice does not settle: it could
//! then claim the deposit without ever paying. So the invoice is read here.
//!
//! bech32 validation is the `bech32` crate's; the tagged fields are read here,
//! because the crate's byte form has lost the five bit grouping that the tags
//! are cut along.

use bitcoin::bech32;
use bitcoin::Network;

use crate::hash::PaymentHash;

/// The bech32 alphabet: each character stands for a five bit value, in order.
const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// The CLTV delta BOLT-11 assumes when an invoice does not state one.
const DEFAULT_MIN_FINAL_CLTV_EXPIRY: u32 = 18;

/// The tag that carries the payment hash.
const TAG_PAYMENT_HASH: u8 = 1;

/// The tag that carries the minimum final CLTV delta.
const TAG_MIN_FINAL_CLTV_EXPIRY: u8 = 24;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Bolt11Error {
    #[error("the invoice is not valid bech32: {0}")]
    Bech32(String),
    #[error("the invoice is for no network this knows: {0}")]
    Network(String),
    #[error("the invoice names no payment hash")]
    NoPaymentHash,
    #[error("the invoice's amount is not a whole number of satoshis")]
    Amount,
    #[error("the invoice's {0} field is malformed")]
    Field(&'static str),
}

/// What a bolt11 invoice commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bolt11 {
    /// The network its prefix names.
    pub network: Network,
    /// What it pays, when it names an amount at all.
    pub amount_sat: Option<u64>,
    /// The hash its payment reveals a preimage for.
    pub payment_hash: PaymentHash,
    /// Blocks a payment for it must be given after the height it is made.
    pub min_final_cltv_expiry: u32,
}

/// Read an invoice.
pub fn decode(text: &str) -> Result<Bolt11, Bolt11Error> {
    // Validating through the crate checks the checksum and the character set, so
    // anything that gets past here is intact.
    let (hrp, _) = bech32::decode(text).map_err(|e| Bolt11Error::Bech32(e.to_string()))?;
    let (network, amount_sat) = prefix(hrp.as_str())?;
    let groups = data_groups(text)?;

    let mut payment_hash = None;
    let mut min_final_cltv_expiry = DEFAULT_MIN_FINAL_CLTV_EXPIRY;

    // Seven groups of timestamp, then tagged fields until the data runs out.
    let mut at = 7;
    while at + 3 <= groups.len() {
        let tag = groups[at];
        let length = ((groups[at + 1] as usize) << 5) | groups[at + 2] as usize;
        let start = at + 3;
        let end = start + length;
        if end > groups.len() {
            break;
        }
        match tag {
            TAG_PAYMENT_HASH => payment_hash = Some(hash_of(&groups[start..end])?),
            TAG_MIN_FINAL_CLTV_EXPIRY => min_final_cltv_expiry = number_of(&groups[start..end])?,
            _ => {}
        }
        at = end;
    }

    Ok(Bolt11 {
        network,
        amount_sat,
        payment_hash: payment_hash.ok_or(Bolt11Error::NoPaymentHash)?,
        min_final_cltv_expiry,
    })
}

/// The network and the amount an invoice's prefix names.
fn prefix(hrp: &str) -> Result<(Network, Option<u64>), Bolt11Error> {
    let rest = hrp
        .strip_prefix("ln")
        .ok_or_else(|| Bolt11Error::Network(hrp.to_owned()))?;

    // Longest first: `bc` is a prefix of `bcrt`.
    let (network, rest) = if let Some(rest) = rest.strip_prefix("bcrt") {
        (Network::Regtest, rest)
    } else if let Some(rest) = rest.strip_prefix("bc") {
        (Network::Bitcoin, rest)
    } else if let Some(rest) = rest.strip_prefix("tb") {
        (Network::Testnet, rest)
    } else if let Some(rest) = rest.strip_prefix("sb") {
        (Network::Signet, rest)
    } else {
        return Err(Bolt11Error::Network(hrp.to_owned()));
    };

    Ok((network, amount_of(rest)?))
}

/// Satoshis named by an amount: digits and an optional multiplier.
///
/// A multiplier divides a bitcoin, so a milli- is 100,000 satoshi, a micro- is
/// 100, and a nano- is a tenth of one. Anything finer than a satoshi is refused
/// rather than rounded, since a swap is not on offer for nothing.
fn amount_of(text: &str) -> Result<Option<u64>, Bolt11Error> {
    if text.is_empty() {
        return Ok(None);
    }
    let (digits, multiplier) = match text.chars().last() {
        Some(c @ ('m' | 'u' | 'n' | 'p')) => (&text[..text.len() - 1], Some(c)),
        _ => (text, None),
    };

    let value: u64 = digits.parse().map_err(|_| Bolt11Error::Amount)?;
    let sat = match multiplier {
        None => value.checked_mul(100_000_000),
        Some('m') => value.checked_mul(100_000),
        Some('u') => value.checked_mul(100),
        Some('n') => Some(value / 10),
        Some('p') => Some(value / 10_000),
        Some(_) => None,
    };

    match sat {
        Some(0) | None => Err(Bolt11Error::Amount),
        Some(sat) => Ok(Some(sat)),
    }
}

/// The five bit groups of the data part.
///
/// The separator is the last `1`, which cannot appear in the data: the alphabet
/// has no `1`. The six characters after it are the checksum.
fn data_groups(text: &str) -> Result<Vec<u8>, Bolt11Error> {
    let (_, data) = text
        .rsplit_once('1')
        .ok_or_else(|| Bolt11Error::Bech32("no separator".to_owned()))?;
    let body = data.get(..data.len().saturating_sub(6)).unwrap_or_default();

    body.bytes()
        .map(|byte| {
            CHARSET
                .iter()
                .position(|&c| c == byte)
                .map(|value| value as u8)
                .ok_or_else(|| Bolt11Error::Bech32(format!("bad character `{}`", byte as char)))
        })
        .collect()
}

/// A payment hash: 52 groups holding 32 bytes and four bits of padding.
fn hash_of(groups: &[u8]) -> Result<PaymentHash, Bolt11Error> {
    if groups.len() != 52 {
        return Err(Bolt11Error::Field("p"));
    }

    let mut bytes = [0u8; 32];
    let mut bit = 0;
    for group in groups {
        for shift in (0..5).rev() {
            if bit < 256 {
                bytes[bit / 8] |= ((group >> shift) & 1) << (7 - bit % 8);
            }
            bit += 1;
        }
    }

    Ok(PaymentHash::from_bytes(bytes))
}

/// A big-endian number packed into five bit groups.
fn number_of(groups: &[u8]) -> Result<u32, Bolt11Error> {
    let mut value: u64 = 0;
    for group in groups {
        value = value
            .checked_shl(5)
            .and_then(|value| value.checked_add(*group as u64))
            .ok_or(Bolt11Error::Field("c"))?;
    }
    value.try_into().map_err(|_| Bolt11Error::Field("c"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real invoice from the regtest taker, made with `--preimage` so the
    /// hash is known. Its own node decodes it as hash `fce3...dc00`, 1000
    /// satoshi and a CLTV delta of 80, which is what the assertions below hold
    /// it to: a hand-rolled reader is only worth anything if it agrees with the
    /// implementation everyone else uses.
    const FIXTURE: &str = concat!(
        "lnbcrt10u1p4v6dqapp5ln355zugu322dtrf5n7y8r324d9hx3",
        "5m3qdgt2x83vsgpfpvmsqqdqhvfhkcap3xysxv6tcw36hyegcq",
        "zzsxqyz5vqsp5la9dwdnyqlszahr2xntjhf5f4p756l9mxpr6y",
        "mct9ss5vgzr3t9s9p4gqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
        "qqqqqqqqqqqqqqqpqysgqyeqvtyjqks2tta4067rf67vcg6jkw",
        "kwkvv4xvn4576yszj3pj0phgrrsljdc2lrg9d0vynnpv72y7sf",
        "pptr4fv93xyn6mqn56p4qwmgpgk24qm",
    );

    #[test]
    fn reads_what_the_node_says_the_fixture_says() {
        let invoice = decode(FIXTURE).unwrap();

        assert_eq!(invoice.network, Network::Regtest);
        assert_eq!(invoice.amount_sat, Some(1_000), "10 micro-bitcoin");
        assert_eq!(
            invoice.payment_hash.to_hex(),
            "fce34a0b88e454a6ac69a4fc438e2aab4b73469b881a85a8c78b2080a42cdc00"
        );
        assert_eq!(invoice.min_final_cltv_expiry, 80);
    }

    #[test]
    fn refuses_an_invoice_whose_checksum_does_not_add_up() {
        let mut broken = FIXTURE.to_owned();
        broken.pop();
        broken.push('q');

        assert!(matches!(decode(&broken), Err(Bolt11Error::Bech32(_))));
    }

    #[test]
    fn refuses_a_string_that_is_not_an_invoice_at_all() {
        assert!(decode("lnbcrt10u1notaninvoice").is_err());
        assert!(decode("hello").is_err());
    }

    #[test]
    fn reads_the_amount_multipliers() {
        assert_eq!(amount_of(""), Ok(None));
        assert_eq!(amount_of("1"), Ok(Some(100_000_000)));
        assert_eq!(amount_of("10u"), Ok(Some(1_000)));
        assert_eq!(amount_of("25m"), Ok(Some(2_500_000)));
        assert_eq!(amount_of("250n"), Ok(Some(25)));
        // Finer than a satoshi is refused rather than rounded down to nothing.
        assert_eq!(amount_of("5p"), Err(Bolt11Error::Amount));
        assert_eq!(amount_of("x"), Err(Bolt11Error::Amount));
    }

    #[test]
    fn reads_the_networks_prefixes() {
        // Longest first: `bcrt` must not be read as `bc` with junk after it.
        assert_eq!(prefix("lnbcrt10u").unwrap().0, Network::Regtest);
        assert_eq!(prefix("lnbc1u").unwrap().0, Network::Bitcoin);
        assert_eq!(prefix("lntb1u").unwrap().0, Network::Testnet);
        assert_eq!(prefix("lnsb1u").unwrap().0, Network::Signet);
        assert_eq!(prefix("lnbc1u").unwrap().1, Some(100));
        assert!(prefix("lnxx1u").is_err());
    }
}
