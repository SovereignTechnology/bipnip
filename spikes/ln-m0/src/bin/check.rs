//! Checks what `run.sh` collected, independently of the nodes that produced it:
//! - `vectors.jsonl`: each node's `signmessage` signature recovers, in `bipnip-core`, to the node key
//!   the node itself reported;
//! - `statements.jsonl`: each node-signed binding statement parses and verifies;
//! - `nostr_events.jsonl`: each event signed through LND's signer verifies as a Nostr event;
//! - `results.jsonl`: outcomes the script recorded (hold invoices), shown as they are.
//!
//! Usage: check <out dir>. Exits 1 if anything failed or nothing was checked.

use std::fs;

use mostro_core::node_binding::{recover_ln_signer, SignedStatement, Statement};
use nostr::prelude::{Event, JsonUtil};
use serde_json::Value;

fn lines(dir: &str, name: &str) -> Vec<Value> {
    fs::read_to_string(format!("{dir}/{name}"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: check <out dir>");
    let mut checked = 0;
    let mut failed = 0;
    let mut report = |name: String, ok: bool, detail: String| {
        checked += 1;
        if !ok {
            failed += 1;
        }
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    };

    for v in lines(&dir, "vectors.jsonl") {
        let r = recover_ln_signer(s(&v, "msg").as_bytes(), s(&v, "sig"));
        let ok = matches!(&r, Ok(pk) if pk.to_string() == s(&v, "pubkey"));
        report(
            format!("signmessage {}", s(&v, "impl")),
            ok,
            format!("{:?}", r.map(|k| k.to_string())),
        );
    }

    for v in lines(&dir, "statements.jsonl") {
        let result = Statement::parse(s(&v, "statement")).and_then(|statement| {
            SignedStatement { statement, node_signature: s(&v, "sig").to_string() }.verify()
        });
        report(format!("binding statement {}", s(&v, "impl")), result.is_ok(), format!("{result:?}"));
    }

    for v in lines(&dir, "nostr_events.jsonl") {
        let json = v["event"].to_string();
        let result = Event::from_json(&json).map_err(|e| e.to_string()).and_then(|e| {
            e.verify().map_err(|e| e.to_string())?;
            Ok(e.pubkey.to_hex())
        });
        let ok = matches!(&result, Ok(pk) if pk == s(&v, "derived_xonly"));
        report(format!("nostr event signed by {}", s(&v, "impl")), ok, format!("{result:?}"));
    }

    for v in lines(&dir, "results.jsonl") {
        report(
            s(&v, "check").to_string(),
            v["ok"].as_bool().unwrap_or(false),
            s(&v, "detail").to_string(),
        );
    }

    println!("checked {checked}, failed {failed}");
    if checked == 0 || failed > 0 {
        std::process::exit(1);
    }
}
