//! `regtestd` - a control panel for the regtest network.
//!
//! Development only, and deliberately its own binary: it mines blocks and reads
//! node wallets, which the coordinator must never be able to do. It exists so a
//! browser-side swap can be driven without shelling into a container to mine a
//! block and see what changed.
//!
//! ```text
//! GET  /status        height, balances, channel capacity, swap records
//! POST /mine          {"blocks": N}
//! GET  /              the panel
//! ```
//!
//! Nothing here is on the swap path. Losing it costs a convenience, not a swap.

use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use swapsite_adapters::{
    http::{serve, Request, Response},
    store::SwapStore,
    BitcoinRpc, LnCli,
};
use swapsite_core::{chain::Chain, lightning::LightningNode};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The most blocks one request may mine, so a typo cannot spin for minutes.
const MAX_BLOCKS: u32 = 200;

#[derive(Parser)]
#[command(name = "regtestd", about = "Control panel for the regtest network")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8091")]
    listen: String,

    /// Bitcoin Core RPC URL. Plain HTTP, credentials inline.
    #[arg(
        long,
        env = "SWAPSITE_BITCOIN_URL",
        default_value = "http://bitcoin:bitcoin@127.0.0.1:18443"
    )]
    bitcoin_url: String,

    /// Wallet to mine to and read the balance from.
    #[arg(long, default_value = "swap")]
    wallet: String,

    #[arg(long, default_value = "regtest")]
    network: String,

    /// The maker's store, read only, for the swap list.
    #[arg(long, default_value = "swaps")]
    dir: PathBuf,

    /// Static files: this panel's page, and the stylesheet it borrows.
    #[arg(long, default_value = "web")]
    web: PathBuf,

    /// Compose file, for a host that has docker to exec `lncli` inside.
    #[arg(
        long,
        env = "SWAPSITE_COMPOSE",
        default_value = "docker/docker-compose.yml"
    )]
    compose: PathBuf,

    /// `host:port` of the maker's gRPC port. Set with `--maker-lnd-dir` it runs
    /// `lncli` directly, which is what a container has to do.
    #[arg(long)]
    maker_lnd_rpc: Option<String>,
    /// Root holding the maker's `tls.cert` and macaroon.
    #[arg(long)]
    maker_lnd_dir: Option<PathBuf>,

    #[arg(long)]
    taker_lnd_rpc: Option<String>,
    #[arg(long)]
    taker_lnd_dir: Option<PathBuf>,

    /// Where the taker page is served, so the panel can link to it.
    #[arg(long)]
    taker_url: Option<String>,
}

impl Cli {
    fn chain(&self) -> Result<BitcoinRpc> {
        Ok(BitcoinRpc::new(
            &self.bitcoin_url,
            Some(&self.wallet),
            self.network()?,
        )?)
    }

    fn network(&self) -> Result<bitcoin::Network> {
        match self.network.as_str() {
            "bitcoin" | "mainnet" => Ok(bitcoin::Network::Bitcoin),
            "testnet" => Ok(bitcoin::Network::Testnet),
            "signet" => Ok(bitcoin::Network::Signet),
            "regtest" => Ok(bitcoin::Network::Regtest),
            other => Err(format!("unknown network `{other}`").into()),
        }
    }

    /// `maker` or `taker`.
    fn node(&self, role: &str) -> Result<LnCli> {
        let (rpc, root) = match role {
            "maker" => (&self.maker_lnd_rpc, &self.maker_lnd_dir),
            "taker" => (&self.taker_lnd_rpc, &self.taker_lnd_dir),
            other => return Err(format!("no such node `{other}`").into()),
        };

        Ok(LnCli::from_config(
            rpc.as_deref(),
            root.as_deref(),
            self.compose.clone(),
            role,
            &self.network,
        )?)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let listen = cli.listen.clone();
    serve(&listen, move |request| route(&cli, &request))?;
    Ok(())
}

/// Route one request. Pure apart from the node calls, and tested as such.
fn route(cli: &Cli, request: &Request) -> Response {
    if request.matches("GET", "/status").is_some() {
        return Response::json(200, &status(cli));
    }

    if request.matches("POST", "/mine").is_some() {
        return mine(cli, request);
    }

    file(cli, &request.path)
}

/// Everything the panel shows, in one round trip.
///
/// Each section reports its own failure rather than failing the whole response:
/// the panel is most useful exactly when something is broken, and a single dead
/// node should not blank the page.
fn status(cli: &Cli) -> Value {
    json!({
        "height": match cli.chain().and_then(|chain| Ok(chain.height()?)) {
            Ok(height) => json!(height),
            Err(e) => json!({ "error": e.to_string() }),
        },
        "bitcoin": section(|| {
            let chain = cli.chain()?;
            Ok(json!({ "sat": chain.balance()?.to_sat() }))
        }),
        "nodes": {
            "maker": node_status(cli, "maker"),
            "taker": node_status(cli, "taker"),
        },
        "swaps": swaps(cli),
        "taker_url": cli.taker_url,
    })
}

fn node_status(cli: &Cli, role: &str) -> Value {
    section(|| {
        let node = cli.node(role)?;
        let info = node.info()?;
        let capacity = node.capacity()?;
        Ok(json!({
            "alias": info.alias,
            "pubkey": info.pubkey,
            "outbound_sat": capacity.outbound_sat,
            "inbound_sat": capacity.inbound_sat,
            "channels": capacity.channels,
        }))
    })
}

/// The maker's swaps, as the panel needs them.
fn swaps(cli: &Cli) -> Value {
    section(|| {
        let store = SwapStore::open(&cli.dir)?;
        let listed: Vec<Value> = store
            .all()?
            .iter()
            .map(|record| {
                json!({
                    "id": record.id,
                    "state": record.state.to_string(),
                    "amount_sat": record.amount_sat,
                    "deposit_sat": record.deposit_sat,
                    "locktime": record.locktime,
                })
            })
            .collect();
        Ok(json!(listed))
    })
}

/// Run a piece of the status and turn either outcome into JSON.
fn section<F>(build: F) -> Value
where
    F: FnOnce() -> Result<Value>,
{
    match build() {
        Ok(value) => value,
        Err(e) => json!({ "error": e.to_string() }),
    }
}

#[derive(Deserialize)]
struct Mine {
    #[serde(default = "one_block")]
    blocks: u32,
}

fn one_block() -> u32 {
    1
}

fn mine(cli: &Cli, request: &Request) -> Response {
    let asked: Mine = if request.body.trim().is_empty() {
        Mine { blocks: 1 }
    } else {
        match serde_json::from_str(&request.body) {
            Ok(asked) => asked,
            Err(e) => return Response::error(400, format!("bad request: {e}")),
        }
    };

    if asked.blocks == 0 || asked.blocks > MAX_BLOCKS {
        return Response::error(400, format!("blocks must be between 1 and {MAX_BLOCKS}"));
    }

    match cli
        .chain()
        .and_then(|chain| Ok(chain.generate(asked.blocks)?))
    {
        Ok(txids) => Response::json(
            200,
            &json!({ "mined": txids.len(), "height": cli.chain().ok().and_then(|c| c.height().ok()) }),
        ),
        Err(e) => Response::error(502, e.to_string()),
    }
}

/// Static files. Same refusals as the coordinator: no traversal, no absolute
/// paths, no hidden files.
fn file(cli: &Cli, path: &str) -> Response {
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() {
        "regtest.html"
    } else {
        name
    };

    let suspicious = name
        .split('/')
        .any(|part| part.is_empty() || part.starts_with('.') || part.contains('\\'));
    if suspicious || name.contains("..") {
        return Response::not_found();
    }

    match std::fs::read(cli.web.join(name)) {
        Ok(bytes) => Response::bytes(200, content_type(name), bytes),
        Err(_) => Response::not_found(),
    }
}

/// Three types are enough for one page with no dependencies.
fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> Cli {
        Cli {
            listen: "127.0.0.1:0".to_owned(),
            // Nothing here reaches a node: these tests are about routing and
            // about what the panel refuses to serve.
            bitcoin_url: "http://bitcoin:bitcoin@127.0.0.1:1".to_owned(),
            wallet: "swap".to_owned(),
            network: "regtest".to_owned(),
            dir: PathBuf::from("/nonexistent-swaps"),
            web: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../web"),
            compose: PathBuf::from("docker/docker-compose.yml"),
            maker_lnd_rpc: None,
            maker_lnd_dir: None,
            taker_lnd_rpc: None,
            taker_lnd_dir: None,
            taker_url: Some("http://127.0.0.1:8095/".to_owned()),
        }
    }

    fn get(path: &str) -> Request {
        Request {
            method: "GET".to_owned(),
            path: path.to_owned(),
            query: Vec::new(),
            body: String::new(),
        }
    }

    #[test]
    fn the_panel_serves_its_page_at_the_root() {
        let response = route(&cli(), &get("/"));
        assert_eq!(response.status, 200);
        assert!(response.content_type.starts_with("text/html"));
    }

    #[test]
    fn a_configured_node_builds() {
        // Configuring an endpoint has to be enough to switch transport, or a
        // container silently execs a docker that is not there. Which transport
        // results is the adapter's test; this is about wiring a role to it.
        let mut panel = cli();
        panel.maker_lnd_rpc = Some("lnd-maker:10009".to_owned());
        panel.maker_lnd_dir = Some(PathBuf::from("/lnd-maker"));

        assert!(panel.node("maker").is_ok());
        assert!(panel.node("taker").is_ok(), "unconfigured roles fall back");
    }

    #[test]
    fn half_a_direct_config_is_refused_rather_than_quietly_ignored() {
        for (endpoint, root) in [(Some("lnd-maker:10009"), None), (None, Some("/lnd-maker"))] {
            let mut panel = cli();
            panel.taker_lnd_rpc = endpoint.map(str::to_owned);
            panel.taker_lnd_dir = root.map(PathBuf::from);
            assert!(panel.node("taker").is_err(), "{endpoint:?} {root:?}");
        }
    }

    #[test]
    fn an_unknown_role_is_an_error_not_a_default() {
        // The two roles are wired to different nodes; guessing one would read a
        // node nobody asked about.
        assert!(cli().node("treasurer").is_err());
    }

    #[test]
    fn static_serving_refuses_traversal_and_hidden_files() {
        for path in ["/../Cargo.toml", "/web/../Cargo.toml", "/.env", "/a//b"] {
            assert_eq!(route(&cli(), &get(path)).status, 404, "served {path}");
        }
    }

    #[test]
    fn content_types_are_pinned_by_extension() {
        assert!(content_type("regtest.html").starts_with("text/html"));
        assert!(content_type("app.css").starts_with("text/css"));
        assert!(content_type("regtest.js").starts_with("text/javascript"));
        assert_eq!(content_type("something.bin"), "application/octet-stream");
    }

    #[test]
    fn mining_refuses_a_silly_block_count() {
        // Reaching the node would be slow and pointless, so this must be
        // refused before it is called.
        for body in [r#"{"blocks": 0}"#, r#"{"blocks": 100000}"#] {
            let mut request = get("/mine");
            request.method = "POST".to_owned();
            request.body = body.to_owned();
            assert_eq!(route(&cli(), &request).status, 400, "allowed {body}");
        }
    }

    #[test]
    fn an_empty_body_mines_one_block() {
        let mut request = get("/mine");
        request.method = "POST".to_owned();
        request.body = String::new();
        // No node to mine with, so this reports a node error rather than 400:
        // the request itself was understood.
        assert_eq!(route(&cli(), &request).status, 502);
    }

    #[test]
    fn a_dead_node_does_not_blank_the_status() {
        // Every section is asked independently, so the page can show the chain
        // and the swaps even when a node is down.
        let value = status(&cli());
        assert!(value.get("height").is_some());
        assert!(value["nodes"]["maker"].get("error").is_some());
        assert!(value["swaps"].get("error").is_some());
        assert_eq!(value["taker_url"], "http://127.0.0.1:8095/");
    }
}
