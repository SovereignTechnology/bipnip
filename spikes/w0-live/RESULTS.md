# W0 live check: results (2026-11-10)

`kata-run --net --timeout 7200 -- bash spikes/w0-live/run.sh` on the agentrun Kata runner. Nothing ran on the
laptop. bipnip-web `4b727687` (pure RoboSats v0.8.8-alpha UI) built in the VM, served by `bipnip-webhost`,
coordinators reached over Tor (arti 0.47.0), headless Chromium (Playwright 1.64.1).

**PASS** (run 6): the Offers page rendered the live federation order book (91 orders; 10 rows on screen)
34 s after the Offers click; **0 CSP violations, 0 requests to any origin but the host, 0 failed requests**.
The only non-2xx was `404 /static/locales/en-US.json` (i18next tries the region locale before `en.json`,
as upstream). Read only: no robot, order or trade; the Garage Key mode toggle only clears the (empty) local
garage.

Through the host, over Tor, read only: mainnet temple, lake, bazaar, ammanaya answered `api/info` and
`api/book` (16–53 orders each); freeport and gumpellite answered `api/info` but 404 on `api/book`
(they publish their book over Nostr only); testnet lake was unreachable (502) in every run.

What the runs found and fixed (each now covered by a test in `bipnip-webhost`):
1. rustls under arti had no crypto provider → panic at Tor bootstrap (#3).
2. The robohash Web Worker was blocked: a worker created by a COEP page needs COEP on its own script (#4).
3. Self-hosted RoboSats uses a MemoryRouter starting at `/`, so deep links render blank → `303 /` (#6).

Flow a user follows (and the check does): open `/`, switch Legacy → Garage Key mode (UI starts in Legacy,
where Offers is disabled), open Offers.
