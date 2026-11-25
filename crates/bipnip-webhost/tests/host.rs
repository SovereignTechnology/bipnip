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
use bipnip_webhost::agent::{AgentLink, MAX_AGENT_BODY, MAX_AGENT_STREAMS};
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
    host_with(None).await
}

/// A host whose page requests go to the agent's web door `agent`, if given.
async fn host_with(agent: Option<AgentLink>) -> SocketAddr {
    host_built(agent, true).await
}

/// The same, with coordinators reached (`tor`) or off, as with `--no-tor`.
async fn host_built(agent: Option<AgentLink>, tor: bool) -> SocketAddr {
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
        connector: if tor {
            Some(Arc::new(LoopbackForTests))
        } else {
            None
        },
        agent,
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
    let (status, h, _) = call(addr, "GET", "/garage/sometoken/", &[], "").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "deep links go to /");
    assert_eq!(hv(&h, "location"), "/");
    let (status, h, body) = call(addr, "GET", "/", &[], "").await;
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
    // Styles too, with no other way in.
    assert!(
        csp.contains(&format!("style-src 'self' 'nonce-{nonce}'")),
        "{csp}"
    );
    assert!(!csp.contains("unsafe-inline"), "{csp}");
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
    // A worker script under a COEP page must carry COEP itself, or Chromium blocks it.
    assert_eq!(hv(&h, "cross-origin-embedder-policy"), "require-corp");
    assert_eq!(hv(&h, "cross-origin-resource-policy"), "same-origin");
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

/// A stand-in for the agent's web door: it records every line it is sent, answers `answer` (one
/// line per item), and then holds the connection open if `hold`, as a watch does.
struct FakeAgent {
    socket: PathBuf,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn fake_agent(name: &str, answer: Vec<&'static str>, hold: bool) -> FakeAgent {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = std::env::temp_dir().join(format!(
        "bipnip-webhost-agent-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let socket = dir.join("web.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let Ok(Some(line)) = lines.next_line().await else {
                    return;
                };
                record.lock().expect("lock").push(line);
                for out in answer {
                    let _ = write.write_all(format!("{out}\n").as_bytes()).await;
                }
                if hold {
                    // Until the host closes its side.
                    while let Ok(Some(_)) = lines.next_line().await {}
                }
            });
        }
    });
    FakeAgent { socket, seen }
}

impl FakeAgent {
    fn link(&self) -> AgentLink {
        AgentLink::with_heartbeat(self.socket.clone(), std::time::Duration::from_millis(200))
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

const SIGNED: &str = r#"{"kind":24735,"content":"{\"v\":1,\"method\":\"status\"}","sig":"00"}"#;

#[tokio::test]
async fn a_page_s_request_reaches_the_agent_as_it_was_sent_and_the_answer_comes_back() {
    let agent = fake_agent(
        "verbatim",
        vec![
            r#"{"status":"started","run":"x"}"#,
            r#"{"ok":true,"result":{}}"#,
        ],
        false,
    )
    .await;
    let addr = host_with(Some(agent.link())).await;
    let origin = format!("http://{addr}");
    let (status, h, body) = call(
        addr,
        "POST",
        "/agent/call",
        &[("origin", &origin), ("content-type", "application/json")],
        SIGNED,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hv(&h, "content-type"), "application/x-ndjson");
    assert_eq!(hv(&h, "cache-control"), "no-store");
    assert_eq!(
        body,
        "{\"status\":\"started\",\"run\":\"x\"}\n{\"ok\":true,\"result\":{}}\n"
    );
    // Byte for byte: the host reads nothing into it and adds nothing to it.
    assert_eq!(agent.seen(), vec![SIGNED.to_string()]);
    // A browser on a plain-HTTP address sends no Sec-Fetch-Site; one that does says same-origin.
    let (status, _, _) = call(
        addr,
        "POST",
        "/agent/call",
        &[
            ("origin", &origin),
            ("content-type", "application/json; charset=utf-8"),
            ("sec-fetch-site", "same-origin"),
        ],
        SIGNED,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn what_is_not_a_page_of_this_origin_asking_never_reaches_the_agent() {
    let agent = fake_agent("refused", vec![r#"{"ok":true,"result":{}}"#], false).await;
    let addr = host_with(Some(agent.link())).await;
    let origin = format!("http://{addr}");
    let json = ("content-type", "application/json");
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_AGENT_BODY));
    // Method, headers, body, and the answer the host must give without touching the agent.
    type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, String, StatusCode);
    let cases: Vec<Case<'_>> = vec![
        (
            "GET",
            vec![("origin", origin.as_str())],
            String::new(),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        ("POST", vec![json], SIGNED.into(), StatusCode::FORBIDDEN),
        (
            "POST",
            vec![("origin", "http://evil.example"), json],
            SIGNED.into(),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            vec![("origin", "null"), json],
            SIGNED.into(),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            vec![
                ("origin", origin.as_str()),
                json,
                ("sec-fetch-site", "cross-site"),
            ],
            SIGNED.into(),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            vec![
                ("origin", origin.as_str()),
                json,
                ("sec-fetch-site", "none"),
            ],
            SIGNED.into(),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            vec![("origin", origin.as_str()), ("content-type", "text/plain")],
            SIGNED.into(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "POST",
            vec![("origin", origin.as_str()), json],
            big,
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "POST",
            vec![("origin", origin.as_str()), json],
            format!("{SIGNED}\n{SIGNED}"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            vec![("origin", origin.as_str()), json],
            "  ".into(),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (method, headers, body, want) in cases {
        let (status, _, text) = call(addr, method, "/agent/call", &headers, &body).await;
        assert_eq!(status, want, "{method} {headers:?}: {text}");
    }
    // Nothing else under /agent/ is a route.
    let (status, _, _) = call(
        addr,
        "POST",
        "/agent/approve",
        &[("origin", origin.as_str()), json],
        SIGNED,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        agent.seen(),
        Vec::<String>::new(),
        "the agent was never reached"
    );
}

#[tokio::test]
async fn without_tor_a_coordinator_route_refuses_in_json_the_page_can_read() {
    let addr = host_built(None, false).await;
    let (status, headers, body) = call(addr, "GET", "/mainnet/temple/api/info/", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(hv(&headers, "content-type"), "application/json");
    assert_eq!(hv(&headers, "cache-control"), "no-store");
    let answer: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert!(
        answer["bad_request"]
            .as_str()
            .is_some_and(|m| m.contains("--no-tor")),
        "{answer}"
    );
}

#[tokio::test]
async fn without_an_agent_or_with_one_that_is_gone_the_page_is_told() {
    let addr = host_with(None).await;
    let origin = format!("http://{addr}");
    let headers = [
        ("origin", origin.as_str()),
        ("content-type", "application/json"),
    ];
    let (status, _, _) = call(addr, "POST", "/agent/call", &headers, SIGNED).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let gone = AgentLink::new(std::env::temp_dir().join("bipnip-webhost-no-such-agent.sock"));
    let addr = host_with(Some(gone)).await;
    let origin = format!("http://{addr}");
    let headers = [
        ("origin", origin.as_str()),
        ("content-type", "application/json"),
    ];
    let (status, _, _) = call(addr, "POST", "/agent/call", &headers, SIGNED).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn open_watches_are_limited_and_one_the_page_drops_frees_its_slot() {
    let agent = fake_agent("watches", vec![r#"{"status":"watching"}"#], true).await;
    let addr = host_with(Some(agent.link())).await;
    let origin = format!("http://{addr}");
    // Each watch on its own connection, held open without reading the body.
    let open = |addr: SocketAddr, origin: String| async move {
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        let task = tokio::spawn(conn);
        let req = hyper::Request::builder()
            .method("POST")
            .uri("/agent/call")
            .header("host", addr.to_string())
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Full::new(axum::body::Bytes::from(SIGNED)))
            .expect("req");
        let resp = sender.send_request(req).await.expect("send");
        (resp, sender, task)
    };
    let mut held = Vec::new();
    for _ in 0..MAX_AGENT_STREAMS {
        let (resp, sender, task) = open(addr, origin.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        held.push((resp, sender, task));
    }
    let headers = [
        ("origin", origin.as_str()),
        ("content-type", "application/json"),
    ];
    let (status, _, _) = call(addr, "POST", "/agent/call", &headers, SIGNED).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "one more than may be open"
    );
    // The page goes away: its connection closes, and the next heartbeat finds it gone.
    let (resp, sender, task) = held.pop().expect("one");
    drop(resp);
    drop(sender);
    task.abort();
    let mut freed = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (resp, sender, task) = open(addr, origin.clone()).await;
        if resp.status() == StatusCode::OK {
            freed = true;
            held.push((resp, sender, task));
            break;
        }
    }
    assert!(freed, "the dropped watch's slot came back");
}
