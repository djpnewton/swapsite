//! Regtest Lightning adapter: drives `lncli` inside the compose containers.
//!
//! A development transport only. It exists so the swap protocol can be exercised
//! against real LND nodes without a TLS and macaroon client in the loop. A REST
//! or gRPC client implements the same [`LightningNode`] trait.

use std::{path::PathBuf, process::Command};

use swapsite_core::{
    hash::{PaymentHash, Preimage},
    lightning::{
        parse_payment_preimage, Invoice, InvoiceState, LightningError, LightningNode, NodeInfo,
    },
};

pub struct LnCli {
    compose: PathBuf,
    service: String,
    network: String,
}

impl LnCli {
    /// `node` is a compose service suffix, e.g. `maker` for `lnd-maker`.
    pub fn new(compose: PathBuf, node: &str, network: &str) -> Self {
        Self {
            compose,
            service: format!("lnd-{node}"),
            network: network.to_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, LightningError> {
        // `Command::output` gives the child a null stdin, so a payer blocking on
        // a hold invoice cannot be suspended by the shell's job control.
        let output = Command::new("docker")
            .args(["compose", "-f"])
            .arg(&self.compose)
            .args(["exec", "-T", &self.service, "lncli", "--network"])
            .arg(&self.network)
            .args(args)
            .output()
            .map_err(|e| LightningError::Command(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LightningError::Command(stderr.trim().to_owned()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
