//! `POST /agent/call`: a paired page's signed request, passed on unread to the agent's web door,
//! and the agent's answer streamed back (`docs/decisions/0011-key-roles-pairing-and-session-keys.md`).
//!
//! The host holds no key and reads no request: the agent checks the signature, the session and the
//! scope, and is the boundary. The host checks what it can see: the request comes from a page of
//! this origin (`Origin`, checked for every POST before this is reached, and `Sec-Fetch-Site` when
//! the browser sends it), is JSON, is one line and not too long. Only then does it take one of
//! [`MAX_AGENT_STREAMS`] slots and open the socket.
//!
//! The agent answers with status lines and a final line (`application/x-ndjson`). A `watch` is a
//! request the agent never ends: a blank line goes to the page every [`HEARTBEAT`] while nothing else
//! does, so a page that went away is found at the next write, and the socket closes with the
//! response.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, Limited};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Interval;

/// Largest request passed to the agent: a signed request line.
pub const MAX_AGENT_BODY: usize = 32 * 1024;
/// How many requests may be open to the agent at once (watches stay open).
pub const MAX_AGENT_STREAMS: usize = 4;
/// How often a blank line goes to a page that is waiting on the agent.
pub const HEARTBEAT: Duration = Duration::from_secs(20);
/// How long the agent's socket has to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The agent's web door, as this host reaches it.
pub struct AgentLink {
    /// The socket (the agent's `[web] socket`).
    pub socket: PathBuf,
    /// One permit per open request.
    slots: Arc<Semaphore>,
    /// How often a waiting page is sent a blank line.
    heartbeat: Duration,
}

impl AgentLink {
    /// The web door at `socket`.
    pub fn new(socket: PathBuf) -> Self {
        Self::with_heartbeat(socket, HEARTBEAT)
    }

    /// The same, with another heartbeat (tests).
    pub fn with_heartbeat(socket: PathBuf, heartbeat: Duration) -> Self {
        AgentLink {
            socket,
            slots: Arc::new(Semaphore::new(MAX_AGENT_STREAMS)),
            heartbeat,
        }
    }
}

fn plain(status: StatusCode, msg: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        msg,
    )
        .into_response()
}

/// What keeps a request to the agent open: its lines, the heartbeat, the socket's other half
/// (closing it would tell the agent the page hung up) and the slot. All of it goes when the
/// response does.
struct Pipe {
    lines: Lines<BufReader<OwnedReadHalf>>,
    beat: Interval,
    _write: OwnedWriteHalf,
    _slot: OwnedSemaphorePermit,
}

/// True when the browser says the request comes from a page of this origin, or says nothing
/// (a page on a plain-HTTP address is not sent `Sec-Fetch-*`). `none` (typed in the address bar,
/// a bookmark) is not a page asking.
fn same_origin(req: &Request) -> bool {
    match req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
    {
        None => true,
        Some(site) => site.eq_ignore_ascii_case("same-origin"),
    }
}

fn is_json(req: &Request) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        })
}

/// Answers `POST /agent/call`. The caller has checked `Host`, and `Origin` for this POST.
pub async fn call(link: Option<&AgentLink>, req: Request) -> Response {
    if req.method() != Method::POST {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if !same_origin(&req) {
        return plain(StatusCode::FORBIDDEN, "cross-site request refused");
    }
    if !is_json(&req) {
        return plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a request is JSON");
    }
    // The whole body is read before anything else is spent on it.
    let body = match Limited::new(req.into_body(), MAX_AGENT_BODY)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
    };
    let Ok(text) = String::from_utf8(body.to_vec()) else {
        return plain(StatusCode::BAD_REQUEST, "a request is text");
    };
    if text.trim().is_empty() || text.contains(['\n', '\r']) {
        return plain(StatusCode::BAD_REQUEST, "a request is one line");
    }
    let Some(link) = link else {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "no agent is connected to this host",
        );
    };
    let Ok(slot) = link.slots.clone().try_acquire_owned() else {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are open to the agent; close one",
        );
    };
    let stream =
        match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&link.socket)).await {
            Ok(Ok(stream)) => stream,
            _ => return plain(StatusCode::BAD_GATEWAY, "the agent cannot be reached"),
        };
    let (read, mut write) = stream.into_split();
    let mut line = text.into_bytes();
    line.push(b'\n');
    if write.write_all(&line).await.is_err() || write.flush().await.is_err() {
        return plain(StatusCode::BAD_GATEWAY, "the agent cannot be reached");
    }
    let start = tokio::time::Instant::now() + link.heartbeat;
    let pipe = Pipe {
        lines: BufReader::new(read).lines(),
        beat: tokio::time::interval_at(start, link.heartbeat),
        _write: write,
        _slot: slot,
    };
    let stream = futures_util::stream::unfold(pipe, |mut pipe| async move {
        tokio::select! {
            line = pipe.lines.next_line() => match line {
                Ok(Some(mut line)) => {
                    line.push('\n');
                    Some((Ok::<Bytes, std::io::Error>(Bytes::from(line)), pipe))
                }
                // The agent said its last, or the socket broke: the response ends.
                _ => None,
            },
            _ = pipe.beat.tick() => Some((Ok(Bytes::from_static(b"\n")), pipe)),
        }
    });
    let mut resp = Response::new(Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
