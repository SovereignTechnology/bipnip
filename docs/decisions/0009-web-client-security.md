# 9. The web client: untrusted tab, agent as the boundary

Date: 2026-11-10

## Status

Accepted (built in M5); amended 2026-11-10: the first web client is a fork of the RoboSats UI
(`bipnip-web`, AGPL-3.0; plan in that repo), and the Leptos client below becomes its later Rust/WebAssembly
rewrite. Everything else here applies to both, plus a per-load style nonce while the React client's
runtime styles need one.

## Context

The project owner wants a RoboSats-style client: run by the user, no app server, all traffic over Nostr
relays, and "super secure". Research (2026-11-10): no stock browser can verify a web app's code on every
load. WEBCAT (Freedom of the Press Foundation) is a Firefox-only alpha; WAICT is a Firefox Nightly
prototype; Isolated Web Apps are limited to managed ChromeOS; SRI cannot protect the HTML document that
loads everything else; service-worker pinning trusts the first load. Mostro's own web app keeps the seed
where page script can reach it. NIP-07 and NIP-46 hand out one key and cannot derive Mostro's per-trade
keys. Tor Browser's Safer and Safest levels block WebAssembly.

## Decision

1. **The agent is the security boundary.** It holds the node credentials and the NIP-06 mnemonic; no key
   ever enters the page. Every client request passes the agent's policy (spend caps per trade, lease and
   day; allowed channel peers; ring limits; rate limits). Above a threshold the agent asks for
   confirmation through a surface it renders itself (TUI, CLI or a paired phone), showing the real
   amounts and peers.
2. **Pairing:** a client gets an ephemeral session key through a ceremony showing the same visual
   fingerprint on both ends, with scoped permissions (actions, caps, expiry), revocable at the agent.
3. **Delivery, strongest first:** (a) UI files embedded in the signed, reproducible agent binary and
   served on loopback or the tailnet with `Host`/`Origin` checks, a random port and a session token;
   (b) an optional Tauri v2 shell (locked capabilities, isolation pattern, signed updates with
   `requireSignedVersion`); (c) for use away from home, a pinned NIP-5A nsite snapshot (kind 5128),
   WEBCAT enrollment, an onion mirror, SRI and `Integrity-Policy` on every script, wasm fetched with
   `fetch(url, {integrity})`. (c) is documented as the weaker path.
4. **The page talks only to the agent.** Local mode: `connect-src` names only the agent, which reaches
   relays over Tor (arti). Remote mode: NIP-44 requests through the user's own relays, `connect-src`
   limited to them.
5. **Headers:** CSP `default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src <agent>;
   img-src 'self' blob:; base-uri 'none'; form-action 'none'; frame-ancestors 'none';
   require-trusted-types-for 'script'`; COOP and CORP `same-origin`, COEP `require-corp`, a deny-all
   Permissions-Policy. In nsite mode only `<meta>` CSP is available; documented.
6. **Untrusted content is inert:** every Nostr string is rendered as a text node; no Markdown or HTML,
   no remote images, no link previews. The client verifies every signature and NIP-44 MAC itself with
   `bipnip-core` compiled to wasm.
7. **Supply chain:** Leptos in client-side mode with Trunk, no npm; pinned rustc, wasm-bindgen and
   wasm-opt; `cargo deny` and `cargo vet`; a reproducible build checked by two independent builders;
   hashes in a transparency log and in a release event signed by the project's key.
8. **Phishing:** the agent pins the project's release key and refuses clients whose build hash is not in
   a signed release; trade details are re-shown by the agent before money moves.

## Consequences

- A compromised page can at most ask the agent for what the policy allows, and big moves still need a
  confirmation it cannot fake.
- Still unsolved, and said so in the user docs: per-load code verification in stock browsers, a
  compromised browser or extension, nsite gateways unless self-hosted, Tor Browser's safer levels.
