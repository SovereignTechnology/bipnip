#!/usr/bin/env python3
"""Fiat trades through a real bipnipd, each side driven by upstream mostro-cli, the way a Mostro user
trades: the seller's sats are held by the instance's node in a hold invoice, the buyer says the fiat
was sent, the seller releases, and the instance pays the buyer.

Run by run-fiat.sh inside a throwaway regtest VM, never anywhere else. Standard library only.

    fiat.py setup                 local identities for seller, buyer and solver; the solver is added
    fiat.py sell|buy|cancel|dispute
    fiat.py gap                   a sell with the instance stopped across the seller's release

The environment names the pieces (set by run-fiat.sh): W, MCLI, INSTANCE_DB, INSTANCE_NSEC,
SOLVER_NSEC, SOLVER_PUB, LNC_SELLER and LNC_BUYER (lncli command lines); `gap` also needs
INSTANCE_PID.
"""

import json
import os
import shlex
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import uuid

W = os.environ["W"]
MCLI = os.environ["MCLI"]
DB = os.environ["INSTANCE_DB"]
LNC = {"seller": shlex.split(os.environ["LNC_SELLER"]), "buyer": shlex.split(os.environ["LNC_BUYER"])}
SATS = 20000
STEP_SECS = 300


def log(text):
    print(f"[fiat {time.strftime('%H:%M:%S', time.gmtime())}] {text}", flush=True)


class Fail(Exception):
    pass


def cli(who, *args, json_out=True, admin_nsec=None, timeout=120):
    """Runs mostro-cli as `who` (its own MCLI_DIR); returns (exit code, JSON document or None, text)."""
    env = {k: v for k, v in os.environ.items() if k != "ADMIN_NSEC"}
    env["MCLI_DIR"] = f"{W}/mcli-{who}"
    if admin_nsec:
        env["ADMIN_NSEC"] = admin_nsec
    argv = [MCLI, *(["--output", "json"] if json_out else []), *args]
    done = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    doc = None
    if json_out:
        try:
            doc = json.loads(done.stdout)
        except ValueError:
            doc = None
    return done.returncode, doc, done.stdout + done.stderr


def ok(who, *args):
    """A JSON command that must succeed; returns its data."""
    code, doc, text = cli(who, *args)
    if code != 0 or not doc or doc.get("status") != "ok":
        raise Fail(f"{who}: mostro-cli {' '.join(args)} failed ({code}): {text.strip()[-600:]}")
    return doc.get("data") or {}


def lnc(role, *args):
    done = subprocess.run([*LNC[role], *args], capture_output=True, text=True, timeout=60)
    if done.returncode != 0:
        raise Fail(f"{role}: lncli {args[0]} failed: {done.stderr.strip()[-300:]}")
    return json.loads(done.stdout)


def lookup(table, column, key_column, order):
    """One column of the row for `order`, as the instance's own database has it (read-only). Table
    and column names come from this file only."""
    # The daemon keeps an order's id as a UUID's 16 bytes; text is looked for too.
    with sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=10) as db:
        row = db.execute(f"SELECT {column} FROM {table} WHERE {key_column} = ? OR {key_column} = ?",
                         (uuid.UUID(order).bytes, order)).fetchone()
    return row[0] if row else None


def status(order):
    return lookup("orders", "status", "id", order)


CANCELED = ("canceled", "cooperatively-canceled")
# mostro-cli's error codes for a request the instance never answered (#63).
UNANSWERED = {"reply_timeout", "relay_connection_failed"}


def unanswered(doc):
    """mostro-cli sent the request and heard nothing back: the instance was down, stalled or out of
    reach, and may or may not have acted on it."""
    return bool(doc) and (doc.get("status") == "uncertain" or (doc.get("error") or {}).get("code") in UNANSWERED)


class Trade:
    """One order between the seller and the buyer, answered step by step as the instance asks."""

    current = None  # the trade under way, for the cleanup after a failure

    def __init__(self, name, maker):
        self.name, self.maker = name, maker
        Trade.current = self
        self.order = None
        self.handled = set()
        self.seen = []
        self.hold = None  # (payment hash, lncli process paying it)
        self.buyer_hash = None
        self.blocked = set()  # next steps this scenario does not answer
        self.resent = 0  # requests sent again after the instance did not answer

    def act(self, role, landed, *args):
        """Sends a request that moves the held trade on, the way a careful client does (#63). The
        instance reads only new events and drops a request more than 10 s old (upstream Mostro), so
        one sent while it is down or stalled is lost for good, and mostro-cli calls it `uncertain`.
        The order is then checked: `landed()` says the step happened; if it did not, the request
        goes again until it does, for up to STEP_SECS. Returns the reply's data, or {} when an
        earlier try is the one that landed."""
        deadline = time.time() + STEP_SECS
        while True:
            code, doc, text = cli(role, *args)
            if code == 0 and doc and doc.get("status") == "ok":
                return doc.get("data") or {}
            if landed():  # an earlier try landed, and this one was refused for the status it left
                return {}
            if not unanswered(doc) or time.time() > deadline:
                raise Fail(f"{role}: mostro-cli {' '.join(args)} failed ({code}): {text.strip()[-600:]}")
            self.resent += 1
            log(f"{self.name}: {role}'s {args[0]} {args[1]} was not answered; checking the order, then sending it again")
            for _ in range(5):
                time.sleep(2)
                if landed():
                    return {}

    def canceled_by(self, role):
        """The role's cancel landed: the order is canceled, or the role asked for a cooperative one."""
        assert role in ("seller", "buyer")
        return status(self.order) in CANCELED or bool(lookup("orders", f"{role}_cooperativecancel", "id", self.order))

    def cancel(self, role):
        self.act(role, lambda: self.canceled_by(role), "order", "cancel", "--order", self.order, "--yes")

    def open(self, side):
        args = ["order", "new", "--side", side, "--currency", "USD", "--fiat-amount", "10",
                "--sats", str(SATS), "--payment-method", "bank transfer", "--yes"]
        data = ok(self.maker, *args)
        self.order = data["order"]["id"]
        log(f"{self.name}: {self.maker} made a {side} order {self.order} for {SATS} sat")
        return self.order

    def take(self, taker, how):
        ok(taker, "order", how, "--order", self.order, "--yes")
        log(f"{self.name}: {taker} took it ({how})")

    def answer(self, role, message):
        step = message.get("next_step")
        payload = message.get("payload") or {}
        self.seen.append((role, message.get("action"), step))
        if not step or step in self.blocked:
            return
        # A message can name a step before it carries what the step needs (a second PayInvoice
        # comes with the invoice): wait for one that does.
        if step == "pay_hold_invoice" and not payload.get("invoice"):
            log(f"{self.name}: {role} was asked to pay with no invoice yet ({payload.get('kind')}); waiting")
            return
        key = (role, step, payload.get("invoice"))
        if key in self.handled:
            return
        self.handled.add(key)
        if step == "pay_hold_invoice":
            invoice = payload["invoice"]
            payment_hash = lnc(role, "decodepayreq", invoice)["payment_hash"]
            proc = subprocess.Popen([*LNC[role], "payinvoice", "--force", "--json", invoice],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.hold = (payment_hash, proc)
            log(f"{self.name}: {role} pays the hold invoice ({payload.get('amount')} sat)")
        elif step == "add_invoice":
            amount = payload.get("amount") or (payload.get("order") or {}).get("sats_amount")
            if not amount:
                self.handled.discard(key)
                log(f"{self.name}: {role} was asked for an invoice with no amount ({payload.get('kind')}); waiting")
                return
            # The instance wants an invoice valid for its whole invoice_expiration_window (an hour by
            # default) from when it checks: one that lives exactly an hour is a second short.
            invoice = lnc(role, "addinvoice", "--amt", str(amount), "--memo", "bipnip fiat itest",
                          "--expiry", "7200")
            self.buyer_hash = invoice["r_hash"]
            self.act(role, lambda: lookup("orders", "buyer_invoice", "id", self.order) is not None,
                     "order", "add-invoice", "--order", self.order, "--invoice", invoice["payment_request"], "--yes")
            log(f"{self.name}: {role} sent an invoice for {amount} sat")
        elif step == "fiat_sent":
            self.act(role, lambda: status(self.order) in ("fiat-sent", "dispute", "settled-hold-invoice", "success"),
                     "order", "fiat-sent", "--order", self.order, "--yes")
            log(f"{self.name}: {role} says the fiat was sent")
        elif step == "release":
            self.release(role)
        elif step == "rate":
            code, _, text = cli(role, "order", "rate", "--order", self.order, "--rating", "5", "--yes")
            log(f"{self.name}: {role} rated" if code == 0 else f"{self.name}: {role} could not rate: {text[-200:]}")
        else:
            raise Fail(f"{self.name}: the instance asked {role} for {step}, which this run never expects")

    def release(self, role):
        self.act(role, lambda: status(self.order) in ("settled-hold-invoice", "success"),
                 "order", "release", "--order", self.order, "--yes")
        log(f"{self.name}: {role} released")

    def drive(self, until, what):
        """Answers both sides until `until(status)` holds; fails after STEP_SECS."""
        deadline = time.time() + STEP_SECS
        while time.time() < deadline:
            now = status(self.order)
            if until(now):
                log(f"{self.name}: {what} (status {now})")
                return now
            for role in ("seller", "buyer"):
                code, doc, _ = cli(role, "message", "list", "--since", "60", "--order", self.order)
                if code != 0 or not doc or doc.get("status") != "ok":
                    continue
                for message in doc["data"]["messages"]:
                    if (role, message["event_id"]) in self.handled:
                        continue
                    self.handled.add((role, message["event_id"]))
                    self.answer(role, message)
            time.sleep(2)
        raise Fail(f"{self.name}: not {what} after {STEP_SECS} s; status {status(self.order)}; "
                   f"messages seen {self.seen}")

    def hold_ends(self, want):
        """Waits for the seller's payment of the hold invoice to end, and checks how."""
        if not self.hold:
            raise Fail(f"{self.name}: the seller was never asked to pay a hold invoice")
        payment_hash, proc = self.hold
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            raise Fail(f"{self.name}: the seller's payment of the hold invoice did not end")
        payments = lnc("seller", "listpayments", "--include_incomplete")["payments"]
        states = [p["status"] for p in payments if p["payment_hash"] == payment_hash]
        if want not in states:
            raise Fail(f"{self.name}: the seller's hold payment ended {states}, not {want}")

    def buyer_paid(self, expected=None):
        if not self.buyer_hash:
            raise Fail(f"{self.name}: the buyer never sent an invoice")
        deadline = time.time() + 120
        while time.time() < deadline:
            invoice = lnc("buyer", "lookupinvoice", self.buyer_hash)
            if invoice["state"] == "SETTLED":
                paid = int(invoice["amt_paid_sat"])
                if expected is not None and paid != expected:
                    raise Fail(f"{self.name}: the buyer was paid {paid} sat, not {expected}")
                return paid
            time.sleep(2)
        raise Fail(f"{self.name}: the buyer's invoice was never paid")

    def buyer_unpaid(self):
        if self.buyer_hash and lnc("buyer", "lookupinvoice", self.buyer_hash)["state"] == "SETTLED":
            raise Fail(f"{self.name}: the buyer was paid on a canceled trade")


def setup():
    for who in ("seller", "buyer", "solver"):
        ok(who, "identity", "init")
    # The daemon's own key adds a solver (what Mostro's admin flow does); the solver then works
    # disputes with its own key.
    code, _, text = cli("admin", "admaddsolver", "-n", os.environ["SOLVER_PUB"], json_out=False,
                        admin_nsec=os.environ["INSTANCE_NSEC"])
    if code != 0 or "added" not in text.lower():
        raise Fail(f"the instance's key could not add a solver: {text.strip()[-600:]}")
    log("identities made; the solver is added")


def sell():
    trade = Trade("sell", maker="seller")
    trade.open("sell")
    trade.take("buyer", "take-sell")
    trade.drive(lambda s: s == "success", "paid out to the buyer")
    trade.hold_ends("SUCCEEDED")
    paid = trade.buyer_paid()
    log(f"sell: the hold invoice was settled and the buyer's node received {paid} sat")


def buy():
    trade = Trade("buy", maker="buyer")
    trade.open("buy")
    trade.take("seller", "take-buy")
    trade.drive(lambda s: s == "success", "paid out to the buyer")
    trade.hold_ends("SUCCEEDED")
    paid = trade.buyer_paid()
    log(f"buy: the hold invoice was settled and the buyer's node received {paid} sat")


def cancel():
    trade = Trade("cancel", maker="seller")
    trade.blocked = {"fiat_sent"}
    trade.open("sell")
    trade.take("buyer", "take-sell")
    trade.drive(lambda s: s == "active", "held: the seller paid and the buyer sent an invoice")
    for role in ("seller", "buyer"):
        trade.cancel(role)
        log(f"cancel: {role} asks to cancel")
    trade.drive(lambda s: s in ("cooperatively-canceled", "canceled"), "canceled by both")
    trade.hold_ends("FAILED")
    trade.buyer_unpaid()
    log("cancel: the hold invoice was canceled, the seller's sats came back, the buyer got nothing")


def dispute():
    trade = Trade("dispute", maker="seller")
    trade.blocked = {"release"}
    trade.open("sell")
    trade.take("buyer", "take-sell")
    trade.drive(lambda s: s == "fiat-sent", "the buyer says the fiat was sent; the seller does not release")
    data = trade.act("buyer", lambda: status(trade.order) == "dispute", "dispute", "open", "--order", trade.order, "--yes")
    dispute_id = data.get("dispute_id") or lookup("disputes", "id", "order_id", trade.order)
    if isinstance(dispute_id, bytes):
        dispute_id = str(uuid.UUID(bytes=dispute_id))
    if not dispute_id:
        raise Fail("dispute: the order is in dispute but no dispute row names it")
    log(f"dispute: the buyer opened dispute {dispute_id}")
    solver = os.environ["SOLVER_NSEC"]
    deadline = time.time() + STEP_SECS
    while True:
        code, _, text = cli("solver", "admtakedispute", "-d", dispute_id, json_out=False, admin_nsec=solver)
        if code == 0 and "error" not in text.lower() and "cant" not in text.lower():
            break
        if time.time() > deadline:
            raise Fail(f"dispute: the solver could not take it: {text.strip()[-600:]}")
        time.sleep(3)
    log("dispute: the solver took it")
    # Sent again until it lands, like every step that moves a held trade on (#63).
    settled = ("settled-hold-invoice", "success", "settled-by-admin", "completed-by-admin")
    deadline = time.time() + STEP_SECS
    while True:
        code, _, text = cli("solver", "admsettle", "-o", trade.order, json_out=False, admin_nsec=solver)
        if code == 0 or status(trade.order) in settled:
            break
        if time.time() > deadline:
            raise Fail(f"dispute: the solver could not settle: {text.strip()[-600:]}")
        trade.resent += 1
        log("dispute: the solver's settle was not answered; sending it again")
        time.sleep(5)
    log("dispute: the solver settled it for the buyer")
    trade.blocked = {"release", "fiat_sent"}
    trade.drive(lambda s: s in ("success", "settled-by-admin", "completed-by-admin"), "settled by the solver")
    trade.hold_ends("SUCCEEDED")
    paid = trade.buyer_paid()
    log(f"dispute: the hold invoice was settled by the solver's decision and the buyer received {paid} sat")


GAP_SECS = 40


def gap():
    """The instance stops answering across the seller's release, as when it restarts (#63). Upstream
    Mostro subscribes to new events only and drops a request more than 10 s old, so a release sent
    while it is stopped is lost, and mostro-cli calls it `uncertain`. The trade must still end."""
    pid = int(os.environ["INSTANCE_PID"])
    trade = Trade("gap", maker="seller")
    trade.blocked = {"release"}
    trade.open("sell")
    trade.take("buyer", "take-sell")
    trade.drive(lambda s: s == "fiat-sent", "the buyer says the fiat was sent")
    os.kill(pid, signal.SIGSTOP)
    log(f"gap: the instance is stopped for {GAP_SECS} s; the seller releases")
    thaw = threading.Timer(GAP_SECS, os.kill, (pid, signal.SIGCONT))
    thaw.start()
    try:
        trade.release("seller")
    finally:
        thaw.cancel()
        os.kill(pid, signal.SIGCONT)
    if not trade.resent:
        raise Fail("gap: the release was answered while the instance was stopped, so the gap tested nothing")
    log(f"gap: the seller sent the release {trade.resent + 1} times")
    trade.drive(lambda s: s == "success", "paid out to the buyer")
    trade.hold_ends("SUCCEEDED")
    paid = trade.buyer_paid()
    log(f"gap: the release landed after the instance came back and the buyer's node received {paid} sat")


if __name__ == "__main__":
    steps = {"setup": setup, "sell": sell, "buy": buy, "cancel": cancel, "dispute": dispute, "gap": gap}
    try:
        steps[sys.argv[1]]()
    except Fail as e:
        log(f"FAIL: {e}")
        # Leave nothing open: a pending order of one side's would refuse that side's next take, and a
        # held one keeps the seller's sats with nobody left to act (#63). Both sides cancel, each
        # until it lands; a cooperative cancel gives the seller's sats back.
        trade = Trade.current
        if trade is not None and trade.order:
            for who in ("seller", "buyer"):
                try:
                    trade.cancel(who)
                    log(f"cleanup: {who} cancel of {trade.order}: ok")
                except Fail as e:
                    log(f"cleanup: {who} cancel of {trade.order}: {str(e)[-160:]}")
            log(f"cleanup: the order is {status(trade.order)}")
        sys.exit(1)
