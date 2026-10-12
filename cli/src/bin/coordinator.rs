//! `coordinator` - the website's service.
//!
//! It quotes, it asks the maker for swaps, it remembers what a taker needs to
//! recover, and it serves the page.
//!
//! It holds no keys and no funds, and it is not on the critical path: a taker
//! that already has a deposit and a preimage can claim through any relay without
//! it, and it cannot forge a deposit because the taker checks the script itself.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::json;
use swapsite_adapters::{
    fetch,
    http::{content_type_for, serve, Request, Response},
    LnCli,
};
use swapsite_core::{lightning::LightningNode, maker_ln2btc::Ln2btcPublicSwap};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "coordinator", about = "Quote, broker and serve swaps")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the API and the page.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// The maker's control plane.
        #[arg(long, default_value = "http://127.0.0.1:3200")]
        maker_url: String,
        /// Where the page sends takers to read the chain and broadcast.
        #[arg(long, default_value = "http://127.0.0.1:3100")]
        relay_url: String,
        /// Where swap records are kept, for a taker that comes back later.
        #[arg(long, default_value = "coordinator-swaps")]
        dir: PathBuf,
        /// Directory of static files to serve.
        #[arg(long, default_value = "web")]
        web: PathBuf,
        /// The compiled taker module, served at `/swapsite_wasm.wasm`.
        #[arg(
            long,
            default_value = "target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
        )]
        wasm: PathBuf,
        /// The maker's fee, in basis points of the amount.
        #[arg(long, default_value_t = 10)]
        fee_bps: u64,
        /// The smallest fee to charge, in satoshis.
        #[arg(long, default_value_t = 1)]
        min_fee_sat: u64,
        /// What the taker is told a claim will cost.
        #[arg(long, default_value_t = 2_000)]
        claim_fee: u64,
        /// What the taker pays up front to the maker, in satoshis.
        ///
        /// The maker's cost recovery, not a margin: it covers the deposit and
        /// refund transactions, and it is what stops an abandoned swap from
        /// being free. Quoted so the page can show the real total.
        #[arg(long, default_value_t = 4_000)]
        bond_fee: u64,
        #[arg(long, default_value = "regtest")]
        network: String,
        /// Compose file for the taker's Lightning node.
        #[arg(
            long,
            env = "SWAPSITE_COMPOSE",
            default_value = "docker/docker-compose.yml"
        )]
        compose: PathBuf,
        /// `host:port` of the taker's gRPC port, for `--dev-pay`.
        ///
        /// Set with `--lnd-dir` it runs `lncli` directly, which is what a
        /// container has to do: there is no docker in there to exec into.
        #[arg(long)]
        lnd_rpc: Option<String>,
        /// Root holding the taker node's `tls.cert` and macaroon.
        #[arg(long)]
        lnd_dir: Option<PathBuf>,
        /// Expose a dev endpoint that pays the taker's invoice on its behalf.
        ///
        /// A real deployment does not have this: the page pays through WebLN and
        /// the preimage never reaches the coordinator. Off unless asked for.
        #[arg(long)]
        dev_pay: bool,
    },

    /// Print the quote for an amount.
    Quote {
        #[arg(long)]
        amount: u64,
        #[arg(long, default_value_t = 10)]
        fee_bps: u64,
        #[arg(long, default_value_t = 1)]
        min_fee_sat: u64,
        #[arg(long, default_value_t = 4_000)]
        bond_fee: u64,
    },
}

/// The maker's cut: a share of the amount, with a floor.
fn fee_for(amount: u64, fee_bps: u64, min_fee_sat: u64) -> u64 {
    (amount * fee_bps / 10_000).max(min_fee_sat)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Quote {
            amount,
            fee_bps,
            min_fee_sat,
            bond_fee,
        } => {
            let fee = fee_for(amount, fee_bps, min_fee_sat);
            println!("amount_sat={amount}");
            println!("fee_sat={fee}");
            println!("invoice_sat={}", amount + fee);
            println!("bond_sat={bond_fee}");
            println!("total_sat={}", amount + fee + bond_fee);
            Ok(())
        }

        Cmd::Serve {
            listen,
            maker_url,
            relay_url,
            dir,
            web,
            wasm,
            fee_bps,
            min_fee_sat,
            claim_fee,
            bond_fee,
            network,
            compose,
            lnd_rpc,
            lnd_dir,
            dev_pay,
        } => {
            // The dev payer is the taker's own node, on the same network.
            let dev_pay = dev_pay
                .then(|| {
                    LnCli::from_config(
                        lnd_rpc.as_deref(),
                        lnd_dir.as_deref(),
                        compose,
                        "taker",
                        &network,
                    )
                })
                .transpose()
                .map_err(|e| format!("{e} (needed for --dev-pay)"))?;
            let service = Service {
                maker_url,
                relay_url,
                dir,
                web,
                wasm,
                fee_bps,
                min_fee_sat,
                claim_fee,
                bond_fee,
                network,
                dev_pay,
            };
            serve(&listen, move |request| service.route(&request))?;
            Ok(())
        }
    }
}

struct Service {
    maker_url: String,
    relay_url: String,
    dir: PathBuf,
    web: PathBuf,
    wasm: PathBuf,
    fee_bps: u64,
    min_fee_sat: u64,
    claim_fee: u64,
    /// What the taker pays the maker up front to start a swap.
    bond_fee: u64,
    network: String,
    /// Present only with `--dev-pay`.
    dev_pay: Option<LnCli>,
}

impl Service {
    fn fee(&self, amount: u64) -> u64 {
        fee_for(amount, self.fee_bps, self.min_fee_sat)
    }

    fn route(&self, request: &Request) -> Response {
        if request.matches("GET", "/config").is_some() {
            return self.config_view();
        }
        if request.matches("GET", "/quote").is_some() {
            return self.quote_view(request);
        }
        if request.matches("GET", "/swaps").is_some() {
            return self.list(request);
        }
        if request.matches("POST", "/swaps").is_some() {
            return self.create(request);
        }
        if let Some(captures) = request.matches("GET", "/swaps/:id") {
            return self.show(&captures[0]);
        }
        if let Some(captures) = request.matches("POST", "/dev/pay/:id") {
            return self.pay(&captures[0]);
        }
        if request.matches("POST", "/btc2ln/offer").is_some() {
            return self.forward("POST", "/btc2ln/offer", &request.body);
        }
        if request.matches("POST", "/btc2ln/funded").is_some() {
            return self.forward("POST", "/btc2ln/funded", &request.body);
        }
        if request.matches("GET", "/btc2ln/swaps").is_some() {
            return self.forward("GET", "/btc2ln/swaps", "");
        }

        self.file(&request.path)
    }

    /// What the page needs to know before it starts.
    fn config_view(&self) -> Response {
        Response::json(
            200,
            &json!({
                "network": self.network,
                "relay_url": self.relay_url,
                "fee_bps": self.fee_bps,
                "min_fee_sat": self.min_fee_sat,
                "claim_fee_sat": self.claim_fee,
                "bond_sat": self.bond_fee,
                "dev_pay": self.dev_pay.is_some(),
            }),
        )
    }

    fn quote_view(&self, request: &Request) -> Response {
        let Some(amount) = request.query("amount").and_then(|a| a.parse::<u64>().ok()) else {
            return Response::error(400, "amount must be a number of satoshis");
        };

        let fee = self.fee(amount);
        Response::json(
            200,
            &json!({
                "amount_sat": amount,
                "fee_sat": fee,
                "invoice_sat": amount + fee,
                "claim_fee_sat": self.claim_fee,
                "bond_sat": self.bond_fee,
                "total_sat": amount + fee + self.bond_fee,
            }),
        )
    }

    /// Every swap this taker has started, so a returned page can pick them up.
    ///
    /// Each one is refreshed from the maker first. A record written at creation
    /// says `created` forever otherwise, and a taker's verdict leans on the
    /// state: a settled swap with no preimage means "go and find it", which
    /// `created` would hide behind "the maker has not funded yet".
    fn list(&self, request: &Request) -> Response {
        let wanted = request.query("claim_pubkey");

        let recorded = match self.load_all() {
            Ok(swaps) => swaps,
            Err(e) => return Response::error(500, e.to_string()),
        };

        let swaps: Vec<Ln2btcPublicSwap> = recorded
            .into_iter()
            .filter(|swap| wanted.is_none_or(|wanted| swap.claim_pubkey == wanted))
            .map(|swap| self.fresh(&swap.id).unwrap_or(swap))
            .collect();

        Response::json(200, &json!({ "swaps": swaps }))
    }

    /// The maker's view of a swap, if it can be had. Anything learned is saved.
    fn fresh(&self, id: &str) -> Option<Ln2btcPublicSwap> {
        let response = fetch("GET", &format!("{}/swaps/{id}", self.maker_url), None).ok()?;
        if response.status != 200 {
            return None;
        }

        let swap: Ln2btcPublicSwap = serde_json::from_str(&response.body).ok()?;
        let _ = self.save(&swap);
        Some(swap)
    }

    fn create(&self, request: &Request) -> Response {
        #[derive(Deserialize)]
        struct Ask {
            amount: u64,
            claim_pubkey: String,
        }

        let ask: Ask = match serde_json::from_str(&request.body) {
            Ok(ask) => ask,
            Err(e) => return Response::error(400, format!("bad request: {e}")),
        };

        // The fee the taker was quoted is the fee the maker is asked for.
        let body = json!({
            "amount": ask.amount,
            "fee": self.fee(ask.amount),
            "claim_pubkey": ask.claim_pubkey,
            "claim_fee": self.claim_fee,
            "bond_fee": self.bond_fee,
        })
        .to_string();

        let swap = match self.ask_maker(&body) {
            Ok(swap) => swap,
            Err(e) => return Response::error(502, e),
        };

        // Recorded before it is returned: a taker that never gets the response
        // can still find its swap afterwards.
        if let Err(e) = self.save(&swap) {
            eprintln!("[coordinator] could not record {}: {e}", swap.id);
        }

        Response::json(201, &json!(swap))
    }

    fn show(&self, id: &str) -> Response {
        // The maker's view is the truth when it is reachable.
        if let Some(swap) = self.fresh(id) {
            return Response::json(200, &json!(swap));
        }

        match self.load(id) {
            Ok(swap) => Response::json(200, &json!(swap)),
            Err(e) => Response::error(404, e.to_string()),
        }
    }

    /// Dev only: pay what the taker's wallet would pay next.
    ///
    /// This stands in for the taker's own wallet. It is why the endpoint is off
    /// by default: with it on, the coordinator learns a preimage it would never
    /// see in production. Before the deposit exists the next thing to pay is the
    /// bond, which reveals nothing, so that case answers with no preimage.
    fn pay(&self, id: &str) -> Response {
        let Some(lightning) = &self.dev_pay else {
            return Response::error(404, "dev pay is not enabled");
        };

        let swap = match self.load(id) {
            Ok(swap) => swap,
            Err(e) => return Response::error(404, e.to_string()),
        };

        if swap.deposit_txid.is_none() {
            return match lightning.pay(&swap.bond_bolt11) {
                Ok(_) => Response::json(200, &json!({ "paid": "bond" })),
                Err(e) => Response::error(502, e.to_string()),
            };
        }

        // Blocks until the maker settles, which is the point: the response is
        // the preimage.
        match lightning.pay(&swap.bolt11) {
            Ok(preimage) => Response::json(200, &json!({ "preimage": preimage.to_hex() })),
            Err(e) => Response::error(502, e.to_string()),
        }
    }

    /// Pass a request to the maker's control plane and hand its answer back.
    ///
    /// The page is served from one origin, so it cannot reach the maker for
    /// itself. Nothing is interpreted on the way through: the maker's status
    /// and body are what the page sees, so a refusal reads the same either way.
    fn forward(&self, method: &str, path: &str, body: &str) -> Response {
        let url = format!("{}{path}", self.maker_url);
        let payload = (method != "GET").then_some(body);

        match fetch(method, &url, payload) {
            Ok(response) => Response::bytes(
                response.status,
                "application/json",
                response.body.into_bytes(),
            ),
            Err(e) => Response::error(502, format!("could not reach the maker: {e}")),
        }
    }

    fn ask_maker(&self, body: &str) -> std::result::Result<Ln2btcPublicSwap, String> {
        let response = fetch("POST", &format!("{}/swaps", self.maker_url), Some(body))
            .map_err(|e| format!("could not reach the maker: {e}"))?;

        if response.status != 201 && response.status != 200 {
            return Err(format!("maker refused: {}", response.body));
        }

        serde_json::from_str(&response.body).map_err(|e| format!("maker sent an odd swap: {e}"))
    }

    /// Static files, including the compiled taker module.
    fn file(&self, path: &str) -> Response {
        let name = path.trim_start_matches('/');

        if name == "swapsite_wasm.wasm" {
            return match std::fs::read(&self.wasm) {
                Ok(bytes) => Response::bytes(200, "application/wasm", bytes),
                Err(e) => Response::error(
                    500,
                    format!("no taker module at {}: {e}", self.wasm.display()),
                ),
            };
        }

        let name = if name.is_empty() { "index.html" } else { name };

        // No traversal, no absolute paths, no hidden files.
        let suspicious = name
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.contains('\\'));
        if suspicious {
            return Response::not_found();
        }

        match std::fs::read(self.web.join(name)) {
            Ok(bytes) => Response::bytes(200, content_type_for(name), bytes),
            Err(_) => Response::error(404, format!("no {name}")),
        }
    }

    fn save(&self, swap: &Ln2btcPublicSwap) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}.json", swap.id));
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_string_pretty(swap)? + "\n")?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Ln2btcPublicSwap> {
        let text = std::fs::read_to_string(self.dir.join(format!("{id}.json")))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn load_all(&self) -> Result<Vec<Ln2btcPublicSwap>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let mut ids: Vec<String> = std::fs::read_dir(&self.dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("json"))
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .collect();
        ids.sort();

        let mut swaps = Vec::new();
        for id in ids {
            swaps.push(self.load(&id)?);
        }
        Ok(swaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dev_pay: bool) -> Service {
        // `cargo test` runs with the crate root as the working directory, so the
        // paths the binary uses at runtime have to be anchored here.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");

        Service {
            maker_url: "http://127.0.0.1:1".to_owned(),
            relay_url: "http://127.0.0.1:3100".to_owned(),
            dir: std::env::temp_dir().join(format!("coordinator-test-{}", std::process::id())),
            web: root.join("web"),
            wasm: root.join("target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"),
            fee_bps: 10,
            min_fee_sat: 1,
            claim_fee: 2_000,
            bond_fee: 4_000,
            network: "regtest".to_owned(),
            dev_pay: dev_pay.then(|| {
                LnCli::new(
                    PathBuf::from("docker/docker-compose.yml"),
                    "taker",
                    "regtest",
                )
            }),
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

    fn json_of(response: Response) -> serde_json::Value {
        serde_json::from_slice(&response.body).expect("body is JSON")
    }

    #[test]
    fn the_fee_is_a_share_with_a_floor() {
        assert_eq!(fee_for(30_000, 10, 1), 30);
        assert_eq!(fee_for(10, 10, 1), 1);
        assert_eq!(fee_for(0, 10, 1), 1);
        assert_eq!(fee_for(1_000_000, 10, 1), 1_000);
    }

    #[test]
    fn a_quote_says_what_the_taker_pays() {
        let response = service(false).route(&request("GET", "/quote?amount=30000", ""));
        assert_eq!(response.status, 200);

        let body = json_of(response);
        assert_eq!(body["amount_sat"], 30_000);
        assert_eq!(body["fee_sat"], 30);
        assert_eq!(body["invoice_sat"], 30_030);
    }

    #[test]
    fn a_quote_needs_an_amount() {
        let response = service(false).route(&request("GET", "/quote", ""));
        assert_eq!(response.status, 400);
        assert_eq!(
            service(false)
                .route(&request("GET", "/quote?amount=abc", ""))
                .status,
            400
        );
    }

    #[test]
    fn the_config_tells_the_page_where_the_relay_is() {
        let body = json_of(service(false).route(&request("GET", "/config", "")));
        assert_eq!(body["relay_url"], "http://127.0.0.1:3100");
        assert_eq!(body["network"], "regtest");
        assert_eq!(body["dev_pay"], false);

        let enabled = json_of(service(true).route(&request("GET", "/config", "")));
        assert_eq!(enabled["dev_pay"], true);
    }

    #[test]
    fn dev_pay_is_absent_unless_asked_for() {
        let response = service(false).route(&request("POST", "/dev/pay/anything", ""));
        assert_eq!(response.status, 404);
    }

    #[test]
    fn creating_a_swap_without_a_maker_says_so() {
        // The maker is at port 1: the coordinator must report that, not panic.
        let response = service(false).route(&request(
            "POST",
            "/swaps",
            r#"{"amount":30000,"claim_pubkey":"02abcd"}"#,
        ));
        assert_eq!(response.status, 502);
        assert!(json_of(response)["error"]
            .as_str()
            .unwrap()
            .contains("could not reach the maker"));
    }

    #[test]
    fn creating_a_swap_needs_a_body() {
        let response = service(false).route(&request("POST", "/swaps", "{}"));
        assert_eq!(response.status, 400);
    }

    #[test]
    fn serving_files_refuses_to_walk_out_of_the_directory() {
        let service = service(false);
        for path in [
            "/../Cargo.toml",
            "/web/../../Cargo.toml",
            "//etc/passwd",
            "/.hidden",
        ] {
            assert_eq!(
                service.route(&request("GET", path, "")).status,
                404,
                "{path}"
            );
        }
    }

    #[test]
    fn serving_a_file_that_is_there() {
        // `web/` really does contain these.
        let service = service(false);
        assert_eq!(service.route(&request("GET", "/", "")).status, 200);
        assert_eq!(service.route(&request("GET", "/app.js", "")).status, 200);
        assert_eq!(service.route(&request("GET", "/nope.js", "")).status, 404);
    }

    #[test]
    fn unknown_api_routes_are_not_found() {
        let service = service(false);
        assert_eq!(service.route(&request("GET", "/nonsense", "")).status, 404);
        assert_eq!(service.route(&request("DELETE", "/swaps", "")).status, 404);
    }
}
