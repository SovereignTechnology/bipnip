//! The daemon's sockets and its main loop.
//!
//! The operator's socket is `<data_dir>/agent.sock`, mode 600, in a data directory that must be
//! mode 700: the directory is what keeps other users out, the socket's own mode a second lock on
//! the same door. A connection from another user id is dropped unanswered.
//!
//! With `[web]` configured there is a second socket, the web door: a web host passes a paired
//! page's signed requests through it ([`super::session`]). It has its own connections and its own
//! queue, and the loop takes the operator's requests first, then the runs' tick, then the
//! door's: a page can slow down neither the operator nor the runs. Its socket may sit outside
//! the data directory, so that a web host of another user can reach it and nothing else; its
//! directory must then be the agent's user's (or root's) and writable by nobody else. There is
//! no network listener.
//!
//! One request per connection ([`super::api`]). A connection task only reads the request (and,
//! at the web door, checks its signature) and forwards the engine's lines; everything that
//! touches keys, the node, a session or a run happens in the engine, one thing at a time.

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

use super::api::{self, ApiError, Call, Code, MAX_LINE_BYTES};
use super::engine::{Engine, Incoming, Lines};
use super::session;
use crate::config::Config;
use crate::keys::Wallet;

/// How many connections are served at once; one more is told `busy`.
pub const MAX_CONNECTIONS: usize = 16;
/// How many connections the web door serves at once, apart from the operator's.
pub const MAX_WEB_CONNECTIONS: usize = 8;
/// How long a client has to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How many lines may wait for a client that reads slowly before lines are dropped.
const LINES_BUFFER: usize = 256;

struct Command {
    incoming: Incoming,
    lines: Lines,
}

/// Which socket a connection came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Door {
    /// The operator's.
    Owner,
    /// The web door.
    Web,
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

/// Checks the directory the web door's socket is to go in: it must be the agent's user's (or
/// root's), and nobody else may write to it, or someone else could put their own socket there.
/// Returns a warning when it is the data directory itself.
fn web_dir_ok(socket: &Path, owner: u32, data_dir: &Path) -> Result<Option<String>, String> {
    let dir = socket
        .parent()
        .ok_or_else(|| format!("{}: no directory", socket.display()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if meta.uid() != owner && meta.uid() != 0 {
        return Err(format!(
            "{} belongs to another user; the web door's directory must be the agent's user's \
             (or root's)",
            dir.display()
        ));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "{} has mode {mode:03o}: others may write to it, so they could put a socket of their \
             own in the web door's place (chmod g-w,o-w)",
            dir.display()
        ));
    }
    let same = std::fs::canonicalize(dir).ok() == std::fs::canonicalize(data_dir).ok();
    Ok(same.then(|| {
        "the web door is in the data directory: only a web host running as the agent's own user \
         can reach it, and that user can read the agent's files; give it a directory of its own \
         to run the web host as another user"
            .to_string()
    }))
}

/// Binds a socket with `mode`, replacing one a daemon that is gone left behind. The caller holds
/// the data directory's lock, so no living daemon is listening there.
fn bind(path: &Path, mode: u32) -> Result<UnixListener, String> {
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{shown}: {e}"))?;
    Ok(listener)
}

async fn write_line(write: &mut OwnedWriteHalf, line: &str) -> std::io::Result<()> {
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await
}

/// Reads one request line as `door` takes it: a plain request at the operator's, a signed one
/// at the web door.
fn understand(door: Door, line: &str) -> Result<Incoming, ApiError> {
    match door {
        Door::Owner => api::parse(line).map(Incoming::Owner),
        Door::Web => {
            let (signed, content) = session::verify(line)?;
            api::parse(&content).map(|request| Incoming::Page(signed, request))
        }
    }
}

async fn connection(stream: UnixStream, commands: mpsc::Sender<Command>, door: Door) {
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
        Ok(Ok(_)) => understand(door, &line),
    };
    let incoming = match request {
        Ok(incoming) => incoming,
        Err(error) => {
            let _ = write_line(&mut write, &api::error_line(&error)).await;
            return;
        }
    };
    let watching = match &incoming {
        Incoming::Owner(request) | Incoming::Page(_, request) => {
            matches!(request.call, Call::Watch(_))
        }
    };
    let (lines, mut answers) = mpsc::channel::<String>(LINES_BUFFER);
    if commands.send(Command { incoming, lines }).await.is_err() {
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

async fn accept(
    listener: UnixListener,
    allowed: Vec<u32>,
    door: Door,
    most: usize,
    commands: mpsc::Sender<Command>,
) {
    let slots = Arc::new(Semaphore::new(most));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        // The directory's mode already keeps other users out; this is the same rule, asked
        // of the kernel for this one connection.
        let uid = stream.peer_cred().map(|cred| cred.uid()).ok();
        if !uid.is_some_and(|uid| allowed.contains(&uid)) {
            continue;
        }
        match slots.clone().try_acquire_owned() {
            Ok(slot) => {
                let commands = commands.clone();
                tokio::spawn(async move {
                    connection(stream, commands, door).await;
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
    let listener = bind(&path, 0o600)?;
    log(format!("listening on {}", path.display()));
    let web = match (config.web_socket_path(), &config.web) {
        (Some(web_path), Some(web)) => {
            if let Some(warning) = web_dir_ok(&web_path, owner, &config.data_dir)? {
                log(format!("note: {warning}"));
            }
            let listener = bind(&web_path, web.socket_mode)?;
            let mut allowed = vec![owner];
            allowed.extend(web.allow_uids.iter().copied());
            log(format!(
                "web door on {} (mode {:03o}; users {allowed:?})",
                web_path.display(),
                web.socket_mode
            ));
            Some((listener, allowed))
        }
        _ => None,
    };
    let mut engine = Engine::start(config, wallet, backend, log).await?;
    let (commands, mut inbox) = mpsc::channel::<Command>(MAX_CONNECTIONS);
    let _accepting = Accepting(tokio::spawn(accept(
        listener,
        vec![owner],
        Door::Owner,
        MAX_CONNECTIONS,
        commands,
    )));
    let (web_commands, mut web_inbox) = mpsc::channel::<Command>(MAX_WEB_CONNECTIONS);
    let mut web_open = web.is_some();
    let _web_accepting = web.map(|(listener, allowed)| {
        Accepting(tokio::spawn(accept(
            listener,
            allowed,
            Door::Web,
            MAX_WEB_CONNECTIONS,
            web_commands,
        )))
    });
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // In this order, every time: the operator, the runs, then pages.
        tokio::select! {
            biased;
            command = inbox.recv() => match command {
                Some(command) => engine.handle(command.incoming, command.lines).await,
                None => return Err("the socket stopped accepting connections".into()),
            },
            _ = ticker.tick() => engine.tick().await,
            command = web_inbox.recv(), if web_open => match command {
                Some(command) => engine.handle(command.incoming, command.lines).await,
                None => web_open = false,
            },
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
        drop(bind(&path, 0o600).expect("first"));
        // The first daemon is gone; its socket file is still there.
        assert!(path.exists());
        let listener = bind(&path, 0o600).expect("again");
        let mode = std::fs::metadata(&path).expect("stat").mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        std::fs::remove_file(&path).expect("rm");
        std::fs::write(&path, "not a socket").expect("write");
        assert!(bind(&path, 0o600).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "not a socket"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_web_door_needs_a_directory_nobody_else_writes_to() {
        let base = std::env::temp_dir().join(format!("bipnip-webdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let door = base.join("door");
        std::fs::create_dir_all(&data).expect("mkdir");
        std::fs::create_dir_all(&door).expect("mkdir");
        let me = std::fs::metadata(&base).expect("stat").uid();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        // In the data directory: allowed, with a warning.
        let inside = web_dir_ok(&data.join("web.sock"), me, &data).expect("inside");
        assert!(inside.is_some_and(|warning| warning.contains("data directory")));
        // A directory of its own, readable by a group: allowed, quietly.
        for mode in [0o750, 0o2750, 0o700] {
            std::fs::set_permissions(&door, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert_eq!(
                web_dir_ok(&door.join("web.sock"), me, &data),
                Ok(None),
                "{mode:o}"
            );
        }
        // Writable by others: refused.
        for mode in [0o770, 0o757, 0o777] {
            std::fs::set_permissions(&door, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert!(
                web_dir_ok(&door.join("web.sock"), me, &data).is_err(),
                "{mode:o}"
            );
        }
        // Another user's directory: refused, unless it is root's (so a run as root, as in a test
        // VM, cannot show this half).
        std::fs::set_permissions(&door, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        if me != 0 {
            assert!(web_dir_ok(&door.join("web.sock"), me + 1, &data).is_err());
        }
        assert!(web_dir_ok(&base.join("missing").join("web.sock"), me, &data).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn each_door_reads_its_own_kind_of_request() {
        use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Keys, Kind};
        let plain = r#"{"v":1,"method":"status"}"#;
        assert!(matches!(
            understand(Door::Owner, plain),
            Ok(Incoming::Owner(_))
        ));
        assert!(
            understand(Door::Web, plain).is_err(),
            "a page's request is signed"
        );
        let page = Keys::generate();
        let signed = EventBuilder::new(Kind::from(session::KIND_WEB_REQUEST), plain)
            .finalize(&page)
            .expect("sign");
        let line = serde_json::to_string(&signed).expect("json");
        match understand(Door::Web, &line) {
            Ok(Incoming::Page(by, request)) => {
                assert_eq!(by.key, page.public_key());
                assert_eq!(request.call, Call::Status);
            }
            other => panic!("not read as a page's request: {other:?}"),
        }
        assert!(
            understand(Door::Owner, &line).is_err(),
            "the operator's socket takes plain requests"
        );
        // A signed event whose content is no request.
        let junk = EventBuilder::new(Kind::from(session::KIND_WEB_REQUEST), "hello")
            .finalize(&page)
            .expect("sign");
        assert!(understand(Door::Web, &serde_json::to_string(&junk).expect("json")).is_err());
    }
}
