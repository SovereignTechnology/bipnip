//! Paired web pages (`docs/decisions/0011-key-roles-pairing-and-session-keys.md`).
//!
//! A web page reaches the daemon through a web host and a second socket, the web door
//! ([`super::server`]). The page holds a key of its own, a *session key*, and every request it sends
//! is a Nostr event of kind [`KIND_WEB_REQUEST`] signed with it: never published, only handed to
//! the agent. The event's content is an ordinary request line (`docs/agent-api.md`).
//!
//! What is checked where:
//!
//! - the connection ([`verify`]): the event's id, its signature and its kind, and that its content
//!   is a request. Nothing about who sent it;
//! - the engine, when it takes the request up: that the event is fresh ([`fresh`]) and not seen
//!   before, that it names this agent, that its key belongs to a session the operator paired and
//!   that has not ended, and that the session's scope covers what is asked ([`page_may`]).
//!
//! Pairing happens only while the operator holds a window open (`bipnip pair open`). A page asks
//! (`pair.request`) and is given a code; the operator types that code into `bipnip pair approve`.
//! Typing it is the comparison: the code is never listed where the agent runs, so a page that was
//! not the operator's cannot be approved by picking it from a list.
//!
//! What a session can never do from a page: approve, deny, pair, revoke, publish the node's binding,
//! or look up the record of a node it names. Every run it asks for is parked until the operator
//! approves it where the agent runs.

use std::path::{Path, PathBuf};

use nostr_sdk::prelude::{Event, EventId, PublicKey};
use serde::{Deserialize, Serialize};

use super::api::{ApiError, Call, Code};
use crate::secretfile::{self, SecretFileError};

/// The kind of a page's request event: in the ephemeral range, so a relay that saw one would keep
/// nothing of it. It is never sent to one.
pub const KIND_WEB_REQUEST: u16 = 24735;
/// How old a request may be, in seconds.
pub const MAX_AGE_SECS: u64 = 120;
/// How far ahead of the agent's clock a request may be dated, in seconds.
pub const MAX_AHEAD_SECS: u64 = 5;
/// How long a pairing window stays open when the operator does not say, in seconds.
pub const PAIR_WINDOW_SECS: u64 = 180;
/// The longest pairing window, in seconds.
pub const MAX_PAIR_WINDOW_SECS: u64 = 600;
/// How long a page's pairing request waits for the operator, in seconds.
pub const PENDING_SECS: u64 = 600;
/// How many pairing requests may wait at once. None is ever pushed out by a newer one.
pub const MAX_PENDING: usize = 4;
/// How long a session lasts when the operator does not say, in days.
pub const DEFAULT_SESSION_DAYS: u64 = 7;
/// The longest a session may last, in days.
pub const MAX_SESSION_DAYS: u64 = 90;
/// How many of a session's requests may wait for approval at once.
pub const MAX_PARKED_PER_SESSION: usize = 3;
/// How long a page's parked request waits for approval before it lapses, in seconds.
pub const PAGE_PARKED_SECS: u64 = 24 * 3600;
/// The least time between two runs a session asks for, in seconds.
pub const START_EVERY_SECS: u64 = 10;
/// How many watches one session may hold open.
pub const MAX_PAGE_WATCHES: usize = 2;
/// How many requests one key may make within [`RATE_WINDOW_SECS`].
pub const RATE_REQUESTS: usize = 60;
/// The window [`RATE_REQUESTS`] is counted over, in seconds.
pub const RATE_WINDOW_SECS: u64 = 60;
/// The longest label a page may give itself, in characters.
pub const MAX_LABEL_CHARS: usize = 32;

/// A request from the web door whose event checked out: who signed it, when, and for whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The session key that signed it.
    pub key: PublicKey,
    /// The event's id: a second request with the same id is a replay.
    pub id: EventId,
    /// When the page says it made the request, unix seconds.
    pub created_at: u64,
    /// The agents it names in `p` tags.
    pub to: Vec<PublicKey>,
}

/// Reads one line from the web door: a signed event of kind [`KIND_WEB_REQUEST`] whose content is
/// a request. Returns who signed it and the request's text.
pub fn verify(line: &str) -> Result<(Signed, String), ApiError> {
    let event: Event = serde_json::from_str(line.trim())
        .map_err(|e| ApiError::bad_request(format!("not a signed request: {e}")))?;
    if event.kind.as_u16() != KIND_WEB_REQUEST {
        return Err(ApiError::bad_request(format!(
            "a request from a page is an event of kind {KIND_WEB_REQUEST}"
        )));
    }
    event
        .verify()
        .map_err(|_| ApiError::bad_request("the request's signature or id does not check out"))?;
    let signed = Signed {
        key: event.pubkey,
        id: event.id,
        created_at: event.created_at.as_secs(),
        to: event.tags.public_keys().collect(),
    };
    Ok((signed, event.content))
}

/// Whether a request made at `created_at` may be taken up at `now` by an agent that started at
/// `started`. A request older than [`MAX_AGE_SECS`] or dated more than [`MAX_AHEAD_SECS`] ahead is
/// stale, and so is one dated before the agent started: what was seen by the agent before a
/// restart can then never be seen again after it. The answer tells the page the agent's time, so a
/// page with a wrong clock can correct itself.
pub fn fresh(created_at: u64, now: u64, started: u64) -> Result<(), ApiError> {
    let earliest = now
        .saturating_sub(MAX_AGE_SECS)
        .max(started.saturating_add(MAX_AHEAD_SECS));
    if created_at < earliest || created_at > now.saturating_add(MAX_AHEAD_SECS) {
        let mut error = ApiError::new(
            Code::Stale,
            "the request is dated too far from the agent's time; ask again",
        );
        error.now = Some(now);
        return Err(error);
    }
    Ok(())
}

/// What a session may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Look: the agent's state, the order book, its runs, its own parked requests.
    Read,
    /// Also ask for runs (each parked until the operator approves it) and cancel its own.
    Trade,
}

impl Scope {
    /// The scope as the operator types it.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "read" => Some(Scope::Read),
            "trade" => Some(Scope::Trade),
            _ => None,
        }
    }
}

fn not_for_a_page() -> ApiError {
    ApiError::refused("a web page cannot ask this; it is asked with `bipnip` where the agent runs")
}

/// Whether a page whose session has `scope` may ask for `call`. Every request has its place
/// here: a new one does not compile until it is given one.
pub fn page_may(call: &Call, scope: Scope) -> Result<(), ApiError> {
    let needs_trade = match call {
        Call::Status
        | Call::Orders(_)
        | Call::LeaseList
        | Call::RingList
        | Call::Approvals
        | Call::Watch(_)
        | Call::PairRequest(_)
        | Call::PairStatus
        | Call::PairEnd => false,
        Call::LeaseOffer(_)
        | Call::LeaseTake(_)
        | Call::LeaseCancel(_)
        | Call::RingCreate(_)
        | Call::RingJoin(_)
        | Call::RingCancel(_) => true,
        // A node the page names would be sent to every relay in a filter: a way out for whatever
        // the page knows. The book carries its sellers' records instead.
        Call::Record(_)
        | Call::BindingPublish
        | Call::Approve(_)
        | Call::Deny(_)
        | Call::PairOpen(_)
        | Call::PairList
        | Call::PairApprove(_)
        | Call::PairDeny(_)
        | Call::Sessions
        | Call::Revoke(_) => return Err(not_for_a_page()),
    };
    if needs_trade && scope != Scope::Trade {
        return Err(ApiError::refused(
            "this page was paired to look only; pair it again with `--scope trade` to ask for this",
        ));
    }
    Ok(())
}

/// Whether the operator's own socket may ask for `call`: everything but what only a page asks.
pub fn owner_may(call: &Call) -> Result<(), ApiError> {
    match call {
        Call::PairRequest(_) | Call::PairStatus | Call::PairEnd => Err(ApiError::bad_request(
            "only a web page asks this, through the web door",
        )),
        _ => Ok(()),
    }
}

/// A label as a page gives it: printable ASCII only, at most [`MAX_LABEL_CHARS`], so it can be
/// shown where the agent runs without carrying anything else.
pub fn clean_label(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_LABEL_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// A fresh pairing code: twelve random digits, written in three groups of four.
pub fn pairing_code() -> Result<String, ApiError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::internal("no randomness for a code"))?;
    let digits = u64::from_le_bytes(bytes) % 1_000_000_000_000;
    let text = format!("{digits:012}");
    Ok(format!("{}-{}-{}", &text[0..4], &text[4..8], &text[8..12]))
}

/// A code as the operator typed it, in the form [`pairing_code`] writes: the digits may be
/// grouped by dashes or spaces, or not at all.
pub fn normal_code(typed: &str) -> Option<String> {
    let digits: String = typed.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    if digits.len() != 12 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}",
        &digits[0..4],
        &digits[4..8],
        &digits[8..12]
    ))
}

/// A page the operator paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The session key, 64 hex: the session's name.
    pub key: String,
    /// What the page called itself, cleaned ([`clean_label`]).
    pub label: String,
    /// What it may do.
    pub scope: Scope,
    /// When it was paired, unix seconds.
    pub created_at: u64,
    /// When it ends, unix seconds.
    pub expires_at: u64,
    /// When it was ended early (revoked by the operator, or ended by the page), if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
}

impl Session {
    /// True while the session may be used.
    pub fn live(&self, now: u64) -> bool {
        self.ended_at.is_none() && now < self.expires_at
    }
}

/// Why the sessions' files could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError(String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sessions: {}", self.0)
    }
}

impl std::error::Error for SessionError {}

fn file_error(path: &Path, error: SecretFileError) -> SessionError {
    SessionError(format!("{}: {error}", path.display()))
}

/// True for a session's name: a key as 64 lower-case hex.
pub fn is_session_name(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The paired pages, one owner-only file each, named by the session key.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Opens the store, creating its directory if needed.
    pub fn open(dir: &Path) -> Result<Self, SessionError> {
        secretfile::create_private_dir(dir).map_err(|e| file_error(dir, e))?;
        Ok(SessionStore {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, key: &str) -> Result<PathBuf, SessionError> {
        if !is_session_name(key) {
            return Err(SessionError("that is not the name of a session".into()));
        }
        Ok(self.dir.join(format!("{key}.json")))
    }

    /// Writes a session, replacing its earlier file.
    pub fn save(&self, session: &Session) -> Result<(), SessionError> {
        let path = self.path(&session.key)?;
        let json = serde_json::to_string(session).map_err(|e| SessionError(e.to_string()))?;
        secretfile::replace_private(&path, &json).map_err(|e| file_error(&path, e))
    }

    /// The session of `key`, if the operator ever paired it.
    pub fn get(&self, key: &str) -> Result<Option<Session>, SessionError> {
        let path = self.path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        let json = secretfile::read_secret(&path).map_err(|e| file_error(&path, e))?;
        let session: Session = serde_json::from_str(&json)
            .map_err(|e| SessionError(format!("{}: {e}", path.display())))?;
        if session.key != key {
            return Err(SessionError(format!(
                "{}: the file is not this session's",
                path.display()
            )));
        }
        Ok(Some(session))
    }

    /// Every session, oldest first.
    pub fn all(&self) -> Result<Vec<Session>, SessionError> {
        let io = |e: std::io::Error| SessionError(format!("{}: {e}", self.dir.display()));
        let mut all = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(io)? {
            let name = entry
                .map_err(io)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(key) = name.strip_suffix(".json")
                && is_session_name(key)
                && let Some(session) = self.get(key)?
            {
                all.push(session);
            }
        }
        all.sort_by(|a, b| (a.created_at, &a.key).cmp(&(b.created_at, &b.key)));
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    use super::*;
    use crate::daemon::api;

    fn request(keys: &Keys, kind: u16, content: &str, at: u64, to: Option<PublicKey>) -> String {
        let mut builder = EventBuilder::new(Kind::from(kind), content)
            .tag(Tag::parse(["nonce", "5f1c"]).expect("tag"))
            .custom_created_at(Timestamp::from_secs(at));
        if let Some(agent) = to {
            builder = builder.tag(Tag::public_key(agent));
        }
        let event = builder.finalize(keys).expect("sign");
        serde_json::to_string(&event).expect("json")
    }

    #[test]
    fn a_signed_request_is_read_with_its_signer_and_the_agent_it_names() {
        let page = Keys::generate();
        let agent = Keys::generate().public_key();
        let content = r#"{"v":1,"method":"status"}"#;
        let line = request(&page, KIND_WEB_REQUEST, content, 1_700_000_000, Some(agent));
        let (signed, text) = verify(&line).expect("verified");
        assert_eq!(signed.key, page.public_key());
        assert_eq!(signed.created_at, 1_700_000_000);
        assert_eq!(signed.to, vec![agent]);
        assert_eq!(text, content);
        assert_eq!(api::parse(&text).expect("a request").call, Call::Status);
    }

    #[test]
    fn a_changed_byte_another_kind_or_a_plain_request_is_refused() {
        let page = Keys::generate();
        let line = request(
            &page,
            KIND_WEB_REQUEST,
            r#"{"v":1,"method":"lease.list"}"#,
            1_700_000_000,
            None,
        );
        // The content changed after signing: the id no longer matches.
        let changed = line.replace("lease.list", "ring.list");
        assert!(verify(&changed).is_err());
        // Another author's signature on this id.
        let mut event: serde_json::Value = serde_json::from_str(&line).expect("json");
        event["pubkey"] = serde_json::json!(Keys::generate().public_key().to_hex());
        assert!(verify(&event.to_string()).is_err());
        // A signed event of another kind.
        let other = request(
            &page,
            1,
            r#"{"v":1,"method":"status"}"#,
            1_700_000_000,
            None,
        );
        assert!(verify(&other).is_err());
        // What the owner's socket takes is not a request here.
        assert!(verify(r#"{"v":1,"method":"status"}"#).is_err());
        assert!(verify("not json").is_err());
    }

    #[test]
    fn freshness_refuses_old_future_and_from_before_the_start() {
        let started = 1_000_000;
        let now = started + 1_000;
        assert!(fresh(now, now, started).is_ok());
        assert!(fresh(now - MAX_AGE_SECS, now, started).is_ok());
        assert!(fresh(now + MAX_AHEAD_SECS, now, started).is_ok());
        let old = fresh(now - MAX_AGE_SECS - 1, now, started).expect_err("old");
        assert_eq!(old.code, Code::Stale);
        assert_eq!(old.now, Some(now));
        assert!(fresh(now + MAX_AHEAD_SECS + 1, now, started).is_err());
        // Just after a start, nothing dated before it (or within the margin after) is taken: a
        // request seen before a restart is never seen again after it.
        let now = started + 30;
        assert!(fresh(started - 1, now, started).is_err());
        assert!(fresh(started + MAX_AHEAD_SECS - 1, now, started).is_err());
        assert!(fresh(started + MAX_AHEAD_SECS, now, started).is_ok());
    }

    #[test]
    fn scopes_cover_what_they_say_and_a_page_never_reaches_the_operator_s_requests() {
        let every = [
            (r#"{"v":1,"method":"status"}"#, Some(Scope::Read)),
            (
                r#"{"v":1,"method":"orders","params":{"records":true}}"#,
                Some(Scope::Read),
            ),
            (r#"{"v":1,"method":"lease.list"}"#, Some(Scope::Read)),
            (r#"{"v":1,"method":"ring.list"}"#, Some(Scope::Read)),
            (r#"{"v":1,"method":"approvals"}"#, Some(Scope::Read)),
            (r#"{"v":1,"method":"watch"}"#, Some(Scope::Read)),
            (r#"{"v":1,"method":"pair.status"}"#, Some(Scope::Read)),
            (r#"{"v":1,"method":"pair.end"}"#, Some(Scope::Read)),
            (
                r#"{"v":1,"method":"pair.request","params":{"label":"laptop"}}"#,
                Some(Scope::Read),
            ),
            (
                r#"{"v":1,"method":"lease.take","params":{"order_id":"5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f","capacity_sat":1}}"#,
                Some(Scope::Trade),
            ),
            (
                r#"{"v":1,"method":"lease.cancel","params":{"run":"buyer-0a"}}"#,
                Some(Scope::Trade),
            ),
            (
                r#"{"v":1,"method":"ring.cancel","params":{"run":"ring-0a"}}"#,
                Some(Scope::Trade),
            ),
            (
                r#"{"v":1,"method":"ring.join","params":{"order_id":"5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f","feerate_sat_vb":2}}"#,
                Some(Scope::Trade),
            ),
            (
                r#"{"v":1,"method":"approve","params":{"approval":"0a"}}"#,
                None,
            ),
            (
                r#"{"v":1,"method":"deny","params":{"approval":"0a"}}"#,
                None,
            ),
            (r#"{"v":1,"method":"binding.publish"}"#, None),
            (
                r#"{"v":1,"method":"record","params":{"node":"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"}}"#,
                None,
            ),
            (r#"{"v":1,"method":"pair.open"}"#, None),
            (r#"{"v":1,"method":"pair.list"}"#, None),
            (
                r#"{"v":1,"method":"pair.approve","params":{"code":"1234-5678-9012"}}"#,
                None,
            ),
            (
                r#"{"v":1,"method":"pair.deny","params":{"code":"1234-5678-9012"}}"#,
                None,
            ),
            (r#"{"v":1,"method":"sessions"}"#, None),
            (
                r#"{"v":1,"method":"revoke","params":{"session":"5c0a3f2e9d8c7b6a5f4e3d2c1b0a99887766554433221100ffeeddccbbaa9988"}}"#,
                None,
            ),
        ];
        for (line, least) in every {
            let call = api::parse(line).expect(line).call;
            let read = page_may(&call, Scope::Read).is_ok();
            let trade = page_may(&call, Scope::Trade).is_ok();
            match least {
                Some(Scope::Read) => assert!(read && trade, "{line}"),
                Some(Scope::Trade) => assert!(!read && trade, "{line}"),
                None => assert!(!read && !trade, "{line}"),
            }
            // What only a page asks is not for the owner's socket; everything else is.
            let page_only = matches!(
                call,
                Call::PairRequest(_) | Call::PairStatus | Call::PairEnd
            );
            assert_eq!(owner_may(&call).is_ok(), !page_only, "{line}");
        }
    }

    #[test]
    fn codes_and_labels() {
        let code = pairing_code().expect("code");
        assert_eq!(code.len(), 14);
        assert_eq!(normal_code(&code), Some(code.clone()));
        assert_eq!(normal_code(&code.replace('-', "")), Some(code.clone()));
        assert_eq!(normal_code(&code.replace('-', " ")), Some(code.clone()));
        assert_eq!(normal_code("1234-5678-901"), None);
        assert_eq!(normal_code("1234-5678-901x"), None);
        assert_ne!(pairing_code().expect("code"), code, "codes are random");
        assert_eq!(clean_label(" my\u{1b}[31m laptop\n"), "my[31m laptop");
        assert_eq!(clean_label(&"x".repeat(100)).len(), MAX_LABEL_CHARS);
    }

    #[test]
    fn sessions_are_stored_one_owner_only_file_each() {
        let dir = std::env::temp_dir().join(format!("bipnip-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).expect("open");
        let key = Keys::generate().public_key().to_hex();
        let session = Session {
            key: key.clone(),
            label: "laptop".into(),
            scope: Scope::Trade,
            created_at: 100,
            expires_at: 200,
            ended_at: None,
        };
        store.save(&session).expect("save");
        assert_eq!(store.get(&key).expect("get"), Some(session.clone()));
        assert!(session.live(150));
        assert!(!session.live(200));
        let ended = Session {
            ended_at: Some(120),
            ..session
        };
        assert!(!ended.live(150));
        store.save(&ended).expect("save");
        assert_eq!(store.all().expect("all"), vec![ended]);
        assert!(store.get("../mnemonic").is_err());
        assert_eq!(
            store
                .get(&Keys::generate().public_key().to_hex())
                .expect("get"),
            None
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(format!("{key}.json")))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
