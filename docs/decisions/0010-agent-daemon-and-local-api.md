# 10. The agent as a daemon, with a local API and spend limits

Date: 2026-11-17

## Status

Accepted. First built without pairing: file permissions on the socket are the only authentication until
the web client (ADR 0009) brings pairing and session keys.

## Context

Until now one command ran one lease or one ring and ended with it (`bipnip-agent lease offer`, `ring join`,
…). That was enough to prove the flows on regtest. It is not how a node runner uses it: an offer has to
stay up for days, several leases and rings run at once, a restart must lose nothing, and something has to
answer the web client later. The pieces are already there: the lease and ring runs take messages and
ticks and do no I/O of their own, every run is saved after every step, and `lease resume` and
`ring resume` already drive several stored runs in a loop.

Two things a long-running agent adds that a one-shot command did not need:

- **A way in.** Something must be able to start a lease, list what is running, and answer a question the
  agent has, without being the agent.
- **Limits.** A command a person typed carries that person's intent for that one lease. A daemon that
  takes requests over a socket needs to know, without asking each time, how much of the node's money a
  request may move.

## Decision

1. **One process holds the keys and the node.** `bipnip-agent run` loads the wallet, connects to the
   node, resumes every stored run that is not finished, and steps all of them on a timer. It is the only
   process that reads the mnemonic or talks to the node.
2. **A lock on the data directory.** The daemon takes an exclusive lock (`<data_dir>/agent.lock`) for
   its lifetime. The one-shot commands take the same lock, so a command and the daemon never drive the
   same run at once, and two daemons never share a wallet.
3. **A unix socket, same user only.** `<data_dir>/agent.sock`, mode 0600, in a data directory the
   daemon insists is 0700. Newline-delimited JSON; one request per connection, then status lines, then
   one final line (`docs/agent-api.md`). No network listener. Whoever can open the socket can ask for
   anything the policy allows, and nothing more: no key, macaroon, mnemonic or preimage ever crosses it.
4. **One relay connection for every run.** The daemon asks the relays once per round for everything
   addressed to the trade keys of the runs under way, and hands each run what is for it, instead of one
   connection per run. Relays are still polled, not subscribed to (as before: a lease moves at the pace
   of blocks).
5. **Policy before money.** A `[policy]` section sets what a request may do unasked: the most fee per
   lease and per day, the most capacity opened per day, the largest bond, how many runs at once, and
   optionally which peers. Every limit defaults to zero: **a fresh agent moves no money on a request
   alone.** A request over a limit is not refused; it is parked as `needs-approval` and listed, and it
   starts only when approved over the same socket (`bipnip approve`). The policy is checked when a run
   is asked for and again at the node: a run the daemon starts reaches the node only through a guard
   that holds its payments and its channel to what the run was granted.
6. **Several instances.** `[[instance]]` may appear more than once; orders are read from all of them.
   Each instance's proof-of-work requirement is read from its info event, not from the config.
7. **`bipnip` is a thin client.** The CLI crate speaks to the socket and holds nothing.
8. **The one-shot commands stay** for scripts and for the test harness, on the same code.

## Consequences

- A restart is the normal case, not the exceptional one: the daemon is the resume loop.
- The socket is as strong as the user account. A process running as the same user can ask for anything
  within the policy; the zero defaults and approvals bound what that is. Pairing, per-client scopes and a
  confirmation surface the client cannot fake come with the web client (ADR 0009).
- Approval over the same socket is not a second factor: it separates "a request arrived" from "the
  operator looked at it", and nothing more. It is honest about that.
- What is used in a day is counted from the grants on disk, so the limits hold across restarts. It is
  counted when a run starts, not when it pays: a run that is canceled still used its share of the day.
- A run a one-shot command started has no grant, and the daemon carries it on unguarded.
- Known gap, older than the daemon: a run is saved before its answer is sent, and only an answer that
  *failed* to send is marked as owed. A process killed between the save and the send leaves that one
  message unsent and unmarked. The one message whose loss costs money is covered: a buyer's run that is
  taken up again in the state "released" sends its preimage again (to the seller alone, after checking
  the channel again; a second copy does no harm). The others are not sent again on a restart, because the
  instance answers a second take or offer with a refusal, and a refusal can end a run. Closing the gap
  for them needs the instance to answer a repeated message with its first answer.
- Not in this step: remote access, a second user, TLS, an HTTP API.
