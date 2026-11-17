//! The daemon's socket and its main loop.
//!
//! The socket is `<data_dir>/agent.sock`, mode 600, in a data directory that must be mode 700:
//! the directory is what keeps other users out, the socket's own mode a second lock on the
//! same door. A connection from another user id is dropped unanswered. There is no network
//! listener.
//!
//! One request per connection ([`super::api`]). A connection task only reads the request and
//! forwards the engine's lines; everything that touches keys, the node or a run happens in the
//! engine, one thing at a time.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bipnip_ln::LightningBackend;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::MissedTickBehavior;

use super::api::{self, ApiError, Call, Code, MAX_LINE_BYTES, Request};
use super::engine::{Engine, Lines};
use crate::config::Config;
use crate::keys::Wallet;

/// How many connections are served at once; one more is told `busy`.
pub const MAX_CONNECTIONS: usize = 16;
/// How long a client has to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How many lines may wait for a client that reads slowly before lines are dropped.
const LINES_BUFFER: usize = 256;

struct Command {
    request: Request,
    lines: Lines,
}

/// The task that accepts connections, stopped when the daemon's loop ends.
struct Accepting(tokio::task::JoinHandle<()>);

impl Drop for Accepting {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The user id that owns the data directory, after checking that nobody else can enter it.
fn private_dir_owner(dir: &Path) -> Result<u32, String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} has mode {mode:03o}; the daemon's socket lives there, so it must be 700 \
             (chmod 700)",
            dir.display()
        ));
    }
    Ok(meta.uid())
}

/// Binds the socket, replacing one a daemon that is gone left behind. The caller holds the
/// data directory's lock, so no living daemon is listening there.
fn bind(path: &Path) -> Result<UnixListener, String> {
    let shown = path.display();
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|e| format!("{shown}: {e}"))?;
        }
        Ok(_) => return Err(format!("{shown} exists and is not a socket")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{shown}: {e}")),
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("{shown}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{shown}: {e}"))?;
    Ok(listener)
}

async fn write_line(write: &mut OwnedWriteHalf, line: &str) -> std::io::Result<()> {
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await
}

async fn connection(stream: UnixStream, commands: mpsc::Sender<Command>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let limit = u64::try_from(MAX_LINE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let read_request = async {
        let mut limited = (&mut reader).take(limit);
        limited.read_line(&mut line).await
    };
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request).await {
        Err(_) => Err(ApiError::bad_request("no request arrived in time")),
        Ok(Err(_)) => Err(ApiError::bad_request("the request is not text")),
        Ok(Ok(_)) => api::parse(&line),
    };
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            let _ = write_line(&mut write, &api::error_line(&error)).await;
            return;
        }
    };
    let watching = matches!(request.call, Call::Watch(_));
    let (lines, mut answers) = mpsc::channel::<String>(LINES_BUFFER);
    if commands.send(Command { request, lines }).await.is_err() {
        let gone = ApiError::internal("the agent is shutting down");
        let _ = write_line(&mut write, &api::error_line(&gone)).await;
        return;
    }
    // Forward the engine's lines until it has said its last, or the client is gone. A client
    // may close its sending side once the request is out; only for `watch`, which never ends
    // by itself, does that mean it has hung up.
    let mut rest = [0u8; 64];
    let mut client_open = true;
    loop {
        tokio::select! {
            answer = answers.recv() => match answer {
                Some(line) => {
                    if write_line(&mut write, &line).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            read = reader.read(&mut rest), if client_open => match read {
                Ok(0) | Err(_) => {
                    if watching {
                        break;
                    }
                    client_open = false;
                }
                // One request per connection: anything more is not read as one.
                Ok(_) => {}
            },
        }
    }
    let _ = write.shutdown().await;
}

async fn accept(listener: UnixListener, owner: u32, commands: mpsc::Sender<Command>) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        // The directory's mode already keeps other users out; this is the same rule, asked
        // of the kernel for this one connection.
        if stream.peer_cred().map(|cred| cred.uid()).ok() != Some(owner) {
            continue;
        }
        match slots.clone().try_acquire_owned() {
            Ok(slot) => {
                let commands = commands.clone();
                tokio::spawn(async move {
                    connection(stream, commands).await;
                    drop(slot);
                });
            }
            Err(_) => {
                tokio::spawn(async move {
                    let (_, mut write) = stream.into_split();
                    let busy = ApiError::new(Code::Busy, "too many connections; try again");
                    let _ = write_line(&mut write, &api::error_line(&busy)).await;
                });
            }
        }
    }
}

/// Runs the daemon: takes up the stored runs, listens on the socket, and steps every run each
/// `poll` until the process is stopped. The caller holds the data directory's lock for as long
/// as this runs. `log` gets one line per thing worth telling, never a secret.
pub async fn serve(
    config: Config,
    wallet: Wallet,
    backend: Box<dyn LightningBackend>,
    poll: Duration,
    mut log: Box<dyn FnMut(String)>,
) -> Result<(), String> {
    let owner = private_dir_owner(&config.data_dir)?;
    let path = config.socket_path();
    let listener = bind(&path)?;
    log(format!("listening on {}", path.display()));
    let mut engine = Engine::start(config, wallet, backend, log).await?;
    let (commands, mut inbox) = mpsc::channel::<Command>(MAX_CONNECTIONS);
    let _accepting = Accepting(tokio::spawn(accept(listener, owner, commands)));
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            command = inbox.recv() => match command {
                Some(command) => engine.handle(command.request, command.lines).await,
                None => return Err("the socket stopped accepting connections".into()),
            },
            _ = ticker.tick() => engine.tick().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_needs_a_directory_nobody_else_can_enter() {
        let dir = std::env::temp_dir().join(format!("bipnip-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("mkdir");
        for (mode, fine) in [
            (0o700, true),
            (0o750, false),
            (0o701, false),
            (0o777, false),
        ] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert_eq!(private_dir_owner(&dir).is_ok(), fine, "{mode:o}");
        }
        assert!(private_dir_owner(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_and_anything_else_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("bipnip-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("mkdir");
        let path = dir.join("agent.sock");
        drop(bind(&path).expect("first"));
        // The first daemon is gone; its socket file is still there.
        assert!(path.exists());
        let listener = bind(&path).expect("again");
        let mode = std::fs::metadata(&path).expect("stat").mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        std::fs::remove_file(&path).expect("rm");
        std::fs::write(&path, "not a socket").expect("write");
        assert!(bind(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "not a socket"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
