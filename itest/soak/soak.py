#!/usr/bin/env python3
"""The soak's driver and auditor. run-soak.sh sets the scene (chain, nodes, channels, settings) and
hands over; this runs the long part. Standard library only.

Threads:
- the miner: one block at a time, at random (exponential) intervals averaging SOAK_BLOCK_SECS;
- the workload: leases and rings between the agents, through their daemons' sockets;
- fiat: trades through upstream mostro-cli (itest/ln/fiat.py), one at a time;
- faults: at random intervals averaging SOAK_FAULT_SECS, an agent or the instance killed, the
  instance's LND node restarted, a relay taken away, or the chain reorganised;
- the auditor: every SOAK_AUDIT_SECS, and once at the end.

Money findings (a run stuck past its deadline, an HTLC held too long, a run that spent or opened
more than its grant) fail the soak. Other findings (a preimage stored unsealed, a secret in a log, a
process that died) are reported, and fail it too. The journal is $W/soak/journal.jsonl; the report
$W/soak/report.json.
"""

import json
import os
import random
import shlex
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
from pathlib import Path

W = os.environ["W"]
NET = os.environ["NET"]
BCLI = shlex.split(os.environ["BCLI"])
MADDR = os.environ["MADDR"]
HOURS = int(os.environ["SOAK_HOURS"])
BLOCK = int(os.environ["SOAK_BLOCK_SECS"])
AUDIT = int(os.environ["SOAK_AUDIT_SECS"])
FAULT = int(os.environ["SOAK_FAULT_SECS"])
SEED = int(os.environ["SOAK_SEED"])
NODES = dict(kv.split("=", 1) for kv in os.environ["SOAK_NODES"].split(","))
ADDRS = dict(kv.split("=", 1) for kv in os.environ["SOAK_ADDRS"].split(","))
LNC = {"alice": shlex.split(os.environ["LNC_SELLER"]), "bob": shlex.split(os.environ["LNC_BUYER"]),
       "carol": shlex.split(os.environ["LNC_CAROL"])}
DB = os.environ["INSTANCE_DB"]
AGENTS = ["alice", "bob", "chris", "erin"]
SELLERS = ["alice", "bob", "chris"]  # an LDK Server sells no lease

rng = random.Random(SEED)
stop = threading.Event()
chain_lock = threading.Lock()
journal_lock = threading.Lock()
JOURNAL = open(f"{W}/soak/journal.jsonl", "a", buffering=1)
findings = []
stats = {"blocks": 0, "leases": 0, "rings": 0, "fiat": {}, "faults": {}, "finished": {}}
quiet_until = {}  # process name -> time it may be down until (a fault in progress)


def note(kind, **fields):
    entry = {"t": round(time.time()), "kind": kind, **fields}
    with journal_lock:
        JOURNAL.write(json.dumps(entry) + "\n")
    short = " ".join(f"{k}={v}" for k, v in fields.items() if not isinstance(v, (dict, list)))
    print(f"[soak {time.strftime('%H:%M:%S', time.gmtime())}] {kind} {short}"[:300], flush=True)


def finding(kind, money, **fields):
    findings.append({"kind": kind, "money": money, **fields})
    note("FINDING", what=kind, money=money, **fields)


def run(argv, timeout=60):
    done = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    return done.returncode, done.stdout, done.stderr


# --- processes the soak starts, stops and restarts -------------------------------------------
class Proc:
    def __init__(self, name, spec):
        self.name, self.spec, self.p = name, spec, None
        self.log = Path(f"{W}/soak/{name}.log")

    def start(self):
        env = dict(os.environ, **self.spec.get("env", {}))
        out = open(self.log, "a")
        self.p = subprocess.Popen(self.spec["argv"], stdout=out, stderr=subprocess.STDOUT,
                                  stdin=subprocess.DEVNULL, env=env, start_new_session=True)

    def alive(self):
        return self.p is not None and self.p.poll() is None

    def kill(self, sig=signal.SIGKILL):
        if self.alive():
            self.p.send_signal(sig)
            try:
                self.p.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()

    def ready(self, timeout=180):
        want = self.spec["ready"]
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not self.alive():
                return False
            if "port" in want:
                try:
                    socket.create_connection(("127.0.0.1", want["port"]), timeout=2).close()
                    return True
                except OSError:
                    pass
            elif "log" in want:
                if want["log"] in self.log.read_text(errors="replace"):
                    return True
            elif "socket" in want:
                if call_path(want["socket"], "status", timeout=10).get("ok"):
                    return True
            time.sleep(2)
        return False


def call_path(path, method, params=None, timeout=60):
    """One request to an agent's socket; the final line, or a stand-in error."""
    request = {"v": 1, "method": method}
    if params is not None:
        request["params"] = params
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(path)
            s.sendall((json.dumps(request) + "\n").encode())
            buf = b""
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
        lines = [json.loads(line) for line in buf.decode().splitlines() if line.strip()]
        finals = [line for line in lines if "ok" in line]
        return finals[-1] if finals else {"ok": False, "error": {"code": "no-answer", "message": ""}}
    except (OSError, ValueError) as e:
        return {"ok": False, "error": {"code": "socket", "message": str(e)}}


def call(agent, method, params=None, timeout=60):
    return call_path(f"{W}/{agent}-agent/agent.sock", method, params, timeout)


# --- the chain ------------------------------------------------------------------------------------
def bcli(*args):
    code, out, err = run([*BCLI, *args])
    if code != 0:
        raise RuntimeError(f"bitcoin-cli {args[0]}: {err.strip()[:200]}")
    return out.strip()


def mine(n=1):
    bcli("generatetoaddress", str(n), MADDR)
    stats["blocks"] += n


def miner():
    while not stop.wait(rng.expovariate(1 / BLOCK)):
        with chain_lock:
            try:
                mine(1)
            except RuntimeError as e:
                note("miner-error", error=str(e))


def reorg(depth):
    with chain_lock:
        height = int(bcli("getblockcount"))
        first = bcli("getblockhash", str(height - depth + 1))
        bcli("invalidateblock", first)
        mine(depth + 1)
    note("fault", what="reorg", depth=depth, from_height=height)


# --- the workload ---------------------------------------------------------------------------------
runs = []  # {agent, run, kind, started, deadline}
runs_lock = threading.Lock()


def track(agent, result, kind, deadline):
    run_id = (result.get("result") or {}).get("run")
    if run_id:
        with runs_lock:
            runs.append({"agent": agent, "run": run_id, "kind": kind,
                         "started": time.time(), "deadline": time.time() + deadline})
    return run_id


def ask(agent, method, params):
    """A request that may be parked: approved when the policy allows it otherwise, as an operator
    who set those limits would."""
    answer = call(agent, method, params)
    error = answer.get("error") or {}
    if error.get("code") == "needs-approval":
        note("parked", agent=agent, method=method, why=error.get("message", "")[:200])
        if "max_runs" in error.get("message", ""):
            call(agent, "deny", {"approval": error["approval"]})
            return answer
        answer = call(agent, "approve", {"approval": error["approval"]})
    return answer


def order_of(agent, run_id, list_method, timeout=300):
    deadline = time.time() + timeout
    while time.time() < deadline and not stop.is_set():
        listed = call(agent, list_method)
        for r in (listed.get("result") or {}).get("runs", []):
            if r.get("run") == run_id and r.get("order_id"):
                return r["order_id"]
        time.sleep(5)
    return None


def lease():
    seller = rng.choice(SELLERS)
    buyer = rng.choice([a for a in AGENTS if a != seller])
    blocks = rng.choice([6, 12, 24, 36])
    offer = {"min_capacity_sat": 200_000, "max_capacity_sat": 600_000, "duration_blocks": blocks,
             "fee_base_sat": 1_000, "fee_ppm": 2_500, "feerate_sat_vb": 2,
             "upfront_percent": rng.choice([50, 100])}
    if rng.random() < 0.3:
        offer["bond_sat"] = 20_000  # the buyer's bond, held by the instance's node
    setup = 3600 + blocks * BLOCK * 3
    answer = ask(seller, "lease.offer", offer)
    run_id = track(seller, answer, "lease", setup + 3600)
    if not run_id:
        note("lease-not-started", seller=seller, error=answer.get("error"))
        return
    order = order_of(seller, run_id, "lease.list")
    if not order:
        note("lease-no-order", seller=seller, run=run_id)
        return
    take = {"order_id": order, "capacity_sat": rng.randrange(200_000, 600_001, 1_000),
            "node_address": ADDRS[buyer]}
    answer = ask(buyer, "lease.take", take)
    if track(buyer, answer, "lease", setup + 3600):
        stats["leases"] += 1
        note("lease", seller=seller, buyer=buyer, order=order, blocks=blocks, capacity=take["capacity_sat"])
    else:
        note("lease-not-taken", buyer=buyer, order=order, error=answer.get("error"))


def ring():
    members = rng.sample(AGENTS, rng.choice([2, 3, 4]))
    creator = members[0]
    create = {"capacity_sat": 250_000, "slots": len(members), "feerate_sat_vb": 2,
              "open_window_secs": 1800, "node_address": ADDRS[creator]}
    if rng.random() < 0.3:
        create["bond_sat"] = 20_000  # every member's bond, held by the instance's node
    answer = ask(creator, "ring.create", create)
    run_id = track(creator, answer, "ring", 1800 + 3600)
    if not run_id:
        note("ring-not-started", creator=creator, error=answer.get("error"))
        return
    order = order_of(creator, run_id, "ring.list")
    if not order:
        note("ring-no-order", creator=creator, run=run_id)
        return
    for member in members[1:]:
        answer = ask(member, "ring.join", {"order_id": order, "feerate_sat_vb": 2, "node_address": ADDRS[member]})
        track(member, answer, "ring", 1800 + 3600)
    stats["rings"] += 1
    note("ring", members=",".join(members), order=order)


def under_way():
    with runs_lock:
        return sum(1 for r in runs if not r.get("done"))


def workload():
    while not stop.wait(rng.uniform(60, 180)):
        if under_way() >= 8:
            continue
        try:
            (lease if rng.random() < 0.6 else ring)()
        except Exception as e:  # a bug in the driver is reported, not fatal
            note("driver-error", error=repr(e)[:300])


def fiat():
    code, out, err = run([sys.executable, os.environ["FIAT_PY"], "setup"], timeout=300)
    if code != 0:
        finding("fiat-setup", False, output=(out + err)[-500:])
        return
    while not stop.wait(rng.uniform(120, 420)):
        trade = rng.choice(["sell", "buy", "cancel", "dispute"])
        code, out, err = run([sys.executable, os.environ["FIAT_PY"], trade], timeout=1200)
        result = "ok" if code == 0 else "failed"
        stats["fiat"][f"{trade}-{result}"] = stats["fiat"].get(f"{trade}-{result}", 0) + 1
        note("fiat", trade=trade, result=result, tail=(out + err)[-300:] if code else "")


# --- faults ---------------------------------------------------------------------------------------
def restart(proc, down_secs):
    quiet_until[proc.name] = time.time() + down_secs + 300
    proc.kill()
    time.sleep(down_secs)
    proc.start()
    if not proc.ready():
        finding("did-not-come-back", False, process=proc.name)


def restart_carol():
    quiet_until["carol"] = time.time() + 900
    run([*LNC["carol"], "stop"])
    time.sleep(rng.uniform(10, 60))
    argv = Path(f"{W}/soak/carol.argv").read_text().split("\n")
    subprocess.Popen([a for a in argv if a], stdout=open(f"{W}/carol/lnd.log", "a"), stderr=subprocess.STDOUT,
                     stdin=subprocess.DEVNULL, start_new_session=True)
    deadline = time.time() + 300
    while time.time() < deadline:
        code, out, _ = run([*LNC["carol"], "getinfo"])
        if code == 0 and json.loads(out).get("synced_to_chain"):
            return
        time.sleep(5)
    finding("did-not-come-back", True, process="carol (the instance's LND)")


def faults(procs):
    while not stop.wait(rng.expovariate(1 / FAULT)):
        what = rng.choices(["agent", "instance", "relay", "carol", "reorg"], [35, 20, 15, 15, 15])[0]
        stats["faults"][what] = stats["faults"].get(what, 0) + 1
        try:
            if what == "agent":
                name = rng.choice(AGENTS)
                note("fault", what="agent killed", agent=name)
                restart(procs[name], rng.uniform(5, 30))
            elif what == "instance":
                note("fault", what="instance killed")
                restart(procs["instance"], rng.uniform(5, 60))
            elif what == "relay":
                secs = rng.uniform(60, 300)
                note("fault", what="relay2 down", secs=round(secs))
                restart(procs["relay2"], secs)
            elif what == "carol":
                note("fault", what="carol (the instance's LND) restarted")
                restart_carol()
            else:
                reorg(rng.choice([1, 2, 3]))
        except Exception as e:
            note("fault-error", what=what, error=repr(e)[:300])


# --- the auditor ----------------------------------------------------------------------------------
htlc_seen = {}


def lnd_htlcs(node):
    code, out, _ = run([*LNC[node], "listchannels"])
    if code != 0:
        return []
    return [(node, h["hash_lock"]) for c in json.loads(out).get("channels", []) for h in c.get("pending_htlcs", [])]


def cln_htlcs():
    code, out, _ = run(["lightning-cli", f"--network={NET}", f"--lightning-dir={os.environ['SOAK_CLN_DIR']}",
                        "listpeerchannels"])
    if code != 0:
        return []
    return [("chris", h["payment_hash"]) for c in json.loads(out).get("channels", []) for h in c.get("htlcs", [])]


def audit(procs, final=False):
    now = time.time()
    # Processes: one that died outside a fault is a finding, and is started again.
    for name, proc in procs.items():
        if not proc.alive() and now > quiet_until.get(name, 0):
            finding("process-died", False, process=name, log_tail=proc.log.read_text(errors="replace")[-400:])
            proc.start()
    # Runs: finished, or within their deadline.
    lists = {}
    for agent in AGENTS:
        for method in ("lease.list", "ring.list"):
            for r in (call(agent, method).get("result") or {}).get("runs", []):
                lists[(agent, r.get("run"))] = r
    with runs_lock:
        for r in runs:
            if r.get("done"):
                continue
            seen = lists.get((r["agent"], r["run"]))
            if seen and seen.get("finished"):
                r["done"] = True
                key = f"{r['kind']}-{seen.get('state')}"
                stats["finished"][key] = stats["finished"].get(key, 0) + 1
            elif now > r["deadline"]:
                r["done"] = True
                finding("run-stuck", True, agent=r["agent"], run=r["run"], kind=r["kind"],
                        state=(seen or {}).get("state"), waiting_on=(seen or {}).get("waiting_on"))
    # HTLCs held longer than an hour or twenty blocks, whichever is longer.
    held = set(lnd_htlcs("alice") + lnd_htlcs("bob") + lnd_htlcs("carol") + cln_htlcs())
    for key in held:
        htlc_seen.setdefault(key, now)
    for key in list(htlc_seen):
        if key not in held:
            del htlc_seen[key]
        elif now - htlc_seen[key] > max(3600, 20 * BLOCK):
            finding("htlc-stuck", True, node=key[0], hash=key[1])
            htlc_seen[key] = now
    # Grants: no run paid or opened more than it was granted.
    for agent in AGENTS:
        for grant_file in Path(f"{W}/{agent}-agent/policy").glob("grant-*.json"):
            try:
                grant = json.loads(grant_file.read_text())
            except ValueError:
                continue
            ask_ = grant.get("ask", {})
            allowance = (ask_.get("fee_sat", 0) + ask_.get("bond_sat", 0)) * 1000
            paid = sum(p.get("msat", 0) for p in grant.get("paid", []))
            if paid > allowance or grant.get("opened_sat", 0) > ask_.get("capacity_sat", 0):
                finding("over-grant", True, agent=agent, run=grant.get("run"), paid_msat=paid,
                        allowance_msat=allowance, opened=grant.get("opened_sat"))
    # Preimages at rest: every one sealed.
    try:
        with sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=10) as db:
            for table in ("orders", "bonds", "liquidity_bonds"):
                n = db.execute(f"SELECT COUNT(*) FROM {table} WHERE preimage IS NOT NULL "
                               "AND preimage NOT LIKE 'sealed1:%'").fetchone()[0]
                if n:
                    finding("preimage-unsealed", False, table=table, rows=n)
    except sqlite3.Error as e:
        note("audit-db-error", error=str(e)[:200])
    # Secrets in logs: no nsec in any log.
    for log in list(Path(f"{W}/soak").glob("*.log")) + [Path(f"{W}/carol/lnd.log")]:
        if "nsec1" in log.read_text(errors="replace"):
            finding("secret-in-log", False, log=log.name)
    note("audit", final=final, runs=len(runs), under_way=under_way(), findings=len(findings))


def main():
    specs = json.load(open(f"{W}/soak/procs.json"))
    procs = {name: Proc(name, spec) for name, spec in specs.items()}
    for name in ["relay1", "relay2", "instance", *AGENTS]:
        procs[name].start()
        if not procs[name].ready():
            print(procs[name].log.read_text(errors="replace")[-2000:], flush=True)
            print(f"[soak] FAIL: {name} did not start", flush=True)
            sys.exit(1)
    note("start", hours=HOURS, block_secs=BLOCK, fault_secs=FAULT, seed=SEED)
    threads = [threading.Thread(target=t, args=a, daemon=True) for t, a in
               [(miner, ()), (workload, ()), (fiat, ()), (faults, (procs,))]]
    for t in threads:
        t.start()
    end = time.time() + HOURS * 3600
    while time.time() < end:
        time.sleep(min(AUDIT, max(1, end - time.time())))
        audit(procs)
    stop.set()
    # Let what is under way finish: mine on, without faults, for up to an hour.
    drain_end = time.time() + 3600
    while under_way() and time.time() < drain_end:
        with chain_lock:
            mine(1)
        time.sleep(30)
        audit(procs)
    audit(procs, final=True)
    money = [f for f in findings if f["money"]]
    report = {"seed": SEED, "hours": HOURS, "block_secs": BLOCK, "stats": stats,
              "findings": findings, "unfinished": under_way()}
    Path(f"{W}/soak/report.json").write_text(json.dumps(report, indent=1))
    print(json.dumps(report), flush=True)
    for proc in procs.values():
        proc.kill(signal.SIGTERM)
    if findings:
        print(f"[soak] SOAK FAIL: {len(findings)} findings, {len(money)} about money", flush=True)
        sys.exit(1)
    print(f"[soak] SOAK PASS: {HOURS} h, {stats['blocks']} blocks, {stats['leases']} leases, "
          f"{stats['rings']} rings, fiat {stats['fiat']}, faults {stats['faults']}, no findings", flush=True)


if __name__ == "__main__":
    main()
