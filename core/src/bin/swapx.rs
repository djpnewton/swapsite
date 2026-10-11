//! `swapx` - drive the legs of one swap.
//!
//! Deliberately stateless: each command takes the keys and parameters it needs,
//! so each party to a swap only ever handles its own secret. Used by the regtest
//! scripts, and the seed of what becomes the maker daemon.

mod lncli;

use std::{path::PathBuf, str::FromStr};

use bitcoin::{
    secp256k1::{Secp256k1, SecretKey},
    Address, Amount, Network, OutPoint, PublicKey,
};
use clap::{Args, Parser, Subcommand};
use swapsite_core::{
    hash::{PaymentHash, Preimage},
    htlc::Htlc,
    lightning::{check_joined, LightningNode},
    spend,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "swapx", about = "Drive the legs of one swap")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Which side of the on-chain leg's script a command is acting as.
#[derive(Clone, Copy)]
enum Role {
    /// Owns the preimage path; supplies `--claim-key`.
    Claimant,
    /// Owns the timelocked path; supplies `--refund-key`.
    Refunder,
    /// Supplies both pubkeys from flags and no secret.
    Neither,
}

/// Parameters of the on-chain leg.
///
/// Each party supplies the other side's pubkey and only its own secret.
#[derive(Args)]
struct HtlcArgs {
    /// Payment hash shared by both legs, hex.
    #[arg(long)]
    payment_hash: String,
    /// Absolute refund height of the on-chain HTLC.
    #[arg(long)]
    locktime: u32,
    /// Key allowed to claim with the preimage, hex.
    #[arg(long)]
    claim_pubkey: Option<String>,
    /// Key allowed to refund after the locktime, hex.
    #[arg(long)]
    refund_pubkey: Option<String>,
    #[arg(long, default_value = "regtest")]
    network: String,
}

impl HtlcArgs {
    fn network(&self) -> Result<Network> {
        parse_network(&self.network)
    }

    /// Build the HTLC, deriving the pubkey of whichever side owns `own_key`.
    fn htlc(&self, own_key: Option<&SecretKey>, role: Role) -> Result<Htlc> {
        let secp = Secp256k1::new();
        let own_pubkey =
            |key: Option<&SecretKey>| key.map(|key| PublicKey::new(key.public_key(&secp)));

        let (claimant, refunder) = match role {
            Role::Claimant => (
                own_pubkey(own_key),
                self.refund_pubkey
                    .as_deref()
                    .map(PublicKey::from_str)
                    .transpose()?,
            ),
            Role::Refunder => (
                self.claim_pubkey
                    .as_deref()
                    .map(PublicKey::from_str)
                    .transpose()?,
                own_pubkey(own_key),
            ),
            Role::Neither => (
                self.claim_pubkey
                    .as_deref()
                    .map(PublicKey::from_str)
                    .transpose()?,
                self.refund_pubkey
                    .as_deref()
                    .map(PublicKey::from_str)
                    .transpose()?,
            ),
        };

        let (claimant, refunder) = match (claimant, refunder) {
            (Some(claimant), Some(refunder)) => (claimant, refunder),
            _ => return Err("supply the pubkey for the side you are not acting as".into()),
        };

        Ok(Htlc::from_height(
            PaymentHash::from_hex(&self.payment_hash)?,
            claimant,
            refunder,
            self.locktime,
        )?)
    }
}

/// Where to reach the regtest Lightning nodes.
#[derive(Args)]
struct NodeArgs {
    /// Which node to talk to: `maker` or `taker`.
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
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a keypair. Prints `secret=` and `pubkey=`.
    Keygen,

    /// Generate a preimage. Prints `preimage=` and `payment_hash=`.
    RandPreimage,

    /// Print the P2WSH address of an HTLC.
    HtlcAddress {
        #[command(flatten)]
        htlc: HtlcArgs,
    },

    /// Build a signed claim transaction. Prints raw hex.
    Claim {
        #[command(flatten)]
        htlc: HtlcArgs,
        /// The claimant secret key, hex.
        #[arg(long)]
        claim_key: String,
        /// The preimage revealed by the Lightning leg, hex.
        #[arg(long)]
        preimage: String,
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

    /// Build a signed refund transaction. Prints raw hex.
    Refund {
        #[command(flatten)]
        htlc: HtlcArgs,
        /// The refunder secret key, hex.
        #[arg(long)]
        refund_key: String,
        #[arg(long)]
        outpoint: String,
        #[arg(long)]
        value: u64,
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 1_000)]
        fee: u64,
    },

    /// Check that a revealed preimage unlocks an HTLC: the join between legs.
    CheckJoin {
        #[command(flatten)]
        htlc: HtlcArgs,
        #[arg(long)]
        preimage: String,
    },

    /// Lightning legs.
    Ln {
        #[command(subcommand)]
        cmd: LnCmd,
    },
}

#[derive(Subcommand)]
enum LnCmd {
    /// Print the node's identity.
    Info {
        #[command(flatten)]
        node: NodeArgs,
    },
    /// Issue a hold invoice: the payer's funds are held until settled.
    AddHold {
        #[command(flatten)]
        node: NodeArgs,
        #[arg(long)]
        preimage: String,
        #[arg(long)]
        amount: u64,
        #[arg(long, default_value = "swap")]
        memo: String,
    },
    /// Print the state of an invoice.
    State {
        #[command(flatten)]
        node: NodeArgs,
        #[arg(long)]
        payment_hash: String,
    },
    /// Settle a held invoice, revealing the preimage to the payer.
    Settle {
        #[command(flatten)]
        node: NodeArgs,
        #[arg(long)]
        preimage: String,
    },
    /// Cancel a held invoice, releasing the payer's funds.
    Cancel {
        #[command(flatten)]
        node: NodeArgs,
        #[arg(long)]
        payment_hash: String,
    },
    /// Pay an invoice, blocking until it settles. Prints `preimage=`.
    Pay {
        #[command(flatten)]
        node: NodeArgs,
        #[arg(long)]
        invoice: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen => keygen(),
        Cmd::RandPreimage => rand_preimage(),
        Cmd::HtlcAddress { htlc } => htlc_address(&htlc),
        Cmd::Claim {
            htlc,
            claim_key,
            preimage,
            outpoint,
            value,
            to,
            fee,
        } => claim(&htlc, &claim_key, &preimage, &outpoint, value, &to, fee),
        Cmd::Refund {
            htlc,
            refund_key,
            outpoint,
            value,
            to,
            fee,
        } => refund(&htlc, &refund_key, &outpoint, value, &to, fee),
        Cmd::CheckJoin { htlc, preimage } => check_join(&htlc, &preimage),
        Cmd::Ln { cmd } => ln(cmd),
    }
}

fn keygen() -> Result<()> {
    let secp = Secp256k1::new();
    let mut rng = bitcoin::secp256k1::rand::thread_rng();
    let secret = SecretKey::new(&mut rng);
    println!("secret={}", hex::encode(secret.secret_bytes()));
    println!("pubkey={}", PublicKey::new(secret.public_key(&secp)));
    Ok(())
}

fn rand_preimage() -> Result<()> {
    let preimage = Preimage::generate();
    eprintln!("this is a secret: it unlocks every leg locked to its hash");
    println!("preimage={}", preimage.to_hex());
    println!("payment_hash={}", preimage.payment_hash().to_hex());
    Ok(())
}

fn htlc_address(args: &HtlcArgs) -> Result<()> {
    let htlc = args.htlc(None, Role::Neither)?;
    eprintln!("payment hash:  {}", htlc.payment_hash);
    eprintln!("refund at:     height {}", htlc.locktime_height());
    eprintln!("redeem script: {}", htlc.script().to_asm_string());
    println!("{}", htlc.address(args.network()?));
    Ok(())
}

fn check_join(args: &HtlcArgs, preimage: &str) -> Result<()> {
    let htlc = args.htlc(None, Role::Neither)?;
    check_joined(&htlc, &Preimage::from_hex(preimage)?)?;
    eprintln!(
        "joined: the preimage unlocks the HTLC locked to {}",
        htlc.payment_hash
    );
    println!("joined");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn claim(
    args: &HtlcArgs,
    claim_key: &str,
    preimage: &str,
    outpoint: &str,
    value: u64,
    to: &str,
    fee: u64,
) -> Result<()> {
    let key = SecretKey::from_str(claim_key)?;
    let htlc = args.htlc(Some(&key), Role::Claimant)?;
    let preimage = Preimage::from_hex(preimage)?;

    // The whole point: the preimage Lightning revealed must unlock this HTLC.
    check_joined(&htlc, &preimage)?;

    let tx = spend::claim(
        &htlc,
        OutPoint::from_str(outpoint)?,
        Amount::from_sat(value),
        &Address::from_str(to)?.require_network(args.network()?)?,
        Amount::from_sat(fee),
        &key,
        &preimage,
    )?;

    eprintln!("claim txid:    {}", tx.compute_txid());
    eprintln!("spends:        {outpoint} ({value} sat)");
    println!("{}", bitcoin::consensus::encode::serialize_hex(&tx));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn refund(
    args: &HtlcArgs,
    refund_key: &str,
    outpoint: &str,
    value: u64,
    to: &str,
    fee: u64,
) -> Result<()> {
    let key = SecretKey::from_str(refund_key)?;
    let htlc = args.htlc(Some(&key), Role::Refunder)?;

    let tx = spend::refund(
        &htlc,
        OutPoint::from_str(outpoint)?,
        Amount::from_sat(value),
        &Address::from_str(to)?.require_network(args.network()?)?,
        Amount::from_sat(fee),
        &key,
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

fn ln(cmd: LnCmd) -> Result<()> {
    match cmd {
        LnCmd::Info { node } => {
            let info = client(&node).info()?;
            println!("pubkey={}", info.pubkey);
            println!("alias={}", info.alias);
        }
        LnCmd::AddHold {
            node,
            preimage,
            amount,
            memo,
        } => {
            let preimage = Preimage::from_hex(&preimage)?;
            let invoice = client(&node).add_hold_invoice(&preimage, amount, &memo)?;
            eprintln!("state:         {}", invoice.state);
            eprintln!("amount:        {amount} sat");
            println!("bolt11={}", invoice.bolt11);
            println!("payment_hash={}", preimage.payment_hash().to_hex());
            println!("state={}", invoice.state);
        }
        LnCmd::State { node, payment_hash } => {
            let state = client(&node).invoice_state(&PaymentHash::from_hex(&payment_hash)?)?;
            eprintln!("payment hash:  {payment_hash}");
            println!("state={state}");
        }
        LnCmd::Settle { node, preimage } => {
            let preimage = Preimage::from_hex(&preimage)?;
            client(&node).settle(&preimage)?;
            eprintln!("settled:       {}", preimage.payment_hash());
            println!("state=SETTLED");
        }
        LnCmd::Cancel { node, payment_hash } => {
            let hash = PaymentHash::from_hex(&payment_hash)?;
            client(&node).cancel(&hash)?;
            eprintln!("canceled:      {hash}");
            println!("state=CANCELED");
        }
        LnCmd::Pay { node, invoice } => {
            let preimage = client(&node).pay(&invoice)?;
            eprintln!("payment hash:  {}", preimage.payment_hash());
            println!("preimage={}", preimage.to_hex());
        }
    }
    Ok(())
}

fn client(args: &NodeArgs) -> lncli::LnCli {
    lncli::LnCli::new(args.compose.clone(), &args.node, &args.network)
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
