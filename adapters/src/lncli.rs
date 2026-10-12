//! Regtest Lightning adapter: drives `lncli`.
//!
//! A development transport only. It exists so the swap protocol can be exercised
//! against real LND nodes without a TLS and macaroon client in the loop. A REST
//! or gRPC client implements the same [`LightningNode`] trait.
//!
//! There are two ways to reach the command, because the two places this runs
//! from have different things available. On a dev host there is docker but no
//! LND credentials to hand, so it execs into the compose service. Inside a
//! container there is no docker to shell out to, but the node's TLS cert and
//! macaroon are mounted, so it names them instead.

use std::{path::PathBuf, process::Command};

use serde_json::Value;
use swapsite_core::{
    hash::{PaymentHash, Preimage},
    lightning::{
        parse_payment_preimage, Invoice, InvoiceState, LightningError, LightningNode, NodeInfo,
    },
};

/// Where `lncli` lives and what it needs to find the node.
enum Transport {
    /// Run it inside the compose service, on a host that has docker.
    Compose { compose: PathBuf, service: String },

    /// Run it against a node's gRPC port, naming the credentials rather than
    /// relying on a home directory to find them.
    Direct {
        rpc_server: String,
        tls_cert: PathBuf,
        macaroon: PathBuf,
    },
}

pub struct LnCli {
    transport: Transport,
    network: String,
}

impl LnCli {
    /// `node` is a compose service suffix, e.g. `maker` for `lnd-maker`.
    pub fn new(compose: PathBuf, node: &str, network: &str) -> Self {
        Self {
            transport: Transport::Compose {
                compose,
                service: format!("lnd-{node}"),
            },
            network: network.to_owned(),
        }
    }

    /// Reach a node over the network, with its credentials passed explicitly.
    ///
    /// `rpc_server` is `host:port` as seen from wherever this runs, and has to
    /// be a name the node's TLS certificate vouches for.
    pub fn direct(
        rpc_server: impl Into<String>,
        tls_cert: PathBuf,
        macaroon: PathBuf,
        network: &str,
    ) -> Self {
        Self {
            transport: Transport::Direct {
                rpc_server: rpc_server.into(),
                tls_cert,
                macaroon,
            },
            network: network.to_owned(),
        }
    }

    /// Pick a transport from configuration.
    ///
    /// An endpoint with a credentials root means run `lncli` directly; neither
    /// means exec it inside the compose service. Half of one is refused rather
    /// than quietly falling back, because the fallback fails with an error about
    /// docker that has nothing to do with the mistake.
    pub fn from_config(
        rpc: Option<&str>,
        root: Option<&std::path::Path>,
        compose: PathBuf,
        node: &str,
        network: &str,
    ) -> Result<Self, String> {
        match (rpc, root) {
            (Some(rpc), Some(root)) => Ok(Self::direct(
                rpc.to_owned(),
                root.join("tls.cert"),
                root.join(format!("data/chain/bitcoin/{network}/admin.macaroon")),
                network,
            )),
            (None, None) => Ok(Self::new(compose, node, network)),
            _ => Err("a node endpoint and its credentials directory go together".to_owned()),
        }
    }

    /// The argv for one call, without running it.
    fn command(&self) -> Command {
        let mut command = match &self.transport {
            Transport::Compose { compose, service } => {
                let mut command = Command::new("docker");
                command
                    .args(["compose", "-f"])
                    .arg(compose)
                    .args(["exec", "-T", service, "lncli"]);
                command
            }
            Transport::Direct {
                rpc_server,
                tls_cert,
                macaroon,
            } => {
                let mut command = Command::new("lncli");
                command
                    .args(["--rpcserver", rpc_server])
                    .arg("--tlscertpath")
                    .arg(tls_cert)
                    .arg("--macaroonpath")
                    .arg(macaroon);
                command
            }
        };

        command.args(["--network", &self.network]);
        command
    }

    fn run(&self, args: &[&str]) -> Result<String, LightningError> {
        // `Command::output` gives the child a null stdin, so a payer blocking on
        // a hold invoice cannot be suspended by the shell's job control.
        let output = self
            .command()
            .args(args)
            .output()
            .map_err(|e| LightningError::Command(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LightningError::Command(stderr.trim().to_owned()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// What this node's channels can move right now.
    pub fn capacity(&self) -> Result<Capacity, LightningError> {
        Capacity::from_lnd(&self.run(&["listchannels"])?)
    }

    /// The node's on-chain wallet balance, in satoshis.
    pub fn wallet_balance(&self) -> Result<u64, LightningError> {
        Ok(sat(
            parse(&self.run(&["walletbalance"])?)?.get("total_balance")
        ))
    }

    /// Payments this node has sent, newest first.
    pub fn sent(&self) -> Result<Vec<Entry>, LightningError> {
        Entry::sent_from_lnd(&self.run(&[
            "listpayments",
            "--include_incomplete",
            "--max_payments",
            "25",
        ])?)
    }

    /// Invoices this node has issued, paid or not.
    pub fn received(&self) -> Result<Vec<Entry>, LightningError> {
        Entry::received_from_lnd(&self.run(&["listinvoices", "--max_invoices", "25"])?)
    }

    /// The node's on-chain transactions, newest first.
    ///
    /// Capped at 50: the panel shows a page at a time, and a wallet with years
    /// of history should not put all of it on the wire.
    pub fn onchain_history(&self) -> Result<Vec<Entry>, LightningError> {
        let mut entries = Entry::onchain_from_lnd(&self.run(&["listchaintxns"])?)?;
        entries.sort_by_key(|entry| -entry.at);
        entries.truncate(50);
        Ok(entries)
    }

    /// A fresh on-chain address belonging to this node.
    ///
    /// Fresh every call, so this belongs behind a button and not behind a poll:
    /// a page that asked twice a second would burn an address each time.
    pub fn new_address(&self) -> Result<String, LightningError> {
        only_field(&self.run(&["newaddress", "p2wkh"])?, "address")
    }

    /// Send on-chain coins out of this node's wallet.
    ///
    /// The fee is fixed rather than estimated: this is regtest, where a satoshi
    /// per vbyte always confirms, and an estimator that changes its mind between
    /// two identical calls is worse than a constant.
    pub fn send_onchain(&self, address: &str, sats: u64) -> Result<String, LightningError> {
        only_field(
            &self.run(&[
                "sendcoins",
                "--addr",
                address,
                "--amt",
                &sats.to_string(),
                "--sat_per_vbyte",
                "1",
            ])?,
            "txid",
        )
    }

    /// Create an invoice and return the bolt11 to pay it.
    pub fn add_invoice(&self, sats: u64, memo: &str) -> Result<String, LightningError> {
        only_field(
            &self.run(&["addinvoice", "--amt", &sats.to_string(), "--memo", memo])?,
            "payment_request",
        )
    }

    /// Create an invoice locked to a preimage the caller chose.
    ///
    /// The bond needs this: its hash is derived from the swap's secret, so the
    /// maker never has to read the node's own bookkeeping to answer "has the
    /// taker paid it yet".
    pub fn add_invoice_with(
        &self,
        preimage: &Preimage,
        sats: u64,
        memo: &str,
    ) -> Result<String, LightningError> {
        only_field(
            &self.run(&[
                "addinvoice",
                "--amt",
                &sats.to_string(),
                "--memo",
                memo,
                "--preimage",
                &preimage.to_hex(),
            ])?,
            "payment_request",
        )
    }
}

/// How much a node can send and receive over its channels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capacity {
    /// Spendable outbound, after each channel's reserve is held back.
    pub outbound_sat: u64,
    /// What the peers can send this node.
    pub inbound_sat: u64,
    /// Every channel the node has, active or not.
    pub channels: usize,
}

impl Capacity {
    pub fn from_lnd(json: &str) -> Result<Self, LightningError> {
        let value: Value = serde_json::from_str(json)
            .map_err(|e| LightningError::Output(format!("listchannels: {e}")))?;
        Ok(Self::from_channels(&value))
    }

    fn from_channels(value: &Value) -> Self {
        let channels = value
            .get("channels")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        let mut outbound_sat = 0;
        let mut inbound_sat = 0;
        for channel in channels {
            // An inactive channel cannot move anything, so counting it would
            // promise capacity that is not there.
            if channel.get("active").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            outbound_sat += sat(channel.get("local_balance"))
                .saturating_sub(sat(channel.get("local_chan_reserve_sat")));
            inbound_sat += sat(channel.get("remote_balance"));
        }

        Self {
            outbound_sat,
            inbound_sat,
            channels: channels.len(),
        }
    }
}

fn parse(json: &str) -> Result<Value, LightningError> {
    serde_json::from_str(json).map_err(|e| LightningError::Output(e.to_string()))
}

fn entries(list: Option<&Value>) -> impl Iterator<Item = &Value> {
    list.and_then(Value::as_array).into_iter().flatten()
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Seconds since the epoch, from a string or a number.
/// The integer in a JSON field, which LND sometimes sends as a string.
fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        _ => 0,
    }
}

/// An invoice carries both a `state` and, on older output, only a `settled` bool.
fn invoice_state(invoice: &Value) -> String {
    let reported = text(invoice.get("state"));
    if !reported.is_empty() {
        return reported;
    }
    match invoice.get("settled").and_then(Value::as_bool) {
        Some(true) => "SETTLED".to_owned(),
        _ => "OPEN".to_owned(),
    }
}

/// Pull one string field out of a node response, and say which one is missing.
///
/// Every command here answers with a single field, and a missing one means this
/// crate's idea of the node's output is wrong rather than the node being odd, so
/// naming the field is the useful part of the error.
fn only_field(json: &str, key: &str) -> Result<String, LightningError> {
    parse(json)?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| LightningError::Output(format!("the node returned no `{key}`")))
}

/// LND reports satoshi amounts as strings, and occasionally as numbers.
fn sat(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Which way value moved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Sent,
    Received,
}

/// Which layer a line of history came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Lightning,
    Onchain,
}

/// One line of a node's history, from either direction and either layer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub kind: Kind,
    pub direction: Direction,
    pub sats: u64,
    /// The state the node reports: `SUCCEEDED`, `IN_FLIGHT`, `SETTLED`, `OPEN`,
    /// or `confirmed`/`unconfirmed` on chain.
    pub state: String,
    /// Unix seconds, or zero when the node did not say.
    pub at: i64,
    /// Payment hash or transaction id, so a line can be matched against a swap.
    pub hash: String,
}

impl Entry {
    /// Read `listpayments` output.
    pub fn sent_from_lnd(json: &str) -> Result<Vec<Self>, LightningError> {
        let value = parse(json)?;
        Ok(entries(value.get("payments"))
            .map(|payment| Self {
                kind: Kind::Lightning,
                direction: Direction::Sent,
                // Both spellings appear across versions; the tie is not close.
                sats: sat(payment.get("value_sat")).max(sat(payment.get("value"))),
                state: text(payment.get("status")),
                at: int(payment.get("creation_date")),
                hash: text(payment.get("payment_hash")),
            })
            .collect())
    }

    /// Read `listinvoices` output.
    pub fn received_from_lnd(json: &str) -> Result<Vec<Self>, LightningError> {
        let value = parse(json)?;
        Ok(entries(value.get("invoices"))
            .map(|invoice| Self {
                kind: Kind::Lightning,
                direction: Direction::Received,
                // `amt_paid_sat` is what arrived; `value` is what was asked for,
                // which is what to show while it is still unpaid.
                sats: sat(invoice.get("amt_paid_sat")).max(sat(invoice.get("value"))),
                state: invoice_state(invoice),
                at: int(invoice.get("settle_date")).max(int(invoice.get("creation_date"))),
                hash: text(invoice.get("r_hash")),
            })
            .collect())
    }

    /// Read `listchaintxns` output.
    ///
    /// The node reports what the wallet's balance did, so a negative `amount` is
    /// a send and its magnitude includes the fee - which is the number that
    /// belongs in a history line, since it is what left the wallet.
    pub fn onchain_from_lnd(json: &str) -> Result<Vec<Self>, LightningError> {
        let value = parse(json)?;
        Ok(entries(value.get("transactions"))
            .map(|tx| {
                let amount = int(tx.get("amount"));
                Self {
                    kind: Kind::Onchain,
                    direction: if amount < 0 {
                        Direction::Sent
                    } else {
                        Direction::Received
                    },
                    sats: amount.unsigned_abs(),
                    state: if int(tx.get("num_confirmations")) > 0 {
                        "confirmed"
                    } else {
                        "unconfirmed"
                    }
                    .to_owned(),
                    at: int(tx.get("time_stamp")),
                    hash: text(tx.get("tx_hash")),
                }
            })
            .collect())
    }
}

impl LightningNode for LnCli {
    fn add_hold_invoice(
        &self,
        preimage: &Preimage,
        amount_sat: u64,
        memo: &str,
        cltv_delta: u32,
    ) -> Result<Invoice, LightningError> {
        let hash = preimage.payment_hash().to_hex();
        let amount = amount_sat.to_string();
        let cltv = cltv_delta.to_string();
        let output = self.run(&[
            "addholdinvoice",
            &hash,
            "--amt",
            &amount,
            "--memo",
            memo,
            "--cltv_expiry_delta",
            &cltv,
        ])?;

        let mut invoice = Invoice::from_lnd(&output)?;
        // The command does not echo the amount back, but we know what we asked for.
        invoice.amount_sat = invoice.amount_sat.or(Some(amount_sat));
        Ok(invoice)
    }

    fn invoice_state(&self, hash: &PaymentHash) -> Result<InvoiceState, LightningError> {
        let hash = hash.to_hex();
        Ok(Invoice::from_lnd(&self.run(&["lookupinvoice", "--rhash", &hash])?)?.state)
    }

    fn settle(&self, preimage: &Preimage) -> Result<(), LightningError> {
        let preimage = preimage.to_hex();
        self.run(&["settleinvoice", &preimage]).map(|_| ())
    }

    fn cancel(&self, hash: &PaymentHash) -> Result<(), LightningError> {
        let hash = hash.to_hex();
        self.run(&["cancelinvoice", "--paymenthash", &hash])
            .map(|_| ())
    }

    fn pay(&self, bolt11: &str) -> Result<Preimage, LightningError> {
        // Blocks until the invoice settles or is canceled. That is what we want:
        // it returns exactly when the preimage becomes known.
        let output = self.run(&["payinvoice", "--force", "--json", bolt11])?;
        parse_payment_preimage(&output)
    }

    fn info(&self) -> Result<NodeInfo, LightningError> {
        NodeInfo::from_lnd(&self.run(&["getinfo"])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv `lncli` would be run with, without running it.
    fn argv(node: &LnCli) -> Vec<String> {
        node.command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_compose_transport_execs_into_the_service() {
        let node = LnCli::new(
            PathBuf::from("docker/docker-compose.yml"),
            "maker",
            "regtest",
        );
        let argv = argv(&node);

        assert_eq!(argv[0], "compose");
        assert!(
            argv.contains(&"docker/docker-compose.yml".to_owned()),
            "{argv:?}"
        );
        assert!(argv.contains(&"lnd-maker".to_owned()), "{argv:?}");
        assert!(argv.contains(&"lncli".to_owned()), "{argv:?}");
        assert!(
            argv.ends_with(&["--network".to_owned(), "regtest".to_owned()]),
            "{argv:?}"
        );
    }

    #[test]
    fn the_direct_transport_names_the_node_and_its_credentials() {
        // The failure this guards against is silent: lncli falling back to a
        // home directory that does not exist in a container, and reporting a
        // missing TLS certificate rather than anything about the node.
        let node = LnCli::direct(
            "lnd-maker:10009",
            PathBuf::from("/lnd/tls.cert"),
            PathBuf::from("/lnd/data/chain/bitcoin/regtest/admin.macaroon"),
            "regtest",
        );

        assert_eq!(
            argv(&node),
            [
                "--rpcserver",
                "lnd-maker:10009",
                "--tlscertpath",
                "/lnd/tls.cert",
                "--macaroonpath",
                "/lnd/data/chain/bitcoin/regtest/admin.macaroon",
                "--network",
                "regtest",
            ]
        );
    }

    #[test]
    fn the_subcommand_comes_after_the_transport_flags() {
        // lncli rejects global flags placed after the subcommand, so the order
        // the command is assembled in is load bearing.
        let node = LnCli::direct(
            "lnd-maker:10009",
            "/lnd/tls.cert".into(),
            "/lnd/admin.macaroon".into(),
            "regtest",
        );
        let full: Vec<String> = node
            .command()
            .args(["getinfo"])
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        let subcommand = full.iter().position(|arg| arg == "getinfo").unwrap();
        let network = full.iter().position(|arg| arg == "--network").unwrap();
        assert!(network < subcommand, "{full:?}");
    }

    #[test]
    fn configuration_picks_the_direct_transport() {
        let node = LnCli::from_config(
            Some("lnd-maker:10009"),
            Some(std::path::Path::new("/lnd-maker")),
            PathBuf::from("docker/docker-compose.yml"),
            "maker",
            "regtest",
        )
        .unwrap();

        let argv = argv(&node);
        assert_eq!(argv[0], "--rpcserver");
        assert!(argv.contains(&"/lnd-maker/tls.cert".to_owned()), "{argv:?}");
        assert!(
            argv.contains(&"/lnd-maker/data/chain/bitcoin/regtest/admin.macaroon".to_owned()),
            "{argv:?}"
        );
    }

    #[test]
    fn configuration_without_an_endpoint_falls_back_to_the_compose_service() {
        let node = LnCli::from_config(
            None,
            None,
            PathBuf::from("docker/docker-compose.yml"),
            "taker",
            "regtest",
        )
        .unwrap();

        let argv = argv(&node);
        assert_eq!(argv[0], "compose");
        assert!(argv.contains(&"lnd-taker".to_owned()), "{argv:?}");
    }

    #[test]
    fn half_a_configuration_is_refused_rather_than_quietly_ignored() {
        for (rpc, root) in [(Some("lnd-maker:10009"), None), (None, Some("/lnd-maker"))] {
            let built = LnCli::from_config(
                rpc,
                root.map(std::path::Path::new),
                PathBuf::from("docker/docker-compose.yml"),
                "maker",
                "regtest",
            );
            assert!(built.is_err(), "{rpc:?} {root:?} was accepted");
        }
    }

    #[test]
    fn capacity_counts_only_what_an_active_channel_can_move() {
        let listed = r#"{
            "channels": [
                {
                    "active": true,
                    "capacity": 5000000,
                    "local_balance": "999430",
                    "remote_balance": "3995570",
                    "local_chan_reserve_sat": "50000"
                },
                {
                    "active": false,
                    "local_balance": "1000000",
                    "remote_balance": "1000000",
                    "local_chan_reserve_sat": "10000"
                }
            ]
        }"#;

        let capacity = Capacity::from_lnd(listed).unwrap();
        assert_eq!(capacity.channels, 2);
        // Reserve is held back, and the inactive channel promises nothing.
        assert_eq!(capacity.outbound_sat, 949_430);
        assert_eq!(capacity.inbound_sat, 3_995_570);
    }

    #[test]
    fn capacity_reserves_never_make_the_outbound_negative() {
        let listed = r#"{"channels": [
            {"active": true, "local_balance": "1000", "remote_balance": "2000",
             "local_chan_reserve_sat": "50000"}
        ]}"#;

        let capacity = Capacity::from_lnd(listed).unwrap();
        assert_eq!(capacity.outbound_sat, 0);
        assert_eq!(capacity.inbound_sat, 2_000);
    }

    #[test]
    fn a_node_with_no_channels_has_no_capacity() {
        for listed in ["{}", r#"{"channels": []}"#] {
            assert_eq!(
                Capacity::from_lnd(listed).unwrap(),
                Capacity {
                    outbound_sat: 0,
                    inbound_sat: 0,
                    channels: 0
                }
            );
        }
    }

    #[test]
    fn capacity_reads_numbers_as_well_as_strings() {
        // Older LND returned numbers for these fields.
        let listed = r#"{"channels": [
            {"active": true, "local_balance": 100000, "remote_balance": 200000,
             "local_chan_reserve_sat": 0}
        ]}"#;

        let capacity = Capacity::from_lnd(listed).unwrap();
        assert_eq!(capacity.outbound_sat, 100_000);
        assert_eq!(capacity.inbound_sat, 200_000);
    }

    #[test]
    fn a_sent_payment_reads_its_amount_and_state() {
        // Field names and string amounts are copied from a real `listpayments`
        // on LND 0.18.3, because guessing these is how a ledger quietly reads 0.
        let listed = r#"{"payments": [
            {"payment_hash": "194ceb2a", "value": "30030", "value_sat": "30030",
             "status": "SUCCEEDED", "creation_date": "1791733750"},
            {"payment_hash": "aa", "value_sat": "5000", "status": "IN_FLIGHT"}
        ]}"#;

        let entries = Entry::sent_from_lnd(listed).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, Kind::Lightning);
        assert_eq!(entries[0].direction, Direction::Sent);
        assert_eq!(entries[0].sats, 30_030);
        assert_eq!(entries[0].state, "SUCCEEDED");
        assert_eq!(entries[0].at, 1_791_733_750);
        assert_eq!(entries[0].hash, "194ceb2a");
        // A line with no date is still a line.
        assert_eq!(entries[1].at, 0);
        assert_eq!(entries[1].sats, 5_000);
    }

    #[test]
    fn an_unpaid_invoice_shows_what_was_asked_for() {
        let listed = r#"{"invoices": [
            {"r_hash": "64809008", "value": "1234", "amt_paid_sat": "0",
             "settled": false, "creation_date": "1791735327", "settle_date": "0"}
        ]}"#;

        let entries = Entry::received_from_lnd(listed).unwrap();
        assert_eq!(entries[0].direction, Direction::Received);
        assert_eq!(entries[0].sats, 1_234);
        assert_eq!(entries[0].state, "OPEN");
        assert_eq!(entries[0].at, 1_791_735_327);
    }

    #[test]
    fn a_paid_invoice_reports_what_arrived_and_when() {
        let listed = r#"{"invoices": [
            {"r_hash": "beef", "value": "1000", "amt_paid_sat": "1000",
             "state": "SETTLED", "creation_date": "1791735327", "settle_date": "1791735400"}
        ]}"#;

        let entries = Entry::received_from_lnd(listed).unwrap();
        assert_eq!(entries[0].state, "SETTLED");
        // Settlement time, not creation time: that is when the money moved.
        assert_eq!(entries[0].at, 1_791_735_400);
    }

    #[test]
    fn an_onchain_send_is_signed_and_a_receipt_is_not() {
        // Copied from a real `listchaintxns` on LND 0.18.3: a send of 1,000 sat
        // that cost 154 in fees reads as -1154, and a claim of 30,000 as +30000.
        let listed = r#"{"transactions": [
            {"tx_hash": "1f3b577d", "amount": -1154, "total_fees": 154,
             "num_confirmations": 0, "time_stamp": 1791826354},
            {"tx_hash": "85f69f98", "amount": 30000, "num_confirmations": 3,
             "time_stamp": 1791822567}
        ]}"#;

        let entries = Entry::onchain_from_lnd(listed).unwrap();
        assert_eq!(entries[0].kind, Kind::Onchain);
        assert_eq!(entries[0].direction, Direction::Sent);
        // What left the wallet, fee included: that is the honest line.
        assert_eq!(entries[0].sats, 1_154);
        assert_eq!(entries[0].state, "unconfirmed");
        assert_eq!(entries[0].hash, "1f3b577d");
        assert_eq!(entries[1].direction, Direction::Received);
        assert_eq!(entries[1].sats, 30_000);
        assert_eq!(entries[1].state, "confirmed");
        assert_eq!(entries[1].at, 1_791_822_567);
    }

    #[test]
    fn an_empty_ledger_is_empty_rather_than_an_error() {
        for listed in [
            "{}",
            r#"{"payments": []}"#,
            r#"{"invoices": []}"#,
            r#"{"transactions": []}"#,
        ] {
            assert!(Entry::sent_from_lnd(listed).unwrap().is_empty());
            assert!(Entry::received_from_lnd(listed).unwrap().is_empty());
            assert!(Entry::onchain_from_lnd(listed).unwrap().is_empty());
        }
    }

    #[test]
    fn ledger_output_that_is_not_json_is_an_error() {
        assert!(Entry::sent_from_lnd("not json").is_err());
        assert!(Entry::received_from_lnd("").is_err());
        assert!(Entry::onchain_from_lnd("not json").is_err());
    }

    #[test]
    fn a_command_answer_yields_its_one_field() {
        assert_eq!(
            only_field(
                r#"{"address": "bcrt1qmxzyy4p23u8ndpyv7e00v4jhpld2d0h75g3y9j"}"#,
                "address"
            )
            .unwrap(),
            "bcrt1qmxzyy4p23u8ndpyv7e00v4jhpld2d0h75g3y9j"
        );
        assert_eq!(
            only_field(
                r#"{"txid": "30401017e809f063f81994c7009b71fc8abc96e9"}"#,
                "txid"
            )
            .unwrap(),
            "30401017e809f063f81994c7009b71fc8abc96e9"
        );
    }

    #[test]
    fn a_missing_field_names_itself_in_the_error() {
        // The message is the whole value of the helper: it is the difference
        // between "the node said something odd" and which field to go and look
        // at.
        for (json, key) in [
            (r#"{}"#, "address"),
            (r#"{"address": null}"#, "address"),
            (r#"{"address": 1234}"#, "address"),
            (r#"{"address": "bcrt1q"}"#, "txid"),
        ] {
            let error = only_field(json, key).unwrap_err().to_string();
            assert!(error.contains(key), "{json} {key}: {error}");
        }
    }

    #[test]
    fn output_that_is_not_json_cannot_name_a_field() {
        // Distinct from the above on purpose: there is nothing to name, and
        // pretending otherwise would be a worse message than the parse error.
        let error = only_field("not json", "txid").unwrap_err().to_string();
        assert!(error.contains("parse"), "{error}");
    }
}
