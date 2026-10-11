//! `maker` - the maker daemon.
//!
//! Drives the maker's side of reverse submarine swaps from observed reality.
//! Every signed transaction is written to the swap record before it is
//! broadcast, so a crash is always recoverable by starting the daemon again.

use std::path::PathBuf;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use bitcoin::{
    consensus::encode::{deserialize, serialize_hex},
    secp256k1::{Secp256k1, SecretKey},
    Address, Amount, Network, PublicKey, Transaction,
};
use clap::{Args, Parser, Subcommand};
use swapsite_adapters::{BitcoinRpc, LnCli, SwapStore};
use swapsite_core::{
    chain::Chain,
    hash::Preimage,
    htlc::Htlc,
    lightning::LightningNode,
    maker::{plan, reconcile, Action, Funding, Observed, SwapRecord},
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
}

#[derive(Args)]
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

    fn lightning(&self) -> LnCli {
        LnCli::new(self.compose.clone(), &self.node, &self.network)
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
            claim_pubkey,
            claim_fee,
            locktime_margin,
            refund_fee,
        } => new_swap(
            &config,
            amount,
            &claim_pubkey,
            claim_fee,
            locktime_margin,
            refund_fee,
        ),
        Cmd::Run { config, once } => run(&config, once),
        Cmd::Status { config, id } => status(&config, id.as_deref()),
    }
}

fn new_swap(
    config: &NodeConfig,
    amount_sat: u64,
    claim_pubkey: &str,
    claim_fee_sat: u64,
    locktime_margin: u32,
    refund_fee_sat: u64,
) -> Result<()> {
    let chain = config.chain()?;
    let lightning = config.lightning();
    let store = config.store()?;

    let claim_pubkey = PublicKey::from_str(claim_pubkey)?;
    let locktime = chain.height()? + locktime_margin;

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

    // Deposit enough for the taker to claim and still net what it paid.
    let deposit_sat = amount_sat + claim_fee_sat;
    let funding = chain.fund(&address, Amount::from_sat(deposit_sat))?;

    let script_pubkey = address.script_pubkey();
    let (vout, sats) = funding
        .output
        .iter()
        .enumerate()
        .find(|(_, output)| output.script_pubkey == script_pubkey)
        .map(|(vout, output)| (vout as u32, output.value.to_sat()))
        .ok_or("the funded transaction does not pay the HTLC")?;

    let invoice = lightning.add_hold_invoice(&preimage, amount_sat, "swap")?;

    let record = SwapRecord {
        id: preimage.payment_hash().to_hex(),
        payment_hash: preimage.payment_hash(),
        preimage,
        amount_sat,
        deposit_sat,
        claim_fee_sat,
        refund_fee_sat,
        claim_pubkey: claim_pubkey.to_string(),
        refund_secret: hex::encode(refund_key.secret_bytes()),
        refund_pubkey: refund_pubkey.to_string(),
        locktime,
        htlc_address: address.to_string(),
        bolt11: invoice.bolt11,
        funding_tx: Some(serialize_hex(&funding)),
        funding: Some(Funding {
            txid: funding.compute_txid().to_string(),
            vout,
            sats,
        }),
        refund_tx: None,
        state: SwapState::Created,
    };

    // Persist before broadcasting, so a crash here still leaves a swap the
    // daemon can finish.
    store.save(&record)?;

    let deposit_txid = chain.broadcast(&funding)?;
    if deposit_txid != funding.compute_txid() {
        return Err(
            format!("node accepted the deposit as an unexpected txid {deposit_txid}").into(),
        );
    }

    eprintln!("swap created: deposit {sats} sat locked until height {locktime}");
    println!("id={}", record.id);
    println!("bolt11={}", record.bolt11);
    println!("htlc_address={}", record.htlc_address);
    println!("payment_hash={}", record.payment_hash);
    println!("deposit_txid={deposit_txid}");
    println!("locktime={locktime}");
    println!("amount={amount_sat}");
    println!("claim_fee={claim_fee_sat}");
    // Public: the taker needs it to rebuild the HTLC and check the deposit.
    println!("refund_pubkey={}", record.refund_pubkey);
    Ok(())
}

fn run(config: &NodeConfig, once: bool) -> Result<()> {
    let chain = config.chain()?;
    let lightning = config.lightning();
    let store = config.store()?;

    loop {
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

        if once {
            return Ok(());
        }
        sleep(Duration::from_secs(config.interval));
    }
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

    Ok(Observed {
        height,
        funding_confirmations,
        refund_confirmations,
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
        println!("deposit={}", record.deposit_sat);
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
