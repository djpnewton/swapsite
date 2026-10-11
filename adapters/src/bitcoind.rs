//! Minimal Bitcoin Core JSON-RPC client.
//!
//! Deliberately small: a plain HTTP POST over a `TcpStream`, no TLS and no HTTP
//! client crate. Bitcoin Core's RPC on a local or trusted host is plain HTTP,
//! and that is the only transport this needs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::str::FromStr;

use base64::Engine as _;
use bitcoin::{Address, Amount, Network, Transaction, Txid};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use swapsite_core::chain::{Chain, ChainError};

pub struct BitcoinRpc {
    host: String,
    port: u16,
    path: String,
    auth: String,
    network: Network,
}

impl BitcoinRpc {
    /// `url` is `http://user:password@host:port`, with no trailing path.
    ///
    /// `wallet` selects a wallet via Core's `/wallet/<name>` URL prefix, which
    /// is what the wallet-scoped calls (`getnewaddress`, `sendrawtransaction`
    /// from a wallet) need.
    pub fn new(url: &str, wallet: Option<&str>, network: Network) -> Result<Self, ChainError> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| ChainError::Rpc(format!("only http:// is supported, got {url}")))?;
        let (credentials, host_port) = rest.rsplit_once('@').ok_or_else(|| {
            ChainError::Rpc("url must look like http://user:password@host:port".to_owned())
        })?;
        let (host, port) = host_port
            .split_once(':')
            .ok_or_else(|| ChainError::Rpc("url needs an explicit port".to_owned()))?;
        let port: u16 = port
            .parse()
            .map_err(|_| ChainError::Rpc(format!("bad port `{port}`")))?;

        Ok(Self {
            host: host.to_owned(),
            port,
            path: match wallet {
                Some(name) => format!("/wallet/{name}"),
                None => "/".to_owned(),
            },
            auth: base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes()),
            network,
        })
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, ChainError> {
        let request = json!({
            "jsonrpc": "1.0",
            "id": "swapsite",
            "method": method,
            "params": params,
        })
        .to_string();

        let response = String::from_utf8_lossy(&self.post(&request)?).into_owned();
        let value: Value = serde_json::from_str(&response)
            .map_err(|e| ChainError::Rpc(format!("{method}: unreadable response: {e}")))?;

        if let Some(error) = value.get("error") {
            if !error.is_null() {
                return Err(ChainError::Node {
                    method: method.to_owned(),
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                });
            }
        }

        let result = value.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result)
            .map_err(|e| ChainError::Rpc(format!("{method}: unexpected result: {e}")))
    }

    fn post(&self, body: &str) -> Result<Vec<u8>, ChainError> {
        let connect = format!("{}:{}", self.host, self.port);
        let mut stream = TcpStream::connect(&connect)
            .map_err(|e| ChainError::Rpc(format!("connect {connect}: {e}")))?;

        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {connect}\r\nAuthorization: Basic {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.path,
            self.auth,
            body.len()
        );

        let write = |stream: &mut TcpStream| -> std::io::Result<()> {
            stream.write_all(head.as_bytes())?;
            stream.write_all(body.as_bytes())?;
            stream.flush()
        };
        write(&mut stream).map_err(|e| ChainError::Rpc(format!("send: {e}")))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| ChainError::Rpc(format!("read: {e}")))?;

        let (status, body) = split_response(&raw)?;
        if !status.contains(" 200 ") {
            return Err(ChainError::Rpc(format!(
                "http status {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        Ok(body)
    }
}

impl Chain for BitcoinRpc {
    fn height(&self) -> Result<u32, ChainError> {
        self.call("getblockcount", json!([]))
    }

    fn confirmations(&self, txid: &Txid) -> Result<u32, ChainError> {
        match self.call::<Value>("getrawtransaction", json!([txid.to_string(), true])) {
            Ok(value) => Ok(value
                .get("confirmations")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32),
            // -5 is Core's "no such mempool or blockchain transaction", which
            // for a deposit we just broadcast simply means zero confirmations.
            Err(ChainError::Node { code: -5, .. }) => Ok(0),
            Err(other) => Err(other),
        }
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Txid, ChainError> {
        let hex = bitcoin::consensus::encode::serialize_hex(tx);
        self.call("sendrawtransaction", json!([hex]))
    }

    fn fund(&self, to: &Address, amount: Amount) -> Result<Transaction, ChainError> {
        let outputs = json!([{ to.to_string(): amount.to_btc() }]);
        let unsigned: String = self.call("createrawtransaction", json!([[], outputs]))?;

        let funded: Value = self.call("fundrawtransaction", json!([unsigned]))?;
        let funded_hex = funded
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| ChainError::Rpc("fundrawtransaction returned no hex".to_owned()))?;

        let signed: Value = self.call("signrawtransactionwithwallet", json!([funded_hex]))?;
        if signed.get("complete").and_then(Value::as_bool) != Some(true) {
            return Err(ChainError::Rpc(
                "wallet could not fully sign the deposit".to_owned(),
            ));
        }
        let signed_hex = signed.get("hex").and_then(Value::as_str).ok_or_else(|| {
            ChainError::Rpc("signrawtransactionwithwallet returned no hex".to_owned())
        })?;

        let bytes = hex_decode(signed_hex)?;
        bitcoin::consensus::deserialize(&bytes)
            .map_err(|e| ChainError::Rpc(format!("signed deposit does not decode: {e}")))
    }

    fn new_address(&self) -> Result<Address, ChainError> {
        let address: String = self.call("getnewaddress", json!([]))?;
        Address::from_str(&address)
            .and_then(|address| address.require_network(self.network))
            .map_err(|e| ChainError::Rpc(format!("node returned a bad address: {e}")))
    }
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, ChainError> {
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(hex.get(i..i + 2).unwrap_or(""), 16)
                .map_err(|_| ChainError::Rpc(format!("bad hex from node: {hex}")))
        })
        .collect()
}

/// Split an HTTP response into its status line and body, de-chunking if needed.
fn split_response(raw: &[u8]) -> Result<(String, Vec<u8>), ChainError> {
    let split = find(raw, b"\r\n\r\n")
        .ok_or_else(|| ChainError::Rpc("malformed HTTP response".to_owned()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = &raw[split + 4..];

    let status = head.lines().next().unwrap_or_default().to_owned();

    if head.contains("transfer-encoding: chunked") {
        return Ok((status, dechunk(body)?));
    }
    Ok((status, body.to_vec()))
}

fn dechunk(body: &[u8]) -> Result<Vec<u8>, ChainError> {
    let mut out = Vec::new();
    let mut rest = body;

    loop {
        let end =
            find(rest, b"\r\n").ok_or_else(|| ChainError::Rpc("bad chunk header".to_owned()))?;
        let header = std::str::from_utf8(&rest[..end])
            .map_err(|_| ChainError::Rpc("bad chunk header".to_owned()))?;
        let size_text = header.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| ChainError::Rpc(format!("bad chunk size `{size_text}`")))?;
        if size == 0 {
            return Ok(out);
        }

        let start = end + 2;
        let chunk = rest
            .get(start..start + size)
            .ok_or_else(|| ChainError::Rpc("truncated chunk".to_owned()))?;
        out.extend_from_slice(chunk);

        rest = &rest[start + size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_url() {
        let rpc = BitcoinRpc::new(
            "http://bitcoin:bitcoin@bitcoind:18443",
            Some("swap"),
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(rpc.host, "bitcoind");
        assert_eq!(rpc.port, 18443);
        assert_eq!(rpc.path, "/wallet/swap");
        assert_eq!(rpc.auth, "Yml0Y29pbjpiaXRjb2lu");
    }

    #[test]
    fn rejects_a_url_without_credentials_or_port() {
        assert!(BitcoinRpc::new("http://bitcoind:18443", None, Network::Regtest).is_err());
        assert!(BitcoinRpc::new("https://u:p@h:1", None, Network::Regtest).is_err());
        assert!(BitcoinRpc::new("http://u:p@h", None, Network::Regtest).is_err());
    }

    #[test]
    fn splits_a_plain_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let (status, body) = split_response(raw).unwrap();
        assert!(status.contains("200"));
        assert_eq!(body, b"hi");
    }

    #[test]
    fn dechunks_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n";
        let (_, body) = split_response(raw).unwrap();
        assert_eq!(body, b"hi");
    }
}
