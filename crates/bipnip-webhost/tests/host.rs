//! End to end through the host, with a local "coordinator" standing in for an onion service
//! (`LoopbackForTests` connects to 127.0.0.1 whatever the onion name).

// Test setup (binding sockets, writing fixtures) panics on failure by design.
#![allow(clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::ws::{Message as WsMsg, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bipnip_webhost::app::{AppState, router};
use bipnip_webhost::connector::LoopbackForTests;
use bipnip_webhost::federation::Coordinators;
use bipnip_webhost::security::Allowed;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::Message as TMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ONION: &str = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx";

async fn mock_coordinator() -> SocketAddr {
    async fn any(req: Request) -> Response {
        let h = req.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        if req.uri().path() == "/evil" {
            return (
                [("content-type", "text/html"), ("set-cookie", "a=b")],
                "<script>alert(1)</script>",
            )
                .into_response();
        }
        let body = serde_json::json!({
            "path": req.uri().to_string(),
            "method": req.method().as_str(),
            "host": get("host"),
            "authorization": get("authorization"),
            "cookie": get("cookie"),
            "origin": get("origin"),
            "referer": get("referer"),
        });
        (
            [("content-type", "application/json"), ("set-cookie", "s=1")],
            body.to_string(),
        )
            .into_response()
    }
    async fn relay(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(m)) = socket.recv().await {
                if let WsMsg::Text(t) = m {
                    let _ = socket
                        .send(WsMsg::Text(format!("echo:{}", t.as_str()).into()))
                        .await;
                }
            }
        })
    }
    let app = Router::new()
        .route("/relay/", axum::routing::get(relay))
        .fallback(any);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    addr
}

fn dist(coordinator_port: u16) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bipnip-webhost-test-{}-{}",
        std::process::id(),
        coordinator_port
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("static/frontend")).expect("mkdir");
    let page = |name: &str| {
        format!(
            "<html><head>\n<meta http-equiv=\"onion-location\" content=\"{{{{ ONION_LOCATION }}}}\" />\n<script>window.RobosatsSettings='{name}'</script>\n<script defer src=\"/static/frontend/main.js\"></script>\n</head><body></body></html>\n"
        )
    };
    std::fs::write(dir.join("basic.html"), page("selfhosted-basic")).expect("write");
    std::fs::write(dir.join("pro.html"), page("selfhosted-pro")).expect("write");
    std::fs::write(dir.join("static/frontend/main.js"), "console.log(1)").expect("write");
    std::fs::write(dir.join("secret.txt"), "outside static").expect("write");
    std::fs::write(
        dir.join("static/federation.json"),
        format!(
            r#"{{"temple": {{"shortAlias": "temple", "mainnet": {{"onion": "http://{ONION}.onion:{coordinator_port}"}}, "testnet": {{"onion": null}}}}}}"#
        ),
    )
    .expect("write");
    dir
}

async fn host() -> SocketAddr {
    let coordinator = mock_coordinator().await;
    let dist = dist(coordinator.port());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let read = |n: &str| std::fs::read_to_string(dist.join(n)).expect("read");
    let state = Arc::new(AppState {
        static_dir: std::fs::canonicalize(dist.join("static")).expect("canon"),
        basic: read("basic.html"),
        pro: read("pro.html"),
        coordinators: Coordinators::from_federation_json(&read("static/federation.json"))
            .expect("federation"),
        allowed: Allowed::new(vec![addr.to_string()]),
        connector: Arc::new(LoopbackForTests),
    });
    tokio::spawn(async move { axum::serve(listener, router(state)).await.expect("serve") });
    addr
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(conn);
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = sender
        .send_request(
            req.body(Full::new(axum::body::Bytes::from(body.to_string())))
                .expect("req"),
        )
        .await
        .expect("send");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn hv<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

#[tokio::test]
async fn pages_carry_a_nonce_matching_the_csp() {
    let addr = host().await;
    let (status, h, body) = call(addr, "GET", "/garage/", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let csp = hv(&h, "content-security-policy");
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .and_then(|r| r.split('\'').next())
        .expect("nonce in csp");
    assert_eq!(
        body.matches(&format!("<script nonce=\"{nonce}\"")).count(),
        2
    );
    assert!(!body.contains("onion-location"));
    assert!(body.contains("selfhosted-basic"));
    assert_eq!(hv(&h, "x-frame-options"), "DENY");
    assert_eq!(hv(&h, "cache-control"), "no-store");
    let (_, h2, _) = call(addr, "GET", "/pro/", &[], "").await;
    assert_ne!(
        hv(&h2, "content-security-policy"),
        csp,
        "nonce is per response"
    );
}

#[tokio::test]
async fn unknown_host_is_refused() {
    let addr = host().await;
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .uri("/")
        .header("host", "evil.example")
        .body(Body::empty())
        .expect("req");
    let resp = sender.send_request(req).await.expect("send");
    assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn proxy_forwards_only_allowed_headers_and_needs_our_origin() {
    let addr = host().await;
    let origin = format!("http://{addr}");
    let (status, _, _) = call(
        addr,
        "POST",
        "/mainnet/temple/api/make/",
        &[("authorization", "Token abc")],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "POST without Origin");
    let (status, _, _) = call(
        addr,
        "POST",
        "/mainnet/temple/api/make/",
        &[("origin", "http://evil.example")],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "POST from another origin");

    let (status, h, body) = call(
        addr,
        "POST",
        "/mainnet/temple/api/make/?x=1",
        &[
            ("origin", &origin),
            ("authorization", "Token abc"),
            ("cookie", "session=1"),
            ("referer", "http://127.0.0.1/garage/"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(seen["path"], "/api/make/?x=1");
    assert_eq!(seen["method"], "POST");
    assert_eq!(seen["authorization"], "Token abc");
    assert!(seen["cookie"].is_null());
    assert!(seen["origin"].is_null());
    assert!(seen["referer"].is_null());
    assert!(
        seen["host"]
            .as_str()
            .unwrap_or("")
            .starts_with(&format!("{ONION}.onion:"))
    );
    assert!(
        h.get("set-cookie").is_none(),
        "coordinator cookies are dropped"
    );
    assert_eq!(
        hv(&h, "content-security-policy"),
        "sandbox; default-src 'none'"
    );
}

#[tokio::test]
async fn cross_site_proxy_fetches_are_refused() {
    let addr = host().await;
    let (status, _, _) = call(
        addr,
        "GET",
        "/mainnet/temple/api/info/",
        &[("sec-fetch-site", "cross-site")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(
        addr,
        "GET",
        "/mainnet/temple/api/info/",
        &[("sec-fetch-site", "same-origin")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn coordinator_html_is_sandboxed() {
    let addr = host().await;
    let (status, h, _) = call(addr, "GET", "/mainnet/temple/evil", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hv(&h, "content-security-policy"),
        "sandbox; default-src 'none'"
    );
    assert_eq!(hv(&h, "x-content-type-options"), "nosniff");
}

#[tokio::test]
async fn unknown_routes_and_traversal_are_404() {
    let addr = host().await;
    for path in [
        "/testnet/temple/api/info/",
        "/mainnet/nobody/api/info/",
        "/static/../secret.txt",
        "/static/%2e%2e/secret.txt",
        "/static/frontend/../../secret.txt",
        "/nothing",
    ] {
        let (status, _, _) = call(addr, "GET", path, &[], "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, h, body) = call(addr, "GET", "/static/frontend/main.js", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "console.log(1)");
    assert!(hv(&h, "content-type").starts_with("text/javascript"));
}

#[tokio::test]
async fn relay_websocket_is_proxied_only_for_our_origin() {
    let addr = host().await;
    let url = format!("ws://{addr}/mainnet/temple/relay/");

    let mut req = url.clone().into_client_request().expect("req");
    req.headers_mut()
        .insert("origin", "http://evil.example".parse().expect("hv"));
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    assert!(
        tokio_tungstenite::client_async(req, stream).await.is_err(),
        "cross-site websocket refused"
    );

    let mut req = url.into_client_request().expect("req");
    req.headers_mut()
        .insert("origin", format!("http://{addr}").parse().expect("hv"));
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (mut ws, _) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws");
    ws.send(TMsg::text("hello")).await.expect("send");
    let reply = ws.next().await.expect("reply").expect("ok");
    assert_eq!(reply.into_text().expect("text").as_str(), "echo:hello");
}
