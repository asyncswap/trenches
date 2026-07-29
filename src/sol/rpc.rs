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
    http: reqwest::Client,
}

impl Rpc {
    pub fn new(url: impl Into<String>) -> Rpc {
        Rpc::new_pool(vec![url.into()])
    }

    /// Build over several endpoints. Empty input is treated as a single bad URL
    /// so callers never have to handle a "no endpoints" case.
    pub fn new_pool(urls: Vec<String>) -> Rpc {
        let urls = if urls.is_empty() { vec![String::new()] } else { urls };
        Rpc {
            urls,
            next: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            http: reqwest::Client::new(),
        }
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
        /// Host only — an API key must never reach a log line.
        fn safe_host(url: &str) -> &str {
            url.split('?').next().unwrap_or(url)
        }

        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut last_err = None;

        // Two passes over the endpoints. One pass is not enough: providers
        // rate-limit in bursts, and when every endpoint 429s at the same instant
        // the round returns nothing — which the tape cannot distinguish from
        // "no trades happened". A brief pause is usually all it takes.
        for pass in 0..2 {
            for hop in 0..self.urls.len() {
                let url = &self.urls[(start + hop) % self.urls.len()];
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
        if let Ok(v) = self.call("getPriorityFeeEstimate", params).await {
            if let Some(f) = v
                .get("priorityFeeLevels")
                .and_then(|l| l.get(level))
                .and_then(|f| f.as_f64())
            {
                // The levels are floats and `unsafeMax` reaches 4.6e10 — cast
                // through f64 deliberately, and let the caller's cap decide.
                if f.is_finite() && f >= 0.0 {
                    return Some(f as u64);
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
        let res = self
            .call(
                "getProgramAccounts",
                json!([program.to_string(), {
                    "encoding": "base64",
                    "filters": [
                        {"memcmp": {"offset": 0, "bytes": bs58::encode(disc).into_string()}},
                        {"memcmp": {"offset": offset, "bytes": key.to_string()}}
                    ]
                }]),
            )
            .await?;
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
        let res = self
            .call("getSignatureStatuses", json!([[sig], {"searchTransactionHistory": false}]))
            .await?;
        let entry = res.get("value").and_then(|v| v.get(0)).cloned().unwrap_or(Value::Null);
        if entry.is_null() {
            return Ok(None);
        }
        // `confirmationStatus` reaching processed/confirmed/finalized means it
        // landed; `err` non-null means it landed but reverted.
        let landed = entry
            .get("confirmationStatus")
            .and_then(|c| c.as_str())
            .map(|s| matches!(s, "processed" | "confirmed" | "finalized"))
            .unwrap_or(false);
        if !landed {
            return Ok(None);
        }
        Ok(Some(entry.get("err").map(|e| e.is_null()).unwrap_or(true)))
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
