//! One process per data directory. The daemon and every command that drives a run take this
//! lock first, so two processes never step the same run, hand out the same trade index, or pay
//! the same invoice side by side (`docs/decisions/0010-agent-daemon-and-local-api.md`).
//!
//! The lock is the operating system's advisory lock on `<data_dir>/agent.lock`. It ends with
//! the process, however the process ends, so a crash leaves nothing to clean up.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;

use crate::config::Config;
use crate::secretfile;

/// The data directory, held. Dropping it lets the next process in.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    /// Takes the lock, or says who is in the way. Never waits.
    pub fn acquire(config: &Config) -> Result<Self, String> {
        let dir = &config.data_dir;
        secretfile::create_private_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = config.lock_path();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(DataDirLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(format!(
                "another bipnip-agent is at work in {} (the daemon, or a lease or ring command); \
                 ask the daemon with `bipnip`, or wait for the command to end",
                dir.display()
            )),
            Err(TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_taker_is_refused_until_the_first_lets_go() {
        let dir = std::env::temp_dir().join(format!("bipnip-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::parse(&format!(
            "network = \"regtest\"\ndata_dir = \"{}\"\n",
            dir.display()
        ))
        .expect("config");
        let first = DataDirLock::acquire(&config).expect("first");
        let second = DataDirLock::acquire(&config).expect_err("second is refused");
        assert!(second.contains("another bipnip-agent"), "{second}");
        drop(first);
        DataDirLock::acquire(&config).expect("free again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
