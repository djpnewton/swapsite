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

/// LND reports satoshi amounts as strings, and occasionally as numbers.
fn sat(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        _ => 0,
    }
}

impl LightningNode for LnCli {
    fn add_hold_invoice(
        &self,
        preimage: &Preimage,
        amount_sat: u64,
        memo: &str,
    ) -> Result<Invoice, LightningError> {
        let hash = preimage.payment_hash().to_hex();
        let amount = amount_sat.to_string();
        let output = self.run(&["addholdinvoice", &hash, "--amt", &amount, "--memo", memo])?;

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
}
