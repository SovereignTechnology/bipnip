//! Lease runs on disk: one directory, owner-only, with a file per run.
//!
//! - `seller-<key>.json`: a seller's run. Nothing secret.
//! - `buyer-<key>.json`: a buyer's run, without the preimage.
//! - `buyer-<key>.lease`: the buyer's contract and preimage. Written once, before the take is
//!   sent, and never rewritten.
//!
//! The state files are replaced in one step, so a crash leaves the old state or the new one.
//! Every file is refused on load if anyone but its owner can read it.

use std::path::{Path, PathBuf};

use super::BuyerLease;
use super::run::{BuyerRun, SellerRun};
use crate::secretfile::{self, SecretFileError};

/// Why lease state was not stored or loaded. Never carries a preimage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The file could not be written or read, or is open to others.
    File(SecretFileError),
    /// The contents are not a run this agent wrote, or do not fit together; the reason says how.
    Corrupt(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::File(e) => write!(f, "{e}"),
            StoreError::Corrupt(what) => write!(f, "stored lease state is not usable: {what}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// What a store held.
#[derive(Debug, Default)]
pub struct Loaded {
    /// The sellers' runs.
    pub sellers: Vec<SellerRun>,
    /// The buyers' runs.
    pub buyers: Vec<BuyerRun>,
    /// Files that were refused, by name, with the reason. The other runs still load.
    pub problems: Vec<(String, StoreError)>,
}

/// The directory lease runs are stored in.
#[derive(Debug, Clone)]
pub struct LeaseStore {
    dir: PathBuf,
}

impl LeaseStore {
    /// Opens the store at `dir`, creating it owner-only if it is not there.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        secretfile::create_private_dir(dir).map_err(StoreError::File)?;
        Ok(LeaseStore {
            dir: dir.to_path_buf(),
        })
    }

    /// Stores a seller's run, replacing its earlier state.
    pub fn save_seller(&self, run: &SellerRun) -> Result<(), StoreError> {
        let path = self.dir.join(format!("seller-{}.json", run.store_key()));
        secretfile::replace_private(&path, &run.to_json()?).map_err(StoreError::File)
    }

    /// Stores a buyer's run, replacing its earlier state. The first save also writes the lease
    /// (contract and preimage), so it must come before the take is sent.
    pub fn save_buyer(&self, run: &BuyerRun) -> Result<(), StoreError> {
        let key = run.store_key();
        match run.save_lease(&self.dir.join(format!("buyer-{key}.lease"))) {
            // Already written by an earlier save. The key commits to the payment hash, so the
            // file there cannot hold another lease's preimage.
            Ok(()) | Err(StoreError::File(SecretFileError::Exists)) => {}
            Err(e) => return Err(e),
        }
        let path = self.dir.join(format!("buyer-{key}.json"));
        secretfile::replace_private(&path, &run.to_json()?).map_err(StoreError::File)
    }

    /// Loads every run in the store. A file that is refused is listed in
    /// [`Loaded::problems`] and does not stop the others from loading.
    pub fn load(&self) -> Result<Loaded, StoreError> {
        let io = |e: std::io::Error| StoreError::File(SecretFileError::Io(e.to_string()));
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(io)? {
            names.push(
                entry
                    .map_err(io)?
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        names.sort();
        let mut loaded = Loaded::default();
        for name in names {
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let json = match secretfile::read_secret(&self.dir.join(&name)) {
                Ok(json) => json,
                Err(e) => {
                    loaded.problems.push((name, StoreError::File(e)));
                    continue;
                }
            };
            if let Some(key) = stem.strip_prefix("seller-") {
                match SellerRun::from_json(&json) {
                    Ok(run) if run.store_key() == key => loaded.sellers.push(run),
                    Ok(_) => loaded.problems.push((name, misnamed())),
                    Err(e) => loaded.problems.push((name, e)),
                }
            } else if let Some(key) = stem.strip_prefix("buyer-") {
                let run = BuyerLease::load(&self.dir.join(format!("buyer-{key}.lease")))
                    .and_then(|lease| BuyerRun::from_json(&json, lease));
                match run {
                    Ok(run) if run.store_key() == key => loaded.buyers.push(run),
                    Ok(_) => loaded.problems.push((name, misnamed())),
                    Err(e) => loaded.problems.push((name, e)),
                }
            }
        }
        Ok(loaded)
    }
}

fn misnamed() -> StoreError {
    StoreError::Corrupt("the file name is not this run's key".into())
}
