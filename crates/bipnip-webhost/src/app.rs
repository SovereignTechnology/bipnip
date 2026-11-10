//! The host: the two UI pages and their static files from `dist/`, and `/<network>/<alias>/…`
//! proxied to that coordinator's onion over Tor (RoboSats' self-hosted mode: the page only ever
//! talks to this origin).
//!
//! Requests carry robot tokens and encrypted keys in `Authorization`; nothing here logs headers or
//! bodies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as TMsg;

use crate::connector::{BoxIo, Connector};
use crate::federation::{Coordinators, NETWORKS, Upstream};
use crate::html::prepare_page;
use crate::security::{self, Allowed};

/// Largest request body forwarded to a coordinator.
pub const MAX_REQUEST_BODY: usize = 1 << 20;
/// Largest response body accepted from a coordinator.
pub const MAX_RESPONSE_BODY: usize = 16 << 20;
/// Largest websocket message relayed to the page.
pub const MAX_WS_MESSAGE: usize = 4 << 20;
/// Largest static file served.
pub const MAX_STATIC_FILE: u64 = 64 << 20;
/// Time allowed to reach a coordinator and get its answer (onion circuits are slow).
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(90);

/// Request headers passed to coordinators; everything else (cookies, origin, referer,
/// user agent, forwarding headers) stays here.
const FORWARD_REQUEST: [header::HeaderName; 3] =
    [header::CONTENT_TYPE, header::AUTHORIZATION, header::ACCEPT];
/// Response headers passed back from coordinators.
const FORWARD_RESPONSE: [header::HeaderName; 2] = [header::CONTENT_TYPE, header::CACHE_CONTROL];
/// A coordinator's response opened directly in the browser renders sandboxed: an opaque origin
/// with no scripts, so it can't act as this origin (which holds the robot tokens).
const PROXIED_CSP: &str = "sandbox; default-src 'none'";

/// Everything a request needs.
pub struct AppState {
    /// Canonical path of `dist/static`.
    pub static_dir: PathBuf,
    /// `dist/basic.html`.
    pub basic: String,
    /// `dist/pro.html`.
    pub pro: String,
    /// Proxy routes.
    pub coordinators: Coordinators,
    /// Host names and origins this server answers to.
    pub allowed: Allowed,
    /// How coordinators are reached.
    pub connector: Arc<dyn Connector>,
}

/// The whole app: one fallback handler, since every route is decided by path shape.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(handle).with_state(state)
}

async fn handle(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let mut resp = route(&st, req).await;
    security::common_headers(resp.headers_mut());
    resp
}

fn plain(status: StatusCode, msg: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        msg,
    )
        .into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Basic,
    Pro,
}

/// The UI page for a browser route (RoboSats' Django routes), if `path` is one.
fn page_for(path: &str) -> Option<Page> {
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return Some(Page::Basic);
    }
    if p == "/pro" {
        return Some(Page::Pro);
    }
    let first = p.trim_start_matches('/').split('/').next().unwrap_or("");
    matches!(first, "create" | "garage" | "offers" | "order" | "settings").then_some(Page::Basic)
}

/// `/<network>/<alias>/<rest>` → `(network, alias, "/<rest>")` for a known network and a plain alias.
fn split_coordinator(path: &str) -> Option<(&str, &str, String)> {
    let mut it = path.trim_start_matches('/').splitn(3, '/');
    let network = it.next()?;
    let alias = it.next()?;
    let rest = it.next().unwrap_or("");
    let plain_alias = !alias.is_empty()
        && alias
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_');
    (NETWORKS.contains(&network) && plain_alias).then(|| (network, alias, format!("/{rest}")))
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

async fn route(st: &Arc<AppState>, req: Request) -> Response {
    if !st.allowed.host_ok(req.headers()) {
        return plain(StatusCode::MISDIRECTED_REQUEST, "unknown host");
    }
    if !st.allowed.origin_ok(req.method(), req.headers()) {
        return plain(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    let path = req.uri().path().to_string();
    let read_only = matches!(*req.method(), Method::GET | Method::HEAD);

    if let Some(page) = page_for(&path) {
        if !read_only {
            return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return html_page(st, page);
    }
    if let Some(rest) = path.strip_prefix("/static/") {
        if !read_only {
            return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return static_file(&st.static_dir, rest).await;
    }
    if let Some((network, alias, rest)) = split_coordinator(&path) {
        if !security::fetch_site_ok(req.headers()) {
            return plain(StatusCode::FORBIDDEN, "cross-site request refused");
        }
        let Some(upstream) = st.coordinators.get(network, alias).cloned() else {
            return plain(StatusCode::NOT_FOUND, "unknown coordinator");
        };
        let target = match req.uri().query() {
            Some(q) => format!("{rest}?{q}"),
            None => rest,
        };
        return if is_websocket(req.headers()) {
            proxy_websocket(st, upstream, target, req).await
        } else {
            proxy_http(st, upstream, target, req).await
        };
    }
    plain(StatusCode::NOT_FOUND, "not found")
}

fn html_page(st: &AppState, page: Page) -> Response {
    let nonce = security::nonce();
    let body = prepare_page(
        match page {
            Page::Basic => &st.basic,
            Page::Pro => &st.pro,
        },
        &nonce,
    );
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response();
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        "cross-origin-embedder-policy",
        HeaderValue::from_static("require-corp"),
    );
    if let Ok(v) = HeaderValue::from_str(&security::csp(&nonce)) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    resp
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn safe_segment(seg: &str) -> bool {
    !seg.is_empty()
        && !seg.starts_with('.')
        && seg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

async fn static_file(static_dir: &Path, rest: &str) -> Response {
    if !rest.split('/').all(safe_segment) {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    let candidate = static_dir.join(rest);
    let Ok(real) = tokio::fs::canonicalize(&candidate).await else {
        return plain(StatusCode::NOT_FOUND, "not found");
    };
    if !real.starts_with(static_dir) {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    match tokio::fs::metadata(&real).await {
        Ok(m) if m.is_file() && m.len() <= MAX_STATIC_FILE => {}
        _ => return plain(StatusCode::NOT_FOUND, "not found"),
    }
    let Ok(bytes) = tokio::fs::read(&real).await else {
        return plain(StatusCode::NOT_FOUND, "not found");
    };
    let cache = if rest.starts_with("frontend/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(rest)),
            (header::CACHE_CONTROL, cache),
        ],
        Bytes::from(bytes),
    )
        .into_response()
}

fn host_header(up: &Upstream) -> String {
    if up.port == 80 {
        up.host.clone()
    } else {
        format!("{}:{}", up.host, up.port)
    }
}

async fn connect(st: &AppState, up: &Upstream) -> Option<BoxIo> {
    match tokio::time::timeout(UPSTREAM_TIMEOUT, st.connector.connect(&up.host, up.port)).await {
        Ok(Ok(io)) => Some(io),
        _ => None,
    }
}

async fn proxy_http(st: &AppState, up: Upstream, target: String, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let mut builder = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(target)
        .header(header::HOST, host_header(&up));
    for name in FORWARD_REQUEST {
        if let Some(v) = parts.headers.get(&name) {
            builder = builder.header(name, v);
        }
    }
    let Ok(upstream_req) = builder.body(Full::new(body)) else {
        return plain(StatusCode::BAD_REQUEST, "bad request");
    };
    let Some(io) = connect(st, &up).await else {
        return plain(StatusCode::BAD_GATEWAY, "coordinator unreachable");
    };
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await
    else {
        return plain(StatusCode::BAD_GATEWAY, "coordinator handshake failed");
    };
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = match tokio::time::timeout(UPSTREAM_TIMEOUT, sender.send_request(upstream_req)).await
    {
        Ok(Ok(r)) => r,
        _ => return plain(StatusCode::GATEWAY_TIMEOUT, "coordinator did not answer"),
    };
    let (rparts, rbody) = resp.into_parts();
    let bytes = match tokio::time::timeout(
        UPSTREAM_TIMEOUT,
        Limited::new(rbody, MAX_RESPONSE_BODY).collect(),
    )
    .await
    {
        Ok(Ok(c)) => c.to_bytes(),
        _ => {
            return plain(
                StatusCode::BAD_GATEWAY,
                "coordinator response too large or cut",
            );
        }
    };
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = rparts.status;
    let h = out.headers_mut();
    for name in FORWARD_RESPONSE {
        if let Some(v) = rparts.headers.get(&name) {
            h.insert(name, v.clone());
        }
    }
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PROXIED_CSP),
    );
    out
}

async fn proxy_websocket(
    st: &Arc<AppState>,
    up: Upstream,
    target: String,
    req: Request,
) -> Response {
    let (mut parts, _body) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, st).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(io) = connect(st, &up).await else {
        return plain(StatusCode::BAD_GATEWAY, "coordinator unreachable");
    };
    let url = format!("ws://{}{target}", host_header(&up));
    let upstream = match tokio::time::timeout(
        UPSTREAM_TIMEOUT,
        tokio_tungstenite::client_async(url, io),
    )
    .await
    {
        Ok(Ok((stream, _))) => stream,
        _ => return plain(StatusCode::BAD_GATEWAY, "coordinator websocket failed"),
    };
    ws.max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| pump(socket, upstream))
}

fn to_upstream(m: WsMsg) -> TMsg {
    match m {
        WsMsg::Text(t) => TMsg::text(t.as_str()),
        WsMsg::Binary(b) => TMsg::binary(b),
        WsMsg::Ping(b) => TMsg::Ping(b),
        WsMsg::Pong(b) => TMsg::Pong(b),
        WsMsg::Close(_) => TMsg::Close(None),
    }
}

fn to_client(m: TMsg) -> Option<WsMsg> {
    match m {
        TMsg::Text(t) => Some(WsMsg::Text(t.as_str().into())),
        TMsg::Binary(b) => Some(WsMsg::Binary(b)),
        TMsg::Ping(b) => Some(WsMsg::Ping(b)),
        TMsg::Pong(b) => Some(WsMsg::Pong(b)),
        TMsg::Close(_) => Some(WsMsg::Close(None)),
        TMsg::Frame(_) => None,
    }
}

async fn pump(client: WebSocket, upstream: WebSocketStream<BoxIo>) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = upstream.split();
    let client_to_upstream = async {
        while let Some(Ok(m)) = c_rx.next().await {
            let m = to_upstream(m);
            let close = matches!(m, TMsg::Close(_));
            if u_tx.send(m).await.is_err() || close {
                break;
            }
        }
        let _ = u_tx.close().await;
    };
    let upstream_to_client = async {
        while let Some(Ok(m)) = u_rx.next().await {
            if m.len() > MAX_WS_MESSAGE {
                break;
            }
            let Some(m) = to_client(m) else { continue };
            let close = matches!(m, WsMsg::Close(_));
            if c_tx.send(m).await.is_err() || close {
                break;
            }
        }
        let _ = c_tx.close().await;
    };
    tokio::select! {
        _ = client_to_upstream => {}
        _ = upstream_to_client => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_follow_robosats_routes() {
        assert_eq!(page_for("/"), Some(Page::Basic));
        assert_eq!(page_for("/pro/"), Some(Page::Pro));
        assert_eq!(page_for("/garage/abc/"), Some(Page::Basic));
        assert_eq!(page_for("/order/temple/42/"), Some(Page::Basic));
        assert_eq!(page_for("/mainnet/temple/api/info/"), None);
        assert_eq!(page_for("/static/x.js"), None);
        assert_eq!(page_for("/professional"), None);
    }

    #[test]
    fn coordinator_paths_split() {
        assert_eq!(
            split_coordinator("/mainnet/temple/api/info/"),
            Some(("mainnet", "temple", "/api/info/".to_string()))
        );
        assert_eq!(
            split_coordinator("/testnet/lake/relay/"),
            Some(("testnet", "lake", "/relay/".to_string()))
        );
        assert_eq!(
            split_coordinator("/mainnet/temple"),
            Some(("mainnet", "temple", "/".to_string()))
        );
        assert_eq!(split_coordinator("/regtest/temple/api/"), None);
        assert_eq!(split_coordinator("/mainnet/Temple/api/"), None);
        assert_eq!(split_coordinator("/mainnet/../api/"), None);
    }

    #[test]
    fn static_segments_are_plain() {
        assert!(safe_segment("main.v0.8.8.abc.js"));
        assert!(!safe_segment(".."));
        assert!(!safe_segment(".env"));
        assert!(!safe_segment(""));
        assert!(!safe_segment("a b"));
        assert!(!safe_segment("a%2e"));
    }
}
