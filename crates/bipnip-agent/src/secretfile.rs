//! Files that hold secrets (the mnemonic): created owner-only, and refused when anyone else could
//! read them. A secret file that is a symlink, or readable by group or others, is an error, not a
//! warning (Mostro issue #1014 is the same class of problem for the daemon's credentials).

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Why a secret file was refused. Never carries file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretFileError {
    /// The file could not be opened, read or written.
    Io(String),
    /// Not a regular file (a symlink, directory, device…).
    NotRegular,
    /// Readable or writable by group or others; the mode is given in octal.
    TooOpen(u32),
    /// The file exists already; secrets are never overwritten.
    Exists,
}

impl std::fmt::Display for SecretFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretFileError::Io(e) => write!(f, "{e}"),
            SecretFileError::NotRegular => write!(f, "not a regular file"),
            SecretFileError::TooOpen(mode) => write!(
                f,
                "mode {mode:03o} lets group or others in; it must be 600 (chmod 600)"
            ),
            SecretFileError::Exists => write!(f, "already exists; refusing to overwrite a secret"),
        }
    }
}

impl std::error::Error for SecretFileError {}

/// Creates `dir` (and parents) with mode 700 if it does not exist.
pub fn create_private_dir(dir: &Path) -> Result<(), SecretFileError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| SecretFileError::Io(e.to_string()))
}

/// Writes `contents` to a new file with mode 600. Fails if anything is already at `path`.
pub fn create_secret(path: &Path, contents: &str) -> Result<(), SecretFileError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => SecretFileError::Exists,
            _ => SecretFileError::Io(e.to_string()),
        })?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| SecretFileError::Io(e.to_string()))
}

/// Writes `contents` to `path` with mode 600, replacing whatever is there in one step: a reader
/// sees the old file or the new one, never half of either. For files that change (lease state);
/// a secret that must never change is written once with [`create_secret`].
pub fn replace_private(path: &Path, contents: &str) -> Result<(), SecretFileError> {
    let io = |e: std::io::Error| SecretFileError::Io(e.to_string());
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(SecretFileError::Io("not a file path".into()));
    };
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    // A leftover from a write that was cut short.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    create_secret(&tmp, contents)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    // The rename itself must survive a power cut.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io)
}

/// Reads a secret file after checking it is a regular file only its owner can access. The check is
/// made on the opened file, so the file read is the file checked.
pub fn read_secret(path: &Path) -> Result<String, SecretFileError> {
    let link = std::fs::symlink_metadata(path).map_err(|e| SecretFileError::Io(e.to_string()))?;
    if !link.file_type().is_file() {
        return Err(SecretFileError::NotRegular);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW) // a symlink swapped in after the check is refused
        .open(path)
        .map_err(|e| SecretFileError::Io(e.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|e| SecretFileError::Io(e.to_string()))?;
    if !meta.file_type().is_file() {
        return Err(SecretFileError::NotRegular);
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(SecretFileError::TooOpen(mode));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| SecretFileError::Io(e.to_string()))?;
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("bipnip-secretfile-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        create_private_dir(&d).expect("dir");
        d
    }

    #[test]
    fn creates_owner_only_and_reads_back() {
        let d = dir("create");
        assert_eq!(std::fs::metadata(&d).expect("meta").mode() & 0o777, 0o700);
        let f = d.join("mnemonic");
        create_secret(&f, "words words").expect("create");
        assert_eq!(std::fs::metadata(&f).expect("meta").mode() & 0o777, 0o600);
        assert_eq!(read_secret(&f).expect("read"), "words words");
        assert_eq!(create_secret(&f, "other"), Err(SecretFileError::Exists));
        assert_eq!(read_secret(&f).expect("read"), "words words");
    }

    #[test]
    fn replaces_in_one_step_and_stays_owner_only() {
        let d = dir("replace");
        let f = d.join("state.json");
        replace_private(&f, "one").expect("first write");
        replace_private(&f, "two").expect("second write");
        assert_eq!(read_secret(&f).expect("read"), "two");
        assert_eq!(std::fs::metadata(&f).expect("meta").mode() & 0o777, 0o600);
        // A leftover temporary file from a cut-short write does not get in the way.
        std::fs::write(d.join(".state.json.tmp"), "half").expect("leftover");
        replace_private(&f, "three").expect("third write");
        assert_eq!(read_secret(&f).expect("read"), "three");
        assert!(!d.join(".state.json.tmp").exists());
        // A symlink at the target is replaced, not followed.
        let elsewhere = d.join("elsewhere");
        create_secret(&elsewhere, "untouched").expect("create");
        let link = d.join("link.json");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        replace_private(&link, "four").expect("replace the link");
        assert_eq!(read_secret(&link).expect("a regular file now"), "four");
        assert_eq!(read_secret(&elsewhere).expect("read"), "untouched");
    }

    #[test]
    fn refuses_files_others_can_read() {
        let d = dir("open");
        let f = d.join("mnemonic");
        create_secret(&f, "words").expect("create");
        for mode in [0o640, 0o604, 0o660, 0o644] {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert_eq!(read_secret(&f), Err(SecretFileError::TooOpen(mode)));
        }
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o400)).expect("chmod");
        assert_eq!(read_secret(&f).expect("read-only is fine"), "words");
    }

    #[test]
    fn refuses_symlinks_and_directories() {
        let d = dir("link");
        let real = d.join("real");
        create_secret(&real, "words").expect("create");
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(read_secret(&link), Err(SecretFileError::NotRegular));
        assert_eq!(read_secret(&d), Err(SecretFileError::NotRegular));
        assert!(matches!(
            read_secret(&d.join("missing")),
            Err(SecretFileError::Io(_))
        ));
    }
}
