//! `bipnip-webhost --dist <bipnip-web dist dir> [--listen ADDR:PORT] [--allow-host HOST:PORT]…
//!                 [--tor-dir DIR]`
//!
//! Serves `dist/basic.html`, `dist/pro.html` and `dist/static/`, and proxies
//! `/<mainnet|testnet>/<alias>/…` to the coordinator's onion (from `dist/static/federation.json`)
//! over Tor. Defaults: listen `127.0.0.1:12596`; allowed hosts `127.0.0.1:<port>` and
//! `localhost:<port>`. Binding a wildcard address requires naming the allowed hosts.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bipnip_webhost::app::{AppState, router};
use bipnip_webhost::connector::Tor;
use bipnip_webhost::federation::Coordinators;
use bipnip_webhost::security::Allowed;

struct Args {
    dist: PathBuf,
    listen: SocketAddr,
    allow_hosts: Vec<String>,
    tor_dir: PathBuf,
}

fn usage() -> ! {
    eprintln!(
        "usage: bipnip-webhost --dist DIR [--listen ADDR:PORT] [--allow-host HOST:PORT]... [--tor-dir DIR]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut dist = None;
    let mut listen: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 12596));
    let mut allow_hosts = Vec::new();
    let mut tor_dir = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--dist" => dist = Some(PathBuf::from(value())),
            "--listen" => listen = value().parse().unwrap_or_else(|_| usage()),
            "--allow-host" => allow_hosts.push(value()),
            "--tor-dir" => tor_dir = Some(PathBuf::from(value())),
            _ => usage(),
        }
    }
    let tor_dir = tor_dir.unwrap_or_else(|| {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("bipnip-webhost/tor")
    });
    Args {
        dist: dist.unwrap_or_else(|| usage()),
        listen,
        allow_hosts,
        tor_dir,
    }
}

fn fail(msg: String) -> ! {
    eprintln!("bipnip-webhost: {msg}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let dist = std::fs::canonicalize(&args.dist)
        .unwrap_or_else(|e| fail(format!("dist {}: {e}", args.dist.display())));
    let read = |name: &str| {
        std::fs::read_to_string(dist.join(name)).unwrap_or_else(|e| {
            fail(format!(
                "{name}: {e} (build bipnip-web with `npm run build:bipnip`)"
            ))
        })
    };
    let basic = read("basic.html");
    let pro = read("pro.html");
    let coordinators = Coordinators::from_federation_json(&read("static/federation.json"))
        .unwrap_or_else(|e| fail(e));
    let static_dir = std::fs::canonicalize(dist.join("static"))
        .unwrap_or_else(|e| fail(format!("dist/static: {e}")));

    let port = args.listen.port();
    let allow_hosts = if !args.allow_hosts.is_empty() {
        args.allow_hosts
    } else if args.listen.ip().is_unspecified() {
        fail("listening on a wildcard address needs --allow-host for each name clients use".into())
    } else {
        vec![
            format!("{}:{port}", args.listen.ip()),
            format!("localhost:{port}"),
        ]
    };

    eprintln!(
        "bipnip-webhost: bootstrapping Tor in {}",
        args.tor_dir.display()
    );
    std::fs::create_dir_all(&args.tor_dir)
        .unwrap_or_else(|e| fail(format!("tor dir {}: {e}", args.tor_dir.display())));
    let tor = Tor::bootstrap(&args.tor_dir)
        .await
        .unwrap_or_else(|e| fail(e));

    let state = Arc::new(AppState {
        static_dir,
        basic,
        pro,
        coordinators,
        allowed: Allowed::new(allow_hosts.clone()),
        connector: Arc::new(tor),
    });
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .unwrap_or_else(|e| fail(format!("listen {}: {e}", args.listen)));
    eprintln!(
        "bipnip-webhost: serving {} for {} ({} coordinator routes over Tor)",
        dist.display(),
        allow_hosts
            .iter()
            .map(|h| format!("http://{h}"))
            .collect::<Vec<_>>()
            .join(", "),
        state.coordinators.len()
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
    {
        fail(format!("serve: {e}"));
    }
}
