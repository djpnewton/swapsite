//! `maker` - the maker daemon and its control plane.
//!
//! Drives the maker's side of reverse submarine swaps from observed reality.
//! Every signed transaction is written to the swap record before it is
//! broadcast, so a crash is always recoverable by starting the daemon again.

use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use bitcoin::{
    consensus::encode::{deserialize, serialize_hex},
    secp256k1::{Secp256k1, SecretKey},
    Address, Amount, Network, PublicKey, Transaction,
};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use swapsite_adapters::{
    http::{serve as serve_http, Request, Response},
    BitcoinRpc, LnCli, SwapStore,
};
use swapsite_core::{
    chain::Chain,
    hash::Preimage,
    htlc::Htlc,
    lightning::{InvoiceState, LightningNode},
    maker::{plan, reconcile, Action, Funding, Observed, PublicSwap, SwapRecord},
    state::{Event, SwapState},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "maker", about = "Maker daemon for trustless swaps")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a swap: fund the on-chain leg and issue the hold invoice.
    NewSwap {
        #[command(flatten)]
        config: NodeConfig,
        /// What the taker pays over Lightning, in satoshis.
        #[arg(long)]
        amount: u64,
        /// The maker's fee, paid by the taker on top of `amount`.
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// The taker's claim key, hex.
        #[arg(long)]
        claim_pubkey: String,
        /// Fee the taker pays to claim, in satoshis. Added to the deposit so
        /// the taker nets exactly `amount`.
        #[arg(long, default_value_t = 2_000)]
        claim_fee: u64,
        /// Blocks from now until the deposit can be refunded.
        #[arg(long, default_value_t = 30)]
        locktime_margin: u32,
        /// Fee for the refund transaction, in satoshis.
        #[arg(long, default_value_t = 2_000)]
        refund_fee: u64,
        /// Bond the taker pays up front, in satoshis. Covers the deposit and
        /// refund transactions, so an abandoned swap is not free.
        #[arg(long, default_value_t = 4_000)]
        bond_fee: u64,
        /// Blocks the taker has to pay the bond in.
        #[arg(long, default_value_t = 30)]
        bond_window: u32,
    },

    /// Run the daemon: watch every swap and act on it.
    Run {
        #[command(flatten)]
        config: NodeConfig,
        /// Run a single tick and exit.
        #[arg(long)]
        once: bool,
    },

    /// Print what is known about the swaps on disk.
    Status {
        #[command(flatten)]
        config: NodeConfig,
        /// Show only this swap.
        #[arg(long)]
        id: Option<String>,
    },

    /// Run the daemon and serve its control plane.
    Serve {
        #[command(flatten)]
        config: NodeConfig,
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:3200")]
        listen: String,
    },
}

#[derive(Args, Clone)]
struct NodeConfig {
    /// Directory holding one JSON file per swap.
    #[arg(long, default_value = "swaps")]
    dir: PathBuf,
    /// Bitcoin Core RPC URL. Plain HTTP, credentials inline.
    #[arg(
        long,
        env = "SWAPSITE_BITCOIN_URL",
        default_value = "http://bitcoin:bitcoin@127.0.0.1:18443"
    )]
    bitcoin_url: String,
    /// Wallet the maker deposits from and receives refunds to.
    #[arg(long, default_value = "swap")]
    wallet: String,
    /// Which Lightning node to talk to.
    #[arg(long, default_value = "maker")]
    node: String,
    /// Compose file that defines the regtest network.
    #[arg(
        long,
        env = "SWAPSITE_COMPOSE",
        default_value = "docker/docker-compose.yml"
    )]
    compose: PathBuf,
    /// `host:port` of the node's gRPC port. Set with `--lnd-dir` it runs
    /// `lncli` directly, which is what a container has to do: there is no
    /// docker in there to exec into.
    #[arg(long)]
    lnd_rpc: Option<String>,
    /// Root holding the node's `tls.cert` and macaroon.
    #[arg(long)]
    lnd_dir: Option<PathBuf>,
    #[arg(long, default_value = "regtest")]
    network: String,
    /// Seconds between ticks.
    #[arg(long, default_value_t = 1)]
    interval: u64,
}

impl NodeConfig {
    fn network(&self) -> Result<Network> {
        match self.network.as_str() {
            "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
            "testnet" => Ok(Network::Testnet),
            "signet" => Ok(Network::Signet),
            "regtest" => Ok(Network::Regtest),
            other => Err(format!("unknown network `{other}`").into()),
        }
    }

    fn chain(&self) -> Result<BitcoinRpc> {
        Ok(BitcoinRpc::new(
            &self.bitcoin_url,
            Some(&self.wallet),
            self.network()?,
        )?)
    }

    fn lightning(&self) -> Result<LnCli> {
        Ok(LnCli::from_config(
            self.lnd_rpc.as_deref(),
            self.lnd_dir.as_deref(),
            self.compose.clone(),
            &self.node,
            &self.network,
        )?)
    }

    fn store(&self) -> Result<SwapStore> {
        Ok(SwapStore::open(&self.dir)?)
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::NewSwap {
            config,
            amount,
            fee,
            claim_pubkey,
            claim_fee,
            locktime_margin,
            refund_fee,
            bond_fee,
            bond_window,
        } => new_swap(
            &config,
            &NewSwapRequest {
                amount,
                fee,
                claim_pubkey,
                claim_fee,
                locktime_margin,
                refund_fee,
                bond_fee,
                bond_window,
            },
        ),
        Cmd::Run { config, once } => run(&config, once),
        Cmd::Status { config, id } => status(&config, id.as_deref()),
        Cmd::Serve { config, listen } => serve(&config, &listen),
    }
}

/// A request to create a swap, from the CLI or over the control plane.
#[derive(Debug, serde::Deserialize)]
struct NewSwapRequest {
    /// What the taker receives on chain.
    amount: u64,
    /// The maker's fee, paid by the taker on top.
    #[serde(default)]
    fee: u64,
    /// The taker's claim key, hex.
    claim_pubkey: String,
    /// Fee the taker pays to claim, in satoshis. Added to the deposit so the
    /// taker nets exactly `amount`.
    #[serde(default = "default_claim_fee")]
    claim_fee: u64,
    /// Blocks from now until the deposit can be refunded.
    #[serde(default = "default_locktime_margin")]
    locktime_margin: u32,
    /// Fee for the refund transaction, in satoshis.
    #[serde(default = "default_refund_fee")]
    refund_fee: u64,
    /// What the taker pays up front to start the swap, in satoshis.
    ///
    /// A floor rather than a price: it covers the entry and exit fees, so an
    /// abandoned swap costs the taker rather than the maker.
    #[serde(default = "default_bond_fee")]
    bond_fee: u64,
    /// Blocks the taker has to pay the bond in.
    ///
    /// The on-chain locktime is measured from the end of this window, so a slow
    /// bond cannot eat into the margin between the deposit and the refund.
    #[serde(default = "default_bond_window")]
    bond_window: u32,
}

fn default_claim_fee() -> u64 {
    2_000
}

fn default_locktime_margin() -> u32 {
    30
}

fn default_refund_fee() -> u64 {
    2_000
}

fn default_bond_fee() -> u64 {
    4_000
}

fn default_bond_window() -> u32 {
    30
}

/// Offer a swap: the invoices and the terms. Nothing is funded yet.
///
/// The bond is collected first, deliberately. Funding before it is what makes
/// walking away free: the taker would risk nothing while parking the maker's
/// capital for the whole locktime, and pay nothing for the privilege.
fn create_swap(config: &NodeConfig, request: &NewSwapRequest) -> Result<SwapRecord> {
    let chain = config.chain()?;
    let lightning = config.lightning()?;
    let store = config.store()?;

    let amount_sat = request.amount;
    let claim_pubkey = PublicKey::from_str(&request.claim_pubkey)?;
    // The bond window is part of the locktime: the refund has to stay
    // `locktime_margin` away from the latest height the maker will fund at.
    let bond_by = chain.height()? + request.bond_window;
    let locktime = bond_by + request.locktime_margin;

    // A fresh refund key per swap, so one leaked record cannot expose the rest.
    let secp = Secp256k1::new();
    let mut rng = bitcoin::secp256k1::rand::thread_rng();
    let refund_key = SecretKey::new(&mut rng);
    let refund_pubkey = PublicKey::new(refund_key.public_key(&secp));

    let preimage = Preimage::generate();
    let htlc = Htlc::from_height(
        preimage.payment_hash(),
        claim_pubkey,
        refund_pubkey,
        locktime,
    )?;
    let address = htlc.address(config.network()?);

    // Deposit enough for the taker to claim and still net what it was quoted.
    let deposit_sat = amount_sat + request.claim_fee;
    let invoice = lightning.add_hold_invoice(&preimage, amount_sat + request.fee, "swap")?;
    // A plain invoice, not a hold one: paying it is final, which is exactly what
    // makes abandoning cost the taker. Its hash is derived, so settling it
    // reveals nothing about the on-chain leg.
    let bond_bolt11 =
        lightning.add_invoice_with(&preimage.bond_preimage(), request.bond_fee, "swap bond")?;

    let record = SwapRecord {
        id: preimage.payment_hash().to_hex(),
        payment_hash: preimage.payment_hash(),
        preimage,
        amount_sat,
        fee_sat: request.fee,
        deposit_sat,
        claim_fee_sat: request.claim_fee,
        refund_fee_sat: request.refund_fee,
        bond_sat: request.bond_fee,
        bond_by,
        claim_pubkey: claim_pubkey.to_string(),
        refund_secret: hex::encode(refund_key.secret_bytes()),
        refund_pubkey: refund_pubkey.to_string(),
        locktime,
        htlc_address: address.to_string(),
        bolt11: invoice.bolt11,
        bond_bolt11,
        funding_tx: None,
        funding: None,
        refund_tx: None,
        state: SwapState::Created,
    };

    // Saved before it is offered: a bond the daemon does not remember collecting
    // is a bond it never collects.
    store.save(&record)?;
    Ok(record)
}

/// Fund and broadcast the on-chain leg. Only ever called once the bond is in.
fn fund_swap(record: &mut SwapRecord, config: &NodeConfig, chain: &BitcoinRpc) -> Result<()> {
    let address = record.htlc()?.address(config.network()?);
    let funding = chain.fund(&address, Amount::from_sat(record.deposit_sat))?;

    let script_pubkey = address.script_pubkey();
    let (vout, sats) = funding
        .output
        .iter()
        .enumerate()
        .find(|(_, output)| output.script_pubkey == script_pubkey)
        .map(|(vout, output)| (vout as u32, output.value.to_sat()))
        .ok_or("the funded transaction does not pay the HTLC")?;

    record.funding_tx = Some(serialize_hex(&funding));
    record.funding = Some(Funding {
        txid: funding.compute_txid().to_string(),
        vout,
        sats,
    });

    // Persist before broadcasting, so a crash here still leaves a swap the
    // daemon can finish.
    config.store()?.save(record)?;

    let deposit_txid = chain.broadcast(&funding)?;
    if deposit_txid != funding.compute_txid() {
        return Err(
            format!("node accepted the deposit as an unexpected txid {deposit_txid}").into(),
        );
    }
    Ok(())
}

fn new_swap(config: &NodeConfig, request: &NewSwapRequest) -> Result<()> {
    let record = create_swap(config, request)?;

    eprintln!(
        "swap offered: {} sat on chain for {} over Lightning, {} sat bond, refund locktime {}",
        record.amount_sat,
        record.invoice_sat(),
        record.bond_sat,
        record.locktime
    );
    println!("id={}", record.id);
    println!("bond_bolt11={}", record.bond_bolt11);
    println!("bond_sat={}", record.bond_sat);
    println!("bond_by={}", record.bond_by);
    println!("bolt11={}", record.bolt11);
    println!("htlc_address={}", record.htlc_address);
    println!("payment_hash={}", record.payment_hash);
    println!(
        "deposit_txid={}",
        record
            .funding
            .as_ref()
            .map(|f| f.txid.as_str())
            .unwrap_or("-")
    );
    println!("locktime={}", record.locktime);
    println!("amount={}", record.amount_sat);
    println!("fee={}", record.fee_sat);
    println!("invoice={}", record.invoice_sat());
    println!("claim_fee={}", record.claim_fee_sat);
    // Public: the taker needs it to rebuild the HTLC and check the deposit.
    println!("refund_pubkey={}", record.refund_pubkey);
    Ok(())
}

/// The daemon loop.
fn run(config: &NodeConfig, once: bool) -> Result<()> {
    loop {
        tick(config)?;
        if once {
            return Ok(());
        }
        sleep(Duration::from_secs(config.interval));
    }
}

/// Run the daemon, and serve its control plane on the side.
fn serve(config: &NodeConfig, listen: &str) -> Result<()> {
    // The swap machine keeps ticking in the background; the control plane only
    // reads and creates.
    let ticker = config.clone();
    thread::spawn(move || loop {
        if let Err(e) = tick(&ticker) {
            eprintln!("[maker] tick failed: {e}");
        }
        sleep(Duration::from_secs(ticker.interval));
    });

    let api = Api {
        config: config.clone(),
    };
    serve_http(listen, move |request| api.route(&request))?;
    Ok(())
}

/// The maker's control plane.
///
/// Everything a coordinator or a taker may see, and nothing they may not: swaps
/// leave here through [`SwapRecord::public`], so the preimage and the refund
/// secret cannot escape by accident.
struct Api {
    config: NodeConfig,
}

impl Api {
    fn route(&self, request: &Request) -> Response {
        if request.matches("GET", "/swaps").is_some() {
            return self.list();
        }
        if request.matches("POST", "/swaps").is_some() {
            return self.create(request);
        }
        if let Some(captures) = request.matches("GET", "/swaps/:id") {
            return self.show(&captures[0]);
        }
        Response::not_found()
    }

    fn list(&self) -> Response {
        let swaps = (|| -> Result<Vec<PublicSwap>> {
            let store = self.config.store()?;
            let mut swaps = Vec::new();
            for record in store.all()? {
                swaps.push(record.public()?);
            }
            Ok(swaps)
        })();

        match swaps {
            Ok(swaps) => Response::json(200, &json!({ "swaps": swaps })),
            Err(e) => Response::error(500, e.to_string()),
        }
    }

    fn show(&self, id: &str) -> Response {
        let swap = (|| -> Result<PublicSwap> { Ok(self.config.store()?.load(id)?.public()?) })();

        match swap {
            Ok(swap) => Response::json(200, &json!(swap)),
            Err(e) => Response::error(404, e.to_string()),
        }
    }

    fn create(&self, request: &Request) -> Response {
        let params: NewSwapRequest = match serde_json::from_str(&request.body) {
            Ok(params) => params,
            Err(e) => return Response::error(400, format!("bad request: {e}")),
        };

        match create_swap(&self.config, &params) {
            Ok(record) => match record.public() {
                Ok(swap) => Response::json(201, &json!(swap)),
                Err(e) => Response::error(500, e.to_string()),
            },
            Err(e) => Response::error(400, e.to_string()),
        }
    }
}

/// One pass over every swap. Everything that changes does so in here.
fn tick(config: &NodeConfig) -> Result<()> {
    let chain = config.chain()?;
    let lightning = config.lightning()?;
    let store = config.store()?;

    let height = chain.height()?;
    for mut record in store.all()? {
        let observed = observe(&record, &chain, &lightning, height)?;
        let before = record.state;
        reconcile(&mut record, &observed)?;
        if before != record.state {
            note(&record, format!("state {before} -> {}", record.state));
        }

        match plan(&record, &observed) {
            // Nothing worth logging every tick: the state transitions above
            // and `maker status` are where progress shows up.
            Action::Wait(_) | Action::Done => {}

            Action::Fund => match fund_swap(&mut record, config, &chain) {
                Ok(()) => note(&record, "bond paid: deposit funded".to_owned()),
                Err(e) => note(&record, format!("could not fund the deposit: {e}")),
            },

            Action::RebroadcastFunding => {
                let tx = decode_hex(record.funding_tx.as_deref().unwrap_or_default())?;
                match chain.broadcast(&tx) {
                    Ok(txid) => note(&record, format!("re-sent deposit {txid}")),
                    Err(e) => note(&record, format!("deposit re-send failed: {e}")),
                }
            }

            Action::Settle => {
                lightning.settle(&record.preimage)?;
                let _ = record.advance(Event::Settle);
                note(
                    &record,
                    "settled, preimage revealed to the taker".to_owned(),
                );
            }

            Action::Refund => {
                // Built once and kept in the record, so every later tick just
                // rebroadcasts the same signed transaction until it confirms.
                let built_now = record.refund_tx.is_none();
                if built_now {
                    build_refund(&mut record, &chain)?;
                    store.save(&record)?;
                }
                let tx = decode_hex(record.refund_tx.as_deref().unwrap_or_default())?;
                match chain.broadcast(&tx) {
                    Ok(txid) if built_now => {
                        note(&record, format!("refund built and broadcast, {txid}"))
                    }
                    Ok(_) => {}
                    Err(e) => note(&record, format!("refund not accepted yet: {e}")),
                }
            }
        }

        store.save(&record)?;
    }
    Ok(())
}

/// Read the state of the world for one swap. Never guesses from the record.
fn observe(
    record: &SwapRecord,
    chain: &BitcoinRpc,
    lightning: &LnCli,
    height: u32,
) -> Result<Observed> {
    let funding_confirmations = match record.funding {
        Some(_) => chain.confirmations(&record.funding_txid()?)?,
        None => 0,
    };
    let refund_confirmations = match record.refund_txid()? {
        Some(txid) => chain.confirmations(&txid)?,
        None => 0,
    };

    // A transaction stays confirmed after its output is spent, so
    // `funding_confirmations` cannot answer this. The outpoint has to be asked
    // about directly, or a refund this daemon did not build is invisible.
    let deposit_spent = match record.funding {
        Some(_) => {
            let outpoint = record.funding_outpoint()?;
            chain.outspent(&outpoint.txid, outpoint.vout)?.is_none()
        }
        // Nothing has been deposited, so there is nothing that could be gone.
        None => false,
    };

    Ok(Observed {
        height,
        funding_confirmations,
        refund_confirmations,
        deposit_spent,
        // Only asked while there is nothing to fund: the daemon funds only once
        // the bond is in, so a swap with a deposit has already collected it.
        bond_paid: record.funding.is_some()
            || lightning.invoice_state(&record.bond_payment_hash())? == InvoiceState::Settled,
        invoice: lightning.invoice_state(&record.payment_hash)?,
    })
}

/// Build and record the refund, but do not broadcast it.
fn build_refund(record: &mut SwapRecord, chain: &BitcoinRpc) -> Result<()> {
    let destination: Address = chain.new_address()?;
    let tx = swapsite_core::spend::refund(
        &record.htlc()?,
        record.funding_outpoint()?,
        Amount::from_sat(record.funding()?.sats),
        &destination,
        Amount::from_sat(record.refund_fee_sat),
        &record.refund_key()?,
    )?;
    record.refund_tx = Some(serialize_hex(&tx));
    Ok(())
}

fn status(config: &NodeConfig, id: Option<&str>) -> Result<()> {
    let store = config.store()?;
    let records = match id {
        Some(id) => vec![store.load(id)?],
        None => store.all()?,
    };

    for record in records {
        println!("id={}", record.id);
        println!("state={}", record.state);
        println!("amount={}", record.amount_sat);
        println!("fee={}", record.fee_sat);
        println!("invoice={}", record.invoice_sat());
        println!("deposit={}", record.deposit_sat);
        println!("bond={}", record.bond_sat);
        println!("bond_by={}", record.bond_by);
        println!("locktime={}", record.locktime);
        println!("htlc_address={}", record.htlc_address);
        println!(
            "deposit_txid={}",
            record
                .funding
                .as_ref()
                .map(|f| f.txid.as_str())
                .unwrap_or("-")
        );
        println!(
            "refund_txid={}",
            record
                .refund_txid()?
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_owned())
        );
    }
    Ok(())
}

fn note(record: &SwapRecord, message: String) {
    eprintln!("[maker] {} {} {}", record.id, record.state, message);
}

fn decode_hex(hex: &str) -> Result<Transaction> {
    let bytes = hex::decode(hex)?;
    Ok(deserialize(&bytes)?)
}
