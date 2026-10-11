//! The Lightning leg of a swap.
//!
//! A Lightning leg is a BOLT11 invoice locked to the same payment hash as the
//! on-chain HTLC. The maker issues it as a **hold invoice**: the payer's funds
//! are committed but not settled until the maker chooses to settle. Settling is
//! what reveals the preimage over Lightning, and that reveal is the only thing
//! that links the two legs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::{HashError, PaymentHash, Preimage};
use crate::htlc::Htlc;

#[derive(Debug, thiserror::Error)]
pub enum LightningError {
    #[error("node command failed: {0}")]
    Command(String),
    #[error("could not parse node output: {0}")]
    Output(String),
    #[error("bad payment hash or preimage: {0}")]
    Hash(#[from] HashError),
    #[error("payment finished without revealing a preimage")]
    NoPreimage,
}

/// State of a Lightning invoice, as LND reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvoiceState {
    /// Created, nothing committed yet.
    Open,
    /// The payer's funds are held and the preimage is still hidden.
    Accepted,
    /// Settled: the preimage has been revealed to the payer.
    Settled,
    /// Canceled: the payer's funds were released, no preimage revealed.
    Canceled,
}

impl InvoiceState {
    /// The payer has committed funds, the preimage is still hidden.
    pub fn is_held(self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// No further transitions are possible.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Settled | Self::Canceled)
    }

    /// The preimage is now public, so the on-chain leg can be claimed.
    pub fn reveals_preimage(self) -> bool {
        matches!(self, Self::Settled)
    }
}

impl std::fmt::Display for InvoiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Open => "OPEN",
            Self::Accepted => "ACCEPTED",
            Self::Settled => "SETTLED",
            Self::Canceled => "CANCELED",
        };
        f.write_str(name)
    }
}

/// A Lightning invoice that can serve as one leg of a swap.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invoice {
    pub bolt11: String,
    /// Amount in satoshis, when the invoice fixes one.
    pub amount_sat: Option<u64>,
    pub state: InvoiceState,
}

impl Invoice {
    /// Parse the JSON that `lncli addholdinvoice` or `lncli lookupinvoice`
    /// prints.
    ///
    /// The two commands disagree on types: some int64 fields come back as JSON
    /// strings and others as numbers, so number handling is deliberately
    /// lenient.
    pub fn from_lnd(json: &str) -> Result<Self, LightningError> {
        let value: Value =
            serde_json::from_str(json).map_err(|e| LightningError::Output(e.to_string()))?;

        let state = match value.get("state").and_then(Value::as_str) {
            Some(name) => parse_state(name)?,
            // `addholdinvoice` does not report a state; a fresh invoice is open.
            None => InvoiceState::Open,
        };

        Ok(Self {
            bolt11: value
                .get("payment_request")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            amount_sat: value.get("value").and_then(as_sat),
            state,
        })
    }
}

/// A Lightning node the swap protocol can drive.
pub trait LightningNode {
    /// Issue an invoice that holds the payer's funds until settled.
    fn add_hold_invoice(
        &self,
        preimage: &Preimage,
        amount_sat: u64,
        memo: &str,
    ) -> Result<Invoice, LightningError>;

    fn invoice_state(&self, hash: &PaymentHash) -> Result<InvoiceState, LightningError>;

    /// Release the held funds, revealing the preimage to the payer.
    fn settle(&self, preimage: &Preimage) -> Result<(), LightningError>;

    /// Release the held funds without revealing the preimage.
    fn cancel(&self, hash: &PaymentHash) -> Result<(), LightningError>;

    /// Pay an invoice and return the preimage the payment reveals.
    fn pay(&self, bolt11: &str) -> Result<Preimage, LightningError>;

    fn info(&self) -> Result<NodeInfo, LightningError>;
}

/// Identity of a node, for liveness checks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NodeInfo {
    pub pubkey: String,
    pub alias: String,
}

impl NodeInfo {
    pub fn from_lnd(json: &str) -> Result<Self, LightningError> {
        let value: Value =
            serde_json::from_str(json).map_err(|e| LightningError::Output(e.to_string()))?;
        Ok(Self {
            pubkey: field(&value, "identity_pubkey"),
            alias: field(&value, "alias"),
        })
    }
}

/// The join between the two legs.
///
/// Nothing is negotiated here and nothing needs to be trusted: the preimage that
/// Lightning reveals has to be the one that unlocks the HTLC. That single
/// equality is the whole atomic link between an on-chain leg and a Lightning
/// leg.
pub fn check_joined(htlc: &Htlc, revealed: &Preimage) -> Result<(), JointError> {
    if htlc.payment_hash.matches(revealed) {
        Ok(())
    } else {
        Err(JointError::PreimageDoesNotUnlockHtlc {
            payment_hash: htlc.payment_hash,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JointError {
    #[error("the revealed preimage does not unlock the on-chain HTLC locked to {payment_hash}")]
    PreimageDoesNotUnlockHtlc { payment_hash: PaymentHash },
}

/// Pull the preimage out of `lncli payinvoice --json` output.
///
/// The output is a stream of concatenated, pretty printed JSON objects, so it is
/// read as a stream rather than line by line. Later objects supersede earlier
/// ones.
pub fn parse_payment_preimage(json_stream: &str) -> Result<Preimage, LightningError> {
    let mut revealed: Option<String> = None;
    let mut failure: Option<String> = None;

    let objects = serde_json::Deserializer::from_str(json_stream).into_iter::<Value>();
    for object in objects {
        let value = object.map_err(|e| LightningError::Output(e.to_string()))?;

        if let Some(preimage) = value.get("payment_preimage").and_then(Value::as_str) {
            if !preimage.is_empty() {
                revealed = Some(preimage.to_owned());
            }
        }

        if value.get("status").and_then(Value::as_str) == Some("FAILED") {
            failure = Some(field(&value, "failure_reason"));
        }
    }

    if let Some(preimage) = revealed {
        return Ok(Preimage::from_hex(&preimage)?);
    }
    if let Some(reason) = failure {
        return Err(LightningError::Command(format!("payment failed: {reason}")));
    }
    Err(LightningError::NoPreimage)
}

fn parse_state(name: &str) -> Result<InvoiceState, LightningError> {
    match name {
        "OPEN" => Ok(InvoiceState::Open),
        "ACCEPTED" => Ok(InvoiceState::Accepted),
        "SETTLED" => Ok(InvoiceState::Settled),
        "CANCELED" => Ok(InvoiceState::Canceled),
        other => Err(LightningError::Output(format!(
            "unknown invoice state `{other}`"
        ))),
    }
}

fn field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Accept an integer whether the node sent it as a number or as a string.
fn as_sat(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        secp256k1::{Secp256k1, SecretKey},
        Network, PublicKey,
    };

    // Captured from the regtest node, trimmed.
    const ADD_HOLD_INVOICE: &str = r#"{
        "payment_request": "lnbcrt10u1p4v4h66pp57nm79m9aju5xe8mnr0aw74nu0mkw9r4qqrkqs8rd8pz0qcdz58vsdqqcqzzsxqyz5vqsp5p9h22d2a",
        "add_index": "1",
        "payment_addr": "b4a0f5a1d3f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2c4d6e8f0a2b4"
    }"#;

    const LOOKUP_ACCEPTED: &str = r#"{
        "memo": "swap",
        "value": "2000",
        "settled": false,
        "amt_paid": "1000000",
        "state": "ACCEPTED",
        "htlcs": []
    }"#;

    const LOOKUP_SETTLED: &str = r#"{
        "memo": "swap",
        "value": 2000,
        "amt_paid": 1000000,
        "state": "SETTLED",
        "htlcs": [{"state": "SETTLED"}]
    }"#;

    fn htlc(preimage: &Preimage) -> Htlc {
        let secp = Secp256k1::new();
        let a = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let b = SecretKey::from_slice(&[2u8; 32]).unwrap();
        Htlc::from_height(
            preimage.payment_hash(),
            PublicKey::new(a.public_key(&secp)),
            PublicKey::new(b.public_key(&secp)),
            800_000,
        )
        .unwrap()
    }

    #[test]
    fn parses_a_fresh_hold_invoice() {
        let invoice = Invoice::from_lnd(ADD_HOLD_INVOICE).unwrap();
        assert!(invoice.bolt11.starts_with("lnbcrt"));
        assert_eq!(invoice.state, InvoiceState::Open);
        // addholdinvoice does not echo the amount back.
        assert_eq!(invoice.amount_sat, None);
    }

    #[test]
    fn parses_amount_whether_number_or_string() {
        let held = Invoice::from_lnd(LOOKUP_ACCEPTED).unwrap();
        assert_eq!(held.amount_sat, Some(2000));
        assert_eq!(held.state, InvoiceState::Accepted);
        assert!(held.state.is_held());
        assert!(!held.state.is_final());

        let settled = Invoice::from_lnd(LOOKUP_SETTLED).unwrap();
        assert_eq!(settled.amount_sat, Some(2000));
        assert_eq!(settled.state, InvoiceState::Settled);
        assert!(settled.state.reveals_preimage());
        assert!(settled.state.is_final());
    }

    #[test]
    fn rejects_an_unknown_state() {
        let json = r#"{"state": "PENDING"}"#;
        assert!(matches!(
            Invoice::from_lnd(json),
            Err(LightningError::Output(_))
        ));
    }

    #[test]
    fn state_names_round_trip_through_serde() {
        for (name, state) in [
            (r#""OPEN""#, InvoiceState::Open),
            (r#""ACCEPTED""#, InvoiceState::Accepted),
            (r#""SETTLED""#, InvoiceState::Settled),
            (r#""CANCELED""#, InvoiceState::Canceled),
        ] {
            let parsed: InvoiceState = serde_json::from_str(name).unwrap();
            assert_eq!(parsed, state);
            assert_eq!(state.to_string().trim_matches('"'), name.trim_matches('"'));
        }
    }

    #[test]
    fn parses_a_stream_of_payment_updates() {
        // `payinvoice --json` prints pretty objects back to back.
        let stream = r#"{
            "status": "IN_FLIGHT",
            "payment_preimage": ""
        }
        {
            "status": "SUCCEEDED",
            "payment_preimage": "b43d86b447f98456cbf0190c657688b0d3b54365fcba8d6483ec12b75c4fd8b7"
        }"#;

        let preimage = parse_payment_preimage(stream).unwrap();
        assert_eq!(
            preimage.to_hex(),
            "b43d86b447f98456cbf0190c657688b0d3b54365fcba8d6483ec12b75c4fd8b7"
        );
    }

    #[test]
    fn reports_a_failed_payment() {
        let stream = r#"{"status": "FAILED", "failure_reason": "FAILURE_REASON_NO_ROUTE"}"#;
        let err = parse_payment_preimage(stream).unwrap_err();
        assert!(err.to_string().contains("NO_ROUTE"), "got: {err}");
    }

    #[test]
    fn a_payment_that_never_settled_has_no_preimage() {
        let stream = r#"{"status": "IN_FLIGHT", "payment_preimage": ""}"#;
        assert!(matches!(
            parse_payment_preimage(stream),
            Err(LightningError::NoPreimage)
        ));
    }

    #[test]
    fn parses_node_info() {
        let info = NodeInfo::from_lnd(r#"{"identity_pubkey": "02ab", "alias": "maker"}"#).unwrap();
        assert_eq!(info.pubkey, "02ab");
        assert_eq!(info.alias, "maker");
    }

    #[test]
    fn the_join_holds_only_for_the_right_preimage() {
        let preimage = Preimage::from_bytes([42u8; 32]);
        let htlc = htlc(&preimage);

        assert!(check_joined(&htlc, &preimage).is_ok());

        let wrong = Preimage::from_bytes([43u8; 32]);
        assert!(matches!(
            check_joined(&htlc, &wrong),
            Err(JointError::PreimageDoesNotUnlockHtlc { .. })
        ));
    }

    #[test]
    fn a_revealed_preimage_from_a_payment_unlocks_the_htlc() {
        // The full link, as the swap script exercises it: Lightning reveals a
        // preimage, and that preimage is what spends the on-chain leg.
        let preimage = Preimage::from_bytes([7u8; 32]);
        let htlc = htlc(&preimage);
        let stream = format!(
            r#"{{"status": "SUCCEEDED", "payment_preimage": "{}"}}"#,
            preimage.to_hex()
        );

        let revealed = parse_payment_preimage(&stream).unwrap();
        assert!(check_joined(&htlc, &revealed).is_ok());
        assert_eq!(
            htlc.address(Network::Regtest).address_type(),
            Some(bitcoin::AddressType::P2wsh)
        );
    }
}
