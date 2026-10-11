//! `swapsite-wasm` - the taker side, compiled for the browser.
//!
//! Everything of substance lives in `swapsite-core`, which does no I/O. This
//! crate is therefore only two things: a JSON request dispatcher, and a four
//! function C ABI for moving bytes in and out of linear memory.
//!
//! Deliberately no `wasm-bindgen`. The ABI is small enough to call by hand from
//! a page or from Node, and a plain `wasm32-unknown-unknown` build needs no
//! tooling beyond `rustup target add`.
//!
//! The dispatcher is ordinary Rust and is unit tested natively, so the browser
//! and the tests run exactly the same code.

use std::str::FromStr;

use bitcoin::{
    secp256k1::{Secp256k1, SecretKey},
    Address, Amount, Network, OutPoint, PublicKey,
};
use serde::Deserialize;
use serde_json::{json, Value};
use swapsite_core::{
    hash::{PaymentHash, Preimage},
    htlc::Htlc,
    recovery::Recovery,
    recovery::TakerView,
    spend,
    state::SwapState,
};

/// One request from the page. Tagged so the wire format reads on its own.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    /// Derive a keypair from caller-supplied entropy.
    ///
    /// A wasm module has no entropy source of its own, so the page supplies 32
    /// random bytes (from `crypto.getRandomValues`) and this does the elliptic
    /// curve maths, which is the part a page cannot do for itself.
    Keypair {
        /// 32 random bytes, hex.
        entropy: String,
    },

    /// The P2WSH address of an HTLC, so the taker can check the maker's deposit.
    HtlcAddress {
        payment_hash: String,
        locktime: u32,
        claim_pubkey: String,
        refund_pubkey: String,
        #[serde(default = "default_network")]
        network: String,
    },

    /// Find the output paying `address` in a raw transaction.
    ///
    /// This is how the taker checks a deposit: the relay only supplies bytes.
    FindOutput {
        tx_hex: String,
        address: String,
        #[serde(default = "default_network")]
        network: String,
    },

    /// Check that a revealed preimage unlocks the HTLC: the join between legs.
    CheckJoin {
        payment_hash: String,
        locktime: u32,
        claim_pubkey: String,
        refund_pubkey: String,
        preimage: String,
        #[serde(default = "default_network")]
        network: String,
    },

    /// Build and sign the claim, revealing the preimage.
    Claim {
        claim_key: String,
        payment_hash: String,
        locktime: u32,
        refund_pubkey: String,
        preimage: String,
        outpoint: String,
        value: u64,
        to: String,
        #[serde(default = "default_fee")]
        fee: u64,
        #[serde(default = "default_network")]
        network: String,
    },

    /// Build and sign a refund of an HTLC this taker funded.
    Refund {
        refund_key: String,
        payment_hash: String,
        locktime: u32,
        claim_pubkey: String,
        outpoint: String,
        value: u64,
        to: String,
        #[serde(default = "default_fee")]
        fee: u64,
        #[serde(default = "default_network")]
        network: String,
    },

    /// What the taker can still do about a swap, and how long it has left.
    ///
    /// Everything this needs is public except `has_preimage`, which is the
    /// taker's own knowledge - so the verdict is computed here rather than
    /// asked of the coordinator, which never learns the secret anyway.
    Assess {
        state: String,
        has_preimage: bool,
        /// Whether the deposit output is still there. Defaults to yes: being
        /// wrong that way costs a failed transaction, while being wrong the
        /// other way tells a taker to give up on a swap that is still theirs.
        #[serde(default = "default_true")]
        deposit_unspent: bool,
        deadline: u32,
        height: u32,
    },
}

fn default_network() -> String {
    "regtest".to_owned()
}

fn default_fee() -> u64 {
    1_000
}

fn default_true() -> bool {
    true
}

/// Run one request and return a JSON response.
///
/// Errors come back as `{"error": "..."}` rather than as a trap, so a page can
/// show them.
pub fn handle(input: &str) -> String {
    match dispatch_request(input) {
        Ok(response) => response.to_string(),
        Err(message) => json!({ "error": message }).to_string(),
    }
}

fn dispatch_request(input: &str) -> Result<Value, String> {
    let request: Request = serde_json::from_str(input).map_err(|e| format!("bad request: {e}"))?;

    match request {
        Request::Keypair { entropy } => {
            let bytes: [u8; 32] = hex::decode(&entropy)
                .map_err(|e| format!("bad entropy hex: {e}"))?
                .try_into()
                .map_err(|_| "entropy must be 32 bytes".to_owned())?;

            let secret = SecretKey::from_slice(&bytes).map_err(|e| e.to_string())?;
            let secp = Secp256k1::new();
            Ok(json!({
                "secret": hex::encode(secret.secret_bytes()),
                "pubkey": PublicKey::new(secret.public_key(&secp)).to_string(),
            }))
        }

        Request::HtlcAddress {
            payment_hash,
            locktime,
            claim_pubkey,
            refund_pubkey,
            network,
        } => {
            let htlc = htlc(&payment_hash, locktime, &claim_pubkey, &refund_pubkey)?;
            let network = parse_network(&network)?;
            Ok(json!({
                "address": htlc.address(network).to_string(),
                "script_pubkey": hex::encode(htlc.script_pubkey(network).as_bytes()),
                "redeem_script": htlc.script().to_asm_string(),
            }))
        }

        Request::FindOutput {
            tx_hex,
            address,
            network,
        } => {
            let network = parse_network(&network)?;
            let address = parse_address(&address, network)?;
            let bytes = hex::decode(&tx_hex).map_err(|e| format!("bad tx hex: {e}"))?;
            let tx: bitcoin::Transaction =
                bitcoin::consensus::deserialize(&bytes).map_err(|e| format!("bad tx: {e}"))?;

            let wanted = address.script_pubkey();
            let found = tx
                .output
                .iter()
                .enumerate()
                .find(|(_, output)| output.script_pubkey == wanted)
                .map(|(vout, output)| (vout as u32, output.value.to_sat()));

            match found {
                Some((vout, sats)) => Ok(json!({
                    "vout": vout,
                    "sats": sats,
                    "txid": tx.compute_txid().to_string(),
                })),
                None => Err(format!("no output pays {address}")),
            }
        }

        Request::CheckJoin {
            payment_hash,
            locktime,
            claim_pubkey,
            refund_pubkey,
            preimage,
            network,
        } => {
            let htlc = htlc(&payment_hash, locktime, &claim_pubkey, &refund_pubkey)?;
            let preimage = Preimage::from_hex(&preimage).map_err(|e| e.to_string())?;
            swapsite_core::lightning::check_joined(&htlc, &preimage).map_err(|e| e.to_string())?;
            Ok(json!({
                "joined": true,
                "payment_hash": htlc.payment_hash.to_hex(),
                // The taker gets the address it should be claiming, so a bad
                // swap cannot pass the check and then be claimed by mistake.
                "address": htlc.address(parse_network(&network)?).to_string(),
            }))
        }

        Request::Assess {
            state,
            has_preimage,
            deposit_unspent,
            deadline,
            height,
        } => {
            let view = TakerView {
                state: parse_state(&state)?,
                has_preimage,
                deposit_unspent,
                deadline,
                height,
            };
            serde_json::to_value(Recovery::assess(view)).map_err(|e| e.to_string())
        }

        Request::Claim {
            claim_key,
            payment_hash,
            locktime,
            refund_pubkey,
            preimage,
            outpoint,
            value,
            to,
            fee,
            network,
        } => {
            let network = parse_network(&network)?;
            let key = secret(&claim_key)?;
            let htlc = htlc_from_key(
                &payment_hash,
                locktime,
                Claimant::Secret(&key),
                &refund_pubkey,
            )?;
            let preimage = Preimage::from_hex(&preimage).map_err(|e| e.to_string())?;

            // The whole point: refuse to build a claim the preimage cannot unlock.
            swapsite_core::lightning::check_joined(&htlc, &preimage).map_err(|e| e.to_string())?;

            let tx = spend::claim(
                &htlc,
                OutPoint::from_str(&outpoint).map_err(|e| e.to_string())?,
                Amount::from_sat(value),
                &parse_address(&to, network)?,
                Amount::from_sat(fee),
                &key,
                &preimage,
            )
            .map_err(|e| e.to_string())?;

            Ok(json!({
                "tx_hex": bitcoin::consensus::encode::serialize_hex(&tx),
                "txid": tx.compute_txid().to_string(),
            }))
        }

        Request::Refund {
            refund_key,
            payment_hash,
            locktime,
            claim_pubkey,
            outpoint,
            value,
            to,
            fee,
            network,
        } => {
            let network = parse_network(&network)?;
            let key = secret(&refund_key)?;
            let htlc = htlc_from_key(
                &payment_hash,
                locktime,
                Claimant::Pubkey(&claim_pubkey),
                &key.public_key(&Secp256k1::new()).to_string(),
            )?;

            let tx = spend::refund(
                &htlc,
                OutPoint::from_str(&outpoint).map_err(|e| e.to_string())?,
                Amount::from_sat(value),
                &parse_address(&to, network)?,
                Amount::from_sat(fee),
                &key,
            )
            .map_err(|e| e.to_string())?;

            Ok(json!({
                "tx_hex": bitcoin::consensus::encode::serialize_hex(&tx),
                "txid": tx.compute_txid().to_string(),
            }))
        }
    }
}

/// Which side of the script the caller supplies as a pubkey.
enum Claimant<'a> {
    Pubkey(&'a str),
    Secret(&'a SecretKey),
}

fn htlc(
    payment_hash: &str,
    locktime: u32,
    claim_pubkey: &str,
    refund_pubkey: &str,
) -> Result<Htlc, String> {
    htlc_from_key(
        payment_hash,
        locktime,
        Claimant::Pubkey(claim_pubkey),
        refund_pubkey,
    )
}

fn htlc_from_key(
    payment_hash: &str,
    locktime: u32,
    claimant: Claimant<'_>,
    refund_pubkey: &str,
) -> Result<Htlc, String> {
    let secp = Secp256k1::new();
    let claimant = match claimant {
        Claimant::Pubkey(hex) => PublicKey::from_str(hex).map_err(|e| e.to_string())?,
        Claimant::Secret(secret) => PublicKey::new(secret.public_key(&secp)),
    };
    let refunder = PublicKey::from_str(refund_pubkey).map_err(|e| e.to_string())?;

    Htlc::from_height(
        PaymentHash::from_hex(payment_hash).map_err(|e| e.to_string())?,
        claimant,
        refunder,
        locktime,
    )
    .map_err(|e| e.to_string())
}

fn parse_state(name: &str) -> Result<SwapState, String> {
    // Serde is the single source of truth for the wire spelling.
    serde_json::from_value(Value::String(name.to_owned()))
        .map_err(|_| format!("unknown swap state `{name}`"))
}

fn secret(hex: &str) -> Result<SecretKey, String> {
    SecretKey::from_str(hex).map_err(|e| e.to_string())
}

fn parse_address(text: &str, network: Network) -> Result<Address, String> {
    Address::from_str(text)
        .map_err(|e| e.to_string())
        .and_then(|address| address.require_network(network).map_err(|e| e.to_string()))
}

fn parse_network(name: &str) -> Result<Network, String> {
    match name {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        other => Err(format!("unknown network `{other}`")),
    }
}

// --- C ABI -----------------------------------------------------------------
//
// Four functions, so a page needs no bindgen:
//   alloc(len) -> ptr            write a request here
//   dispatch(ptr, len) -> ptr    returns [u32 little-endian length][json]
//   dealloc(ptr, len)            free either of the above

/// Allocate `len` bytes for the caller to write into.
#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len);
    let pointer = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    pointer
}

/// Free a buffer returned by [`alloc`] or [`dispatch`].
///
/// # Safety
///
/// `ptr` must come from this module and `len` must be its exact length.
#[no_mangle]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    drop(Vec::from_raw_parts(ptr, len, len));
}

/// Run one request: `[ptr, ptr+len)` is UTF-8 JSON, the result is length
/// prefixed JSON.
///
/// # Safety
///
/// `ptr` and `len` must describe a buffer returned by [`alloc`].
#[no_mangle]
pub unsafe extern "C" fn dispatch(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);
    let response = handle(&String::from_utf8_lossy(input));

    let mut buffer = Vec::with_capacity(response.len() + 4);
    buffer.extend_from_slice(&(response.len() as u32).to_le_bytes());
    buffer.extend_from_slice(response.as_bytes());

    let pointer = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    pointer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_round_trip(request: Value) -> Value {
        let response = handle(&request.to_string());
        let value: Value = serde_json::from_str(&response).expect("response is JSON");
        assert!(value.get("error").is_none(), "unexpected error: {response}");
        value
    }

    fn keypair() -> (String, String) {
        // A page would pass fresh `crypto.getRandomValues` bytes each time.
        static NEXT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
        let byte = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let out = json_round_trip(json!({ "op": "keypair", "entropy": hex::encode([byte; 32]) }));
        (
            out["secret"].as_str().unwrap().to_owned(),
            out["pubkey"].as_str().unwrap().to_owned(),
        )
    }

    /// The taker's whole view of a swap, built from public values.
    fn swap() -> (String, String, String, Preimage) {
        let (_, claim_pubkey) = keypair();
        let (_, refund_pubkey) = keypair();
        // In this direction the maker picks the preimage, so the taker only
        // ever learns it after settling. The tests invent one.
        let preimage = Preimage::from_bytes([7u8; 32]);
        let hash = preimage.payment_hash().to_hex();
        (claim_pubkey, refund_pubkey, hash, preimage)
    }

    #[test]
    fn keypair_derives_the_pubkey_for_the_supplied_entropy() {
        let entropy = hex::encode([5u8; 32]);
        let out = json_round_trip(json!({ "op": "keypair", "entropy": entropy }));

        let secret = SecretKey::from_str(out["secret"].as_str().unwrap()).unwrap();
        let secp = Secp256k1::new();
        assert_eq!(
            PublicKey::new(secret.public_key(&secp)).to_string(),
            out["pubkey"].as_str().unwrap()
        );
    }

    #[test]
    fn keypair_rejects_bad_entropy() {
        for entropy in ["", "zz", "0011"] {
            let response = handle(&json!({ "op": "keypair", "entropy": entropy }).to_string());
            let value: Value = serde_json::from_str(&response).unwrap();
            assert!(
                value.get("error").is_some(),
                "expected an error for {entropy}"
            );
        }
    }

    #[test]
    fn the_taker_derives_the_same_address_the_core_does() {
        let (claim_pubkey, refund_pubkey, hash, preimage) = swap();
        let out = json_round_trip(json!({
            "op": "htlc_address",
            "payment_hash": hash,
            "locktime": 800_000,
            "claim_pubkey": claim_pubkey,
            "refund_pubkey": refund_pubkey,
        }));

        let expected = Htlc::from_height(
            preimage.payment_hash(),
            PublicKey::from_str(&claim_pubkey).unwrap(),
            PublicKey::from_str(&refund_pubkey).unwrap(),
            800_000,
        )
        .unwrap();

        assert_eq!(
            out["address"].as_str().unwrap(),
            expected.address(Network::Regtest).to_string()
        );
        assert!(out["redeem_script"].as_str().unwrap().contains("OP_SHA256"));
    }

    #[test]
    fn the_join_is_refused_for_the_wrong_preimage() {
        let (claim_pubkey, refund_pubkey, hash, _) = swap();
        let wrong = Preimage::from_bytes([9u8; 32]).to_hex();

        let response = handle(
            &json!({
                "op": "check_join",
                "payment_hash": hash,
                "locktime": 800_000,
                "claim_pubkey": claim_pubkey,
                "refund_pubkey": refund_pubkey,
                "preimage": wrong,
            })
            .to_string(),
        );

        let value: Value = serde_json::from_str(&response).unwrap();
        assert!(
            value["error"].as_str().unwrap().contains("does not unlock"),
            "got {response}"
        );
    }

    #[test]
    fn the_taker_asks_itself_what_is_left() {
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": true,
            "deadline": 506,
            "height": 500,
        }));

        assert_eq!(out["action"]["kind"], "claim");
        assert_eq!(out["action"]["blocks_left"], 6);
        assert_eq!(out["deadline"], 506);
        assert_eq!(out["height"], 500);
        assert_eq!(out["urgent"], true);
    }

    #[test]
    fn a_taker_missing_its_preimage_is_told_to_find_it() {
        // Settled, paid for, and this page cannot claim it yet. Not an error,
        // and not urgent until the deadline is near.
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": false,
            "deadline": 506,
            "height": 400,
        }));

        assert_eq!(out["action"]["kind"], "find_preimage");
        assert_eq!(out["action"]["blocks_left"], 106);
        assert_eq!(out["urgent"], false);
    }

    #[test]
    fn past_the_deadline_the_verdict_is_a_race() {
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": true,
            "deadline": 506,
            "height": 510,
        }));

        assert_eq!(out["action"]["kind"], "racing");
        assert_eq!(out["action"]["blocks_since"], 4);
        assert_eq!(out["urgent"], true);
    }

    #[test]
    fn a_spent_deposit_ends_it_even_before_the_deadline() {
        // The case the coordinator cannot see: a maker that refunds a swap it
        // was already paid for, so no record anywhere says `refunded`.
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": true,
            "deposit_unspent": false,
            "deadline": 506,
            "height": 480,
        }));

        assert_eq!(out["action"]["kind"], "gone");
        assert_eq!(out["urgent"], false);
    }

    #[test]
    fn an_unasked_deposit_is_assumed_to_be_there() {
        // Optimistic on purpose: a caller that has not checked must not be told
        // to abandon a swap.
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": true,
            "deadline": 506,
            "height": 480,
        }));

        assert_eq!(out["action"]["kind"], "claim");
    }

    #[test]
    fn the_verdict_carries_every_field_the_page_reads() {
        // Nothing type-checks across the JSON boundary, so a field that is
        // renamed, or one left as a Rust method that never crosses at all,
        // breaks the page silently.
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "settled",
            "has_preimage": true,
            "deadline": 506,
            "height": 480,
        }));

        for key in ["action", "deadline", "height", "urgent", "actionable"] {
            assert!(out.get(key).is_some(), "the verdict has no `{key}`: {out}");
        }
        assert_eq!(out["actionable"], true);
    }

    #[test]
    fn a_claim_that_is_merely_waiting_is_not_actionable() {
        let out = json_round_trip(json!({
            "op": "assess",
            "state": "initiator_funded",
            "has_preimage": false,
            "deadline": 506,
            "height": 400,
        }));

        assert_eq!(out["action"]["kind"], "wait_for_payment");
        assert_eq!(out["actionable"], false);
    }

    #[test]
    fn an_unknown_state_is_an_error_and_not_a_default() {
        let response = handle(
            &json!({
                "op": "assess",
                "state": "wandering",
                "has_preimage": true,
                "deadline": 506,
                "height": 500,
            })
            .to_string(),
        );

        let value: Value = serde_json::from_str(&response).unwrap();
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("unknown swap state"),
            "got {response}"
        );
    }

    #[test]
    fn a_claim_is_refused_when_the_preimage_does_not_unlock() {
        let (_, refund_pubkey, hash, _) = swap();
        let (claim_key, _) = keypair();
        let wrong = Preimage::from_bytes([9u8; 32]).to_hex();

        let response = handle(
            &json!({
                "op": "claim",
                "claim_key": claim_key,
                "payment_hash": hash,
                "locktime": 800_000,
                "refund_pubkey": refund_pubkey,
                "preimage": wrong,
                "outpoint": format!("{}:0", "00".repeat(32)),
                "value": 32_000,
                "to": Address::p2wsh(&bitcoin::ScriptBuf::new(), Network::Regtest).to_string(),
                "fee": 2_000,
            })
            .to_string(),
        );

        let value: Value = serde_json::from_str(&response).unwrap();
        assert!(value["error"].as_str().unwrap().contains("does not unlock"));
    }

    #[test]
    fn the_taker_builds_a_claim_with_the_revealed_preimage() {
        let (_, refund_pubkey, hash, preimage) = swap();
        let (claim_key, _) = keypair();

        let to = Address::p2wsh(&bitcoin::ScriptBuf::new(), Network::Regtest).to_string();
        let out = json_round_trip(json!({
            "op": "claim",
            "claim_key": claim_key,
            "payment_hash": hash,
            "locktime": 800_000,
            "refund_pubkey": refund_pubkey,
            "preimage": preimage.to_hex(),
            "outpoint": format!("{}:1", "11".repeat(32)),
            "value": 32_000,
            "to": to,
            "fee": 2_000,
        }));

        let bytes = hex::decode(out["tx_hex"].as_str().unwrap()).unwrap();
        let tx: bitcoin::Transaction = bitcoin::consensus::deserialize(&bytes).unwrap();
        assert_eq!(tx.output[0].value.to_sat(), 30_000);
        assert_eq!(tx.compute_txid().to_string(), out["txid"].as_str().unwrap());
        // Claim carries the preimage in the witness, refund does not.
        assert_eq!(tx.input[0].witness.len(), 4);
    }

    #[test]
    fn the_taker_finds_its_deposit_in_a_funding_transaction() {
        let (claim_pubkey, refund_pubkey, hash, _) = swap();
        let htlc = Htlc::from_height(
            PaymentHash::from_hex(&hash).unwrap(),
            PublicKey::from_str(&claim_pubkey).unwrap(),
            PublicKey::from_str(&refund_pubkey).unwrap(),
            800_000,
        )
        .unwrap();
        let address = htlc.address(Network::Regtest);

        // A transaction that pays the HTLC, then some unrelated output.
        let funding = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint {
                    txid: bitcoin::Txid::from_str(&"03".repeat(32)).unwrap(),
                    vout: 0,
                },
                script_sig: bitcoin::ScriptBuf::new(),
                sequence: bitcoin::Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![
                bitcoin::TxOut {
                    value: Amount::from_sat(1_000),
                    script_pubkey: bitcoin::ScriptBuf::new(),
                },
                bitcoin::TxOut {
                    value: Amount::from_sat(32_000),
                    script_pubkey: address.script_pubkey(),
                },
            ],
        };

        let out = json_round_trip(json!({
            "op": "find_output",
            "tx_hex": bitcoin::consensus::encode::serialize_hex(&funding),
            "address": address.to_string(),
        }));

        assert_eq!(out["vout"].as_u64(), Some(1));
        assert_eq!(out["sats"].as_u64(), Some(32_000));
    }

    #[test]
    fn the_taker_rejects_a_deposit_that_does_not_pay_the_htlc() {
        let (claim_pubkey, refund_pubkey, hash, _) = swap();
        let response = handle(
            &json!({
                "op": "find_output",
                "tx_hex": bitcoin::consensus::encode::serialize_hex(&bitcoin::Transaction {
                    version: bitcoin::transaction::Version::TWO,
                    lock_time: bitcoin::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![bitcoin::TxOut {
                        value: Amount::from_sat(1_000),
                        script_pubkey: bitcoin::ScriptBuf::new(),
                    }],
                }),
                "address": Htlc::from_height(
                    PaymentHash::from_hex(&hash).unwrap(),
                    PublicKey::from_str(&claim_pubkey).unwrap(),
                    PublicKey::from_str(&refund_pubkey).unwrap(),
                    800_000,
                )
                .unwrap()
                .address(Network::Regtest)
                .to_string(),
            })
            .to_string(),
        );

        assert!(serde_json::from_str::<Value>(&response).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("no output pays"));
    }

    #[test]
    fn bad_input_is_an_error_not_a_trap() {
        for input in [
            "not json",
            r#"{"op":"nonsense"}"#,
            r#"{"op":"claim"}"#,
            r#"{"op":"htlc_address","payment_hash":"zz","locktime":1,"claim_pubkey":"x","refund_pubkey":"y"}"#,
        ] {
            let value: Value = serde_json::from_str(&handle(input)).unwrap();
            assert!(
                value.get("error").is_some(),
                "expected an error for {input}"
            );
        }
    }

    #[test]
    fn the_c_abi_round_trips_a_request() {
        let request = br#"{"op":"keypair","entropy":"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a"}"#;

        // Exactly what the JS glue does: alloc, write, dispatch, read back.
        let input = alloc(request.len());
        assert!(!input.is_null());
        unsafe {
            std::ptr::copy_nonoverlapping(request.as_ptr(), input, request.len());

            let output = dispatch(input, request.len());
            let length = std::slice::from_raw_parts(output, 4);
            let length = u32::from_le_bytes([length[0], length[1], length[2], length[3]]) as usize;
            let body = std::slice::from_raw_parts(output.add(4), length);
            let value: Value = serde_json::from_slice(body).unwrap();

            assert!(value["pubkey"].as_str().is_some(), "got {value}");
            assert!(value.get("error").is_none());

            dealloc(output, length + 4);
            dealloc(input, request.len());
        }
    }
}
