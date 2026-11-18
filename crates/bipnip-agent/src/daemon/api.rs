//! The words of the agent's local API (`docs/agent-api.md`): one request line in, status lines
//! and one final line out. Nothing here does I/O; the socket is in [`super::server`] and the
//! work in [`super::engine`].
//!
//! Unknown methods, unknown fields and a wrong version are errors. A request is parsed into a
//! [`Call`] before anything acts on it, so nothing downstream sees loose JSON.

use nostr_sdk::prelude::PublicKey;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The version of the request format this agent speaks.
pub const API_VERSION: u32 = 1;
/// The longest request line read, in bytes.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Why a request failed, as the client is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Not JSON, an unknown method, a missing or malformed parameter.
    BadRequest,
    /// No such run, order or approval.
    NotFound,
    /// A check failed; the message says which.
    Refused,
    /// Over a limit; parked until approved.
    NeedsApproval,
    /// Too many connections.
    Busy,
    /// The Lightning node could not be asked.
    Node,
    /// The relays could not be reached.
    Relay,
    /// The agent itself failed (a file it could not write, for one).
    Internal,
}

impl Code {
    /// The code as it is written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::BadRequest => "bad-request",
            Code::NotFound => "not-found",
            Code::Refused => "refused",
            Code::NeedsApproval => "needs-approval",
            Code::Busy => "busy",
            Code::Node => "node",
            Code::Relay => "relay",
            Code::Internal => "internal",
        }
    }
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// What kind of failure.
    pub code: Code,
    /// What happened, for a person. Never holds a secret.
    pub message: String,
    /// For [`Code::NeedsApproval`]: the name of the parked request.
    pub approval: Option<String>,
}

impl ApiError {
    /// A failure of kind `code`.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        ApiError {
            code,
            message: message.into(),
            approval: None,
        }
    }

    /// A malformed request.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message)
    }

    /// A check that failed.
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(Code::Refused, message)
    }

    /// Something the agent does not have.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::NotFound, message)
    }

    /// A failure of the agent itself.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Code::Internal, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

fn full() -> u8 {
    100
}

fn three() -> u32 {
    3
}

fn yes() -> bool {
    true
}

fn a_day() -> u64 {
    86_400
}

fn two_days() -> u64 {
    172_800
}

/// `lease.offer`: liquidity to sell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseOfferParams {
    /// Smallest channel, in satoshi.
    pub min_capacity_sat: u64,
    /// Largest channel, in satoshi.
    pub max_capacity_sat: u64,
    /// Lease length in blocks.
    pub duration_blocks: u32,
    /// Flat part of the fee, in satoshi.
    pub fee_base_sat: u64,
    /// Part of the fee proportional to the capacity, in parts per million.
    pub fee_ppm: u32,
    /// Fee rate of the funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
    /// Share of the fee paid upfront, in percent.
    #[serde(default = "full")]
    pub upfront_percent: u8,
    /// Confirmations the buyer waits for.
    #[serde(default = "three")]
    pub min_confirmations: u32,
    /// Whether the channel is announced.
    #[serde(default = "yes")]
    pub announce: bool,
    /// How long the offer stays open, in seconds.
    #[serde(default = "a_day")]
    pub expires_in_secs: u64,
    /// What the buyer locks at the instance, in satoshi; 0 for none.
    #[serde(default)]
    pub bond_sat: u64,
    /// The instance to offer on (npub or hex), when several are configured.
    #[serde(default)]
    pub instance: Option<String>,
}

/// `lease.take`: an order to take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseTakeParams {
    /// The order, as the instance published it.
    pub order_id: Uuid,
    /// The capacity wanted, in satoshi.
    pub capacity_sat: u64,
    /// Where the seller can reach our node (`host:port`).
    #[serde(default)]
    pub node_address: Option<String>,
    /// The instance the order is on, when several are configured.
    #[serde(default)]
    pub instance: Option<String>,
}

/// `ring.create`: a ring to propose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RingCreateParams {
    /// Capacity of every channel, in satoshi.
    pub capacity_sat: u64,
    /// How many members the ring has, ourselves included.
    pub slots: u8,
    /// Fee rate of our funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
    /// Whether the channels are announced.
    #[serde(default = "yes")]
    pub announce: bool,
    /// Seconds members have to open once the ring is full.
    #[serde(default = "two_days")]
    pub open_window_secs: u64,
    /// How long the slots stay open, in seconds.
    #[serde(default = "a_day")]
    pub expires_in_secs: u64,
    /// What every member locks at the instance, in satoshi; 0 for none.
    #[serde(default)]
    pub bond_sat: u64,
    /// Where the member that opens to us can reach our node (`host:port`).
    #[serde(default)]
    pub node_address: Option<String>,
    /// The instance to create it on, when several are configured.
    #[serde(default)]
    pub instance: Option<String>,
}

/// `ring.join`: a ring to take a slot of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RingJoinParams {
    /// The ring's order, as the instance published it.
    pub order_id: Uuid,
    /// Fee rate of our funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
    /// Where the member that opens to us can reach our node (`host:port`).
    #[serde(default)]
    pub node_address: Option<String>,
    /// The instance the ring is on, when several are configured.
    #[serde(default)]
    pub instance: Option<String>,
}

/// Which orders `orders` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrderKind {
    /// Liquidity offered for lease.
    Lease,
    /// Rings with slots left.
    Ring,
}

/// `orders`: what to list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdersParams {
    /// Only this kind.
    #[serde(default)]
    pub kind: Option<OrderKind>,
    /// Only this instance.
    #[serde(default)]
    pub instance: Option<String>,
    /// Leases that offer this capacity (priced at it), or rings with channels at least this
    /// large, in satoshi.
    #[serde(default)]
    pub capacity_sat: Option<u64>,
    /// Leases whose whole fee is at most this, in satoshi.
    #[serde(default)]
    pub max_fee_sat: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunParams {
    run: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalParams {
    approval: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchParams {
    #[serde(default)]
    run: Option<String>,
}

/// A request, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// How the agent is.
    Status,
    /// The open orders of the configured instances.
    Orders(OrdersParams),
    /// Offer liquidity.
    LeaseOffer(LeaseOfferParams),
    /// Take a lease order.
    LeaseTake(LeaseTakeParams),
    /// Back out of a lease run.
    LeaseCancel(String),
    /// The stored lease runs.
    LeaseList,
    /// Propose a ring.
    RingCreate(RingCreateParams),
    /// Take a slot of a ring.
    RingJoin(RingJoinParams),
    /// Back out of a ring run.
    RingCancel(String),
    /// The stored ring runs.
    RingList,
    /// Publish our node's binding.
    BindingPublish,
    /// The parked requests.
    Approvals,
    /// Let a parked request go ahead.
    Approve(String),
    /// Drop a parked request.
    Deny(String),
    /// Follow one run, or all of them.
    Watch(Option<String>),
}

impl Call {
    /// True for the requests that start a run, the only ones that can be parked.
    pub fn starts_a_run(&self) -> bool {
        matches!(
            self,
            Call::LeaseOffer(_) | Call::LeaseTake(_) | Call::RingCreate(_) | Call::RingJoin(_)
        )
    }
}

/// A request as it arrived and as it was understood. The words are kept so that a parked
/// request can be stored and read again exactly as it was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The method's name.
    pub method: String,
    /// Its parameters, as sent.
    pub params: Value,
    /// What they mean.
    pub call: Call,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u32,
    method: String,
    #[serde(default)]
    params: Value,
}

/// True for a name that is safe to look up as a file: what the agent itself gives runs and
/// approvals (lower-case letters, digits and dashes). Anything else is not a name of ours, and
/// never reaches the file system.
pub fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 96
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn typed<T: serde::de::DeserializeOwned>(method: &str, params: &Value) -> Result<T, ApiError> {
    // A method whose parameters are all optional may be sent without any.
    let params = match params {
        Value::Null => Value::Object(Map::new()),
        other => other.clone(),
    };
    serde_json::from_value(params).map_err(|e| ApiError::bad_request(format!("{method}: {e}")))
}

fn none(method: &str, params: &Value) -> Result<(), ApiError> {
    match params {
        Value::Null => Ok(()),
        Value::Object(map) if map.is_empty() => Ok(()),
        _ => Err(ApiError::bad_request(format!(
            "{method} takes no parameters"
        ))),
    }
}

fn name(method: &str, what: &str, text: String) -> Result<String, ApiError> {
    if is_name(&text) {
        Ok(text)
    } else {
        Err(ApiError::bad_request(format!(
            "{method}: that is not the name of {what}"
        )))
    }
}

/// Reads a method and its parameters into a [`Call`].
pub fn understand(method: &str, params: &Value) -> Result<Call, ApiError> {
    Ok(match method {
        "status" => none(method, params).map(|()| Call::Status)?,
        "orders" => Call::Orders(typed(method, params)?),
        "lease.offer" => Call::LeaseOffer(typed(method, params)?),
        "lease.take" => Call::LeaseTake(typed(method, params)?),
        "lease.cancel" => {
            let p: RunParams = typed(method, params)?;
            Call::LeaseCancel(name(method, "a run", p.run)?)
        }
        "lease.list" => none(method, params).map(|()| Call::LeaseList)?,
        "ring.create" => Call::RingCreate(typed(method, params)?),
        "ring.join" => Call::RingJoin(typed(method, params)?),
        "ring.cancel" => {
            let p: RunParams = typed(method, params)?;
            Call::RingCancel(name(method, "a run", p.run)?)
        }
        "ring.list" => none(method, params).map(|()| Call::RingList)?,
        "binding.publish" => none(method, params).map(|()| Call::BindingPublish)?,
        "approvals" => none(method, params).map(|()| Call::Approvals)?,
        "approve" => {
            let p: ApprovalParams = typed(method, params)?;
            Call::Approve(name(method, "an approval", p.approval)?)
        }
        "deny" => {
            let p: ApprovalParams = typed(method, params)?;
            Call::Deny(name(method, "an approval", p.approval)?)
        }
        "watch" => {
            let p: WatchParams = typed(method, params)?;
            Call::Watch(match p.run {
                None => None,
                Some(run) => Some(name(method, "a run", run)?),
            })
        }
        other => {
            return Err(ApiError::bad_request(format!("unknown method {other:?}")));
        }
    })
}

/// Reads one request line.
pub fn parse(line: &str) -> Result<Request, ApiError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ApiError::bad_request("the request line is too long"));
    }
    let envelope: Envelope = serde_json::from_str(line.trim())
        .map_err(|e| ApiError::bad_request(format!("not a request: {e}")))?;
    if envelope.v != API_VERSION {
        return Err(ApiError::bad_request(format!(
            "this agent speaks version {API_VERSION} of the API, not {}",
            envelope.v
        )));
    }
    let call = understand(&envelope.method, &envelope.params)?;
    Ok(Request {
        method: envelope.method,
        params: envelope.params,
        call,
    })
}

/// An instance named in a request: an npub or 64 hex.
pub fn instance_key(text: &str) -> Result<PublicKey, ApiError> {
    PublicKey::parse(text).map_err(|_| ApiError::bad_request("instance must be an npub or 64 hex"))
}

/// A status line: something that happened before the final answer. `fields` must be a JSON
/// object, or is left out.
pub fn status_line(status: &str, fields: Value) -> String {
    let mut line = Map::new();
    line.insert("status".into(), Value::String(status.into()));
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            if key != "status" {
                line.insert(key, value);
            }
        }
    }
    Value::Object(line).to_string()
}

/// The final line of a request that worked.
pub fn ok_line(result: Value) -> String {
    json!({"ok": true, "result": result}).to_string()
}

/// The final line of a request that did not.
pub fn error_line(error: &ApiError) -> String {
    let mut body = Map::new();
    body.insert("code".into(), Value::String(error.code.as_str().into()));
    body.insert("message".into(), Value::String(error.message.clone()));
    if let Some(approval) = &error.approval {
        body.insert("approval".into(), Value::String(approval.clone()));
    }
    json!({"ok": false, "error": Value::Object(body)}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Result<Call, ApiError> {
        parse(line).map(|request| request.call)
    }

    #[test]
    fn reads_every_method() {
        let order = "5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f";
        for (line, want) in [
            (r#"{"v":1,"method":"status"}"#.to_string(), Call::Status),
            (
                r#"{"v":1,"method":"status","params":{}}"#.to_string(),
                Call::Status,
            ),
            (
                r#"{"v":1,"method":"orders"}"#.to_string(),
                Call::Orders(OrdersParams::default()),
            ),
            (
                r#"{"v":1,"method":"orders","params":{"kind":"ring"}}"#.to_string(),
                Call::Orders(OrdersParams {
                    kind: Some(OrderKind::Ring),
                    instance: None,
                }),
            ),
            (
                r#"{"v":1,"method":"lease.list"}"#.to_string(),
                Call::LeaseList,
            ),
            (
                r#"{"v":1,"method":"ring.list"}"#.to_string(),
                Call::RingList,
            ),
            (
                r#"{"v":1,"method":"binding.publish"}"#.to_string(),
                Call::BindingPublish,
            ),
            (
                r#"{"v":1,"method":"approvals"}"#.to_string(),
                Call::Approvals,
            ),
            (
                r#"{"v":1,"method":"approve","params":{"approval":"ab12"}}"#.to_string(),
                Call::Approve("ab12".into()),
            ),
            (
                r#"{"v":1,"method":"deny","params":{"approval":"ab12"}}"#.to_string(),
                Call::Deny("ab12".into()),
            ),
            (r#"{"v":1,"method":"watch"}"#.to_string(), Call::Watch(None)),
            (
                r#"{"v":1,"method":"watch","params":{"run":"ring-0f"}}"#.to_string(),
                Call::Watch(Some("ring-0f".into())),
            ),
            (
                r#"{"v":1,"method":"lease.cancel","params":{"run":"seller-0f"}}"#.to_string(),
                Call::LeaseCancel("seller-0f".into()),
            ),
            (
                r#"{"v":1,"method":"ring.cancel","params":{"run":"ring-0f"}}"#.to_string(),
                Call::RingCancel("ring-0f".into()),
            ),
            (
                format!(
                    r#"{{"v":1,"method":"lease.take","params":{{"order_id":"{order}","capacity_sat":600000}}}}"#
                ),
                Call::LeaseTake(LeaseTakeParams {
                    order_id: order.parse().expect("uuid"),
                    capacity_sat: 600_000,
                    node_address: None,
                    instance: None,
                }),
            ),
            (
                format!(
                    r#"{{"v":1,"method":"ring.join","params":{{"order_id":"{order}","feerate_sat_vb":2,"node_address":"127.0.0.1:9735"}}}}"#
                ),
                Call::RingJoin(RingJoinParams {
                    order_id: order.parse().expect("uuid"),
                    feerate_sat_vb: 2,
                    node_address: Some("127.0.0.1:9735".into()),
                    instance: None,
                }),
            ),
        ] {
            assert_eq!(call(&line), Ok(want), "{line}");
        }
    }

    #[test]
    fn what_is_left_out_of_an_offer_or_a_ring_gets_the_commands_defaults() {
        let offer = call(
            r#"{"v":1,"method":"lease.offer","params":{"min_capacity_sat":500000,
               "max_capacity_sat":1000000,"duration_blocks":4320,"fee_base_sat":100,
               "fee_ppm":2500,"feerate_sat_vb":2}}"#,
        )
        .expect("offer");
        let Call::LeaseOffer(offer) = offer else {
            panic!("an offer");
        };
        assert_eq!(offer.upfront_percent, 100);
        assert_eq!(offer.min_confirmations, 3);
        assert!(offer.announce);
        assert_eq!(offer.expires_in_secs, 86_400);
        assert_eq!(offer.bond_sat, 0);

        let ring = call(
            r#"{"v":1,"method":"ring.create","params":{"capacity_sat":300000,"slots":3,
               "feerate_sat_vb":2,"bond_sat":20000}}"#,
        )
        .expect("ring");
        let Call::RingCreate(ring) = ring else {
            panic!("a ring");
        };
        assert_eq!(ring.open_window_secs, 172_800);
        assert_eq!(ring.bond_sat, 20_000);
        assert!(ring.announce);
        assert!(Call::RingCreate(ring).starts_a_run());
        assert!(!Call::Status.starts_a_run());
    }

    #[test]
    fn refuses_what_it_does_not_understand() {
        for (line, why) in [
            ("", "empty"),
            ("status", "not JSON"),
            (r#"{"method":"status"}"#, "no version"),
            (r#"{"v":2,"method":"status"}"#, "another version"),
            (r#"{"v":1,"method":"wallet.dump"}"#, "unknown method"),
            (r#"{"v":1,"method":"status","extra":1}"#, "unknown field"),
            (
                r#"{"v":1,"method":"status","params":{"x":1}}"#,
                "parameters where none are taken",
            ),
            (
                r#"{"v":1,"method":"lease.take","params":{"order_id":"nope","capacity_sat":1}}"#,
                "not an order id",
            ),
            (
                r#"{"v":1,"method":"lease.take","params":{"order_id":"5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f"}}"#,
                "a missing parameter",
            ),
            (
                r#"{"v":1,"method":"lease.take","params":{"order_id":"5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f","capacity_sat":1,"mnemonic":"x"}}"#,
                "an unknown parameter",
            ),
            (
                r#"{"v":1,"method":"lease.take","params":{"order_id":"5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f","capacity_sat":-1}}"#,
                "a negative amount",
            ),
            (
                r#"{"v":1,"method":"lease.cancel","params":{"run":"../mnemonic"}}"#,
                "a path for a run",
            ),
            (
                r#"{"v":1,"method":"approve","params":{"approval":"a/b"}}"#,
                "a path for an approval",
            ),
            (
                r#"{"v":1,"method":"watch","params":{"run":"Seller-0F"}}"#,
                "a name we never give",
            ),
        ] {
            let error = call(line).expect_err(why);
            assert_eq!(error.code, Code::BadRequest, "{why}");
        }
        let long = format!(
            r#"{{"v":1,"method":"status","params":{{}}}}{}"#,
            " ".repeat(MAX_LINE_BYTES)
        );
        assert_eq!(call(&long).expect_err("too long").code, Code::BadRequest);
    }

    #[test]
    fn names_are_what_we_hand_out_and_nothing_else() {
        for good in ["seller-00ff", "buyer-abc123", "ring-0", "ab12cd34"] {
            assert!(is_name(good), "{good}");
        }
        let long = "a".repeat(97);
        for bad in ["", "..", "a/b", "a.json", "A", "a b", "a\n", long.as_str()] {
            assert!(!is_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn answers_are_single_lines() {
        let read = |line: &str| -> Value { serde_json::from_str(line).expect("json") };
        let status = status_line("started", json!({"run": "ring-0f", "status": "ignored"}));
        assert_eq!(
            read(&status),
            json!({"status": "started", "run": "ring-0f"})
        );
        assert_eq!(
            read(&status_line("tick", Value::Null)),
            json!({"status": "tick"})
        );
        assert_eq!(
            read(&ok_line(json!({"state": "taking"}))),
            json!({"ok": true, "result": {"state": "taking"}})
        );
        let mut parked = ApiError::new(Code::NeedsApproval, "over max_runs\nby one");
        parked.approval = Some("ab12".into());
        let line = error_line(&parked);
        assert!(
            !line.contains('\n'),
            "a line break inside a message is escaped"
        );
        let back = read(&line);
        assert_eq!(back["ok"], json!(false));
        assert_eq!(back["error"]["code"], json!("needs-approval"));
        assert_eq!(back["error"]["approval"], json!("ab12"));
        assert_eq!(
            read(&error_line(&ApiError::refused("no"))),
            json!({"ok": false, "error": {"code": "refused", "message": "no"}})
        );
    }
}
