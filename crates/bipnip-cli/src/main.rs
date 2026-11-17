//! `bipnip`: the command line of a running agent (`bipnip-agent run`). It holds no key and
//! talks to no node or relay: it writes one request to the agent's unix socket and prints what
//! comes back (`docs/agent-api.md`).
//!
//! ```text
//! bipnip [--socket PATH] status
//! bipnip [--socket PATH] orders [--kind lease|ring] [--instance KEY]
//! bipnip [--socket PATH] lease offer --min-capacity SAT --max-capacity SAT --duration BLOCKS
//!         --fee-base SAT --fee-ppm PPM --feerate SAT_PER_VB [--upfront PERCENT] [--confs N]
//!         [--private] [--expires-in SECS] [--bond SAT] [--instance KEY]
//! bipnip [--socket PATH] lease take ORDER_ID --capacity SAT [--node-address HOST:PORT]
//!         [--instance KEY]
//! bipnip [--socket PATH] lease cancel RUN
//! bipnip [--socket PATH] lease list
//! bipnip [--socket PATH] ring create --capacity SAT --slots N --feerate SAT_PER_VB
//!         [--window SECS] [--expires-in SECS] [--bond SAT] [--private]
//!         [--node-address HOST:PORT] [--instance KEY]
//! bipnip [--socket PATH] ring join ORDER_ID --feerate SAT_PER_VB [--node-address HOST:PORT]
//!         [--instance KEY]
//! bipnip [--socket PATH] ring cancel RUN
//! bipnip [--socket PATH] ring list
//! bipnip [--socket PATH] bind
//! bipnip [--socket PATH] approvals
//! bipnip [--socket PATH] approve APPROVAL
//! bipnip [--socket PATH] deny APPROVAL
//! bipnip [--socket PATH] watch [RUN]
//! ```
//!
//! The socket is `--socket`, else `$BIPNIP_SOCKET`, else `~/.bipnip/agent.sock`.
//!
//! The result goes to standard output: the lines of the runs for a list, JSON otherwise. What
//! happens on the way, and errors, go to standard error. `watch` prints one line per change.
//!
//! Exit status: 0 when the agent did it, 1 when it refused or failed, 2 when the command line
//! is wrong or the agent cannot be reached, 3 when the request is parked and waits for
//! `bipnip approve`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

const USAGE: &str = "usage: bipnip [--socket PATH] <status | orders | lease <offer|take|cancel|list> | \
ring <create|join|cancel|list> | bind | approvals | approve APPROVAL | deny APPROVAL | \
watch [RUN]> [options] (the options are listed in the README and at the top of the source)";

/// Exit status when the request was parked.
const EXIT_PARKED: u8 = 3;

/// `--name value` options, the switches named in `switches`, and the other words in order.
struct Options {
    values: HashMap<String, String>,
    switches: Vec<String>,
    words: Vec<String>,
}

impl Options {
    fn parse(args: &[String], switches: &[&str]) -> Result<Self, String> {
        let mut options = Options {
            values: HashMap::new(),
            switches: Vec::new(),
            words: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.strip_prefix("--") {
                None => options.words.push(arg.clone()),
                Some(name) if switches.contains(&name) => options.switches.push(name.to_string()),
                Some(name) => {
                    let value = args.next().ok_or(format!("--{name} needs a value"))?;
                    if options
                        .values
                        .insert(name.to_string(), value.clone())
                        .is_some()
                    {
                        return Err(format!("--{name} is given twice"));
                    }
                }
            }
        }
        Ok(options)
    }

    /// Moves `--option` into `params` under `key` as a whole number, if it was given.
    fn number(
        &mut self,
        params: &mut Map<String, Value>,
        option: &str,
        key: &str,
        required: bool,
    ) -> Result<(), String> {
        match self.values.remove(option) {
            Some(text) => {
                let number: u64 = text
                    .parse()
                    .map_err(|_| format!("--{option}: {text:?} is not a whole number"))?;
                params.insert(key.to_string(), json!(number));
                Ok(())
            }
            None if required => Err(format!("--{option} is required")),
            None => Ok(()),
        }
    }

    /// Moves `--option` into `params` under `key` as text, if it was given.
    fn text(&mut self, params: &mut Map<String, Value>, option: &str, key: &str) {
        if let Some(text) = self.values.remove(option) {
            params.insert(key.to_string(), Value::String(text));
        }
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    /// The words left, once every option was taken. An option nobody took is an error, so a
    /// typo is not a silent default.
    fn done(self) -> Result<Vec<String>, String> {
        match self.values.keys().next() {
            Some(name) => Err(format!("unknown option --{name}")),
            None => Ok(self.words),
        }
    }
}

fn one_word(options: Options, what: &str) -> Result<String, String> {
    match options.done()?.as_slice() {
        [word] => Ok(word.clone()),
        _ => Err(format!("give exactly one {what}")),
    }
}

fn no_words(options: Options) -> Result<(), String> {
    if options.done()?.is_empty() {
        Ok(())
    } else {
        Err(USAGE.to_string())
    }
}

/// The request a command line means: its method and parameters.
fn request(args: &[String]) -> Result<(String, Value), String> {
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    let mut params = Map::new();
    let method = match (command.as_str(), rest) {
        ("status", []) => "status",
        ("bind", []) => "binding.publish",
        ("approvals", []) => "approvals",
        ("approve", [approval]) => {
            params.insert("approval".into(), json!(approval));
            "approve"
        }
        ("deny", [approval]) => {
            params.insert("approval".into(), json!(approval));
            "deny"
        }
        ("watch", []) => "watch",
        ("watch", [run]) => {
            params.insert("run".into(), json!(run));
            "watch"
        }
        ("orders", rest) => {
            let mut options = Options::parse(rest, &[])?;
            options.text(&mut params, "kind", "kind");
            options.text(&mut params, "instance", "instance");
            no_words(options)?;
            "orders"
        }
        ("lease", [sub, rest @ ..]) => match sub.as_str() {
            "list" if rest.is_empty() => "lease.list",
            "cancel" => {
                let run = one_word(Options::parse(rest, &[])?, "run")?;
                params.insert("run".into(), json!(run));
                "lease.cancel"
            }
            "offer" => {
                let mut options = Options::parse(rest, &["private"])?;
                options.number(&mut params, "min-capacity", "min_capacity_sat", true)?;
                options.number(&mut params, "max-capacity", "max_capacity_sat", true)?;
                options.number(&mut params, "duration", "duration_blocks", true)?;
                options.number(&mut params, "fee-base", "fee_base_sat", true)?;
                options.number(&mut params, "fee-ppm", "fee_ppm", true)?;
                options.number(&mut params, "feerate", "feerate_sat_vb", true)?;
                options.number(&mut params, "upfront", "upfront_percent", false)?;
                options.number(&mut params, "confs", "min_confirmations", false)?;
                options.number(&mut params, "expires-in", "expires_in_secs", false)?;
                options.number(&mut params, "bond", "bond_sat", false)?;
                options.text(&mut params, "instance", "instance");
                if options.switch("private") {
                    params.insert("announce".into(), json!(false));
                }
                no_words(options)?;
                "lease.offer"
            }
            "take" => {
                let mut options = Options::parse(rest, &[])?;
                options.number(&mut params, "capacity", "capacity_sat", true)?;
                options.text(&mut params, "node-address", "node_address");
                options.text(&mut params, "instance", "instance");
                let order = one_word(options, "order id")?;
                params.insert("order_id".into(), json!(order));
                "lease.take"
            }
            _ => return Err(USAGE.to_string()),
        },
        ("ring", [sub, rest @ ..]) => match sub.as_str() {
            "list" if rest.is_empty() => "ring.list",
            "cancel" => {
                let run = one_word(Options::parse(rest, &[])?, "run")?;
                params.insert("run".into(), json!(run));
                "ring.cancel"
            }
            "create" => {
                let mut options = Options::parse(rest, &["private"])?;
                options.number(&mut params, "capacity", "capacity_sat", true)?;
                options.number(&mut params, "slots", "slots", true)?;
                options.number(&mut params, "feerate", "feerate_sat_vb", true)?;
                options.number(&mut params, "window", "open_window_secs", false)?;
                options.number(&mut params, "expires-in", "expires_in_secs", false)?;
                options.number(&mut params, "bond", "bond_sat", false)?;
                options.text(&mut params, "node-address", "node_address");
                options.text(&mut params, "instance", "instance");
                if options.switch("private") {
                    params.insert("announce".into(), json!(false));
                }
                no_words(options)?;
                "ring.create"
            }
            "join" => {
                let mut options = Options::parse(rest, &[])?;
                options.number(&mut params, "feerate", "feerate_sat_vb", true)?;
                options.text(&mut params, "node-address", "node_address");
                options.text(&mut params, "instance", "instance");
                let order = one_word(options, "order id")?;
                params.insert("order_id".into(), json!(order));
                "ring.join"
            }
            _ => return Err(USAGE.to_string()),
        },
        _ => return Err(USAGE.to_string()),
    };
    Ok((method.to_string(), Value::Object(params)))
}

fn socket_path(given: Option<String>) -> Result<PathBuf, String> {
    if let Some(path) = given {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("BIPNIP_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or("no --socket, no $BIPNIP_SOCKET and no $HOME")?;
    Ok(PathBuf::from(home).join(".bipnip").join("agent.sock"))
}

/// Prints a result the way a person reads it: the lines of runs for a list, the JSON otherwise.
fn show(method: &str, result: &Value) {
    if matches!(method, "lease.list" | "ring.list")
        && let Some(runs) = result["runs"].as_array()
    {
        for run in runs {
            let line = run["line"].as_str().unwrap_or_default();
            match run["halted"].as_str() {
                Some(why) => println!("{line} halted: {why}"),
                None => println!("{line}"),
            }
        }
        for refused in result["refused"].as_array().into_iter().flatten() {
            println!(
                "refused {}: {}",
                refused["file"].as_str().unwrap_or_default(),
                refused["why"].as_str().unwrap_or_default()
            );
        }
        return;
    }
    match serde_json::to_string_pretty(result) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{result}"),
    }
}

/// Sends the request and prints the answer. The exit status says how it went.
fn talk(socket: &Path, method: &str, params: Value) -> Result<u8, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| {
        format!(
            "cannot reach the agent at {}: {e} (is `bipnip-agent run` running?)",
            socket.display()
        )
    })?;
    let line = json!({"v": 1, "method": method, "params": params}).to_string();
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .map_err(|e| format!("cannot send the request: {e}"))?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| format!("the agent's answer broke off: {e}"))?;
        let Ok(answer) = serde_json::from_str::<Value>(&line) else {
            return Err("the agent answered with something that is not JSON".into());
        };
        match answer["ok"].as_bool() {
            // A status line: something that happened on the way. For `watch` these are the
            // answer; for anything else they are remarks, kept apart from the result.
            None => {
                let text = answer["line"].as_str().map(str::to_string).unwrap_or(line);
                if method == "watch" {
                    println!("{text}");
                } else {
                    eprintln!("{text}");
                }
            }
            Some(true) => {
                show(method, &answer["result"]);
                return Ok(0);
            }
            Some(false) => {
                let error = &answer["error"];
                let code = error["code"].as_str().unwrap_or("error");
                eprintln!(
                    "bipnip: {code}: {}",
                    error["message"].as_str().unwrap_or_default()
                );
                return Ok(if code == "needs-approval" {
                    EXIT_PARKED
                } else {
                    1
                });
            }
        }
    }
    // `watch` ends when the agent does; anything else owes a final line.
    if method == "watch" {
        Ok(0)
    } else {
        Err("the agent closed the connection without an answer".into())
    }
}

fn run() -> Result<u8, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = if args.first().map(String::as_str) == Some("--socket") {
        if args.len() < 2 {
            return Err(USAGE.to_string());
        }
        let path = args.remove(1);
        args.remove(0);
        Some(path)
    } else {
        None
    };
    let (method, params) = request(&args)?;
    talk(&socket_path(socket)?, &method, params)
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(e) => {
            eprintln!("bipnip: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn commands_become_the_requests_the_agent_documents() {
        for (line, method, params) in [
            ("status", "status", json!({})),
            ("bind", "binding.publish", json!({})),
            ("approvals", "approvals", json!({})),
            ("approve ab12", "approve", json!({"approval": "ab12"})),
            ("deny ab12", "deny", json!({"approval": "ab12"})),
            ("watch", "watch", json!({})),
            ("watch ring-0f", "watch", json!({"run": "ring-0f"})),
            ("orders", "orders", json!({})),
            ("orders --kind ring", "orders", json!({"kind": "ring"})),
            ("lease list", "lease.list", json!({})),
            ("ring list", "ring.list", json!({})),
            (
                "lease cancel seller-0f",
                "lease.cancel",
                json!({"run": "seller-0f"}),
            ),
            (
                "lease offer --min-capacity 500000 --max-capacity 1000000 --duration 4320 \
                 --fee-base 100 --fee-ppm 2500 --feerate 2 --bond 20000 --private",
                "lease.offer",
                json!({"min_capacity_sat": 500_000, "max_capacity_sat": 1_000_000,
                       "duration_blocks": 4_320, "fee_base_sat": 100, "fee_ppm": 2_500,
                       "feerate_sat_vb": 2, "bond_sat": 20_000, "announce": false}),
            ),
            (
                "lease take 5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f --capacity 600000 \
                 --node-address 127.0.0.1:9735",
                "lease.take",
                json!({"order_id": "5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f",
                       "capacity_sat": 600_000, "node_address": "127.0.0.1:9735"}),
            ),
            (
                "ring create --capacity 300000 --slots 3 --feerate 2 --window 3600",
                "ring.create",
                json!({"capacity_sat": 300_000, "slots": 3, "feerate_sat_vb": 2,
                       "open_window_secs": 3_600}),
            ),
            (
                "ring join 5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f --feerate 2",
                "ring.join",
                json!({"order_id": "5d1c3a0e-8c2b-4a43-9a53-2f0e1c2d3e4f", "feerate_sat_vb": 2}),
            ),
        ] {
            assert_eq!(
                request(&words(line)),
                Ok((method.to_string(), params)),
                "{line}"
            );
        }
    }

    #[test]
    fn a_wrong_command_line_never_becomes_a_request() {
        for line in [
            "",
            "wallet",
            "status now",
            "approve",
            "approve a b",
            "lease",
            "lease list all",
            "lease take --capacity 600000",
            "lease take a b --capacity 600000",
            "lease take 5d1c --capacity lots",
            "lease take 5d1c",
            "lease offer --min-capacity 1",
            "lease take 5d1c --capacity 1 --capacity 2",
            "lease take 5d1c --capacity 1 --mnemonic x",
            "ring create --capacity 300000 --slots 3",
            "ring join --feerate 2",
            "orders extra",
        ] {
            assert!(request(&words(line)).is_err(), "{line:?}");
        }
    }

    #[test]
    fn the_socket_is_the_one_named_first() {
        assert_eq!(
            socket_path(Some("/run/x.sock".into())),
            Ok(PathBuf::from("/run/x.sock"))
        );
    }
}
