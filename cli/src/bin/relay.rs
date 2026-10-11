//! `relay` - the two things a browser taker cannot do for itself.
//!
//! A taker with no node still has to read the chain and broadcast. This serves
//! exactly that and nothing else:
//!
//! ```text
//! GET  /height          -> {"height": N}
//! GET  /tx/<txid>       -> {"txid", "confirmations", "hex"}
//! GET  /utxo/<txid>:<vout> -> {"unspent": bool, "sats": N|null}
//! POST /tx              -> {"txid"}      (body is raw transaction hex)
//! ```
//!
//! A relay can lie about what it sees and can refuse to broadcast, but it cannot
//! take funds: the taker checks the deposit script itself and signs its own
//! transaction with a key the relay never sees.
//!
//! Deliberately tiny: the shared local HTTP server, and four routes.

use std::str::FromStr;

use bitcoin::{Transaction, Txid};
use clap::Parser;
use serde_json::json;
use swapsite_adapters::{
    http::{serve, Request, Response},
    BitcoinRpc, RawTransaction,
};
use swapsite_core::chain::Chain;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "relay", about = "Chain observation and broadcast for a taker")]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: String,
    /// Bitcoin Core RPC URL. Plain HTTP, credentials inline.
    #[arg(
        long,
        env = "SWAPSITE_BITCOIN_URL",
        default_value = "http://bitcoin:bitcoin@127.0.0.1:18443"
    )]
    bitcoin_url: String,
    #[arg(long, default_value = "regtest")]
    network: String,
}

/// The node operations the relay exposes. A trait so the routing can be tested
/// without a node.
trait Node: Send + Sync {
    fn height(&self) -> std::result::Result<u32, String>;
    fn transaction(&self, txid: &Txid) -> std::result::Result<Option<RawTransaction>, String>;
    /// What an output is worth while it is still unspent, and `None` once it
    /// has been spent.
    fn unspent(&self, txid: &Txid, vout: u32) -> std::result::Result<Option<u64>, String>;
    fn broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, String>;
}

impl Node for BitcoinRpc {
    fn height(&self) -> std::result::Result<u32, String> {
        Chain::height(self).map_err(|e| e.to_string())
    }

    fn transaction(&self, txid: &Txid) -> std::result::Result<Option<RawTransaction>, String> {
        BitcoinRpc::transaction(self, txid).map_err(|e| e.to_string())
    }

    fn unspent(&self, txid: &Txid, vout: u32) -> std::result::Result<Option<u64>, String> {
        BitcoinRpc::outspent(self, txid, vout)
            .map(|found| found.map(|amount| amount.to_sat()))
            .map_err(|e| e.to_string())
    }

    fn broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, String> {
        Chain::broadcast(self, tx).map_err(|e| e.to_string())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let node = BitcoinRpc::new(&cli.bitcoin_url, None, parse_network(&cli.network)?)?;
    serve(&cli.listen, move |request| route(&node, &request))?;
    Ok(())
}

/// Route one request. Pure apart from the node calls, and tested as such.
fn route(node: &dyn Node, request: &Request) -> Response {
    if request.matches("GET", "/height").is_some() {
        return match node.height() {
            Ok(height) => Response::json(200, &json!({ "height": height })),
            Err(e) => Response::json(502, &json!({ "error": e })),
        };
    }

    if let Some(captures) = request.matches("GET", "/tx/:txid") {
        return match Txid::from_str(&captures[0]) {
            Err(e) => Response::error(400, format!("bad txid: {e}")),
            Ok(txid) => match node.transaction(&txid) {
                Ok(Some(tx)) => Response::json(
                    200,
                    &json!({
                        "txid": txid.to_string(),
                        "confirmations": tx.confirmations,
                        "hex": tx.hex,
                    }),
                ),
                Ok(None) => Response::error(404, "unknown transaction"),
                Err(e) => Response::json(502, &json!({ "error": e })),
            },
        };
    }

    if let Some(captures) = request.matches("GET", "/utxo/:outpoint") {
        let Some((txid, vout)) = captures[0].split_once(':') else {
            return Response::error(400, "an outpoint looks like <txid>:<vout>");
        };

        let txid = match Txid::from_str(txid) {
            Ok(txid) => txid,
            Err(e) => return Response::error(400, format!("bad txid: {e}")),
        };
        let vout = match vout.parse::<u32>() {
            Ok(vout) => vout,
            Err(e) => return Response::error(400, format!("bad vout: {e}")),
        };

        return match node.unspent(&txid, vout) {
            Ok(Some(sats)) => Response::json(200, &json!({ "unspent": true, "sats": sats })),
            Ok(None) => Response::json(200, &json!({ "unspent": false, "sats": null })),
            Err(e) => Response::json(502, &json!({ "error": e })),
        };
    }

    if request.matches("POST", "/tx").is_some() {
        return match hex::decode(request.body.trim()) {
            Err(e) => Response::error(400, format!("bad hex: {e}")),
            Ok(bytes) => match bitcoin::consensus::deserialize::<Transaction>(&bytes) {
                Err(e) => Response::error(400, format!("bad transaction: {e}")),
                Ok(tx) => match node.broadcast(&tx) {
                    Ok(txid) => Response::json(200, &json!({ "txid": txid.to_string() })),
                    Err(e) => Response::error(400, e),
                },
            },
        };
    }

    Response::not_found()
}

fn parse_network(name: &str) -> Result<bitcoin::Network> {
    match name {
        "bitcoin" | "mainnet" => Ok(bitcoin::Network::Bitcoin),
        "testnet" => Ok(bitcoin::Network::Testnet),
        "signet" => Ok(bitcoin::Network::Signet),
        "regtest" => Ok(bitcoin::Network::Regtest),
        other => Err(format!("unknown network `{other}`").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        absolute::LockTime, Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness,
    };
    use serde_json::Value;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeNode {
        height: u32,
        transactions: Vec<(Txid, RawTransaction)>,
        /// Outpoints that are still unspent, with what they hold.
        unspent: Vec<((Txid, u32), u64)>,
        broadcast: Mutex<Vec<Txid>>,
        fail_broadcast: bool,
    }

    impl Node for FakeNode {
        fn height(&self) -> std::result::Result<u32, String> {
            Ok(self.height)
        }

        fn transaction(&self, txid: &Txid) -> std::result::Result<Option<RawTransaction>, String> {
            Ok(self
                .transactions
                .iter()
                .find(|(id, _)| id == txid)
                .map(|(_, tx)| tx.clone()))
        }

        fn unspent(&self, txid: &Txid, vout: u32) -> std::result::Result<Option<u64>, String> {
            Ok(self
                .unspent
                .iter()
                .find(|((id, n), _)| id == txid && *n == vout)
                .map(|(_, sats)| *sats))
        }

        fn broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, String> {
            if self.fail_broadcast {
                return Err("rejected".to_owned());
            }
            let txid = tx.compute_txid();
            self.broadcast.lock().unwrap().push(txid);
            Ok(txid)
        }
    }

    fn sample_tx() -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_str(&"04".repeat(32)).unwrap(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    fn request(method: &str, path: &str, body: &str) -> Request {
        let raw = format!(
            "{method} {path} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut reader = std::io::Cursor::new(raw.into_bytes());
        swapsite_adapters::http::read_request(&mut reader)
            .unwrap()
            .unwrap()
    }

    fn get(path: &str) -> Request {
        request("GET", path, "")
    }

    fn post(path: &str, body: &str) -> Request {
        request("POST", path, body)
    }

    fn body_of(response: Response) -> Value {
        serde_json::from_slice(&response.body).expect("response body is JSON")
    }

    #[test]
    fn serves_the_height() {
        let node = FakeNode {
            height: 4321,
            ..Default::default()
        };
        let response = route(&node, &get("/height"));
        assert_eq!(response.status, 200);
        assert_eq!(body_of(response)["height"], 4321);
    }

    #[test]
    fn serves_a_transaction_by_id() {
        let tx = sample_tx();
        let txid = tx.compute_txid();
        let node = FakeNode {
            transactions: vec![(
                txid,
                RawTransaction {
                    confirmations: 3,
                    hex: bitcoin::consensus::encode::serialize_hex(&tx),
                },
            )],
            ..Default::default()
        };

        let response = route(&node, &get(&format!("/tx/{txid}")));
        assert_eq!(response.status, 200);
        let body = body_of(response);
        assert_eq!(body["confirmations"], 3);
        assert_eq!(
            body["hex"].as_str().unwrap(),
            bitcoin::consensus::encode::serialize_hex(&tx)
        );
    }

    #[test]
    fn a_query_string_does_not_confuse_the_router() {
        let node = FakeNode {
            height: 7,
            ..Default::default()
        };
        let response = route(&node, &get("/height?cachebust=1"));
        assert_eq!(response.status, 200);
    }

    #[test]
    fn reports_an_unknown_transaction() {
        let node = FakeNode::default();
        let response = route(&node, &get(&format!("/tx/{}", "ab".repeat(32))));
        assert_eq!(response.status, 404);
        assert!(body_of(response)["error"]
            .as_str()
            .unwrap()
            .contains("unknown"));
    }

    #[test]
    fn rejects_a_malformed_txid() {
        let node = FakeNode::default();
        let response = route(&node, &get("/tx/not-a-txid"));
        assert_eq!(response.status, 400);
    }

    #[test]
    fn reports_an_unspent_outpoint_and_its_value() {
        let txid = sample_tx().compute_txid();
        let node = FakeNode {
            unspent: vec![((txid, 1), 32_000)],
            ..FakeNode::default()
        };

        let response = route(&node, &get(&format!("/utxo/{txid}:1")));
        assert_eq!(response.status, 200);
        let body = body_of(response);
        assert_eq!(body["unspent"], true);
        assert_eq!(body["sats"], 32_000);
    }

    #[test]
    fn a_spent_outpoint_is_not_an_error() {
        // Spent and never-existed are the same answer from `gettxout`, and the
        // same answer here: it is not there. Asking about it is not a mistake.
        let txid = sample_tx().compute_txid();
        let node = FakeNode {
            unspent: vec![((txid, 1), 32_000)],
            ..FakeNode::default()
        };

        let response = route(&node, &get(&format!("/utxo/{txid}:0")));
        assert_eq!(response.status, 200);
        let body = body_of(response);
        assert_eq!(body["unspent"], false);
        assert_eq!(body["sats"], Value::Null);
    }

    #[test]
    fn an_outpoint_needs_both_halves() {
        let node = FakeNode::default();
        let response = route(&node, &get("/utxo/nonsense"));
        assert_eq!(response.status, 400);
        assert!(body_of(response)["error"]
            .as_str()
            .unwrap()
            .contains("outpoint"));
    }

    #[test]
    fn broadcasts_raw_hex() {
        let node = FakeNode::default();
        let hex = bitcoin::consensus::encode::serialize_hex(&sample_tx());

        let response = route(&node, &post("/tx", &hex));
        assert_eq!(response.status, 200);
        assert_eq!(
            body_of(response)["txid"].as_str().unwrap(),
            sample_tx().compute_txid().to_string()
        );
        assert_eq!(node.broadcast.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_broadcast_is_reported_not_swallowed() {
        let node = FakeNode {
            fail_broadcast: true,
            ..Default::default()
        };
        let hex = bitcoin::consensus::encode::serialize_hex(&sample_tx());
        let response = route(&node, &post("/tx", &hex));
        assert_eq!(response.status, 400);
        assert_eq!(body_of(response)["error"], "rejected");
    }

    #[test]
    fn rejects_rubbish_on_the_broadcast_route() {
        let node = FakeNode::default();
        assert_eq!(route(&node, &post("/tx", "not hex")).status, 400);
        assert_eq!(route(&node, &post("/tx", "00")).status, 400);
    }

    #[test]
    fn unknown_routes_are_not_found() {
        // Preflights are answered by the shared server before routing.
        let node = FakeNode::default();
        assert_eq!(route(&node, &get("/nothing")).status, 404);
        assert_eq!(route(&node, &request("DELETE", "/tx", "")).status, 404);
    }
}
