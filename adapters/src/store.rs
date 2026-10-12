//! On-disk swap records, one JSON file per swap.
//!
//! Writes go through a temporary file and a rename, so a crash can never leave
//! a half-written record behind. Unreadable records are reported as errors: a
//! maker must not silently forget a swap.

use std::fs;
use std::path::{Path, PathBuf};

use swapsite_core::maker::SwapRecord;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("swap store at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("swap record {id}: {source}")]
    Json {
        id: String,
        #[source]
        source: serde_json::Error,
    },
}

pub struct SwapStore {
    dir: PathBuf,
}

impl SwapStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write a record atomically.
    pub fn save(&self, record: &SwapRecord) -> Result<(), StoreError> {
        let path = self.path(&record.id);
        let temp = path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(record).map_err(|source| StoreError::Json {
            id: record.id.clone(),
            source,
        })?;

        fs::write(&temp, json + "\n").map_err(|source| StoreError::Io {
            path: temp.clone(),
            source,
        })?;
        fs::rename(&temp, &path).map_err(|source| StoreError::Io { path, source })
    }

    pub fn load(&self, id: &str) -> Result<SwapRecord, StoreError> {
        let path = self.path(id);
        let text = fs::read_to_string(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| StoreError::Json {
            id: id.to_owned(),
            source,
        })
    }

    /// Ids of every record, sorted, so a tick is deterministic.
    pub fn ids(&self) -> Result<Vec<String>, StoreError> {
        let entries = fs::read_dir(&self.dir).map_err(|source| StoreError::Io {
            path: self.dir.clone(),
            source,
        })?;

        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: self.dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn all(&self) -> Result<Vec<SwapRecord>, StoreError> {
        self.ids()?.into_iter().map(|id| self.load(&id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{secp256k1::Secp256k1, Network, PublicKey};
    use swapsite_core::{Htlc, Preimage, SwapState};

    fn record(id: &str) -> SwapRecord {
        let secp = Secp256k1::new();
        let mut rng = bitcoin::secp256k1::rand::thread_rng();
        let refund = bitcoin::secp256k1::SecretKey::new(&mut rng);
        let claim = bitcoin::secp256k1::SecretKey::new(&mut rng);
        let preimage = Preimage::generate();
        let claim_pubkey = PublicKey::new(claim.public_key(&secp));
        let refund_pubkey = PublicKey::new(refund.public_key(&secp));
        let htlc =
            Htlc::from_height(preimage.payment_hash(), claim_pubkey, refund_pubkey, 500).unwrap();

        SwapRecord {
            id: id.to_owned(),
            payment_hash: preimage.payment_hash(),
            preimage,
            amount_sat: 30_000,
            fee_sat: 30,
            deposit_sat: 32_000,
            claim_fee_sat: 2_000,
            refund_fee_sat: 2_000,
            bond_sat: 4_000,
            bond_by: 450,
            claim_pubkey: claim_pubkey.to_string(),
            refund_secret: hex::encode(refund.secret_bytes()),
            refund_pubkey: refund_pubkey.to_string(),
            locktime: 500,
            cltv_delta: 40,
            htlc_address: htlc.address(Network::Regtest).to_string(),
            bolt11: "lnbcrt1...".to_owned(),
            bond_bolt11: "lnbcrt1bond...".to_owned(),
            funding_tx: None,
            funding: None,
            refund_tx: None,
            state: SwapState::Created,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("swapsite-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_a_record_including_its_secrets() {
        let store = SwapStore::open(temp_dir("roundtrip")).unwrap();
        let original = record("swap-1");
        store.save(&original).unwrap();

        let loaded = store.load("swap-1").unwrap();
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.payment_hash, original.payment_hash);
        // The preimage and refund key have to survive: they are what the maker
        // needs to settle and to recover.
        assert_eq!(loaded.preimage.to_hex(), original.preimage.to_hex());
        assert_eq!(loaded.refund_key().unwrap(), original.refund_key().unwrap());
        assert_eq!(loaded.htlc().unwrap(), original.htlc().unwrap());
    }

    #[test]
    fn lists_records_in_a_stable_order() {
        let store = SwapStore::open(temp_dir("listing")).unwrap();
        for id in ["b", "a", "c"] {
            store.save(&record(id)).unwrap();
        }
        assert_eq!(store.ids().unwrap(), vec!["a", "b", "c"]);
        assert_eq!(store.all().unwrap().len(), 3);
    }

    #[test]
    fn save_leaves_no_temporary_files() {
        let dir = temp_dir("atomic");
        let store = SwapStore::open(&dir).unwrap();
        store.save(&record("swap-1")).unwrap();

        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["swap-1.json"]);
    }

    #[test]
    fn reports_a_missing_record() {
        let store = SwapStore::open(temp_dir("missing")).unwrap();
        assert!(store.load("nope").is_err());
    }
}
