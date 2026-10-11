//! `relay` - the two things a browser taker cannot do for itself.
//!
//! A taker with no node still has to read the chain and broadcast. This serves
//! exactly that and nothing else:
//!
//! ```text
//! GET  /height        -> {"height": N}
//! GET  /tx/<txid>     -> {"txid", "confirmations", "hex"}
//! POST /tx            -> {"txid"}      (body is raw transaction hex)
//! ```
//!
//! A relay can lie about what it sees and can refuse to broadcast, but it cannot
//! take funds: the taker checks the deposit script itself and signs its own
//! transaction with a key the relay never sees.
//!
//! Deliberately a hand-rolled HTTP/1.1 server. It answers one request per
//! connection and serves three routes on a trusted host.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::str::FromStr;
use std::thread;

use bitcoin::{Transaction, Txid};
use clap::Parser;
use serde_json::{json, Value};
use swapsite_adapters::{BitcoinRpc, RawTransaction};
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

/// The three node operations the relay exposes. A trait so the routing can be
/// tested without a node.
trait Node: Send + Sync {
    fn height(&self) -> std::result::Result<u32, String>;
    fn transaction(&self, txid: &Txid) -> std::result::Result<Option<RawTransaction>, String>;
    fn broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, String>;
}

impl Node for BitcoinRpc {
    fn height(&self) -> std::result::Result<u32, String> {
        Chain::height(self).map_err(|e| e.to_string())
    }

    fn transaction(&self, txid: &Txid) -> std::result::Result<Option<RawTransaction>, String> {
        BitcoinRpc::transaction(self, txid).map_err(|e| e.to_string())
    }

    fn broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, String> {
        Chain::broadcast(self, tx).map_err(|e| e.to_string())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let node = BitcoinRpc::new(&cli.bitcoin_url, None, parse_network(&cli.network)?)?;
    let listener = TcpListener::bind(&cli.listen)?;
    eprintln!("relay listening on http://{}", cli.listen);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let node = node.clone();
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &node) {
                        eprintln!("connection: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    Ok(())
}

fn serve(mut stream: TcpStream, node: &dyn Node) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or("/").to_owned();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let body = if content_length > 0 {
        let mut buffer = vec![0u8; content_length];
        reader.read_exact(&mut buffer)?;
        String::from_utf8_lossy(&buffer).into_owned()
    } else {
        String::new()
    };

    let (status, payload) = route(node, &method, &path, &body);
    let body = payload.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Connection: close\r\n\r\n",
        reason(status),
        body.len()
    );

    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Route one request. Pure apart from the node calls, and tested as such.
fn route(node: &dyn Node, method: &str, path: &str, body: &str) -> (u16, Value) {
    let path = path.split('?').next().unwrap_or(path);

    match (method, path) {
        // Posting raw hex as text/plain avoids a CORS preflight, but answer one
        // anyway in case a page sends something else.
        ("OPTIONS", _) => (204, json!({})),

        ("GET", "/height") => match node.height() {
            Ok(height) => (200, json!({ "height": height })),
            Err(e) => (502, json!({ "error": e })),
        },

        ("GET", tx) if tx.starts_with("/tx/") => {
            let text = &tx["/tx/".len()..];
            match Txid::from_str(text) {
                Err(e) => (400, json!({ "error": format!("bad txid: {e}") })),
                Ok(txid) => match node.transaction(&txid) {
                    Ok(Some(tx)) => (
                        200,
                        json!({
                            "txid": txid.to_string(),
                            "confirmations": tx.confirmations,
                            "hex": tx.hex,
                        }),
                    ),
                    Ok(None) => (404, json!({ "error": "unknown transaction" })),
                    Err(e) => (502, json!({ "error": e })),
                },
            }
        }

        ("POST", "/tx") => match hex::decode(body.trim()) {
            Err(e) => (400, json!({ "error": format!("bad hex: {e}") })),
            Ok(bytes) => match bitcoin::consensus::deserialize::<Transaction>(&bytes) {
                Err(e) => (400, json!({ "error": format!("bad transaction: {e}") })),
                Ok(tx) => match node.broadcast(&tx) {
                    Ok(txid) => (200, json!({ "txid": txid.to_string() })),
                    Err(e) => (400, json!({ "error": e })),
                },
            },
        },

        _ => (404, json!({ "error": "not found" })),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "OK",
    }
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
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeNode {
        height: u32,
        transactions: Vec<(Txid, RawTransaction)>,
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

    #[test]
    fn serves_the_height() {
        let node = FakeNode {
            height: 4321,
            ..Default::default()
        };
        let (status, body) = route(&node, "GET", "/height", "");
        assert_eq!(status, 200);
        assert_eq!(body["height"], 4321);
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

        let (status, body) = route(&node, "GET", &format!("/tx/{txid}"), "");
        assert_eq!(status, 200);
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
        let (status, _) = route(&node, "GET", "/height?cachebust=1", "");
        assert_eq!(status, 200);
    }

    #[test]
    fn reports_an_unknown_transaction() {
        let node = FakeNode::default();
        let (status, body) = route(&node, "GET", &format!("/tx/{}", "ab".repeat(32)), "");
        assert_eq!(status, 404);
        assert!(body["error"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn rejects_a_malformed_txid() {
        let node = FakeNode::default();
        let (status, _) = route(&node, "GET", "/tx/not-a-txid", "");
        assert_eq!(status, 400);
    }

    #[test]
    fn broadcasts_raw_hex() {
        let node = FakeNode::default();
        let hex = bitcoin::consensus::encode::serialize_hex(&sample_tx());

        let (status, body) = route(&node, "POST", "/tx", &hex);
        assert_eq!(status, 200);
        assert_eq!(
            body["txid"].as_str().unwrap(),
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
        let (status, body) = route(&node, "POST", "/tx", &hex);
        assert_eq!(status, 400);
        assert_eq!(body["error"], "rejected");
    }

    #[test]
    fn rejects_rubbish_on_the_broadcast_route() {
        let node = FakeNode::default();
        assert_eq!(route(&node, "POST", "/tx", "not hex").0, 400);
        assert_eq!(route(&node, "POST", "/tx", "00").0, 400);
    }

    #[test]
    fn preflight_and_unknown_routes() {
        let node = FakeNode::default();
        assert_eq!(route(&node, "OPTIONS", "/tx", "").0, 204);
        assert_eq!(route(&node, "GET", "/nothing", "").0, 404);
    }
}
