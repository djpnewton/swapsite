//! `swapx` - drive a single on-chain HTLC leg.
//!
//! Used by the regtest scripts, and the seed of what becomes the maker daemon.

use std::{fs, path::PathBuf, str::FromStr};

use bitcoin::{
    secp256k1::{Secp256k1, SecretKey},
    Address, Amount, Network, OutPoint, PublicKey,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use swapsite_core::{
    hash::{PaymentHash, Preimage},
    htlc::Htlc,
    spend,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "swapx", about = "Drive one on-chain HTLC leg")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a preimage and keys, write swap state, print the HTLC address.
    Init {
        #[arg(long, value_name = "FILE")]
        state: PathBuf,
        /// Absolute refund height.
        #[arg(long)]
        locktime: u32,
        #[arg(long, default_value = "regtest")]
        network: String,
    },

    /// Print the address and redeem script for a state file.
    Show {
        #[arg(long, value_name = "FILE")]
        state: PathBuf,
    },

    /// Build a signed claim transaction and print its raw hex.
    Claim {
        #[arg(long, value_name = "FILE")]
        state: PathBuf,
        /// Funding outpoint, `txid:vout`.
        #[arg(long)]
        outpoint: String,
        /// Value of the funding output, in satoshis.
        #[arg(long)]
        value: u64,
        /// Address to pay the claimed value to.
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 1_000)]
        fee: u64,
    },

    /// Build a signed refund transaction and print its raw hex.
    Refund {
        #[arg(long, value_name = "FILE")]
        state: PathBuf,
        #[arg(long)]
        outpoint: String,
        #[arg(long)]
        value: u64,
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 1_000)]
        fee: u64,
    },
}

/// Everything one side of one on-chain leg needs to remember.
#[derive(Serialize, Deserialize)]
struct State {
    network: String,
    locktime: u32,
    claim_key: String,
    refund_key: String,
    preimage: String,
    payment_hash: String,
}

impl State {
    fn load(path: &PathBuf) -> Result<Self> {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    fn save(&self, path: &PathBuf) -> Result<()> {
        fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    fn network(&self) -> Result<Network> {
        parse_network(&self.network)
    }

    fn claim_key(&self) -> Result<SecretKey> {
        Ok(SecretKey::from_str(&self.claim_key)?)
    }

    fn refund_key(&self) -> Result<SecretKey> {
        Ok(SecretKey::from_str(&self.refund_key)?)
    }

    fn preimage(&self) -> Result<Preimage> {
        Ok(Preimage::from_hex(&self.preimage)?)
    }

    fn htlc(&self) -> Result<Htlc> {
        let secp = Secp256k1::new();
        let claim = self.claim_key()?;
        let refund = self.refund_key()?;
        Ok(Htlc::from_height(
            PaymentHash::from_hex(&self.payment_hash)?,
            PublicKey::new(claim.public_key(&secp)),
            PublicKey::new(refund.public_key(&secp)),
            self.locktime,
        )?)
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init {
            state,
            locktime,
            network,
        } => init(&state, locktime, &network),
        Cmd::Show { state } => show(&state),
        Cmd::Claim {
            state,
            outpoint,
            value,
            to,
            fee,
        } => claim(&state, &outpoint, value, &to, fee),
        Cmd::Refund {
            state,
            outpoint,
            value,
            to,
            fee,
        } => refund(&state, &outpoint, value, &to, fee),
    }
}

fn init(path: &PathBuf, locktime: u32, network: &str) -> Result<()> {
    let net = parse_network(network)?;
    let secp = Secp256k1::new();
    let mut rng = bitcoin::secp256k1::rand::thread_rng();
    let claim_key = SecretKey::new(&mut rng);
    let refund_key = SecretKey::new(&mut rng);
    let preimage = Preimage::generate();

    let htlc = Htlc::from_height(
        preimage.payment_hash(),
        PublicKey::new(claim_key.public_key(&secp)),
        PublicKey::new(refund_key.public_key(&secp)),
        locktime,
    )?;

    State {
        network: network.to_string(),
        locktime,
        claim_key: hex::encode(claim_key.secret_bytes()),
        refund_key: hex::encode(refund_key.secret_bytes()),
        preimage: preimage.to_hex(),
        payment_hash: htlc.payment_hash.to_hex(),
    }
    .save(path)?;

    eprintln!("state:         {}", path.display());
    eprintln!("payment hash:  {}", htlc.payment_hash);
    eprintln!("refund at:     height {locktime}");
    eprintln!("redeem script: {}", htlc.script().to_asm_string());
    println!("{}", htlc.address(net));
    Ok(())
}

fn show(path: &PathBuf) -> Result<()> {
    let state = State::load(path)?;
    let net = state.network()?;
    let htlc = state.htlc()?;

    eprintln!("payment hash:  {}", state.payment_hash);
    eprintln!("refund at:     height {}", state.locktime);
    eprintln!("redeem script: {}", htlc.script().to_asm_string());
    eprintln!("script pubkey: {}", htlc.script_pubkey(net));
    println!("{}", htlc.address(net));
    Ok(())
}

fn claim(path: &PathBuf, outpoint: &str, value: u64, to: &str, fee: u64) -> Result<()> {
    let state = State::load(path)?;
    let net = state.network()?;
    let htlc = state.htlc()?;

    let tx = spend::claim(
        &htlc,
        OutPoint::from_str(outpoint)?,
        Amount::from_sat(value),
        &Address::from_str(to)?.require_network(net)?,
        Amount::from_sat(fee),
        &state.claim_key()?,
        &state.preimage()?,
    )?;

    eprintln!("claim txid:    {}", tx.compute_txid());
    eprintln!("spends:        {outpoint} ({value} sat)");
    println!("{}", bitcoin::consensus::encode::serialize_hex(&tx));
    Ok(())
}

fn refund(path: &PathBuf, outpoint: &str, value: u64, to: &str, fee: u64) -> Result<()> {
    let state = State::load(path)?;
    let net = state.network()?;
    let htlc = state.htlc()?;

    let tx = spend::refund(
        &htlc,
        OutPoint::from_str(outpoint)?,
        Amount::from_sat(value),
        &Address::from_str(to)?.require_network(net)?,
        Amount::from_sat(fee),
        &state.refund_key()?,
    )?;

    eprintln!("refund txid:   {}", tx.compute_txid());
    eprintln!(
        "locktime:      {} (HTLC opens at {})",
        tx.lock_time,
        htlc.locktime_height()
    );
    eprintln!("spends:        {outpoint} ({value} sat)");
    println!("{}", bitcoin::consensus::encode::serialize_hex(&tx));
    Ok(())
}

fn parse_network(name: &str) -> Result<Network> {
    match name {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        other => Err(format!("unknown network `{other}`").into()),
    }
}
