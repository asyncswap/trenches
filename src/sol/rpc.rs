// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Minimal Solana JSON-RPC client over the `reqwest` client the EVM side already
//! uses — no `solana-client` dependency (it drags in the whole agave tree).
//!
//! Only the calls the bot actually needs: read accounts, read balances, fetch a
//! blockhash, send and confirm a transaction.

use std::time::Duration;

use serde_json::{json, Value};
use solana_pubkey::Pubkey;

const RPC_TIMEOUT: Duration = Duration::from_secs(8);

/// A JSON-RPC handle over one or more endpoints.
///
/// Requests round-robin across the configured endpoints, and a failed request
/// retries on the next one. That does two useful things at once: it spreads load
/// so no single provider's rate limit is the ceiling, and it survives one
/// provider throttling or going down — which public RPC does constantly.
#[derive(Clone)]
pub struct Rpc {
    urls: Vec<String>,
    /// Rotates per request. Shared across clones so the whole app spreads evenly.
    next: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Smoothed round-trip per endpoint (micros). Requests try the fastest
    /// rested endpoint first — pure round-robin meant every Nth request ate
    /// the slowest endpoint's multi-second latency even while a fast one sat
    /// idle, which showed up as 2–4s poller rounds.
    ewma_us: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>>,
    /// Unix millis until which each endpoint is benched (429 or broken).
    /// Shared across clones: one clone learning an endpoint is limited
    /// spares every other clone the same refusal.
    cooldown: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>>,
    http: reqwest::Client,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long a 429 benches an endpoint. Solana public endpoints throttle in
/// ~10s windows; parsing a reset hint is not possible (there is none).
const COOLDOWN_LIMITED_MS: u64 = 10_000;
/// How long a connection error / 5xx benches one — broken is usually brief.
const COOLDOWN_BROKEN_MS: u64 = 3_000;

impl Rpc {
    pub fn new(url: impl Into<String>) -> Rpc {
        Rpc::new_pool(vec![url.into()])
    }

    /// Build over several endpoints. Empty input is treated as a single bad URL
    /// so callers never have to handle a "no endpoints" case.
    pub fn new_pool(urls: Vec<String>) -> Rpc {
        let urls = if urls.is_empty() { vec![String::new()] } else { urls };
        Rpc {
            cooldown: std::sync::Arc::new(
                urls.iter().map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
            ),
            ewma_us: std::sync::Arc::new(
                urls.iter().map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
            ),
            urls,
            next: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            http: reqwest::Client::new(),
        }
    }

    /// Any endpoint in the pool speaks Helius's extended API.
    fn has_helius(&self) -> bool {
        self.urls.iter().any(|u| u.contains("helius"))
    }

    /// How many endpoints are in rotation.
    pub fn endpoint_count(&self) -> usize {
        self.urls.len()
    }

    /// One JSON-RPC round trip, retried across endpoints on failure.
    ///
    /// Returns the `result` value, or an error carrying the node's message —
    /// RPC errors arrive in-band with HTTP 200, not as a status code.
    async fn call(&self, method: &str, params: Value) -> eyre::Result<Value> {
        self.call_on(method, params, |_| true).await
    }

    /// `call`, restricted to endpoints `allow` accepts — for provider-specific
    /// methods. Asking everyone meant every rotation hop before the right
    /// provider answered "Method not found" into the log and a phantom
    /// failure into the health stats, every poll, forever.
    async fn call_on(
        &self,
        method: &str,
        params: Value,
        allow: impl Fn(&str) -> bool,
    ) -> eyre::Result<Value> {
        /// Host only — an API key must never reach a log line.
        fn safe_host(url: &str) -> &str {
            url.split('?').next().unwrap_or(url)
        }

        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        // Fastest first (unmeasured endpoints sort first so each gets timed
        // once); the round-robin cursor only breaks ties, so equal endpoints
        // still share load.
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut order: Vec<usize> = (0..self.urls.len()).collect();
        order.sort_by_key(|&i| {
            (self.ewma_us[i].load(std::sync::atomic::Ordering::Relaxed), (start + i) % self.urls.len())
        });
        let mut last_err = None;

        // Two passes over the endpoints. One pass is not enough: providers
        // rate-limit in bursts, and when every endpoint 429s at the same instant
        // the round returns nothing — which the tape cannot distinguish from
        // "no trades happened". A brief pause is usually all it takes.
        for pass in 0..2 {
            for (hop, &i) in order.iter().enumerate() {
                let url = &self.urls[i];
                if !allow(url) {
                    continue;
                }
                // Skip an endpoint that recently answered 429 — asking again
                // inside its window converts one refusal into a stream of
                // them. Unless it is the last hope this pass, in which case
                // trying beats returning nothing.
                let benched = self.cooldown[i].load(std::sync::atomic::Ordering::Relaxed) > now_ms();
                if benched && hop + 1 < self.urls.len() {
                    continue;
                }
                // Timed into the same counters the EVM side uses, so the health
                // light and the RPC rollup read this chain when it is the one
                // running. Only one chain is live at a time, so one set of
                // counters is the whole picture.
                let t0 = std::time::Instant::now();
                let attempt = async {
                    let resp = tokio::time::timeout(RPC_TIMEOUT, self.http.post(url).json(&body).send())
                        .await
                        .map_err(|_| eyre::eyre!("{method}: rpc timeout"))??;
                    // Check the STATUS before parsing. A 429 or 5xx body is not
                    // JSON-RPC: it has neither `error` nor `result`, so parsing
                    // it yielded `Null`, which callers read as a legitimately
                    // empty answer. That turned rate limiting into phantom
                    // "no trades" instead of a retry.
                    let status = resp.status();
                    if !status.is_success() {
                        if status.as_u16() == 429 {
                            super::trace(&format!("rpc 429 from {}", safe_host(url)));
                            self.cooldown[i].store(
                                now_ms() + COOLDOWN_LIMITED_MS,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        } else if status.is_server_error() {
                            self.cooldown[i].store(
                                now_ms() + COOLDOWN_BROKEN_MS,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        eyre::bail!("{method}: http {status}");
                    }
                    let v: Value = resp.json().await?;
                    if let Some(e) = v.get("error") {
                        let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
                        // A failed simulation puts the REASON in `data`, not in
                        // `message` — which just says "Transaction simulation
                        // failed". Dropping it left every rejected trade
                        // reading "UNKNOWN" with nothing to act on.
                        let detail = e.get("data").map(|d| {
                            let err = d.get("err").map(|x| x.to_string()).unwrap_or_default();
                            // Anchor names the offending account in a log line
                            // ("AnchorError caused by account: sharing_config.
                            // Error Code: ConstraintSeeds"), several lines above
                            // the final "failed: custom program error" — so
                            // prefer that line and fall back to the last one.
                            // Without it a constraint failure says only which
                            // instruction broke, not which account.
                            let logs = d.get("logs").and_then(|l| l.as_array());
                            let named = logs.and_then(|a| {
                                a.iter().filter_map(|l| l.as_str()).find(|l| {
                                    l.contains("AnchorError") || l.contains("Error Code:")
                                })
                            });
                            let last = named
                                .or_else(|| {
                                    logs.and_then(|a| a.iter().rev().find_map(|l| l.as_str()))
                                })
                                .unwrap_or("");
                            match (err.is_empty(), last.is_empty()) {
                                (true, true) => String::new(),
                                (false, true) => format!(" ({err})"),
                                (true, false) => format!(" ({last})"),
                                (false, false) => format!(" ({err}: {last})"),
                            }
                        }).unwrap_or_default();
                        super::trace(&format!("rpc {method} error: {msg}{detail}"));
                        eyre::bail!("{method}: {msg}{detail}");
                    }
                    // A JSON-RPC reply with neither field is malformed, not empty.
                    match v.get("result") {
                        Some(r) => Ok::<Value, eyre::Report>(r.clone()),
                        None => eyre::bail!("{method}: reply had no result"),
                    }
                }
                .await;
                match attempt {
                    Ok(v) => {
                        crate::rpcstats::record(method, true, t0.elapsed(), None);
                        let us = t0.elapsed().as_micros() as u64;
                        let prev = self.ewma_us[i].load(std::sync::atomic::Ordering::Relaxed);
                        let next = if prev == 0 { us } else { (prev * 7 + us) / 8 };
                        self.ewma_us[i].store(next, std::sync::atomic::Ordering::Relaxed);
                        return Ok(v);
                    }
                    Err(e) => {
                        crate::rpcstats::record(method, false, t0.elapsed(), Some(&e.to_string()));
                        last_err = Some(e);
                    }
                }
            }
            if pass == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        Err(last_err.unwrap_or_else(|| eyre::eyre!("{method}: no endpoints configured")))
    }

    /// Raw account data (base64-decoded) plus its owning program. `None` when the
    /// account doesn't exist — which is meaningful (e.g. an ATA not yet created).
    pub async fn account(&self, key: &Pubkey) -> eyre::Result<Option<(Vec<u8>, Pubkey)>> {
        let res = self
            .call(
                "getAccountInfo",
                json!([key.to_string(), {"encoding": "base64", "commitment": "processed"}]),
            )
            .await?;
        let val = match res.get("value") {
            Some(Value::Null) | None => return Ok(None),
            Some(v) => v,
        };
        let b64 = val
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|d| d.as_str())
            .ok_or_else(|| eyre::eyre!("getAccountInfo: no data"))?;
        let bytes = b64_decode(b64)?;
        let owner = val
            .get("owner")
            .and_then(|o| o.as_str())
            .and_then(|s| s.parse::<Pubkey>().ok())
            .ok_or_else(|| eyre::eyre!("getAccountInfo: no owner"))?;
        Ok(Some((bytes, owner)))
    }

    /// Several accounts in ONE round trip, in the order requested.
    ///
    /// The trenches refresh every visible coin's curve on a timer; one request
    /// per coin would be dozens of round trips a tick and would rate-limit long
    /// before the list got interesting. Solana caps this at 100 keys per call,
    /// so the caller must chunk.
    pub async fn accounts(&self, keys: &[Pubkey]) -> eyre::Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let strs: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        let res = self
            .call(
                "getMultipleAccounts",
                json!([strs, {"encoding": "base64", "commitment": "processed"}]),
            )
            .await?;
        let arr = res.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(keys.len());
        for item in arr {
            out.push(
                item.get("data")
                    .and_then(|d| d.get(0))
                    .and_then(|d| d.as_str())
                    .and_then(|b| b64_decode(b).ok()),
            );
        }
        // A short reply must not silently shift results onto the wrong coin.
        out.resize(keys.len(), None);
        Ok(out)
    }

    /// Several token-account balances in ONE round trip, in the order asked.
    ///
    /// `None` for an account that doesn't exist yet (an ATA before its first
    /// buy) — distinct from a real zero balance.
    pub async fn token_balances(&self, keys: &[Pubkey]) -> eyre::Result<Vec<Option<u64>>> {
        let datas = self.accounts(keys).await?;
        Ok(datas
            .into_iter()
            .map(|d| {
                let d = d?;
                // SPL token account layout: amount is a u64 LE at offset 64.
                if d.len() < 72 {
                    return None;
                }
                Some(u64::from_le_bytes(d[64..72].try_into().ok()?))
            })
            .collect())
    }

    /// A mint's `(supply, decimals, owning token program)` in one read.
    ///
    /// None of these may be assumed. pump coins are 6-decimal with a 1B supply,
    /// but any SPL mint can list on PumpSwap: a hardcoded supply understated one
    /// live coin's market cap by 100x, and a hardcoded decimal count would
    /// misprice sizing by orders of magnitude.
    ///
    /// SPL mint layout: `mint_authority(36) | supply u64 @36 | decimals u8 @44`.
    /// Token-2022 mints share this prefix, so one decoder serves both.
    pub async fn mint_info(&self, mint: &Pubkey) -> eyre::Result<(u64, u8, Pubkey)> {
        let (data, owner) = self
            .account(mint)
            .await?
            .ok_or_else(|| eyre::eyre!("mint {mint} not found"))?;
        if data.len() < 45 {
            eyre::bail!("mint {mint} is too small to be a token mint");
        }
        let supply = u64::from_le_bytes(data[36..44].try_into()?);
        Ok((supply, data[44], owner))
    }

    /// The program that owns a mint — SPL Token or Token-2022. Must be read from
    /// chain, never assumed: pump's own docs warn the HTTP API can report it stale.
    pub async fn mint_owner(&self, mint: &Pubkey) -> eyre::Result<Pubkey> {
        self.account(mint)
            .await?
            .map(|(_, owner)| owner)
            .ok_or_else(|| eyre::eyre!("mint {mint} not found"))
    }

    /// Native SOL balance, in lamports.
    pub async fn balance(&self, key: &Pubkey) -> eyre::Result<u64> {
        let res = self.call("getBalance", json!([key.to_string()])).await?;
        Ok(res.get("value").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    /// SPL token balance in base units (0 when the ATA doesn't exist yet).
    pub async fn token_balance(&self, ata: &Pubkey) -> eyre::Result<u64> {
        match self.call("getTokenAccountBalance", json!([ata.to_string()])).await {
            Ok(res) => Ok(res
                .get("value")
                .and_then(|v| v.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)),
            // A missing ATA is a normal state (nothing bought yet), not an error.
            Err(_) => Ok(0),
        }
    }

    /// What a competitive transaction is paying, in micro-lamports per compute
    /// unit, for the given writable accounts.
    ///
    /// Contention is PER ACCOUNT — the fee to land ahead of everyone buying one
    /// hot launch says nothing about a quiet coin — so the caller passes the
    /// accounts its trade actually writes.
    ///
    /// Two sources, because they answer different questions.
    /// `getPriorityFeeEstimate` (Helius) reports what competitive transactions
    /// paid, which is what decides whether we land FIRST. The standard
    /// `getRecentPrioritizationFees` reports the floor of what landed at all,
    /// and since most blocks aren't full that floor is usually zero — useful
    /// only as a fallback on endpoints without the Helius method.
    pub async fn priority_fee_micro(&self, accounts: &[Pubkey], level: &str) -> Option<u64> {
        let keys: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        let params = json!([{
            "accountKeys": keys,
            "options": { "includeAllPriorityFeeLevels": true },
        }]);
        // Only ask for the Helius method when a Helius endpoint is in the
        // pool. Asking everyone anyway meant, every 1.5s poll, two full passes
        // over endpoints that can only answer "method not found" — plus the
        // 250ms between-pass sleep — before falling back. Four doomed requests
        // per poll, forever, out of the same budget the tape needs.
        if self.has_helius() {
            if let Ok(v) =
                self.call_on("getPriorityFeeEstimate", params, |u| u.contains("helius")).await
            {
                if let Some(f) = v
                    .get("priorityFeeLevels")
                    .and_then(|l| l.get(level))
                    .and_then(|f| f.as_f64())
                {
                    // The levels are floats and `unsafeMax` reaches 4.6e10 —
                    // cast through f64 deliberately; the caller's cap decides.
                    if f.is_finite() && f >= 0.0 {
                        return Some(f as u64);
                    }
                }
            }
        }
        // Fallback: the 75th percentile of the slots that actually paid
        // something. Averaging in the zero-fee slots would return zero on a
        // contested account and quietly turn priority off.
        let v = self.call("getRecentPrioritizationFees", json!([keys])).await.ok()?;
        let mut paid: Vec<u64> = v
            .as_array()?
            .iter()
            .filter_map(|e| e.get("prioritizationFee").and_then(|f| f.as_u64()))
            .filter(|f| *f > 0)
            .collect();
        if paid.is_empty() {
            return Some(0);
        }
        paid.sort_unstable();
        Some(paid[paid.len() * 3 / 4])
    }

    /// Recent transaction signatures touching `addr`, newest first.
    pub async fn signatures_for(&self, addr: &Pubkey, limit: u32) -> eyre::Result<Vec<String>> {
        let res = self
            // `confirmed`, matching `transaction()`. The default is `finalized`,
            // which lags confirmed by ~13s — new trades were simply not visible
            // yet while every explorer already showed them.
            .call(
                "getSignaturesForAddress",
                json!([addr.to_string(), {"limit": limit, "commitment": "confirmed"}]),
            )
            .await?;
        Ok(res
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("signature").and_then(|s| s.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// A transaction in `json` encoding. `maxSupportedTransactionVersion` is
    /// required — without it v0 transactions error out instead of decoding.
    pub async fn transaction(&self, sig: &str) -> eyre::Result<Value> {
        self.call(
            "getTransaction",
            json!([sig, {"maxSupportedTransactionVersion": 0, "encoding": "json", "commitment": "confirmed"}]),
        )
        .await
    }

    /// A whole round of transactions in ONE HTTP request, via JSON-RPC
    /// batching. Fetching them one request each capped the tape at ~13 tx/s
    /// on paper and far less under real latency — a busy launch outran the
    /// fetcher, signatures scrolled past the window, and whole seconds of
    /// trades were simply never seen. Index-aligned with `sigs`; a sub-answer
    /// that failed (or a tx not yet visible) is `None`.
    pub async fn transactions(&self, sigs: &[String]) -> Vec<Option<Value>> {
        if sigs.is_empty() {
            return Vec::new();
        }
        let body: Vec<Value> = sigs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                json!({
                    "jsonrpc": "2.0", "id": i, "method": "getTransaction",
                    "params": [s, {"maxSupportedTransactionVersion": 0, "encoding": "json", "commitment": "confirmed"}]
                })
            })
            .collect();
        let mut out: Vec<Option<Value>> = vec![None; sigs.len()];
        // Fastest rested endpoint first, same ordering the single-call path
        // uses; one retry on the next endpoint if the whole batch failed.
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut order: Vec<usize> = (0..self.urls.len()).collect();
        order.sort_by_key(|&i| {
            (self.ewma_us[i].load(std::sync::atomic::Ordering::Relaxed), (start + i) % self.urls.len())
        });
        for &i in &order {
            if self.cooldown[i].load(std::sync::atomic::Ordering::Relaxed) > now_ms()
                && order.iter().any(|&j| {
                    self.cooldown[j].load(std::sync::atomic::Ordering::Relaxed) <= now_ms()
                })
            {
                continue;
            }
            let url = &self.urls[i];
            let t0 = std::time::Instant::now();
            let resp = tokio::time::timeout(RPC_TIMEOUT, self.http.post(url).json(&body).send()).await;
            let Ok(Ok(resp)) = resp else {
                crate::rpcstats::record("getTransaction(batch)", false, t0.elapsed(), Some("send failed"));
                continue;
            };
            if resp.status().as_u16() == 429 {
                self.cooldown[i]
                    .store(now_ms() + COOLDOWN_LIMITED_MS, std::sync::atomic::Ordering::Relaxed);
                crate::rpcstats::record("getTransaction(batch)", false, t0.elapsed(), Some("http 429"));
                continue;
            }
            let Ok(items) = resp.json::<Vec<Value>>().await else {
                crate::rpcstats::record("getTransaction(batch)", false, t0.elapsed(), Some("bad batch body"));
                continue;
            };
            for item in items {
                let (Some(id), Some(res)) = (
                    item.get("id").and_then(|v| v.as_u64()),
                    item.get("result").filter(|r| !r.is_null()),
                ) else {
                    continue;
                };
                if let Some(slot) = out.get_mut(id as usize) {
                    *slot = Some(res.clone());
                }
            }
            crate::rpcstats::record("getTransaction(batch)", true, t0.elapsed(), None);
            let us = t0.elapsed().as_micros() as u64;
            let prev = self.ewma_us[i].load(std::sync::atomic::Ordering::Relaxed);
            self.ewma_us[i].store(
                if prev == 0 { us } else { (prev * 7 + us) / 8 },
                std::sync::atomic::Ordering::Relaxed,
            );
            return out;
        }
        out
    }

    /// Current slot — the Solana analogue of the EVM block height shown in the
    /// header, so "is the chain moving" is visible at a glance.
    pub async fn slot(&self) -> eyre::Result<u64> {
        let res = self.call("getSlot", json!([])).await?;
        Ok(res.as_u64().unwrap_or(0))
    }

    /// Program accounts of an exact size whose bytes at `offset` equal `key`.
    ///
    /// Used to locate a coin's PumpSwap pool: the pool PDA is seeded with a
    /// creator+index we don't know for an arbitrary mint, so it's found by
    /// matching `base_mint` inside the account instead.
    pub async fn program_accounts_memcmp(
        &self,
        program: &Pubkey,
        disc: &[u8; 8],
        offset: usize,
        key: &Pubkey,
    ) -> eyre::Result<Vec<(Pubkey, Vec<u8>)>> {
        // Filter on the account DISCRIMINATOR, not on an exact dataSize.
        // Programs append fields across upgrades: PumpSwap pools exist at both
        // 261 and 301 bytes on mainnet today, and a hard-coded size silently
        // hides every newer pool. The discriminator is what actually identifies
        // the account type, and it survives layout growth.
        // Retried, because this specific call is the one providers ration.
        //
        // `getProgramAccounts` scans a whole program's accounts, so it runs
        // against a separate index that is throttled on its own — a node can be
        // answering everything else in 500ms and still refuse this with
        // "account index service overloaded, please try again". That message is
        // an instruction, and surfacing it as "could not load" ignored it.
        //
        // Only for the overload case: a malformed filter or a bad program id
        // fails the same way every time, and retrying those just spends the
        // budget three times before saying so.
        let params = json!([program.to_string(), {
            "encoding": "base64",
            "filters": [
                {"memcmp": {"offset": 0, "bytes": bs58::encode(disc).into_string()}},
                {"memcmp": {"offset": offset, "bytes": key.to_string()}}
            ]
        }]);
        let mut res = None;
        let mut last: Option<eyre::Report> = None;
        for attempt in 0..3u32 {
            match self.call("getProgramAccounts", params.clone()).await {
                Ok(v) => {
                    res = Some(v);
                    break;
                }
                Err(e) => {
                    let overloaded = {
                        let m = e.to_string().to_lowercase();
                        m.contains("overloaded") || m.contains("please try again")
                    };
                    last = Some(e);
                    if !overloaded || attempt == 2 {
                        break;
                    }
                    // Short, growing: the index is busy, not broken.
                    tokio::time::sleep(std::time::Duration::from_millis(400 << attempt)).await;
                }
            }
        }
        let res = match res {
            Some(v) => v,
            None => return Err(last.unwrap_or_else(|| eyre::eyre!("getProgramAccounts failed"))),
        };
        let mut out = Vec::new();
        for item in res.as_array().into_iter().flatten() {
            let Some(pk) = item.get("pubkey").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()) else {
                continue;
            };
            let Some(b64) = item.get("account").and_then(|a| a.get("data")).and_then(|d| d.get(0)).and_then(|d| d.as_str())
            else {
                continue;
            };
            if let Ok(bytes) = b64_decode(b64) {
                out.push((pk, bytes));
            }
        }
        Ok(out)
    }

    /// A sample of a program's accounts of one type — used by tests to pick a
    /// real account without knowing a specific mint. Discriminator-matched for
    /// the same reason as `program_accounts_memcmp`: sizes change, types don't.
    pub async fn program_accounts_memcmp_any(
        &self,
        program: &Pubkey,
        disc: &[u8; 8],
    ) -> eyre::Result<Vec<(Pubkey, Vec<u8>)>> {
        let res = self
            .call(
                "getProgramAccounts",
                json!([program.to_string(), {
                    "encoding": "base64",
                    "filters": [{"memcmp": {"offset": 0, "bytes": bs58::encode(disc).into_string()}}]
                }]),
            )
            .await?;
        let mut out = Vec::new();
        for item in res.as_array().into_iter().flatten().take(40) {
            let Some(pk) = item.get("pubkey").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()) else {
                continue;
            };
            let Some(b64) = item.get("account").and_then(|a| a.get("data")).and_then(|d| d.get(0)).and_then(|d| d.as_str())
            else {
                continue;
            };
            if let Ok(bytes) = b64_decode(b64) {
                out.push((pk, bytes));
            }
        }
        Ok(out)
    }

    /// Latest blockhash, for transaction assembly.
    pub async fn latest_blockhash(&self) -> eyre::Result<String> {
        let res = self
            .call("getLatestBlockhash", json!([{"commitment": "finalized"}]))
            .await?;
        res.get("value")
            .and_then(|v| v.get("blockhash"))
            .and_then(|b| b.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| eyre::eyre!("getLatestBlockhash: no blockhash"))
    }

    /// Submit a signed transaction. `wire` is the serialized transaction; it is
    /// sent base64 and the encoding is stated explicitly — pump's docs call out
    /// that relying on the RPC's default encoding causes silent failures.
    /// Simulate a signed transaction and return the POST-state token amount
    /// (base units) of `ata`. This is the chain quoting its own swap: fee
    /// tiers, boost reserves, whatever pump invents next — the simulation
    /// already priced it. `None` on any failure; callers fall back to math.
    pub async fn simulate_post_token(
        &self,
        wire: &[u8],
        ata: &Pubkey,
    ) -> Option<u64> {
        let res = self
            .call(
                "simulateTransaction",
                json!([
                    b64_encode(wire),
                    {
                        "encoding": "base64",
                        "sigVerify": false,
                        "replaceRecentBlockhash": true,
                        "commitment": "processed",
                        "accounts": {"encoding": "base64", "addresses": [ata.to_string()]}
                    }
                ]),
            )
            .await
            .ok()?;
        let val = res.get("value")?;
        if !val.get("err").map(|e| e.is_null()).unwrap_or(false) {
            return None;
        }
        let acc = val.get("accounts")?.get(0)?;
        let data = b64_decode(acc.get("data")?.get(0)?.as_str()?).ok()?;
        // SPL token account layout: mint(32) owner(32) amount(u64 LE).
        data.get(64..72).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes)
    }

    /// Dry-run a signed transaction. `Some(err)` means it would fail.
    ///
    /// Cheaper than finding out on-chain, and the only pre-flight available for
    /// a transaction this app did not build itself — see `jupiter`.
    pub async fn simulate_err(&self, wire: &[u8]) -> eyre::Result<Option<String>> {
        let res = self
            .call(
                "simulateTransaction",
                json!([
                    b64_encode(wire),
                    {
                        "encoding": "base64",
                        // The signature is ours and already checked; what is
                        // being tested here is whether the ROUTE executes.
                        "sigVerify": false,
                        "replaceRecentBlockhash": true,
                        "commitment": "processed"
                    }
                ]),
            )
            .await?;
        let Some(val) = res.get("value") else {
            return Ok(Some("simulation returned nothing".into()));
        };
        match val.get("err") {
            Some(e) if !e.is_null() => Ok(Some(e.to_string())),
            _ => Ok(None),
        }
    }

    /// Every SPL token this wallet holds: (mint, base amount, decimals).
    ///
    /// Read from the chain rather than from anything the app remembers, because
    /// a wallet holds what it holds — USDC arrived by transfer, not by a trade
    /// this app saw, and nothing in its own records would ever mention it.
    /// Both token programs, because a wallet can hold either.
    ///
    /// Asking only the classic program misses every Token-2022 mint — and
    /// pump.fun issues those, so "everything you hold" would silently omit
    /// real holdings. The program comes back with each mint because it decides
    /// the associated-account address: derive it against the wrong one and the
    /// transfer targets an account that does not exist.
    pub async fn owned_tokens(
        &self,
        owner: &Pubkey,
    ) -> eyre::Result<Vec<(Pubkey, u64, u32, Pubkey)>> {
        let mut all = Vec::new();
        for program in [super::TOKEN_PROGRAM, super::TOKEN_2022_PROGRAM] {
            all.extend(self.owned_tokens_of(owner, &program).await.unwrap_or_default());
        }
        Ok(all)
    }

    async fn owned_tokens_of(
        &self,
        owner: &Pubkey,
        program: &Pubkey,
    ) -> eyre::Result<Vec<(Pubkey, u64, u32, Pubkey)>> {
        let res = self
            .call(
                "getTokenAccountsByOwner",
                json!([
                    owner.to_string(),
                    {"programId": program.to_string()},
                    // Parsed: the amount and decimals come back named rather
                    // than as offsets into a byte array we would have to keep
                    // in step with the token program.
                    {"encoding": "jsonParsed", "commitment": "confirmed"}
                ]),
            )
            .await?;
        let mut out = Vec::new();
        for item in res.get("value").and_then(|v| v.as_array()).into_iter().flatten() {
            let info = item
                .get("account")
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("parsed"))
                .and_then(|p| p.get("info"));
            let Some(info) = info else { continue };
            let Some(mint) = info.get("mint").and_then(|m| m.as_str()).and_then(|s| s.parse().ok())
            else {
                continue;
            };
            let amt = info.get("tokenAmount");
            let raw = amt
                .and_then(|a| a.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let dec = amt.and_then(|a| a.get("decimals")).and_then(|d| d.as_u64()).unwrap_or(0) as u32;
            // An empty account is a leftover, not a holding.
            if raw > 0 {
                out.push((mint, raw, dec, *program));
            }
        }
        Ok(out)
    }

    /// Whether an account is a program. A transfer to one is unrecoverable, so
    /// this is checked before a destination is accepted.
    pub async fn is_executable(&self, key: &Pubkey) -> bool {
        self.call("getAccountInfo", json!([key.to_string(), {"encoding": "base64"}]))
            .await
            .ok()
            .and_then(|r| r.get("value").cloned())
            .and_then(|v| v.get("executable").and_then(|e| e.as_bool()))
            .unwrap_or(false)
    }

    pub async fn send_transaction(&self, wire: &[u8]) -> eyre::Result<String> {
        let res = self
            .call(
                "sendTransaction",
                json!([
                    b64_encode(wire),
                    {"encoding": "base64", "skipPreflight": false, "preflightCommitment": "processed", "maxRetries": 3}
                ]),
            )
            .await?;
        res.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| eyre::eyre!("sendTransaction: no signature returned"))
    }

    /// Confirmation state of a signature: `Ok(Some(true))` = confirmed and
    /// successful, `Ok(Some(false))` = landed but failed, `Ok(None)` = still
    /// pending / unknown.
    pub async fn signature_ok(&self, sig: &str) -> eyre::Result<Option<bool>> {
        Ok(self.signatures_ok(std::slice::from_ref(&sig.to_string())).await?.pop().flatten())
    }

    /// Statuses for a whole batch of signatures in ONE request — the method
    /// takes an array natively. Polling per signature was N requests every
    /// 800ms where one carries them all.
    ///
    /// Result is index-aligned with `sigs`: `None` = not landed (yet),
    /// `Some(true)` = landed clean, `Some(false)` = landed but reverted.
    pub async fn signatures_ok(&self, sigs: &[String]) -> eyre::Result<Vec<Option<bool>>> {
        if sigs.is_empty() {
            return Ok(Vec::new());
        }
        let res = self
            .call("getSignatureStatuses", json!([sigs, {"searchTransactionHistory": false}]))
            .await?;
        let vals = res.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        Ok((0..sigs.len())
            .map(|k| {
                let entry = vals.get(k).cloned().unwrap_or(Value::Null);
                if entry.is_null() {
                    return None;
                }
                // `confirmationStatus` reaching processed/confirmed/finalized
                // means it landed; `err` non-null means it landed but reverted.
                let landed = entry
                    .get("confirmationStatus")
                    .and_then(|c| c.as_str())
                    .map(|s| matches!(s, "processed" | "confirmed" | "finalized"))
                    .unwrap_or(false);
                if !landed {
                    return None;
                }
                Some(entry.get("err").map(|e| e.is_null()).unwrap_or(true))
            })
            .collect())
    }
}

// ---- base64 (tiny, dependency-free) --------------------------------------
// Only used for account data and transaction wire format, so a full base64
// crate isn't worth another dependency.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn b64_decode(s: &str) -> eyre::Result<Vec<u8>> {
    let val = |c: u8| -> eyre::Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => eyre::bail!("bad base64 byte {c:#x}"),
        })
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=' && !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_including_padding() {
        // Lengths 0..=3 mod 3 cover every padding case.
        for n in 0..12usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let enc = b64_encode(&data);
            assert_eq!(b64_decode(&enc).unwrap(), data, "roundtrip failed for len {n}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    #[test]
    fn rotation_spreads_across_endpoints() {
        let rpc = Rpc::new_pool(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(rpc.endpoint_count(), 3);
        // Each call advances the cursor, so load spreads rather than pinning
        // every request on the first provider.
        let picks: Vec<usize> = (0..6)
            .map(|_| rpc.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 3)
            .collect();
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn empty_pool_is_still_usable() {
        // Never panic on a missing config — one bad endpoint that errors is
        // easier to diagnose than a crash at startup.
        assert_eq!(Rpc::new_pool(vec![]).endpoint_count(), 1);
    }

    /// A dead endpoint must fail over to a live one rather than erroring out.
    #[tokio::test]
    #[ignore]
    async fn live_failover_skips_a_dead_endpoint() {
        let rpc = Rpc::new_pool(vec![
            "http://127.0.0.1:1".into(), // always refuses
            "https://api.mainnet-beta.solana.com".into(),
        ]);
        let slot = rpc.slot().await.expect("should fail over to the live endpoint");
        println!("slot via failover: {slot}");
        assert!(slot > 0);
    }
}
