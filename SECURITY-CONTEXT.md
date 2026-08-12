# Audit context — Trenches

Read this before auditing. It says what the software is, what an attacker
would actually want from it, where the trust boundaries are, and what we
already know. Written for an automated auditor; no marketing.

## What this is

Two codebases, one product.

**1. `trenches` — a terminal (TUI) trading bot.** Rust, ~30k LoC, AGPL-3.0,
public at `github.com/asyncswap/trenches`. Runs on a user's own machine.
Holds password-encrypted keystores and **signs and submits real transactions**
on Solana (pump.fun bonding curves, PumpSwap AMM) and Robinhood Chain (an
Arbitrum-stack EVM L2; Uniswap V3/V4, pons.family, Flaunch). Users are
retail traders; balances are typically small but real. Distributed as signed
release binaries via a `curl | sh` installer.

**2. `trenches-rpc` — a Cloudflare Worker.** TypeScript, ~1.2k LoC, private
repo. An authenticated JSON-RPC proxy at `rpc.trenches.sh`: it holds *our*
paid provider credentials (Helius, Alchemy), issues per-subscriber API keys
(`trk_…`) stored in Workers KV, meters usage into Analytics Engine, enforces
a free-tier rate limit at the edge, proxies Solana websockets, and runs
Stripe subscription billing (Checkout + webhook + Customer Portal). Also
serves the subscriber dashboard (React SPA) from the same origin.

## What an attacker wants (ranked)

1. **A user's private key.** Keystores at `~/.config/trenches/keystores` and
   `~/.foundry/keystores` (shared with Foundry, deliberately). Anything that
   exfiltrates a key, weakens the password path, or gets a key written in
   plaintext is the top finding.
2. **A user signing something they didn't intend.** The app's core promise is
   "nothing here trades by itself — every order is a key you pressed."
   Anything that induces a transaction, alters its parameters (size,
   slippage floor, recipient), or misrepresents what is being signed matters
   as much as key theft.
3. **Our provider credentials** in the Worker's secrets, or any path where an
   error/response echoes one.
4. **Other subscribers' API keys**, or free-tier users escaping the rate
   limit / plan checks (revenue, and our upstream bill).
5. **Stripe abuse** — forging a webhook to grant `pro`, or reaching another
   account's billing portal.

## Trust boundaries (where hostile data enters)

Everything below is attacker-controlled. Anyone can create a token, name it
anything, and trade against a pool the user is watching.

- **Token metadata**: name, symbol, and a metadata **URI** read from
  Metaplex accounts / Token-2022 extensions, plus the **off-chain JSON** that
  URI points at (fetched over HTTP by the user's machine). Rendered in the
  TUI. See `src/sol/metadata.rs`, `clean_text` in `src/sol/discover.rs`.
- **Chain data**: borsh/TLV decoders for bonding curves, AMM pools, global
  config; log-subscription notifications; transaction JSON. Hostile lengths
  and truncation are expected. `src/sol/{pumpfun,pumpswap,discover,engine}.rs`,
  `src/engine.rs`, `src/discover.rs`.
- **The tape**: other people's trades, including their addresses, flow into
  the UI and into persisted files.
- **RPC responses**: the app trusts its configured endpoints for pricing and
  balances. A malicious/compromised RPC is in scope — what can it make the
  app do? (Relevant: users paste provider URLs into config.)
- **Config file**: `~/.config/trenches/config.json`, user-authored, JSON
  Schema published. Network entries, endpoint URLs.
- **The Worker's public surface**: every `/portal/*`, `/auth/*`,
  `/stripe/webhook`, `/admin/*` route, and the product paths
  `/<key>/<chain>`, `/<chain>?api=<key>`.

## Design decisions that are intentional (don't report as bugs)

- **No seed phrases anywhere.** Config has no field for one; accounts are
  encrypted keystores only. This is deliberate.
- **Keystores are shared with Foundry** (`~/.foundry/keystores`, read-only —
  we never write there). Interop is intended.
- **The app never auto-trades.** No timers, strategies, or background loops
  submit transactions. `b`/`s`/`x` are the only keys that spend.
- **Solana keystores use the Ethereum (Web3 Secret Storage) format** rather
  than Solana's plaintext JSON array — encrypted at rest beats convention.
- **The free tier is enforced at the edge** (Cloudflare rate limiter), not in
  the app.
- **Session = HMAC-signed cookie**, OAuth state = signed self-expiring token
  (no state cookie). `SESSION_SECRET` in Worker secrets.
- **`/portal/keys/<key>` (GET) is unauthenticated by design** — it answers
  only what possession of that key already grants (expiry/created).

## Known-weak areas we'd most like eyes on

Ranked by our own unease, not by what's easy to scan for:

1. **The signing path end to end** — `src/wallet.rs`, `src/sol/wallet.rs`,
   `src/sol/{tx,trade,engine}.rs`, `src/engine.rs`. Password handling and
   zeroization (the password currently lives as a plain `String`; an unused
   zeroizing wrapper was recently deleted rather than wired up — we consider
   this an open weakness). Keystore filename handling (a `name` becomes a
   filename; there is a traversal guard + test, please try to beat it).
2. **Slippage floors and quote math.** `min_tokens_out` / `min_sol_output`
   are the only thing standing between a user and a hostile fill. As of
   v0.2.4 the AMM buy path derives its floor from a `simulateTransaction`
   result (`Rpc::simulate_post_token`) — i.e. **a value returned by an RPC
   endpoint now influences a signed transaction's protection**. That is a
   deliberate trade-off; attack it. Fee math, boost/virtual reserves,
   inverted-orientation pools (SOL as base vs quote) are all places a sign
   error becomes a fund loss.
3. **Untrusted text into a terminal.** ANSI/control filtering happens in
   `clean_text`. We just fixed bidi/zero-width passthrough (see Fixed below).
   Look for other paths that print chain-derived strings without it — logs,
   session files, panel labels, error messages.
4. **The Worker's authorization checks.** Every `/portal/*` route re-loads
   the account and must verify ownership (`acct.keys.includes(key)`) before
   acting. We have edited these routes rapidly; assume one is wrong.
5. **Stripe webhook**: signature verification (`src/stripe.ts`,
   `verifySignature`) is hand-rolled HMAC with a 5-minute tolerance. It is
   the **only** writer of the `pro` plan. Forge it and you get free service.
6. **The batch splitter** (`src/index.ts`): array bodies are parsed and
   fanned out. Resource limits, amplification, and metering accuracy.
7. **Persistence**: per-coin tape and per-wallet order files under
   `~/.trenches/` are JSON written from chain data and re-read at startup.
   Path construction, unbounded growth, and parse-on-load are all in scope.
8. **The updater** (`src/update.rs`): downloads a release, verifies
   SHA256SUMS with a pinned minisign key. Anything that lets an unsigned or
   substituted binary land is critical.

## Recently fixed (don't re-report; useful as a fingerprint of our blind spots)

- **Bidi/zero-width spoofing in token names** — `clean_text` filtered
  `is_control` only; U+202E, U+200B, isolates and friends are *format*
  characters and passed through, letting a coin's on-screen name differ from
  its bytes (Trojan-Source applied to token metadata). Now filtered; test in
  `src/sol/discover.rs::text_safety_tests`.
- **SSRF via metadata URI** — the socials fetch followed any `http(s)` URI
  from on-chain metadata, including `localhost`, RFC1918, `169.254.169.254`
  and `.local`. Now HTTPS-only with a public-host check; test in
  `src/sol/metadata.rs::tests`.

## Things we know are imperfect (context, not findings to pad)

- Non-constant-time comparisons for the session HMAC and the admin bearer
  token (string `!==`). We judge remote timing impractical here; argue us out
  of it if you disagree.
- The usage query interpolates a key into an Analytics Engine SQL string
  after stripping to `[A-Za-z0-9_]` and after an ownership check.
- `~/.trenches/` session logs record addresses and amounts, never keys or
  passwords. If you find a path that writes a secret there, that's critical.

## Build / run

```
cargo build --release --features solana      # what ships
cargo test  --release --features solana      # 240 tests
```

Worker: `bunx tsc --noEmit`, `bunx wrangler deploy`. Secrets are never in the
repo; `wrangler secret list` names them.

## Contact

Security reports: support@asyncswap.org — or open an issue at
`github.com/asyncswap/trenches/issues` (there is an RPC/security-aware issue
form). We ship fixes same-day; several of this week's releases were
user-reported bugs fixed within the hour.
