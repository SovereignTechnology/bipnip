//! Ring runs on disk: one owner-only directory with a `ring-<key>.json` per run. Nothing in a
//! ring run is secret, but the files are kept as private as the lease files next to them and are
//! refused on load if anyone else can read them. Each save replaces the file in one step.

use std::path::{Path, PathBuf};

use super::run::RingRun;
use crate::lease::store::StoreError;
use crate::secretfile::{self, SecretFileError};

/// What a ring store held.
#[derive(Debug, Default)]
pub struct Loaded {
    /// The runs.
    pub runs: Vec<RingRun>,
    /// Files that were refused, by name, with the reason. The other runs still load.
    pub problems: Vec<(String, StoreError)>,
}

/// The directory ring runs are stored in.
#[derive(Debug, Clone)]
pub struct RingStore {
    dir: PathBuf,
}

impl RingStore {
    /// Opens the store at `dir`, creating it owner-only if it is not there.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        secretfile::create_private_dir(dir).map_err(StoreError::File)?;
        Ok(RingStore {
            dir: dir.to_path_buf(),
        })
    }

    /// Stores a run, replacing its earlier state.
    pub fn save(&self, run: &RingRun) -> Result<(), StoreError> {
        let path = self.dir.join(format!("ring-{}.json", run.store_key()));
        secretfile::replace_private(&path, &run.to_json()?).map_err(StoreError::File)
    }

    /// Loads every run in the store. A file that is refused is listed in [`Loaded::problems`].
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
            let Some(key) = name
                .strip_prefix("ring-")
                .and_then(|rest| rest.strip_suffix(".json"))
            else {
                continue;
            };
            let run = secretfile::read_secret(&self.dir.join(&name))
                .map_err(StoreError::File)
                .and_then(|json| RingRun::from_json(&json));
            match run {
                Ok(run) if run.store_key() == key => loaded.runs.push(run),
                Ok(_) => loaded.problems.push((
                    name,
                    StoreError::Corrupt("the file name is not this run's key".into()),
                )),
                Err(e) => loaded.problems.push((name, e)),
            }
        }
        Ok(loaded)
    }
}
